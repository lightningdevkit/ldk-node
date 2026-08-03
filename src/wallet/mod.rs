// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_chain::ChainPosition;
use bdk_wallet::descriptor::ExtendedDescriptor;
use bdk_wallet::error::{BuildFeeBumpError, CreateTxError};
#[allow(deprecated)]
use bdk_wallet::SignOptions;
use bdk_wallet::{Balance, KeychainKind, LocalOutput, PersistedWallet, Update, WalletEvent};
use bitcoin::address::NetworkUnchecked;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::{self, Psbt};
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{All, PublicKey, Scalar, Secp256k1, SecretKey};
use bitcoin::transaction::Sequence;
use bitcoin::{
	Address, Amount, FeeRate, OutPoint, ScriptBuf, SignedAmount, Transaction, TxOut, Txid,
	WPubkeyHash, Weight, WitnessProgram, WitnessVersion,
};
use lightning::chain::chaininterface::{
	ChannelFunding, FundingCandidate, FundingPurpose, TransactionType as LdkTransactionType,
	INCREMENTAL_RELAY_FEE_SAT_PER_1000_WEIGHT,
};
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::transaction::OutPoint as LdkOutPoint;
use lightning::chain::{BlockLocator, ClaimId, Listen};
use lightning::ln::channel_state::{SpliceCandidateDetails, SpliceCandidateStatus, SpliceDetails};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::ln::script::ShutdownScript;
use lightning::ln::types::ChannelId;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeysManager, NodeSigner, OutputSpender,
	PeerStorageKey, Recipient, SignerProvider, SpendableOutputDescriptor,
};
use lightning::util::message_signing;
use lightning::util::wallet_utils::{
	CoinSelection, CoinSelectionSource, ConfirmedUtxo, Input, Utxo, WalletSource,
};
use lightning_invoice::RawBolt11Invoice;
use persist::KVStoreWalletPersister;

use crate::config::{Config, ADDRESS_POOL_SIZE};
use crate::data_store::StorableObject;
#[cfg(test)]
use crate::data_store::{KeepAllEntries, KeepLeastRecentlyUsed};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, log_trace, log_warn, LdkLogger, Logger};
use crate::payment::pending_payment_store::PendingPaymentDetailsUpdate;
use crate::payment::store::{ConfirmationStatus, PaymentDetailsUpdate};
use crate::payment::{
	FundingTxCandidate, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
	PendingPaymentDetails, TransactionType,
};
use crate::runtime::Runtime;
use crate::types::{Broadcaster, PaymentStore, PendingPaymentStore};
use crate::{ChainSource, Error};

pub(crate) enum OnchainSendAmount {
	ExactRetainingReserve { amount_sats: u64, cur_anchor_reserve_sats: u64 },
	AllRetainingReserve { cur_anchor_reserve_sats: u64 },
	AllDrainingReserve,
}

pub(crate) enum FundingAmount {
	Exact { amount_sats: u64 },
	Max,
}

pub(crate) mod persist;
pub(crate) mod ser;

const DUST_LIMIT_SATS: u64 = 546;

/// The number of external addresses kept revealed, persisted, and ready for handout via
/// [`Wallet::pop_pooled_address`] and [`Wallet::get_new_address`].
///
/// Each channel open consumes two pooled addresses (one for the destination script and one for
/// the upfront shutdown script), and the pool is refilled after every handout, so this bounds
/// how many channels can be opened while wallet persistence is unavailable rather than steady
/// state throughput.
///
/// Pooled addresses are revealed-but-unused, widening what incremental chain syncs must watch.
/// Handouts consume the pool oldest first, which keeps the pool beyond the handed-out
/// addresses, where it cannot hide funds from a full scan's stop gap — except transiently when
/// a handout fails while a concurrent one proceeds (see [`Wallet::get_new_address`]); such
/// inversions are bounded by the pool size and consumed by the next handouts. Handed-out
/// scripts that have yet to appear on-chain (e.g. the shutdown scripts of open channels) count
/// against the configured gap, as they did before the pool existed.
pub(crate) const ADDRESS_POOL_TARGET_SIZE: usize = ADDRESS_POOL_SIZE as usize;

/// A pool of pre-revealed external addresses whose derivation indices are already persisted,
/// allowing LDK's synchronous [`SignerProvider`] callbacks to obtain fresh addresses without
/// waiting on wallet persistence.
struct AddressPool {
	/// Addresses ready for handout: their reveal is durably persisted, so every chain sync path
	/// watches their scripts.
	available: VecDeque<(u32, bitcoin::Address)>,
	/// Addresses revealed in-memory whose persistence has not succeeded yet. They are published
	/// to `available` by the next successful [`Wallet::refill_address_pool`] run.
	unpublished: Vec<(u32, bitcoin::Address)>,
}

impl AddressPool {
	/// Rebuilds the pool from its persisted derivation indices; reloading them is what keeps
	/// restarts from burning fresh derivation indices on every run.
	fn new(
		persisted_indices: Vec<u32>, wallet: &PersistedWallet<KVStoreWalletPersister>,
		logger: &Logger,
	) -> Self {
		let last_revealed = wallet.derivation_index(KeychainKind::External);
		let mut available = VecDeque::new();
		for index in persisted_indices {
			// Only trust indices the persisted wallet actually revealed: anything beyond
			// `last_revealed` would hand out a script no chain sync path watches. The record is
			// written before the wallet change set it references, so a crash between the writes
			// leaves it listing indices the wallet never revealed; the next refill re-derives
			// them.
			if last_revealed.map_or(false, |last| index <= last) {
				let address = wallet.peek_address(KeychainKind::External, index).address;
				available.push_back((index, address));
			} else {
				log_error!(
					logger,
					"Dropping persisted address pool index {} beyond the wallet's last revealed index",
					index
				);
			}
		}
		Self { available, unpublished: Vec::new() }
	}
}

pub(crate) struct Wallet {
	// A BDK on-chain wallet.
	inner: Mutex<PersistedWallet<KVStoreWalletPersister>>,
	persister: tokio::sync::Mutex<KVStoreWalletPersister>,
	address_pool: Mutex<AddressPool>,
	// Serializes refill runs so concurrent pops never over-reveal.
	address_pool_refill_lock: tokio::sync::Mutex<()>,
	broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	chain_source: Arc<ChainSource>,
	payment_store: Arc<PaymentStore>,
	runtime: Arc<Runtime>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	pending_payment_store: Arc<PendingPaymentStore>,
	// Serializes the writers that must observe the payment record and its pending-store entry
	// (candidate history included) as one consistent unit: classification and wallet sync's event
	// arms each hold it from payment-id resolution through their last write (classification's
	// being its two-store pair). Without it, a confirmation landing between classification's two
	// writes sees the record classified but the candidate history absent — resolving the wrong
	// payment id or stamping the confirmed candidate with another candidate's figures — and a
	// classification landing inside an arm's decision sequence gets overwritten by the arm's
	// stale generic fallback. Graduation stays off this lock: it decides from the live record
	// under the payment store's mutation lock and writes only the status, so it carries nothing
	// a concurrent classification could lose.
	funding_payment_update_lock: tokio::sync::Mutex<()>,
}

impl Wallet {
	pub(crate) fn new(
		wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		wallet_persister: KVStoreWalletPersister, persisted_pool_indices: Vec<u32>,
		broadcaster: Arc<Broadcaster>, fee_estimator: Arc<OnchainFeeEstimator>,
		chain_source: Arc<ChainSource>, payment_store: Arc<PaymentStore>, runtime: Arc<Runtime>,
		config: Arc<Config>, logger: Arc<Logger>, pending_payment_store: Arc<PendingPaymentStore>,
	) -> Self {
		let address_pool = Mutex::new(AddressPool::new(persisted_pool_indices, &wallet, &logger));
		let inner = Mutex::new(wallet);
		let persister = tokio::sync::Mutex::new(wallet_persister);
		let address_pool_refill_lock = tokio::sync::Mutex::new(());
		Self {
			inner,
			persister,
			address_pool,
			address_pool_refill_lock,
			broadcaster,
			fee_estimator,
			chain_source,
			payment_store,
			runtime,
			config,
			logger,
			pending_payment_store,
			funding_payment_update_lock: tokio::sync::Mutex::new(()),
		}
	}

	pub(crate) fn get_full_scan_request(&self) -> FullScanRequest<KeychainKind> {
		self.inner.lock().expect("lock").start_full_scan().build()
	}

	pub(crate) fn get_incremental_sync_request(&self) -> SyncRequest<(KeychainKind, u32)> {
		self.inner.lock().expect("lock").start_sync_with_revealed_spks().build()
	}

	pub(crate) fn get_cached_txs(&self) -> Vec<Arc<Transaction>> {
		self.inner.lock().expect("lock").tx_graph().full_txs().map(|tx_node| tx_node.tx).collect()
	}

	#[cfg(feature = "chain-bitcoind")]
	pub(crate) fn get_unconfirmed_txids(&self) -> Vec<Txid> {
		self.inner
			.lock()
			.expect("lock")
			.transactions()
			.filter(|t| t.chain_position.is_unconfirmed())
			.map(|t| t.tx_node.txid)
			.collect()
	}

	#[cfg(feature = "chain-bitcoind")]
	pub(crate) fn current_best_block(&self) -> BlockLocator {
		let checkpoint = self.inner.lock().expect("lock").latest_checkpoint();
		let mut current_block = Some(checkpoint.clone());
		let previous_blocks = std::array::from_fn(|_| {
			let child = current_block.take()?;
			// BDK's checkpoint chain may be sparse; only accept contiguous parents.
			let parent = child.prev().filter(|cp| cp.height() + 1 == child.height())?;
			current_block = Some(parent.clone());
			Some(parent.hash())
		});
		BlockLocator { block_hash: checkpoint.hash(), height: checkpoint.height(), previous_blocks }
	}

	pub(crate) async fn apply_update(&self, update: impl Into<Update>) -> Result<(), Error> {
		let mut locked_persister = self.persister.lock().await;
		let events = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			match locked_wallet.apply_update_events(update) {
				Ok(events) => events,
				Err(e) => {
					log_error!(self.logger, "Sync failed due to chain connection error: {}", e);
					return Err(Error::WalletOperationFailed);
				},
			}
		};
		self.update_payment_store(events).await.map_err(|e| {
			log_error!(self.logger, "Failed to update payment store: {}", e);
			Error::PersistenceFailed
		})?;

		let change_set = self.inner.lock().expect("lock").take_staged().unwrap_or_default();
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(())
	}

	#[cfg(feature = "chain-bitcoind")]
	pub(crate) async fn apply_mempool_txs(
		&self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		if unconfirmed_txs.is_empty() && evicted_txids.is_empty() {
			return Ok(());
		}

		let mut locked_persister = self.persister.lock().await;
		let events = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			locked_wallet
				.events_helper(|wallet| -> Result<(), std::convert::Infallible> {
					wallet.apply_unconfirmed_txs(unconfirmed_txs);
					wallet.apply_evicted_txs(evicted_txids);
					Ok(())
				})
				.expect("applying mempool updates cannot fail")
		};

		self.update_payment_store(events).await.map_err(|e| {
			log_error!(self.logger, "Failed to update payment store: {}", e);
			Error::PersistenceFailed
		})?;

		let change_set = self.inner.lock().expect("lock").take_staged().unwrap_or_default();
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		Ok(())
	}

	async fn update_payment_store(&self, mut events: Vec<WalletEvent>) -> Result<(), Error> {
		if events.is_empty() {
			return Ok(());
		}

		// Sort events to ensure proper sequencing for data consistency:
		// 1. TXReplaced (0) before TxUnconfirmed (1) - Critical for RBF handling
		//    When a transaction is replaced via RBF, both events fire. Processing
		//    TXReplaced first stores the replaced transaction, allowing TxUnconfirmed
		//    to detect and skip duplicate payment record creation.
		// 2. TxConfirmed (2) before ChainTipChanged (3) - Ensures height accuracy
		//    ChainTipChanged updates block height. Processing TxConfirmed first ensures
		//    it references the correct height for confirmation depth calculations.
		// 3. Other events follow in deterministic order for predictable processing
		if events.len() > 1 {
			events.sort_by_key(|e| match e {
				WalletEvent::TxReplaced { .. } => 0,
				WalletEvent::TxUnconfirmed { .. } => 1,
				WalletEvent::TxConfirmed { .. } => 2,
				WalletEvent::ChainTipChanged { .. } => 3,
				WalletEvent::TxDropped { .. } => 4,
				_ => 5,
			});
		}

		for event in events {
			match event {
				WalletEvent::TxConfirmed { txid, tx, block_time, .. } => {
					let cur_height = self.inner.lock().expect("lock").latest_checkpoint().height();
					let confirmation_height = block_time.block_id.height;
					let payment_status = if cur_height >= confirmation_height + ANTI_REORG_DELAY - 1
					{
						PaymentStatus::Succeeded
					} else {
						PaymentStatus::Pending
					};

					let confirmation_status = ConfirmationStatus::Confirmed {
						block_hash: block_time.block_id.hash,
						height: confirmation_height,
						timestamp: block_time.confirmation_time,
					};

					// Hold the cross-store lock from payment-id resolution through the last write:
					// a classification landing in between would leave the id resolved against a
					// torn candidate index and the generic fallback below overwriting (or
					// duplicating) the record classification just wrote.
					let guard = self.funding_payment_update_lock.lock().await;

					let mut payment_id = self
						.find_payment_by_txid(txid)
						.await?
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					match self
						.apply_funding_status_update_locked(
							&guard,
							payment_id,
							txid,
							confirmation_status,
						)
						.await?
					{
						FundingStatusUpdate::Applied => continue,
						FundingStatusUpdate::NotFunding => {},
						// Not part of the funding payment's history (e.g. a close spending the
						// funding outpoint): record it under its own id below instead.
						FundingStatusUpdate::Foreign => {
							payment_id = PaymentId(txid.to_byte_array());
						},
					}

					// The fallback id belongs to a settled funding payment whose entry is gone:
					// skip rather than resurrect it (see `has_funding_record`).
					if self.has_funding_record(&payment_id).await? {
						log_debug!(
							self.logger,
							"Skipping wallet event for transaction {} of a settled funding payment",
							txid,
						);
						continue;
					}

					let payment = {
						let locked_wallet = self.inner.lock().expect("lock");
						self.create_payment_from_tx(
							&locked_wallet,
							txid,
							payment_id,
							&tx,
							payment_status,
							confirmation_status,
						)
					};

					self.payment_store.insert_or_update(payment.clone()).await?;

					if payment_status == PaymentStatus::Pending {
						self.upsert_pending_payment(payment, Vec::new()).await?;
					}
				},
				WalletEvent::ChainTipChanged { new_tip, .. } => {
					let pending_payments: Vec<PendingPaymentDetails> = self
						.pending_payment_store
						.list_filter(|p| match p.details() {
							// A pre-broadcast splice intent carries no payment yet and cannot
							// graduate.
							None => false,
							Some(details) => {
								debug_assert!(
									details.status == PaymentStatus::Pending,
									"Non-pending payment {:?} found in pending store",
									details.id,
								);
								details.status == PaymentStatus::Pending
									&& matches!(details.kind, PaymentKind::Onchain { .. })
							},
						})
						.await;

					let mut unconfirmed_outbound_txids: Vec<Txid> = Vec::new();

					for payment in pending_payments {
						// The filter admits only Tracked funding payments.
						let PendingPaymentDetails::Tracked { ref details, .. } = payment else {
							continue;
						};
						match details.kind {
							PaymentKind::Onchain {
								status: ConfirmationStatus::Confirmed { height, .. },
								..
							} => {
								let payment_id = details.id;
								if new_tip.height >= height + ANTI_REORG_DELAY - 1 {
									// Graduate from the live record, not the snapshot listed
									// above: a classification landing since then must not have
									// its figures rolled back. The status-only update carries
									// no figures/txid/confirmation, so nothing a concurrent
									// writer wrote can be clobbered; the update machinery bumps
									// `latest_update_timestamp` and no-ops when the record is
									// already `Succeeded`. A record that has diverged from the
									// snapshot (or was removed) declines, leaving future
									// events to drive it.
									let mut graduated = false;
									self.payment_store
										.mutate(&payment_id, |existing| {
											let current = existing?;
											match current.kind {
												PaymentKind::Onchain {
													status:
														ConfirmationStatus::Confirmed { height, .. },
													..
												} if new_tip.height
													>= height + ANTI_REORG_DELAY - 1 =>
												{
													graduated = true;
													let mut update =
														PaymentDetailsUpdate::new(payment_id);
													update.status = Some(PaymentStatus::Succeeded);
													let mut updated = current.clone();
													updated.update(update).then_some(updated)
												},
												_ => None,
											}
										})
										.await?;
									if graduated {
										self.pending_payment_store.remove(&payment_id).await?;
									}
								}
							},
							PaymentKind::Onchain {
								txid,
								status: ConfirmationStatus::Unconfirmed,
								..
							} => {
								if self
									.fail_funding_payment_lost_to_conflict(&payment, new_tip.height)
									.await?
								{
									continue;
								}
								if details.direction == PaymentDirection::Outbound {
									unconfirmed_outbound_txids.push(txid);
								}
							},
							_ => {},
						}
					}

					if !unconfirmed_outbound_txids.is_empty() {
						let txs_to_broadcast: Vec<Transaction> = {
							let locked_wallet = self.inner.lock().expect("lock");
							unconfirmed_outbound_txids
								.iter()
								.filter_map(|txid| {
									locked_wallet
										.get_tx(*txid)
										.map(|tx| tx.tx_node.tx.as_ref().clone())
								})
								.collect()
						};

						if !txs_to_broadcast.is_empty() {
							let tx_count = txs_to_broadcast.len();
							for tx in txs_to_broadcast {
								self.broadcaster.broadcast_unclassified_transaction(tx);
							}
							log_info!(
								self.logger,
								"Rebroadcast {} unconfirmed transactions on chain tip change",
								tx_count
							);
						}
					}
				},
				WalletEvent::TxUnconfirmed { txid, tx, .. } => {
					// See `TxConfirmed`: id resolution and the writes below must not interleave
					// with classification.
					let guard = self.funding_payment_update_lock.lock().await;

					let mut payment_id = self
						.find_payment_by_txid(txid)
						.await?
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					match self
						.apply_funding_status_update_locked(
							&guard,
							payment_id,
							txid,
							ConfirmationStatus::Unconfirmed,
						)
						.await?
					{
						FundingStatusUpdate::Applied => continue,
						FundingStatusUpdate::NotFunding => {},
						// Not part of the funding payment's history (e.g. a close spending the
						// funding outpoint): record it under its own id below instead.
						FundingStatusUpdate::Foreign => {
							payment_id = PaymentId(txid.to_byte_array());
						},
					}

					// The fallback id belongs to a settled funding payment whose entry is gone:
					// skip rather than resurrect it (see `has_funding_record`).
					if self.has_funding_record(&payment_id).await? {
						log_debug!(
							self.logger,
							"Skipping wallet event for transaction {} of a settled funding payment",
							txid,
						);
						continue;
					}

					let payment = {
						let locked_wallet = self.inner.lock().expect("lock");
						self.create_payment_from_tx(
							&locked_wallet,
							txid,
							payment_id,
							&tx,
							PaymentStatus::Pending,
							ConfirmationStatus::Unconfirmed,
						)
					};
					self.payment_store.insert_or_update(payment.clone()).await?;
					self.upsert_pending_payment(payment, Vec::new()).await?;
				},
				WalletEvent::TxReplaced { txid, conflicts, .. } => {
					// See `TxConfirmed`: id resolution and the writes below must not interleave
					// with classification. The pending entry written below embeds a read of the
					// payment record, which must not go stale against a concurrent
					// classification either.
					let _guard = self.funding_payment_update_lock.lock().await;

					let Some(payment_id) = self.find_payment_by_txid(txid).await? else {
						log_error!(
							self.logger,
							"Could not find payment for replaced transaction {}. Skipping.",
							txid
						);
						continue;
					};

					// Collect all conflict txids
					let mut conflict_txids: Vec<Txid> =
						conflicts.iter().map(|(_, conflict_txid)| *conflict_txid).collect();

					conflict_txids.push(txid);
					// The payment already exists in the store at this point: `bump_fee_rbf`
					// updates the payment store with the replacement txid before the next sync
					// cycle, and an id resolved through the candidate history comes from a
					// classification whose payment-store write strictly precedes the candidate
					// history it was resolved from. So we can safely fetch it here.
					let stored_payment = self.payment_store.get(&payment_id).await?;
					debug_assert!(
						stored_payment.is_some(),
						"Payment {:?} expected in store during WalletEvent::TxReplaced but not found",
						payment_id,
					);
					let payment = stored_payment.ok_or(Error::InvalidPaymentId)?;

					// A terminal record means the entry is the leftover of an interrupted settle
					// — the record write landed, the entry removal was lost to a crash — and this
					// event is the restart's replay of the same transition. Re-embedding the
					// record would stamp the terminal status into the entry and hide it from the
					// pending listing that repairs such leftovers; finish the interrupted removal
					// instead.
					if payment.status != PaymentStatus::Pending {
						self.pending_payment_store.remove(&payment_id).await?;
						continue;
					}

					self.upsert_pending_payment(payment, conflict_txids).await?;
				},
				WalletEvent::TxDropped { txid, tx } => {
					// See `TxConfirmed`: id resolution and the writes below must not interleave
					// with classification.
					let guard = self.funding_payment_update_lock.lock().await;

					let mut payment_id = self
						.find_payment_by_txid(txid)
						.await?
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					match self
						.apply_funding_status_update_locked(
							&guard,
							payment_id,
							txid,
							ConfirmationStatus::Unconfirmed,
						)
						.await?
					{
						FundingStatusUpdate::Applied => continue,
						FundingStatusUpdate::NotFunding => {},
						// Not part of the funding payment's history (e.g. a close spending the
						// funding outpoint): record it under its own id below instead.
						FundingStatusUpdate::Foreign => {
							payment_id = PaymentId(txid.to_byte_array());
						},
					}

					// The fallback id belongs to a settled funding payment whose entry is gone:
					// skip rather than resurrect it (see `has_funding_record`).
					if self.has_funding_record(&payment_id).await? {
						log_debug!(
							self.logger,
							"Skipping wallet event for transaction {} of a settled funding payment",
							txid,
						);
						continue;
					}

					let payment = {
						let locked_wallet = self.inner.lock().expect("lock");
						self.create_payment_from_tx(
							&locked_wallet,
							txid,
							payment_id,
							&tx,
							PaymentStatus::Pending,
							ConfirmationStatus::Unconfirmed,
						)
					};
					self.payment_store.insert_or_update(payment.clone()).await?;
					self.upsert_pending_payment(payment, Vec::new()).await?;
				},
				_ => {
					continue;
				},
			};
		}

		Ok(())
	}

	/// Whether a funding-classified record exists under the given id. A funding record's id is
	/// anchored to its first candidate's txid, so a wallet event for that transaction falls back
	/// to this id whenever the pending entry no longer maps it — which only happens once the
	/// negotiation settled and the entry was removed. The generic event handling must then skip
	/// its write: merging a wallet-view `Pending` payment into the settled record would resurrect
	/// it with figures no classification derived.
	async fn has_funding_record(&self, payment_id: &PaymentId) -> Result<bool, Error> {
		Ok(self.payment_store.get(payment_id).await?.is_some_and(|payment| {
			matches!(
				payment.kind,
				PaymentKind::Onchain {
					tx_type: Some(
						TransactionType::Funding { .. }
							| TransactionType::InteractiveFunding { .. }
					),
					..
				}
			)
		}))
	}

	/// Fails a funding payment whose transaction has irrevocably lost a conflict: a transaction
	/// outside the record's candidate history — e.g. a channel close double-spending a pending
	/// splice's shared input — has confirmed through [`ANTI_REORG_DELAY`] while neither the
	/// record's transaction nor any candidate is canonical anymore. Returns whether the payment
	/// was failed; failing also removes the pending entry, dropping the dead record from the
	/// tip-change pass. (Its transaction was already excluded from rebroadcast by the same
	/// canonical-only `get_tx` gate used below.)
	///
	/// Only funding-classified records are considered: nothing re-submits a replaced funding
	/// transaction under the same record (an RBF round is a new candidate), so a buried foreign
	/// conflict is final for them. The liveness check guards the case where the conflict
	/// double-spent only one round of the negotiation: as long as some candidate — any recorded
	/// round, or the record's own transaction should wallet sync have rotated it to an
	/// unrecorded one — can still confirm, the record must stay pending.
	async fn fail_funding_payment_lost_to_conflict(
		&self, payment: &PendingPaymentDetails, tip_height: u32,
	) -> Result<bool, Error> {
		let payment_id = match payment.details() {
			Some(details) => match details.kind {
				PaymentKind::Onchain {
					status: ConfirmationStatus::Unconfirmed,
					tx_type:
						Some(
							TransactionType::Funding { .. }
							| TransactionType::InteractiveFunding { .. },
						),
					..
				} => details.id,
				_ => return Ok(false),
			},
			None => return Ok(false),
		};
		if payment.conflicting_txids().is_empty() {
			return Ok(false);
		}

		// Serialize with the funding-record writers, which extend the candidate history: the
		// decision below must see that history in its settled form, and holding the lock keeps a
		// concurrent write from resurrecting the entry removed at the end.
		let _guard = self.funding_payment_update_lock.lock().await;

		// Re-read the entry under the lock; the listing snapshot may predate a record write.
		let entry = match self.pending_payment_store.get(&payment_id).await? {
			Some(entry) => entry,
			None => return Ok(false),
		};
		let PendingPaymentDetails::Tracked { details, conflicting_txids, candidates, .. } = &entry
		else {
			return Ok(false);
		};
		let record_txid = match details.kind {
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type:
					Some(
						TransactionType::Funding { .. }
						| TransactionType::InteractiveFunding { .. },
					),
			} => txid,
			_ => return Ok(false),
		};

		let foreign_conflicts: Vec<Txid> = conflicting_txids
			.iter()
			.copied()
			.filter(|conflict| *conflict != record_txid && entry.candidate(*conflict).is_none())
			.collect();
		if foreign_conflicts.is_empty() {
			return Ok(false);
		}

		let lost = {
			let locked_wallet = self.inner.lock().expect("lock");
			// `get_tx` is canonical-only: a transaction that lost to a confirmed conflict
			// returns `None`, while one that can still confirm is `Some`.
			let a_candidate_is_live = locked_wallet.get_tx(record_txid).is_some()
				|| candidates.iter().any(|c| locked_wallet.get_tx(c.txid).is_some());
			!a_candidate_is_live
				&& foreign_conflicts.iter().any(|conflict| {
					match locked_wallet.get_tx(*conflict).map(|tx| tx.chain_position) {
						Some(ChainPosition::Confirmed { anchor, .. }) => {
							tip_height >= anchor.block_id.height + ANTI_REORG_DELAY - 1
						},
						_ => false,
					}
				})
		};
		if !lost {
			return Ok(false);
		}

		let payment_id = entry.id();
		let outcome =
			self.fail_unconfirmed_funding_payment_locked(&_guard, payment_id, record_txid).await?;
		match outcome {
			FundingPaymentFailure::Failed => log_info!(
				self.logger,
				"Failed funding payment {}: transaction {} lost to a conflicting transaction confirmed beyond the reorg depth",
				payment_id,
				record_txid,
			),
			FundingPaymentFailure::EntryRemoved => log_info!(
				self.logger,
				"Removed the lingering entry of failed funding payment {}: transaction {} lost to \
				a conflicting transaction confirmed beyond the reorg depth",
				payment_id,
				record_txid,
			),
			FundingPaymentFailure::MovedOn => {},
		}
		Ok(outcome != FundingPaymentFailure::MovedOn)
	}

	/// Fails the funding payment `payment_id` while its record still waits on the unconfirmed
	/// funding transaction `record_txid`, and removes its pending entry, reporting what it did. As
	/// with graduation, the decision is made from the live record and only the status is written.
	/// A record already `Failed` — a prior pass whose entry removal was lost to a crash — still
	/// matches, no-ops the update, and gets its lingering entry removed.
	async fn fail_unconfirmed_funding_payment_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, payment_id: PaymentId, record_txid: Txid,
	) -> Result<FundingPaymentFailure, Error> {
		let mut outcome = FundingPaymentFailure::MovedOn;
		self.payment_store
			.mutate(&payment_id, |existing| {
				let current = existing?;
				match current.kind {
					PaymentKind::Onchain {
						txid,
						status: ConfirmationStatus::Unconfirmed,
						tx_type:
							Some(
								TransactionType::Funding { .. }
								| TransactionType::InteractiveFunding { .. },
							),
					} if txid == record_txid => {
						let mut update = PaymentDetailsUpdate::new(payment_id);
						update.status = Some(PaymentStatus::Failed);
						let mut updated = current.clone();
						if updated.update(update) {
							outcome = FundingPaymentFailure::Failed;
							Some(updated)
						} else {
							outcome = FundingPaymentFailure::EntryRemoved;
							None
						}
					},
					_ => None,
				}
			})
			.await?;
		if outcome != FundingPaymentFailure::MovedOn {
			self.pending_payment_store.remove(&payment_id).await?;
		}
		Ok(outcome)
	}

	/// Resolves the funding payments of the closed channel `channel_id`, whose monitor settled on
	/// and still watches `held_rounds` (as [`closed_channel_held_rounds`] lists them): a round
	/// nothing ever broadcast is dropped from its record, as [`Self::drop_abandoned_splice_rounds`]
	/// does, and every payment left waiting on an unconfirmed splice round with no round of ours
	/// among `held_rounds`, and none LDK promoted to the channel's funding before, is failed. The
	/// monitor watches every pending round of ours that can still confirm, and a round that was
	/// the funding once — a zero-conf splice locks before its transaction confirms — can confirm
	/// still, every later splice building on it, so such a payment waits for a transaction that
	/// cannot.
	///
	/// In the usual order the monitor still watches every pending round when the channel closes,
	/// and the `DiscardFunding` events it queues once the close matures find the channel no longer
	/// listed and resolve the payments the same way, by what the monitor holds then. The order
	/// flips when one sync delivers the close and its maturity while the background processor is
	/// between the channel manager's event pass and the chain monitor's: the monitor's events then
	/// find the channel still listed, and an event for a listed channel resolves no payment — the
	/// promotion of a sibling round does, when there is one, and here there is none. This settles
	/// what those events left behind.
	pub(crate) async fn resolve_closed_channel_splice_rounds(
		&self, channel_id: ChannelId, held_rounds: &[Txid],
	) -> Result<(), Error> {
		// Serialize with the other funding-record writers, which all hold this lock from their
		// reads through their last write.
		let guard = self.funding_payment_update_lock.lock().await;
		self.resolve_closed_channel_splice_rounds_locked(&guard, channel_id, held_rounds).await
	}

	/// [`Self::resolve_closed_channel_splice_rounds`] for a caller already holding the
	/// funding-record writers' lock.
	async fn resolve_closed_channel_splice_rounds_locked(
		&self, guard: &tokio::sync::MutexGuard<'_, ()>, channel_id: ChannelId, held_rounds: &[Txid],
	) -> Result<(), Error> {
		self.drop_abandoned_splice_rounds_locked(guard, channel_id, held_rounds).await?;
		self.fail_funding_payments_without_held_round_locked(
			guard,
			channel_id,
			held_rounds,
			FundingResolution::Close,
		)
		.await?;
		// Logged whatever the two passes found: a payment graduated by a sync running alongside
		// leaves them nothing to log, and the decision should still show.
		log_debug!(
			self.logger,
			"Resolved the funding payments of channel {} after its close by the {} round(s) its \
			monitor holds",
			channel_id,
			held_rounds.len(),
		);
		Ok(())
	}

	/// Fails every funding payment of `channel_id` still waiting on an unconfirmed splice round
	/// while no round of ours in its record is among `held_rounds` or was promoted to the channel's
	/// funding (see [`Self::resolve_promoted_splice_round`]), removing its pending entry; a payment
	/// with such a round is left as it is. The rounds of ours are the candidates recorded with a
	/// stake and the record's own transaction. A payment that moved on — its round confirmed, or
	/// it was failed already — is not touched beyond the entry a failure cut short left behind.
	/// `resolution` names the occasion in what is logged.
	async fn fail_funding_payments_without_held_round_locked(
		&self, guard: &tokio::sync::MutexGuard<'_, ()>, channel_id: ChannelId,
		held_rounds: &[Txid], resolution: FundingResolution,
	) -> Result<(), Error> {
		let occasion = match resolution {
			FundingResolution::Close => format!("of closed channel {}", channel_id),
			FundingResolution::Promotion(promoted) => {
				format!("of channel {} once splice round {} locked", channel_id, promoted)
			},
		};
		let entries =
			self.pending_payment_store.list_filter(|entry| tracks_channel(entry, channel_id)).await;
		for entry in entries {
			let details = match entry.details() {
				Some(details) => details,
				None => continue,
			};
			let payment_id = details.id;
			let record_txid = match &details.kind {
				PaymentKind::Onchain {
					txid,
					status: ConfirmationStatus::Unconfirmed,
					tx_type: Some(TransactionType::InteractiveFunding { .. }),
				} => *txid,
				_ => {
					log_debug!(
						self.logger,
						"Funding payment {} {} no longer waits on an unconfirmed round",
						payment_id,
						occasion,
					);
					continue;
				},
			};
			let mut rounds_of_ours = entry
				.candidates()
				.iter()
				.filter(|candidate| candidate.amount_msat.is_some())
				.map(|candidate| candidate.txid)
				.chain(std::iter::once(record_txid));
			if let Some(kept) = rounds_of_ours
				.find(|txid| held_rounds.contains(txid) || entry.locked_rounds().contains(txid))
			{
				log_info!(
					self.logger,
					"Splice round {} of ours can still confirm: keeping funding payment {} {}",
					kept,
					payment_id,
					occasion,
				);
				continue;
			}
			match self
				.fail_unconfirmed_funding_payment_locked(guard, payment_id, record_txid)
				.await?
			{
				FundingPaymentFailure::Failed => log_info!(
					self.logger,
					"Failed funding payment {} {}: no round of ours can confirm",
					payment_id,
					occasion,
				),
				FundingPaymentFailure::EntryRemoved => log_info!(
					self.logger,
					"Removed the lingering entry of failed funding payment {} {}",
					payment_id,
					occasion,
				),
				FundingPaymentFailure::MovedOn => log_warn!(
					self.logger,
					"Funding payment {} {} moved on from transaction {}: leaving it as it is",
					payment_id,
					occasion,
					record_txid,
				),
			}
		}
		Ok(())
	}

	/// Resolves what LDK's promotion of the splice round `promoted` to the funding of `channel_id`,
	/// as its `ChannelReady` reports, means for the channel's funding payments. `held_rounds` lists
	/// the rounds LDK holds for the channel once promoted, as [`held_splice_rounds`] does — the
	/// promoted round alone, unless a contribution queued behind it was negotiated already — or is
	/// `None` for a channel the manager no longer lists, whose close settles its payments.
	///
	/// The promotion is recorded first, in the funding payment whose record holds the round. A
	/// zero-conf splice is promoted as soon as `splice_locked` is exchanged, before its transaction
	/// confirms, and every later splice builds on it, so the round can still confirm once the
	/// channel's funding has moved on from it and once the channel has closed — when neither the
	/// channel manager nor the monitor holds it anymore — and its payment is kept then. Nothing is
	/// recorded for a round no funding payment holds — this node did not contribute to it, or its
	/// record graduated already — or recorded as promoted already (a replayed event).
	///
	/// LDK discards the round's siblings as it promotes the round, queuing a `DiscardFunding` for
	/// each contribution of ours it returns — one naming the contribution, not the round — so the
	/// channel's other payments are resolved here, from the rounds LDK holds: a round nothing ever
	/// broadcast is dropped from its record, as [`Self::drop_abandoned_splice_rounds`] does, and
	/// every payment left waiting on an unconfirmed round with no round of ours among `held_rounds`
	/// and none promoted before is failed: no round of ours can confirm anymore, a round this node
	/// did not contribute to having locked. A replayed event finds the promoted round recorded and
	/// keeps its payment whatever LDK holds by then.
	pub(crate) async fn resolve_promoted_splice_round(
		&self, channel_id: ChannelId, promoted: Txid, held_rounds: Option<&[Txid]>,
	) -> Result<(), Error> {
		// Serialize with the other funding-record writers, which all hold this lock from their
		// reads through their last write.
		let guard = self.funding_payment_update_lock.lock().await;
		self.record_locked_splice_round_locked(&guard, channel_id, promoted).await?;
		let held_rounds = match held_rounds {
			Some(held_rounds) => held_rounds,
			None => {
				log_debug!(
					self.logger,
					"Channel {} is no longer listed as splice round {} locks: leaving its funding \
					payments to its close",
					channel_id,
					promoted,
				);
				return Ok(());
			},
		};
		// The drop goes first: a round nothing broadcast is taken back rather than failed, and
		// the payment recorded for it alone goes with it.
		self.drop_abandoned_splice_rounds_locked(&guard, channel_id, held_rounds).await?;
		self.fail_funding_payments_without_held_round_locked(
			&guard,
			channel_id,
			held_rounds,
			FundingResolution::Promotion(promoted),
		)
		.await?;
		log_debug!(
			self.logger,
			"Resolved the funding payments of channel {} as splice round {} locked, by the {} \
			round(s) LDK holds",
			channel_id,
			promoted,
			held_rounds.len(),
		);
		Ok(())
	}

	/// Records that LDK promoted the splice round `txid` to the funding of `channel_id` in the
	/// funding payment whose record holds the round, for a caller holding the funding-record
	/// writers' lock (see [`Self::resolve_promoted_splice_round`]).
	async fn record_locked_splice_round_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, channel_id: ChannelId, txid: Txid,
	) -> Result<(), Error> {
		let entries = self
			.pending_payment_store
			.list_filter(|entry| {
				tracks_channel(entry, channel_id)
					&& entry.candidate(txid).is_some()
					&& !entry.locked_rounds().contains(&txid)
			})
			.await;
		for entry in entries {
			let payment_id = entry.id();
			self.pending_payment_store
				.mutate(&payment_id, |existing| {
					let mut entry = existing?.clone();
					if !entry.record_locked_round(txid) {
						return None;
					}
					Some(entry)
				})
				.await?;
			log_info!(
				self.logger,
				"Splice round {} of funding payment {} locked as the funding of channel {}",
				txid,
				payment_id,
				channel_id,
			);
		}
		Ok(())
	}

	#[allow(deprecated)]
	pub(crate) async fn create_funding_transaction(
		&self, output_script: ScriptBuf, amount: Amount, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);
		let mut locked_persister = self.persister.lock().await;
		let (psbt, change_set) = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			let mut tx_builder = locked_wallet.build_tx();
			tx_builder.add_recipient(output_script, amount).fee_rate(fee_rate).nlocktime(locktime);

			let mut psbt = match tx_builder.finish() {
				Ok(psbt) => {
					log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);
					psbt
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create funding transaction: {}", err);
					return Err(err.into());
				},
			};

			match locked_wallet.sign(&mut psbt, SignOptions::default()) {
				Ok(finalized) => {
					if !finalized {
						return Err(Error::OnchainTxCreationFailed);
					}
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create funding transaction: {}", err);
					return Err(err.into());
				},
			}

			(psbt, locked_wallet.take_staged().unwrap_or_default())
		};
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			e
		})?;

		Ok(tx)
	}

	/// Returns a fresh address, served from the address pool so that external handouts consume
	/// the oldest revealed index first.
	///
	/// Allocating in reveal order keeps the window of revealed-but-unused scripts compact: as
	/// soon as a handed-out address is used on-chain, everything before it no longer counts
	/// towards a from-seed restore's full-scan stop gap. Minting a fresh index here instead
	/// would strand the pooled indices as an ever-growing unused tail in front of every address
	/// a restore must discover. The order has one exception: a handout that fails while a
	/// concurrent one proceeds can return its address to the pool below an index already handed
	/// out.
	///
	/// Unlike [`Wallet::pop_pooled_address`], this may wait on persistence, so the handout is
	/// made durable before the address is returned: the awaited refill rewrites the pool record
	/// (no longer containing the popped index) before topping the pool back up, so a restart
	/// never hands the returned address out again. On failure the address instead returns to
	/// the pool unhanded-out, with a compensating record write covering the case where the
	/// failed refill had already rewritten the record.
	pub(crate) async fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let (index, address) = loop {
			if let Some(entry) = self.address_pool.lock().expect("lock").available.pop_front() {
				break entry;
			}
			// Another caller may pop what this refill publishes before the re-check, so loop
			// rather than assuming a successful refill leaves the pool non-empty.
			self.refill_address_pool().await?;
		};

		// Force the record rewrite: a failed handout's push-back can leave the pool over its
		// target size, and an early-returning refill would then leave the just-popped index
		// durably recorded, handing the address out again after a restart.
		match self.refill_address_pool_inner(true).await {
			Ok(()) => Ok(address),
			Err(e) => {
				// The address was never handed out, so return it for the next caller rather
				// than leaving its index revealed but unreachable. Reinsert by index:
				// concurrent failed handouts complete in pop order, so pushing to the front
				// would reverse their segment and let the next successful handout skip past a
				// lower index, stranding it behind a used address in a from-seed restore's scan.
				{
					let mut locked_pool = self.address_pool.lock().expect("lock");
					let position = locked_pool.available.partition_point(|(i, _)| *i < index);
					locked_pool.available.insert(position, (index, address));
				}
				// The refill may have failed after rewriting the record, which then durably
				// excludes the pushed-back index; rewrite it from the restored pool so a crash
				// before the next successful refill doesn't strand the index outside the pool.
				self.rewrite_pool_record().await;
				Err(e)
			},
		}
	}

	/// Returns an address whose reveal is already durably persisted, or `None` if the pool is
	/// exhausted.
	///
	/// This is safe to call from sync callbacks (e.g., [`SignerProvider`]) that LDK invokes on
	/// runtime worker threads while holding channel locks: it never waits on persistence, only
	/// popping from the pre-persisted pool and scheduling a background refill. Blocking such a
	/// callback on persistence can deadlock the runtime, as other tasks blocking synchronously on
	/// the same channel locks may capture the remaining workers, leaving none to drive the
	/// persistence future the callback would wait on.
	///
	/// Failing closed on an empty pool (rather than revealing an unpersisted address) ensures we
	/// never hand out a script that would go unwatched if the node crashed before its reveal
	/// landed: incremental chain syncs only query scripts the persisted wallet has revealed.
	///
	/// The handout itself is not persisted: if the node restarts before the refill scheduled here
	/// rewrites the pool record, the popped address may be handed out again after the restart.
	/// Its reveal is durable either way, so the script always stays watched — the cost is bounded
	/// address reuse, not fund visibility.
	pub(crate) fn pop_pooled_address(self: &Arc<Self>) -> Option<bitcoin::Address> {
		let popped = self.address_pool.lock().expect("lock").available.pop_front();

		// Spawning cancellable lets shutdown abort an in-flight refill rather than wait on it.
		// Aborting mid-refill (or dropping a refill spawned during shutdown) is safe: the reveals
		// are staged with the persister in the same critical section that takes them from the
		// wallet, and nothing is published whose persistence the refill did not see complete.
		let wallet = Arc::clone(self);
		self.runtime.spawn_cancellable_background_task(async move {
			if let Err(e) = wallet.refill_address_pool().await {
				log_error!(wallet.logger, "Failed to refill the address pool: {}", e);
			}
		});

		popped.map(|(_, address)| address)
	}

	/// Tops the address pool up to [`ADDRESS_POOL_TARGET_SIZE`], publishing newly revealed
	/// addresses only after their reveal has been durably persisted.
	pub(crate) async fn refill_address_pool(&self) -> Result<(), Error> {
		self.refill_address_pool_inner(false).await
	}

	/// [`Wallet::refill_address_pool`], where `force_record_rewrite` makes the pool-record
	/// rewrite unconditional: a pool at or over its target size otherwise skips it, which after
	/// a pop would leave the popped index in the record.
	async fn refill_address_pool_inner(&self, force_record_rewrite: bool) -> Result<(), Error> {
		let _refill_guard = self.address_pool_refill_lock.lock().await;

		if !force_record_rewrite {
			let locked_pool = self.address_pool.lock().expect("lock");
			if locked_pool.unpublished.is_empty()
				&& locked_pool.available.len() >= ADDRESS_POOL_TARGET_SIZE
			{
				return Ok(());
			}
		}

		let mut locked_persister = self.persister.lock().await;
		let indices = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			let mut locked_pool = self.address_pool.lock().expect("lock");
			let needed = ADDRESS_POOL_TARGET_SIZE
				.saturating_sub(locked_pool.available.len() + locked_pool.unpublished.len());
			for _ in 0..needed {
				let address_info = locked_wallet.reveal_next_address(KeychainKind::External);
				locked_pool.unpublished.push((address_info.index, address_info.address));
			}
			// Hand the reveals straight to the persister: this refill may run as a task the
			// runtime aborts at shutdown, and holding the taken change set across an await
			// would lose the reveals if the abort lands there — a later refill run would then
			// publish addresses no persisted wallet state covers.
			locked_persister.stage(locked_wallet.take_staged().unwrap_or_default());
			locked_pool
				.available
				.iter()
				.chain(locked_pool.unpublished.iter())
				.map(|(index, _)| *index)
				.collect::<Vec<u32>>()
		};

		// Persist the pool record before the reveals. A crash between the two writes then leaves
		// record entries the persisted wallet doesn't cover, which reloading drops and the next
		// refill re-derives to the same indices — rather than durably revealed indices missing
		// from the record, which no path would ever pool or hand out again (burning them).
		// Writing the record first also drops popped indices from it as early as possible,
		// narrowing the restart window in which a handed-out address is handed out again.
		// Skip the reveal flush when the record write fails: reveals made durable without
		// record coverage would, after a crash, be indices no path ever pools or hands out
		// again — permanently skipped in the keychain, widening the gap a restore from seed
		// must scan across. Retained in the persister instead, they either flush with a later
		// persist call or die with the process, in which case the next run re-derives the same
		// indices. (An unrelated persist call can still flush them before the record retry
		// succeeds, so the window is narrowed, not closed.)
		locked_persister.persist_address_pool(indices).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist address pool: {}", e);
			Error::PersistenceFailed
		})?;
		// On failure the reveals stay in `unpublished` (never handed out) and the persister
		// retains the change set, so the next refill run retries both.
		locked_persister.persist_staged().await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Both writes are durable, so the addresses may be handed out.
		let mut locked_pool = self.address_pool.lock().expect("lock");
		let unpublished = core::mem::take(&mut locked_pool.unpublished);
		locked_pool.available.extend(unpublished);
		Ok(())
	}

	/// Best-effort rewrite of the pool record from the pool's current contents, used to
	/// re-include a pushed-back index whose handout's record write succeeded before the handout
	/// failed. Failures are only logged: the pool still covers the index in memory and the next
	/// successful refill rewrites the record anyway, so only a crash before then strands the
	/// index outside the pool.
	async fn rewrite_pool_record(&self) {
		let mut locked_persister = self.persister.lock().await;
		let indices: Vec<u32> = {
			let locked_pool = self.address_pool.lock().expect("lock");
			locked_pool
				.available
				.iter()
				.chain(locked_pool.unpublished.iter())
				.map(|(index, _)| *index)
				.collect()
		};
		let _ = locked_persister.persist_address_pool(indices).await;
	}

	pub(crate) async fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_persister = self.persister.lock().await;
		let (address_info, change_set) = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
			(address_info, locked_wallet.take_staged().unwrap_or_default())
		};
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
	}

	pub(crate) async fn cancel_tx(&self, tx: Transaction) -> Result<(), Error> {
		let mut locked_persister = self.persister.lock().await;
		let change_set = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			Self::cancel_tx_inner(&mut locked_wallet, tx);
			locked_wallet.take_staged().unwrap_or_default()
		};
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		Ok(())
	}

	fn cancel_tx_inner(
		locked_wallet: &mut PersistedWallet<KVStoreWalletPersister>, tx: Transaction,
	) {
		for txout in tx.output {
			if let Some((keychain, index)) = locked_wallet.derivation_of_spk(txout.script_pubkey) {
				// This mirrors the removed BDK helper: it only frees superficial usage marks.
				locked_wallet.unmark_used(keychain, index);
			}
		}
	}

	pub(crate) fn get_balances(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let balance = self.inner.lock().expect("lock").balance();

		// Make sure `list_confirmed_utxos` returns at least one `Utxo` we could use to spend/bump
		// Anchors if we have any confirmed amounts.
		#[cfg(debug_assertions)]
		if balance.confirmed != Amount::ZERO {
			debug_assert!(
				self.list_confirmed_utxos_inner().map_or(false, |v| !v.is_empty()),
				"Confirmed amounts should always be available for Anchor spending"
			);
		}

		self.get_balances_inner(balance, total_anchor_channels_reserve_sats)
	}

	fn get_balances_inner(
		&self, balance: Balance, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let (total, spendable) = (
			balance.total().to_sat(),
			balance.trusted_spendable().to_sat().saturating_sub(total_anchor_channels_reserve_sats),
		);

		Ok((total, spendable))
	}

	pub(crate) fn get_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		self.get_balances(total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	fn build_drain_psbt(
		&self, locked_wallet: &mut PersistedWallet<KVStoreWalletPersister>,
		drain_script: ScriptBuf, cur_anchor_reserve_sats: u64, fee_rate: FeeRate,
		shared_input: Option<&Input>,
	) -> Result<Psbt, Error> {
		let anchor_address = if cur_anchor_reserve_sats > DUST_LIMIT_SATS {
			Some(locked_wallet.peek_address(KeychainKind::Internal, 0))
		} else {
			None
		};

		let mut tx_builder = locked_wallet.build_tx();
		tx_builder.drain_wallet().drain_to(drain_script).fee_rate(fee_rate);

		if let Some(address_info) = anchor_address {
			tx_builder.add_recipient(
				address_info.address.script_pubkey(),
				Amount::from_sat(cur_anchor_reserve_sats),
			);
		}

		if let Some(input) = shared_input {
			let psbt_input = psbt::Input {
				witness_utxo: Some(input.previous_utxo.clone()),
				..Default::default()
			};
			let weight = ldk_to_bdk_satisfaction_weight(input.satisfaction_weight);
			tx_builder.only_witness_utxo().exclude_unconfirmed();
			tx_builder.add_foreign_utxo(input.outpoint, psbt_input, weight).map_err(|e| {
				log_error!(self.logger, "Failed to add shared input for fee estimation: {e}");
				Error::ChannelSplicingFailed
			})?;
		}

		let psbt = tx_builder.finish().map_err(|err| {
			log_error!(self.logger, "Failed to create temporary drain transaction: {err}");
			err
		})?;

		Ok(psbt)
	}

	/// Builds a temporary drain transaction and returns the maximum amount that would be sent to
	/// the drain output, along with the PSBT for further inspection.
	///
	/// The caller is responsible for cancelling the PSBT via `locked_wallet.cancel_tx()`.
	fn get_max_drain_amount(
		&self, locked_wallet: &mut PersistedWallet<KVStoreWalletPersister>,
		drain_script: ScriptBuf, cur_anchor_reserve_sats: u64, fee_rate: FeeRate,
		shared_input: Option<&Input>,
	) -> Result<(u64, Psbt), Error> {
		let balance = locked_wallet.balance();
		let spendable_amount_sats =
			self.get_balances_inner(balance, cur_anchor_reserve_sats).map(|(_, s)| s).unwrap_or(0);

		if spendable_amount_sats == 0 {
			log_error!(
				self.logger,
				"Unable to determine max amount: no spendable funds available."
			);
			return Err(Error::InsufficientFunds);
		}

		let tmp_psbt = self.build_drain_psbt(
			locked_wallet,
			drain_script.clone(),
			cur_anchor_reserve_sats,
			fee_rate,
			shared_input,
		)?;

		let drain_output_value = tmp_psbt
			.unsigned_tx
			.output
			.iter()
			.find(|o| o.script_pubkey == drain_script)
			.map(|o| o.value)
			.ok_or_else(|| {
				log_error!(self.logger, "Failed to find drain output in temporary transaction");
				Error::InsufficientFunds
			})?;

		let shared_input_value = shared_input.map(|i| i.previous_utxo.value.to_sat()).unwrap_or(0);

		let max_amount = drain_output_value.to_sat().saturating_sub(shared_input_value);

		if max_amount < DUST_LIMIT_SATS {
			log_error!(
				self.logger,
				"Unable to proceed: available funds would be consumed entirely by fees. \
				Available: {spendable_amount_sats}sats, drain output: {}sats.",
				drain_output_value.to_sat(),
			);
			return Err(Error::InsufficientFunds);
		}

		Ok((max_amount, tmp_psbt))
	}

	/// Returns the maximum amount available for funding a channel, accounting for on-chain fees
	/// and anchor reserves.
	pub(crate) fn get_max_funding_amount(
		&self, cur_anchor_reserve_sats: u64, fee_rate: FeeRate,
	) -> Result<u64, Error> {
		let mut locked_wallet = self.inner.lock().expect("lock");

		// Use a dummy P2WSH script (34 bytes) to match the size of a real funding output.
		let dummy_p2wsh_script = ScriptBuf::new().to_p2wsh();

		let (max_amount, tmp_psbt) = self.get_max_drain_amount(
			&mut locked_wallet,
			dummy_p2wsh_script,
			cur_anchor_reserve_sats,
			fee_rate,
			None,
		)?;

		Self::cancel_tx_inner(&mut locked_wallet, tmp_psbt.unsigned_tx);

		Ok(max_amount)
	}

	/// Returns the maximum amount available for splicing into an existing channel, accounting for
	/// on-chain fees and anchor reserves, along with the wallet UTXOs to use as inputs.
	pub(crate) fn get_max_splice_in_amount(
		&self, shared_input: Input, shared_output_script: ScriptBuf, cur_anchor_reserve_sats: u64,
		fee_rate: FeeRate,
	) -> Result<u64, Error> {
		let mut locked_wallet = self.inner.lock().expect("lock");

		debug_assert!(matches!(
			locked_wallet.public_descriptor(KeychainKind::External),
			ExtendedDescriptor::Wpkh(_)
		));
		debug_assert!(matches!(
			locked_wallet.public_descriptor(KeychainKind::Internal),
			ExtendedDescriptor::Wpkh(_)
		));

		let (splice_amount, tmp_psbt) = self.get_max_drain_amount(
			&mut locked_wallet,
			shared_output_script,
			cur_anchor_reserve_sats,
			fee_rate,
			Some(&shared_input),
		)?;

		Self::cancel_tx_inner(&mut locked_wallet, tmp_psbt.unsigned_tx);

		Ok(splice_amount)
	}

	pub(crate) fn parse_and_validate_address(&self, address: &Address) -> Result<Address, Error> {
		Address::<NetworkUnchecked>::from_str(address.to_string().as_str())
			.map_err(|_| Error::InvalidAddress)?
			.require_network(self.config.network)
			.map_err(|_| Error::InvalidAddress)
	}

	#[allow(deprecated)]
	pub(crate) async fn send_to_address(
		&self, address: &bitcoin::Address, send_amount: OnchainSendAmount,
		fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let mut locked_persister = self.persister.lock().await;
		let (psbt, change_set) = {
			let mut locked_wallet = self.inner.lock().expect("lock");

			// Prepare the tx_builder. We properly check the reserve requirements (again) further down.
			let tx_builder = match send_amount {
				OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
					let mut tx_builder = locked_wallet.build_tx();
					let amount = Amount::from_sat(amount_sats);
					tx_builder.add_recipient(address.script_pubkey(), amount).fee_rate(fee_rate);
					tx_builder
				},
				OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
					if cur_anchor_reserve_sats > DUST_LIMIT_SATS =>
				{
					let (max_amount, tmp_psbt) = self.get_max_drain_amount(
						&mut locked_wallet,
						address.script_pubkey(),
						cur_anchor_reserve_sats,
						fee_rate,
						None,
					)?;

					let estimated_tx_fee =
						locked_wallet.calculate_fee(&tmp_psbt.unsigned_tx).map_err(|e| {
							log_error!(
								self.logger,
								"Failed to calculate fee of temporary transaction: {}",
								e
							);
							e
						})?;

					Self::cancel_tx_inner(&mut locked_wallet, tmp_psbt.unsigned_tx);

					let mut tx_builder = locked_wallet.build_tx();
					tx_builder
						.add_recipient(address.script_pubkey(), Amount::from_sat(max_amount))
						.fee_absolute(estimated_tx_fee);
					tx_builder
				},
				OnchainSendAmount::AllDrainingReserve
				| OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats: _ } => {
					let mut tx_builder = locked_wallet.build_tx();
					tx_builder.drain_wallet().drain_to(address.script_pubkey()).fee_rate(fee_rate);
					tx_builder
				},
			};

			let mut psbt = match tx_builder.finish() {
				Ok(psbt) => {
					log_trace!(self.logger, "Created PSBT: {:?}", psbt);
					psbt
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				},
			};

			// Check the reserve requirements (again) and return an error if they aren't met.
			match send_amount {
				OnchainSendAmount::ExactRetainingReserve {
					amount_sats,
					cur_anchor_reserve_sats,
				} => {
					let balance = locked_wallet.balance();
					let spendable_amount_sats = self
						.get_balances_inner(balance, cur_anchor_reserve_sats)
						.map(|(_, s)| s)
						.unwrap_or(0);
					let tx_fee_sats = locked_wallet
						.calculate_fee(&psbt.unsigned_tx)
						.map_err(|e| {
							log_error!(
								self.logger,
								"Failed to calculate fee of candidate transaction: {}",
								e
							);
							e
						})?
						.to_sat();
					if spendable_amount_sats < amount_sats.saturating_add(tx_fee_sats) {
						log_error!(self.logger,
							"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}sats + {}sats fee",
							spendable_amount_sats,
							amount_sats,
							tx_fee_sats,
						);
						return Err(Error::InsufficientFunds);
					}
				},
				OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
					let balance = locked_wallet.balance();
					let spendable_amount_sats = self
						.get_balances_inner(balance, cur_anchor_reserve_sats)
						.map(|(_, s)| s)
						.unwrap_or(0);
					let (sent, received) = locked_wallet.sent_and_received(&psbt.unsigned_tx);
					let drain_amount = sent - received;
					if spendable_amount_sats < drain_amount.to_sat() {
						log_error!(self.logger,
							"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}",
							spendable_amount_sats,
							drain_amount,
						);
						return Err(Error::InsufficientFunds);
					}
				},
				_ => {},
			}

			match locked_wallet.sign(&mut psbt, SignOptions::default()) {
				Ok(finalized) => {
					if !finalized {
						return Err(Error::OnchainTxCreationFailed);
					}
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				},
			}

			(psbt, locked_wallet.take_staged().unwrap_or_default())
		};
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			e
		})?;

		let txid = tx.compute_txid();
		self.broadcaster.broadcast_unclassified_transaction(tx);

		match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
				log_info!(
					self.logger,
					"Created new transaction {} sending {}sats on-chain to address {}",
					txid,
					amount_sats,
					address
				);
			},
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
				log_info!(
					self.logger,
					"Created new transaction {} sending available on-chain funds retaining a reserve of {}sats to address {}",
					txid,
					cur_anchor_reserve_sats,
					address,
				);
			},
			OnchainSendAmount::AllDrainingReserve => {
				log_info!(
					self.logger,
					"Created new transaction {} sending all available on-chain funds to address {}",
					txid,
					address
				);
			},
		}

		Ok(txid)
	}

	pub(crate) async fn select_confirmed_utxos(
		&self, must_spend: Vec<Input>, must_pay_to: &[TxOut], fee_rate: FeeRate,
	) -> Result<CoinSelection, ()> {
		let mut locked_persister = self.persister.lock().await;
		let (coin_selection, change_set) = {
			let mut locked_wallet = self.inner.lock().expect("lock");

			debug_assert!(matches!(
				locked_wallet.public_descriptor(KeychainKind::External),
				ExtendedDescriptor::Wpkh(_)
			));
			debug_assert!(matches!(
				locked_wallet.public_descriptor(KeychainKind::Internal),
				ExtendedDescriptor::Wpkh(_)
			));

			let mut tx_builder = locked_wallet.build_tx();
			tx_builder.only_witness_utxo();

			for input in &must_spend {
				let psbt_input = psbt::Input {
					witness_utxo: Some(input.previous_utxo.clone()),
					..Default::default()
				};
				let weight = ldk_to_bdk_satisfaction_weight(input.satisfaction_weight);
				tx_builder.add_foreign_utxo(input.outpoint, psbt_input, weight).map_err(|_| ())?;
			}

			for output in must_pay_to {
				tx_builder.add_recipient(output.script_pubkey.clone(), output.value);
			}

			tx_builder.fee_rate(fee_rate);
			tx_builder.exclude_unconfirmed();

			let unsigned_tx = tx_builder
				.finish()
				.map_err(|e| {
					log_error!(self.logger, "Failed to select confirmed UTXOs: {}", e);
				})?
				.unsigned_tx;

			let confirmed_utxos = unsigned_tx
				.input
				.iter()
				.filter(|txin| {
					must_spend.iter().all(|input| input.outpoint != txin.previous_output)
				})
				.filter_map(|txin| {
					locked_wallet
						.tx_details(txin.previous_output.txid)
						.map(|tx_details| tx_details.tx.deref().clone())
						.map(|prevtx| ConfirmedUtxo::new_p2wpkh(prevtx, txin.previous_output.vout))
				})
				.collect::<Result<Vec<_>, ()>>()?;

			if unsigned_tx.output.len() > must_pay_to.len() + 1 {
				log_error!(
					self.logger,
					"Unexpected number of change outputs during coin selection: {}",
					unsigned_tx.output.len() - must_pay_to.len(),
				);
				return Err(());
			}

			let change_output = unsigned_tx
				.output
				.into_iter()
				.find(|txout| must_pay_to.iter().all(|output| output != txout));
			let change_set = if change_output.is_some() {
				Some(locked_wallet.take_staged().unwrap_or_default())
			} else {
				None
			};

			(CoinSelection { confirmed_utxos, change_output }, change_set)
		};

		if let Some(change_set) = change_set {
			locked_persister.persist_changeset(change_set).await.map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
			})?;
		}

		Ok(coin_selection)
	}

	fn list_confirmed_utxos_inner(&self) -> Result<Vec<Utxo>, ()> {
		let locked_wallet = self.inner.lock().expect("lock");
		let mut utxos = Vec::new();
		let confirmed_txs: Vec<Txid> = locked_wallet
			.transactions()
			.filter(|t| t.chain_position.is_confirmed())
			.map(|t| t.tx_node.txid)
			.collect();
		let unspent_confirmed_utxos =
			locked_wallet.list_unspent().filter(|u| confirmed_txs.contains(&u.outpoint.txid));

		for u in unspent_confirmed_utxos {
			let script_pubkey = u.txout.script_pubkey;
			match script_pubkey.witness_version() {
				Some(version @ WitnessVersion::V0) => {
					// According to the SegWit rules of [BIP 141] a witness program is defined as:
					// > A scriptPubKey (or redeemScript as defined in BIP16/P2SH) that consists of
					// > a 1-byte push opcode (one of OP_0,OP_1,OP_2,.. .,OP_16) followed by a direct
					// > data push between 2 and 40 bytes gets a new special meaning. The value of
					// > the first push is called the "version byte". The following byte vector
					// > pushed is called the "witness program"."
					//
					// We therefore skip the first byte we just read via `witness_version` and use
					// the rest (i.e., the data push) as the raw bytes to construct the
					// `WitnessProgram` below.
					//
					// [BIP 141]: https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki#witness-program
					let witness_bytes = &script_pubkey.as_bytes()[2..];
					let witness_program =
						WitnessProgram::new(version, witness_bytes).map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

					let wpkh = WPubkeyHash::from_slice(&witness_program.program().as_bytes())
						.map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;
					let utxo = Utxo::new_v0_p2wpkh(u.outpoint, u.txout.value, &wpkh);
					utxos.push(utxo);
				},
				Some(version @ WitnessVersion::V1) => {
					// According to the SegWit rules of [BIP 141] a witness program is defined as:
					// > A scriptPubKey (or redeemScript as defined in BIP16/P2SH) that consists of
					// > a 1-byte push opcode (one of OP_0,OP_1,OP_2,.. .,OP_16) followed by a direct
					// > data push between 2 and 40 bytes gets a new special meaning. The value of
					// > the first push is called the "version byte". The following byte vector
					// > pushed is called the "witness program"."
					//
					// We therefore skip the first byte we just read via `witness_version` and use
					// the rest (i.e., the data push) as the raw bytes to construct the
					// `WitnessProgram` below.
					//
					// [BIP 141]: https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki#witness-program
					let witness_bytes = &script_pubkey.as_bytes()[2..];
					let witness_program =
						WitnessProgram::new(version, witness_bytes).map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

					XOnlyPublicKey::from_slice(&witness_program.program().as_bytes()).map_err(
						|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						},
					)?;

					let utxo = Utxo {
						outpoint: u.outpoint,
						output: TxOut {
							value: u.txout.value,
							script_pubkey: ScriptBuf::new_witness_program(&witness_program),
						},
						satisfaction_weight: 1 /* empty script_sig */ * WITNESS_SCALE_FACTOR as u64 +
							1 /* witness items */ + 1 /* schnorr sig len */ + 64, // schnorr sig
						sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
					};
					utxos.push(utxo);
				},
				Some(version) => {
					log_error!(self.logger, "Unexpected witness version: {}", version,);
				},
				None => {
					log_error!(
						self.logger,
						"Tried to use a non-witness script. This must never happen."
					);
					panic!("Tried to use a non-witness script. This must never happen.");
				},
			}
		}

		Ok(utxos)
	}

	#[allow(deprecated)]
	async fn get_change_script_inner(&self) -> Result<ScriptBuf, ()> {
		let mut locked_persister = self.persister.lock().await;
		let (address_info, change_set) = {
			let mut locked_wallet = self.inner.lock().expect("lock");
			let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
			(address_info, locked_wallet.take_staged().unwrap_or_default())
		};
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
		})?;
		Ok(address_info.address.script_pubkey())
	}

	#[allow(deprecated)]
	pub(crate) fn sign_owned_inputs(&self, unsigned_tx: Transaction) -> Result<Transaction, ()> {
		let locked_wallet = self.inner.lock().expect("lock");

		let mut psbt = Psbt::from_unsigned_tx(unsigned_tx).map_err(|e| {
			log_error!(self.logger, "Failed to construct PSBT: {}", e);
		})?;
		// Use list_output rather than get_utxo to include outputs spent by unconfirmed
		// transactions (e.g., a prior splice being replaced via RBF), which a synced wallet would
		// otherwise no longer treat as an owned UTXO.
		let mut wallet_outputs: HashMap<OutPoint, LocalOutput> =
			locked_wallet.list_output().map(|output| (output.outpoint, output)).collect();
		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			if let Some(utxo) = wallet_outputs.remove(&txin.previous_output) {
				psbt.inputs[i] = locked_wallet.get_psbt_input(utxo, None, true).map_err(|e| {
					log_error!(self.logger, "Failed to construct PSBT input: {}", e);
				})?;
			}
		}

		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;

		match locked_wallet.sign(&mut psbt, sign_options) {
			Ok(finalized) => debug_assert!(!finalized),
			Err(e) => {
				log_error!(self.logger, "Failed to sign owned inputs: {}", e);
				return Err(());
			},
		}

		match psbt.extract_tx() {
			Ok(tx) => Ok(tx),
			Err(bitcoin::psbt::ExtractTxError::MissingInputValue { tx }) => Ok(tx),
			Err(e) => {
				log_error!(self.logger, "Failed to extract transaction: {}", e);
				Err(())
			},
		}
	}

	#[allow(deprecated)]
	fn sign_psbt_inner(&self, mut psbt: Psbt) -> Result<Transaction, ()> {
		let locked_wallet = self.inner.lock().expect("lock");

		// While BDK populates both `witness_utxo` and `non_witness_utxo` fields, LDK does not. As
		// BDK by default doesn't trust the witness UTXO to account for the Segwit bug, we must
		// disable it here as otherwise we fail to sign.
		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;

		match locked_wallet.sign(&mut psbt, sign_options) {
			Ok(_finalized) => {
				// BDK will fail to finalize for all LDK-provided inputs of the PSBT. Unfortunately
				// we can't check more fine grained if it succeeded for all the other inputs here,
				// so we just ignore the returned `finalized` bool.
			},
			Err(err) => {
				log_error!(self.logger, "Failed to sign transaction: {}", err);
				return Err(());
			},
		}

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			()
		})?;

		Ok(tx)
	}

	/// Classifies an on-chain broadcast handed to the broadcaster by LDK, recording a payment for it
	/// before it is sent when it affects this node's wallet.
	pub(crate) async fn classify_broadcast(
		&self, tx: &Transaction, tx_type: &LdkTransactionType,
	) -> Result<(), Error> {
		match tx_type {
			LdkTransactionType::Funding { channels } => {
				self.classify_funding(tx, channels, tx_type.clone().into()).await
			},
			// A splice round this node contributed to is recorded when it is signed
			// ([`Self::record_signed_funding`]) and marked as broadcast once LDK reports the splice
			// negotiated ([`Self::record_broadcast_splice_round`]), so its broadcast has nothing
			// left to record; a round without a contribution of ours is left for wallet sync.
			LdkTransactionType::InteractiveFunding { .. } => Ok(()),
			LdkTransactionType::UnilateralClose { .. } => Ok(()),
			LdkTransactionType::CooperativeClose { .. }
			| LdkTransactionType::AnchorBump { .. }
			| LdkTransactionType::Claim { .. }
			| LdkTransactionType::Sweep { .. } => {
				self.classify_regular_broadcast(tx, tx_type.clone().into()).await
			},
		}
	}

	/// Records a single-channel funding (channel open) broadcast as a pending on-chain payment,
	/// tagged with its transaction type. Amount and fee come from the wallet's view of the
	/// transaction. Batched funding is left for wallet sync.
	async fn classify_funding(
		&self, tx: &Transaction, channels: &[(PublicKey, ChannelId)], tx_type: TransactionType,
	) -> Result<(), Error> {
		if channels.len() != 1 {
			if channels.len() > 1 {
				log_trace!(
					self.logger,
					"Skipping funding classification for batched broadcast ({} channels)",
					channels.len()
				);
			}
			return Ok(());
		}

		let (_counterparty_node_id, channel_id) = channels[0];
		let txid = tx.compute_txid();
		let (amount_msat, fee_paid_msat, direction) = self.onchain_payment_fields(tx);

		// A funding transaction that moves no wallet funds carries nothing to record — e.g. LDK
		// re-broadcasts a promoted-but-unconfirmed 0conf splice through its generic funding path,
		// including splices the signing-time recording deliberately declined (no local
		// contribution, or a splice-out moving no wallet funds). Recording it here would
		// mint a zero-amount payment that nothing ever confirms. Skip on the wallet-derived
		// amount alone — the condition `interactive_funding_record` declines on; anything
		// declined there must be skipped here, or its re-broadcast resurrects the record. The fee
		// is no participation signal: the wallet resolves a splice's shared input whenever the
		// previous funding transaction touched it (e.g. it funded the original channel open).
		//
		// TODO(https://git.rust-bitcoin.org/lightningdevkit/rust-lightning/issues/4878): The
		// re-typed re-broadcasts are upstream behavior that should be fixed in `rust-lightning`:
		// the re-offer ought to keep its `InteractiveFunding` classification, or not recur at
		// all. `zero_conf_splice_out_funding_rebroadcast_canary` pins the current behavior by
		// asserting the log line below; when it fails against a newer LDK, re-evaluate whether
		// this skip still sees traffic.
		if amount_msat == Some(0) {
			log_trace!(
				self.logger,
				"Not recording channel-funding broadcast {} as a payment: no wallet-level activity",
				txid,
			);
			return Ok(());
		}

		// Resolution and the writes below must share one lock acquisition: resolved outside it,
		// the id could go stale against a record wallet sync creates for the same transaction,
		// and the write below would create a divergent record.
		let guard = self.funding_payment_update_lock.lock().await;

		// Adopt the id of a record that already tracks this transaction — e.g. a 0conf splice
		// re-broadcast through LDK's generic funding path resolves back to its
		// interactive-funding record here — otherwise generate a fresh id.
		let payment_id = self.find_payment_by_txid(txid).await?.unwrap_or_else(random_payment_id);

		// A promoted-but-unconfirmed 0conf splice comes back through this generic path re-typed
		// and carrying wallet-view figures; `funding_reclassification_update` declines the
		// downgrade, leaving no trace that a re-broadcast arrived. Log the arrival so tests can
		// observe the traffic; the read serves the log line alone, so a stale read costs no more.
		if let Some(current) = self.payment_store.get(&payment_id).await? {
			if matches!(
				current.kind,
				PaymentKind::Onchain {
					tx_type: Some(TransactionType::InteractiveFunding { .. }),
					..
				}
			) {
				log_trace!(
					self.logger,
					"Keeping interactive-funding classification over funding-typed rebroadcast {}",
					txid,
				);
			}
		}

		let details = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(tx_type),
			},
			amount_msat,
			fee_paid_msat,
			direction,
			PaymentStatus::Pending,
		);
		self.persist_funding_payment_locked(&guard, details, Vec::new()).await?;
		log_debug!(
			self.logger,
			"Recorded channel-funding broadcast {} for channel {}",
			txid,
			channel_id,
		);
		Ok(())
	}

	/// Resolves the id under which the interactive funding with negotiated history `candidates` is
	/// recorded: that of a record already tracking any of its rounds (wallet sync may record a
	/// round before this node does), else a fresh one. A record already failed is passed over:
	/// wallet sync fails a payment whose round lost to a conflicting spend confirmed while the
	/// channel stays open, LDK still holds the round and a fee bump of it is signed with the round
	/// among its candidates, and nothing revisits a failed record's status, so the bump filed under
	/// it would go untracked. An id derived from a txid would tie the record's identity to one
	/// round of a replaceable transaction — resolution through the record's txid history is what
	/// keeps its identity stable across RBF replacements. The caller holds the cross-store lock:
	/// resolved outside it, the id could go stale against a record wallet sync creates for the same
	/// transaction before the caller's write.
	async fn resolve_interactive_funding_id(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, candidates: &[FundingCandidate],
	) -> Result<PaymentId, Error> {
		for candidate in candidates.iter() {
			if let Some(id) = self.find_payment_by_txid(candidate.txid).await? {
				let failed = self
					.payment_store
					.get(&id)
					.await?
					.is_some_and(|payment| payment.status == PaymentStatus::Failed);
				if !failed {
					return Ok(id);
				}
			}
		}
		Ok(random_payment_id())
	}

	/// Builds the payment record, under the resolved `payment_id`, and the per-candidate figures
	/// for recording the `active` round of an interactive funding whose negotiated history is
	/// `candidates`. Returns `None` when there is nothing to record: no local contribution to the
	/// round, or no wallet-level activity.
	fn interactive_funding_record(
		&self, payment_id: PaymentId, candidates: &[FundingCandidate], active: &FundingCandidate,
		tx: &Transaction, tx_type: TransactionType,
	) -> Option<(PaymentDetails, Vec<FundingTxCandidate>)> {
		let txid = active.txid;

		let aggregate = aggregate_local_stakes(active);
		let amount_msat = match aggregate.amount_msat {
			Some(amt) => Some(amt),
			None => {
				log_trace!(
					self.logger,
					"Not recording signed funding {} as a payment: no local contribution",
					txid,
				);
				return None;
			},
		};
		let fee_paid_msat = aggregate.fee_paid_msat;
		let direction = aggregate.direction;

		// A contribution doesn't mean the tx touches our on-chain wallet: a splice-out to an
		// external address sends channel funds to a third party, which BDK sees as zero wallet
		// movement. Nothing for the on-chain payment store to record, so skip it.
		let (wallet_amount_msat, _wallet_fee_msat, _wallet_direction) =
			self.onchain_payment_fields(tx);
		if wallet_amount_msat == Some(0) {
			log_trace!(
				self.logger,
				"Not recording signed funding {} as a payment: no wallet-level activity",
				txid,
			);
			return None;
		}

		// Record every candidate's figures (`None` for any round we didn't contribute to, e.g. a
		// counterparty-initiated splice our `splice_in` later joined via RBF) so the confirmed
		// candidate's amount/fee can be applied on confirmation, even if it isn't the last one
		// broadcast or one we contributed to.
		let candidate_records: Vec<FundingTxCandidate> = candidates
			.iter()
			.map(|candidate| {
				let aggregate = aggregate_local_stakes(candidate);
				FundingTxCandidate {
					txid: candidate.txid,
					amount_msat: aggregate.amount_msat,
					fee_paid_msat: aggregate.fee_paid_msat,
					awaiting_broadcast: false,
				}
			})
			.collect();

		let details = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(tx_type),
			},
			amount_msat,
			fee_paid_msat,
			direction,
			PaymentStatus::Pending,
		);
		Some((details, candidate_records))
	}

	/// Records the funding payment of a splice round this node is about to sign, before
	/// [`ChannelManager::funding_transaction_signed`] releases our signatures: without them the
	/// counterparty cannot broadcast, so the record precedes anything wallet sync could observe.
	/// The round's broadcast adds nothing to the record; its `SpliceNegotiated` event only marks it
	/// as broadcast ([`Self::record_broadcast_splice_round`]).
	///
	/// `candidates` is the channel's pending splice history as [`funding_candidates`] lists it from
	/// the channel's [`SpliceDetails`], so the record is written in full, under the id
	/// [`Self::resolve_interactive_funding_id`] resolves (that of a record already tracking any
	/// round of the history, else a fresh one). The signed round is marked as awaiting broadcast
	/// until LDK reports the splice negotiated and [`Self::record_broadcast_splice_round`] clears
	/// the mark: only such a round can be abandoned without a trace, and
	/// [`Self::drop_abandoned_splice_rounds`] takes it back once LDK no longer holds it.
	///
	/// Nothing is recorded for a round missing from the history (reset between the event's
	/// emission and its handling, so LDK will refuse the signed transaction), already recorded (a
	/// replayed event), or without a local contribution or wallet-level activity. A failed write
	/// leaves no half-written record behind for the replayed event to build on.
	///
	/// [`ChannelManager::funding_transaction_signed`]: lightning::ln::channelmanager::ChannelManager::funding_transaction_signed
	pub(crate) async fn record_signed_funding(
		&self, tx: &Transaction, candidates: &[FundingCandidate],
	) -> Result<(), Error> {
		let txid = tx.compute_txid();
		let signed_round = match candidates.iter().find(|candidate| candidate.txid == txid) {
			Some(round) => round,
			None => {
				log_trace!(
					self.logger,
					"Not recording signed funding {}: not among the channel's pending splice rounds",
					txid,
				);
				// An earlier attempt at recording the round may have failed between the two
				// stores and failed to roll back; the round is gone, so what it left goes too.
				return self.drop_unindexed_signing_record(txid).await;
			},
		};
		let tx_type =
			LdkTransactionType::InteractiveFunding { candidates: candidates.to_vec() }.into();

		// Resolution, the reads and the writes below must share one lock acquisition, as in every
		// funding-record write: done outside it, the record could change under us before the write.
		let guard = self.funding_payment_update_lock.lock().await;
		let payment_id = self.resolve_interactive_funding_id(&guard, candidates).await?;
		let (details, mut history) = match self.interactive_funding_record(
			payment_id,
			candidates,
			signed_round,
			tx,
			tx_type,
		) {
			Some(record) => record,
			None => return Ok(()),
		};
		// Only the signed round awaits broadcast: LDK broadcast the others once their signatures
		// were exchanged.
		if let Some(signed) = history.iter_mut().find(|candidate| candidate.txid == txid) {
			signed.awaiting_broadcast = true;
		}

		let prior_pending = self.pending_payment_store.get(&payment_id).await?;
		// A replayed signing event re-offers a transaction already recorded; nothing to add.
		if prior_pending.as_ref().is_some_and(|entry| entry.candidate(txid).is_some()) {
			return Ok(());
		}
		// Merge LDK's history into the recorded one — refreshing the rounds both list, appending
		// the new ones — rather than replace it: LDK's history omits a recorded round it has since
		// abandoned, whose removal is `drop_abandoned_splice_rounds`' job once LDK reports the
		// failure, so a recorded round LDK no longer lists must survive the write.
		//
		// Refreshing an earlier round clears its awaiting-broadcast mark, which is right only
		// because LDK refuses a new negotiation while one awaits signatures and handles events in
		// order, stopping at the first failure: the earlier round's `SpliceNegotiated` event was
		// pushed before this signing event and has been handled by now. Should LDK ever reorder
		// them, this would clear the mark of a round whose event has not been handled yet.
		let mut recorded =
			prior_pending.as_ref().map(|entry| entry.candidates().to_vec()).unwrap_or_default();
		for candidate in history {
			match recorded.iter_mut().find(|stored| stored.txid == candidate.txid) {
				Some(stored) => *stored = candidate,
				None => recorded.push(candidate),
			}
		}

		// The write pair can fail between its two stores. The lock keeps the other writers of this
		// record out, bar graduation, which only ever moves a record out of `Pending`: put the
		// payment store back as it was while the record is still pending, or the replayed event
		// would find the half-written record and take it for prior state.
		let prior_details = self.payment_store.get(&payment_id).await?;
		if let Err(e) = self.persist_funding_payment_locked(&guard, details, recorded).await {
			let rollback = match &prior_details {
				Some(prior) => self
					.payment_store
					.mutate(&payment_id, |existing| {
						let current = existing?;
						(current.status == PaymentStatus::Pending && current != prior)
							.then(|| prior.clone())
					})
					.await
					.map(|_| ()),
				None => self.payment_store.remove(&payment_id).await,
			};
			if let Err(rollback_error) = rollback {
				log_error!(
					self.logger,
					"Failed to roll back the half-written funding record of payment {}: {}",
					payment_id,
					rollback_error,
				);
			}
			return Err(e);
		}
		log_debug!(
			self.logger,
			"Recorded signed splice funding {} ({} candidates)",
			txid,
			candidates.len(),
		);
		Ok(())
	}

	/// Marks a splice round recorded when signing ([`Self::record_signed_funding`]) as broadcast
	/// once LDK reports the splice negotiated: `SpliceNegotiated` is emitted as LDK hands the fully
	/// signed round to the broadcaster, so the counterparty holds our signatures by then and the
	/// round can no longer be abandoned without a trace. Nothing is written for a round no funding
	/// payment of `channel_id` tracks (no local contribution, or no wallet-level activity) or one
	/// already marked (a replayed event).
	pub(crate) async fn record_broadcast_splice_round(
		&self, channel_id: ChannelId, txid: Txid,
	) -> Result<(), Error> {
		// Serialize with the other funding-record writers, which all hold this lock from their
		// reads through their last write.
		let _guard = self.funding_payment_update_lock.lock().await;

		let entries = self
			.pending_payment_store
			.list_filter(|entry| {
				tracks_channel(entry, channel_id)
					&& entry.candidate(txid).is_some_and(|candidate| candidate.awaiting_broadcast)
			})
			.await;
		for entry in entries {
			let payment_id = entry.id();
			self.pending_payment_store
				.mutate(&payment_id, |existing| {
					let mut entry = existing?.clone();
					let PendingPaymentDetails::Tracked { candidates, .. } = &mut entry else {
						return None;
					};
					let round = candidates
						.iter_mut()
						.find(|candidate| candidate.txid == txid && candidate.awaiting_broadcast)?;
					round.awaiting_broadcast = false;
					Some(entry)
				})
				.await?;
			log_debug!(
				self.logger,
				"Marked splice round {} of channel {} as broadcast in funding payment {}",
				txid,
				channel_id,
				payment_id,
			);
		}
		Ok(())
	}

	/// Drops from a channel's funding records the splice rounds LDK abandoned before they could be
	/// broadcast. A round this node signed is recorded before our signatures leave the node
	/// ([`Self::record_signed_funding`]) and marked as awaiting broadcast until its
	/// `SpliceNegotiated` event clears the mark ([`Self::record_broadcast_splice_round`]). Should
	/// LDK drop the round in between — the counterparty aborts before the signatures are exchanged,
	/// or the channel closes — nothing can broadcast it anymore, and left in place the record would
	/// wait forever on a payment nothing can confirm.
	///
	/// `held_rounds` lists the rounds LDK still holds for the channel, as [`held_splice_rounds`]
	/// reads them (for a closed channel, its last funding and the rounds its monitor still watches,
	/// as [`closed_channel_held_rounds`] reads them). A recorded round is dropped if it awaits
	/// broadcast, LDK no longer holds it, and the wallet has not seen its transaction either — the
	/// counterparty may broadcast a round it received our signatures for while LDK still waits on
	/// its own. A round LDK handed the broadcaster keeps its place once its `SpliceNegotiated`
	/// event has cleared the mark, whether wallet sync has seen it yet or not; one whose event is
	/// still unhandled when the channel closes is listed in `held_rounds` because the channel's
	/// monitor, which saw the counterparty commit to it, still watches it, and so keeps its place
	/// as well, as does a round LDK promoted to the channel's funding (recorded by
	/// [`Self::resolve_promoted_splice_round`]), broadcast with its signatures exchanged whether
	/// or not its `SpliceNegotiated` event has cleared the mark yet. Dropping the record's current
	/// round hands the record back to the last remaining round this node contributed to, figures
	/// included; dropping the last such round removes the record, as whatever rounds remain are not
	/// this node's payment (LDK keeps this node's contributions to a suffix of the rounds). A record
	/// that no longer waits on the dropped round — wallet sync moved it on, or an earlier drop was
	/// cut short after moving it — keeps its state and only loses the round from its history.
	pub(crate) async fn drop_abandoned_splice_rounds(
		&self, channel_id: ChannelId, held_rounds: &[Txid],
	) -> Result<(), Error> {
		// Serialize with the other funding-record writers, which all hold this lock from their
		// reads through their last write.
		let guard = self.funding_payment_update_lock.lock().await;
		self.drop_abandoned_splice_rounds_locked(&guard, channel_id, held_rounds).await
	}

	/// [`Self::drop_abandoned_splice_rounds`] for a caller already holding the funding-record
	/// writers' lock.
	async fn drop_abandoned_splice_rounds_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, channel_id: ChannelId,
		held_rounds: &[Txid],
	) -> Result<(), Error> {
		let entries = self
			.pending_payment_store
			.list_filter(|entry| {
				tracks_channel(entry, channel_id)
					&& entry.candidates().iter().any(|candidate| candidate.awaiting_broadcast)
			})
			.await;

		for entry in entries {
			let payment_id = match entry.details() {
				Some(details) => details.id,
				None => continue,
			};
			let (abandoned, remaining): (Vec<FundingTxCandidate>, Vec<FundingTxCandidate>) = {
				let locked_wallet = self.inner.lock().expect("lock");
				// TODO(#1037): the graph learns a round LDK broadcast from wallet sync alone
				// today, so this check only adds what a sync has already seen to `held_rounds`.
				// It catches every broadcast round by itself, whichever caller — the startup
				// sweep or a live event — runs the drop, only once the `InteractiveFunding`
				// broadcast arm applies the round to the graph, which #1037 does not do: it
				// prepares only `Funding`-typed packages.
				entry.candidates().iter().cloned().partition(|candidate| {
					candidate.awaiting_broadcast
						&& !held_rounds.contains(&candidate.txid)
						&& !entry.locked_rounds().contains(&candidate.txid)
						&& locked_wallet.tx_graph().get_tx(candidate.txid).is_none()
				})
			};
			if abandoned.is_empty() {
				continue;
			}
			let abandoned_txids: Vec<Txid> = abandoned.iter().map(|c| c.txid).collect();
			// The record's transaction and figures are only handed back while they still describe
			// an abandoned round; a record wallet sync has since moved on is left as it stands,
			// and only its history shrinks.
			let waits_on_abandoned = |record: &PaymentDetails| {
				record.status == PaymentStatus::Pending
					&& matches!(
						&record.kind,
						PaymentKind::Onchain { txid, status: ConfirmationStatus::Unconfirmed, .. }
							if abandoned_txids.contains(txid)
					)
			};
			let record = self.payment_store.get(&payment_id).await?;
			let hands_back = record.as_ref().map_or(true, waits_on_abandoned);
			// A last remaining round without a contribution of ours means no remaining round has
			// one.
			let handed_back = remaining.last().filter(|round| round.amount_msat.is_some());

			if hands_back && handed_back.is_none() {
				// Nothing of this node's was ever broadcast under the record, so it goes rather
				// than fail a payment for a transaction that never existed. The payment record
				// goes first: the entry keeps resolving the rounds' txids, so a removal that
				// fails midway is finished by the replayed event.
				self.payment_store.remove(&payment_id).await?;
				self.pending_payment_store.remove(&payment_id).await?;
				log_info!(
					self.logger,
					"Dropped abandoned splice round(s) {:?} and funding payment {} with them",
					abandoned_txids,
					payment_id,
				);
				continue;
			}

			let mut mirrored = None;
			match handed_back {
				Some(active) if hands_back => {
					self.payment_store
						.mutate(&payment_id, |existing| {
							let current = existing?;
							if !waits_on_abandoned(current) {
								mirrored = Some(current.clone());
								return None;
							}
							let mut update = PaymentDetailsUpdate::new(payment_id);
							update.txid = Some(active.txid);
							update.confirmation_status = Some(ConfirmationStatus::Unconfirmed);
							update.amount_msat = Some(active.amount_msat);
							update.fee_paid_msat = Some(active.fee_paid_msat);
							let mut updated = current.clone();
							updated.update(update);
							mirrored = Some(updated.clone());
							Some(updated)
						})
						.await?;
				},
				_ => {
					// The record does not wait on the dropped rounds: wallet sync moved it on, or
					// an earlier drop was cut short between the two stores. Only its history
					// shrinks, and the entry's copy of the record catches up with the record while
					// the record is still pending.
					mirrored = record.filter(|current| current.status == PaymentStatus::Pending);
					log_warn!(
						self.logger,
						"Funding payment {} does not wait on abandoned splice round(s) {:?}: \
						dropping them from its history only",
						payment_id,
						abandoned_txids,
					);
				},
			}
			self.pending_payment_store
				.mutate(&payment_id, |existing| {
					let mut entry = existing?.clone();
					if let PendingPaymentDetails::Tracked { details, candidates, .. } = &mut entry {
						candidates.retain(|c| !abandoned_txids.contains(&c.txid));
						if let Some(mirrored) = mirrored {
							*details = mirrored;
						}
					}
					Some(entry)
				})
				.await?;
			log_info!(
				self.logger,
				"Dropped abandoned splice round(s) {:?} from funding payment {}",
				abandoned_txids,
				payment_id,
			);
		}
		Ok(())
	}

	/// Drops the splice rounds recorded when signing that LDK does not hold once the node restarts.
	/// LDK reports the loss of a negotiation its last channel manager write carried mid-way, but a
	/// round committed, negotiated and signed since that write is gone without a report if the
	/// node stopped before the next one. `held_rounds` yields the rounds LDK holds for a channel,
	/// as [`held_splice_rounds`] lists them, or `None` for a channel LDK no longer lists, which is
	/// left to its `ChannelClosed` event: LDK queues one for every channel it drops, and handling
	/// it takes back what neither the closed channel's funding nor its monitor holds. Runs before
	/// events are processed again, so no round is recorded while LDK's view is being read.
	pub(crate) async fn drop_splice_rounds_lost_across_restart(
		&self, held_rounds: impl Fn(ChannelId) -> Option<Vec<Txid>>,
	) -> Result<(), Error> {
		let channels: HashSet<ChannelId> = self
			.pending_payment_store
			.list_filter(|entry| {
				entry.candidates().iter().any(|candidate| candidate.awaiting_broadcast)
			})
			.await
			.iter()
			.flat_map(|entry| match entry.details().map(|details| &details.kind) {
				Some(PaymentKind::Onchain {
					tx_type: Some(TransactionType::InteractiveFunding { channels }),
					..
				}) => channels.iter().map(|channel| channel.channel_id).collect(),
				_ => Vec::new(),
			})
			.collect();
		for channel_id in channels {
			let Some(held) = held_rounds(channel_id) else {
				log_debug!(
					self.logger,
					"Leaving the signed splice rounds of channel {} to its ChannelClosed event",
					channel_id,
				);
				continue;
			};
			self.drop_abandoned_splice_rounds(channel_id, &held).await?;
		}
		Ok(())
	}

	/// Removes the half-written record of a signed round LDK has since abandoned: its write failed
	/// between the two stores and the rollback failed as well, leaving the payment record without
	/// the pending entry that indexes it. The replayed signing event, finding the round gone from
	/// the history, ends up here; a fully recorded round (its entry in place) is left to
	/// [`Self::drop_abandoned_splice_rounds`]. Only a first round can be left so: the record of a
	/// bump keeps the entry of the rounds before it, and wallet sync moves it on as an earlier
	/// round confirms or fails.
	async fn drop_unindexed_signing_record(&self, txid: Txid) -> Result<(), Error> {
		let _guard = self.funding_payment_update_lock.lock().await;
		let payment_id = match self.find_payment_by_txid(txid).await? {
			Some(id) => id,
			None => return Ok(()),
		};
		if self.pending_payment_store.get(&payment_id).await?.is_some() {
			return Ok(());
		}
		let unindexed = self.payment_store.get(&payment_id).await?.is_some_and(|record| {
			record.status == PaymentStatus::Pending
				&& matches!(
					&record.kind,
					PaymentKind::Onchain {
						txid: recorded,
						status: ConfirmationStatus::Unconfirmed,
						tx_type: Some(TransactionType::InteractiveFunding { .. }),
					} if *recorded == txid
				)
		});
		if unindexed {
			self.payment_store.remove(&payment_id).await?;
			log_info!(
				self.logger,
				"Dropped the half-written funding record of abandoned splice round {}",
				txid,
			);
		}
		Ok(())
	}

	/// Records a non-funding LDK broadcast as an on-chain payment, tagged with its transaction type.
	/// Wallet sync later refreshes confirmation status while preserving the type.
	async fn classify_regular_broadcast(
		&self, tx: &Transaction, tx_type: TransactionType,
	) -> Result<(), Error> {
		let txid = tx.compute_txid();
		let (amount_msat, fee_paid_msat, direction) = self.onchain_payment_fields(tx);

		if amount_msat == Some(0) && fee_paid_msat == Some(0) {
			log_trace!(
				self.logger,
				"Not recording classified broadcast {} as a payment: no wallet-level activity",
				txid,
			);
			return Ok(());
		}

		let details = PaymentDetails::new(
			PaymentId(txid.to_byte_array()),
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(tx_type),
			},
			amount_msat,
			fee_paid_msat,
			direction,
			PaymentStatus::Pending,
		);
		self.payment_store.insert_or_update(details).await?;
		log_debug!(self.logger, "Recorded classified on-chain broadcast {}", txid);
		Ok(())
	}

	/// Writes a freshly-classified funding payment to the authoritative payment store and adds a
	/// pending-store index entry, so wallet sync graduates it through `ANTI_REORG_DELAY`.
	///
	/// Production callers go through [`Self::persist_funding_payment_locked`] because they resolve
	/// the record's id under the same lock acquisition; this wrapper models that acquisition for
	/// tests writing a record mid-flow.
	#[cfg(test)]
	async fn persist_funding_payment(
		&self, details: PaymentDetails, candidates: Vec<FundingTxCandidate>,
	) -> Result<(), Error> {
		// Hold the cross-store lock across both writes so a funding confirmation never observes
		// the record classified but the candidate history it needs still missing.
		let guard = self.funding_payment_update_lock.lock().await;
		self.persist_funding_payment_locked(&guard, details, candidates).await
	}

	/// Writes a freshly recorded funding payment to the authoritative payment store and adds a
	/// pending-store index entry, so wallet sync graduates it through `ANTI_REORG_DELAY`. The
	/// caller holds the cross-store lock, resolving the record's id and performing both store
	/// writes under one acquisition, so a funding confirmation never observes the record written
	/// but the candidate history it needs still missing, and the resolved id never goes stale
	/// against a concurrent sync write.
	async fn persist_funding_payment_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, details: PaymentDetails,
		candidates: Vec<FundingTxCandidate>,
	) -> Result<(), Error> {
		// Everything this write does depends on the record's current state, so all of it must be
		// decided inside the store's critical section. When a record exists — no matter when it
		// appeared — only the classification (`tx_type`) and the figures of whichever candidate
		// the record's state makes authoritative are merged: a full merge of the fresh
		// Pending/Unconfirmed details would downgrade the confirmation state the wallet-sync
		// events own. Which candidate is authoritative is equally stateful: substituting the
		// confirmed candidate's figures requires seeing the confirmation. Selected from a read
		// taken before the lock, the choice goes stale when a confirmation lands in between —
		// the update still names the actively-broadcast candidate, the confirmed-figures guard
		// then rightly refuses it, and the record is left with figures no classification derived.
		let id = details.id;
		let mut update = None;
		self.payment_store
			.mutate(&id, |existing| {
				let reclassification =
					funding_reclassification_update(details.clone(), &candidates, existing);
				update = Some(reclassification.clone());
				match existing {
					None => Some(details.clone()),
					Some(current) => {
						let mut updated = current.clone();
						updated.update(reclassification).then_some(updated)
					},
				}
			})
			.await?;
		let update = update.expect("the mutate closure always runs");

		// The pending index must exist exactly while the authoritative record is Pending:
		// graduation and rebroadcast read it, and a graduated payment must not be re-indexed.
		// Deciding by the post-write status rather than by whether the write inserted also
		// repairs a missing index — a crash or failed write between the two stores leaves a
		// Pending record with no entry, and a merge alone would never recreate it, leaving the
		// payment unable to graduate and its txids unmapped.
		//
		// The status must be read inside the pending store's critical section. Graduation writes
		// `Succeeded` before removing the entry, so a read there that still observes `Pending`
		// is ordered before the removal, which then also deletes anything inserted here. A
		// status read taken before this write goes stale when graduation lands in between, and
		// would re-index the graduated payment.
		let payment_store = Arc::clone(&self.payment_store);
		self.pending_payment_store
			.mutate_async(&id, move |existing| async move {
				// The record was written above and a failed write has already returned, so it is
				// absent only if the user removed the payment meanwhile; fall back to the fresh
				// details.
				let recorded = payment_store.get(&id).await?.unwrap_or(details);
				Ok(match existing {
					// The inserted entry embeds the post-write record rather than the fresh
					// details, so a confirmation wallet sync already recorded keeps driving
					// graduation.
					None if recorded.status == PaymentStatus::Pending => {
						Some(PendingPaymentDetails::new(recorded, Vec::new(), candidates))
					},
					// The payment already advanced beyond Pending: the graduation path removed
					// the entry and it must not be re-created.
					None => None,
					// The entry predates this write — wallet sync recorded the transaction
					// before it was recorded as a funding (its arms and this write pair
					// serialize on the cross-store lock, so nothing lands in between): merge
					// only the funding classification into the existing entry.
					Some(mut entry) => {
						let pending_update = PendingPaymentDetailsUpdate {
							id,
							payment_update: Some(update),
							conflicting_txids: None,
							candidates,
							splice_intent: None,
						};
						entry.update(pending_update).then_some(entry)
					},
				})
			})
			.await?;
		Ok(())
	}

	/// Returns the wallet's view of a transaction as `(amount_msat, fee_msat, direction)`.
	pub(crate) fn onchain_payment_fields(
		&self, tx: &Transaction,
	) -> (Option<u64>, Option<u64>, PaymentDirection) {
		let locked_wallet = self.inner.lock().expect("lock");
		self.onchain_payment_fields_locked(&locked_wallet, tx)
	}

	/// [`Self::onchain_payment_fields`] against an already-locked wallet, so callers that hold the
	/// lock (e.g. [`Self::create_payment_from_tx`]) can reuse the derivation without re-locking.
	fn onchain_payment_fields_locked(
		&self, locked_wallet: &PersistedWallet<KVStoreWalletPersister>, tx: &Transaction,
	) -> (Option<u64>, Option<u64>, PaymentDirection) {
		let fee = locked_wallet.calculate_fee(tx).unwrap_or(Amount::ZERO);
		let (sent, received) = locked_wallet.sent_and_received(tx);
		let fee_sat = fee.to_sat();

		let (direction, amount_msat) = if sent > received {
			(
				PaymentDirection::Outbound,
				Some(
					(sent.to_sat().saturating_sub(fee_sat).saturating_sub(received.to_sat()))
						* 1000,
				),
			)
		} else {
			(
				PaymentDirection::Inbound,
				Some(
					received.to_sat().saturating_sub(sent.to_sat().saturating_sub(fee_sat)) * 1000,
				),
			)
		};

		(amount_msat, Some(fee_sat * 1000), direction)
	}

	fn create_payment_from_tx(
		&self, locked_wallet: &PersistedWallet<KVStoreWalletPersister>, txid: Txid,
		payment_id: PaymentId, tx: &Transaction, payment_status: PaymentStatus,
		confirmation_status: ConfirmationStatus,
	) -> PaymentDetails {
		// TODO: It would be great to introduce additional variants for
		// `ChannelFunding` and `ChannelClosing`. For the former, we could just
		// take a reference to `ChannelManager` here and check against
		// `list_channels`. But for the latter the best approach is much less
		// clear: for force-closes/HTLC spends we should be good querying
		// `OutputSweeper::tracked_spendable_outputs`, but regular channel closes
		// (i.e., `SpendableOutputDescriptor::StaticOutput` variants) are directly
		// spent to a wallet address. The only solution I can come up with is to
		// create and persist a list of 'static pending outputs' that we could use
		// here to determine the `PaymentKind`, but that's not really satisfactory, so
		// we're punting on it until we can come up with a better solution.

		let kind = PaymentKind::Onchain { txid, status: confirmation_status, tx_type: None };

		let (amount_msat, fee_paid_msat, direction) =
			self.onchain_payment_fields_locked(locked_wallet, tx);

		PaymentDetails::new(payment_id, kind, amount_msat, fee_paid_msat, direction, payment_status)
	}

	/// Inserts or refreshes the pending-store entry tracking `payment` toward graduation,
	/// atomically with reading the entry's current state.
	async fn upsert_pending_payment(
		&self, payment: PaymentDetails, conflicting_txids: Vec<Txid>,
	) -> Result<(), Error> {
		let id = payment.id;
		let payment_store = Arc::clone(&self.payment_store);
		self.pending_payment_store
			.mutate_async(&id, move |existing| async move {
				// Only `Pending` payments belong in the pending store. Like in
				// [`Self::persist_funding_payment`], the authoritative status is re-read inside
				// the store's critical section, where it cannot go stale against graduation.
				let is_pending = payment_store
					.get(&id)
					.await?
					.map_or(payment.status == PaymentStatus::Pending, |recorded| {
						recorded.status == PaymentStatus::Pending
					});
				if !is_pending {
					return Ok(None);
				}
				Ok(match existing {
					None => {
						Some(PendingPaymentDetails::new(payment, conflicting_txids, Vec::new()))
					},
					// Promote a pre-broadcast splice intent: wallet sync saw the splice
					// transaction before this node recorded it as a funding payment. Carrying the
					// intent into the `Tracked` record makes the entry visible to txid lookups
					// while the retrier keeps the intent until the splice locks.
					Some(PendingPaymentDetails::PendingSplice { intent, .. }) => {
						Some(PendingPaymentDetails::tracked(
							payment,
							conflicting_txids,
							Vec::new(),
							Some(intent),
						))
					},
					Some(mut tracked @ PendingPaymentDetails::Tracked { .. }) => {
						let fresh =
							PendingPaymentDetails::new(payment, conflicting_txids, Vec::new());
						tracked.update(fresh.to_update()).then_some(tracked)
					},
				})
			})
			.await?;
		Ok(())
	}

	async fn find_payment_by_txid(&self, target_txid: Txid) -> Result<Option<PaymentId>, Error> {
		let direct_payment_id = PaymentId(target_txid.to_byte_array());
		if self.pending_payment_store.contains_key(&direct_payment_id).await? {
			return Ok(Some(direct_payment_id));
		}

		if let Some(replaced_details) = self
			.pending_payment_store
			.list_filter(|p| {
				p.details().is_some_and(
					|d| matches!(d.kind, PaymentKind::Onchain { txid, .. } if txid == target_txid),
				) || p.conflicting_txids().contains(&target_txid)
					// A middle RBF round is not the record's current txid and may never have
					// received a `TxReplaced` event of its own, so map any of its candidate
					// txids (an earlier RBF round may confirm) back to the record.
					|| p.candidate(target_txid).is_some()
			})
			.await
			.first()
		{
			return Ok(Some(replaced_details.id()));
		}

		// The pending store only indexes in-flight records — graduation removes the entry — so a
		// graduated record's transaction resolves through the payment store itself. Without this, a
		// funding-typed broadcast classified after graduation (e.g. LDK re-broadcasting a promoted
		// 0conf splice whose confirmation landed while the node was offline) would create a
		// duplicate record, and a post-graduation reorg's events would never reach the record.
		let mut page_token = None;
		loop {
			let page = self.payment_store.list_page(page_token).await?;
			if let Some(payment) = page.objects.iter().find(
				|p| matches!(p.kind, PaymentKind::Onchain { txid, .. } if txid == target_txid),
			) {
				return Ok(Some(payment.id));
			}
			match page.next_page_token {
				Some(token) => page_token = Some(token),
				None => break,
			}
		}

		Ok(None)
	}

	/// If `payment_id` refers to a classified funding payment, refreshes its confirmation status
	/// and the candidate txid the event refers to, while preserving the contribution-derived
	/// amount/fee and `tx_type` that wallet sync must not recompute from its own view: the wallet's
	/// `sent`/`received` don't capture our contribution to a shared funding output. Returns
	/// [`FundingStatusUpdate::Applied`] when it handled the payment, so the caller skips the
	/// default on-chain path — or [`FundingStatusUpdate::Foreign`] when the transaction is not
	/// part of the payment's funding history, so the caller records it under its own id.
	/// Graduation to `Succeeded` is left to `ChainTipChanged` after `ANTI_REORG_DELAY`.
	///
	/// The caller must hold [`Self::funding_payment_update_lock`] — from resolving `payment_id`
	/// through its own last write, not just across this call — so that classification's two-store
	/// write pair cannot interleave with the caller's decision sequence. The `_guard` parameter
	/// serves as a reminder of that contract.
	async fn apply_funding_status_update_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, payment_id: PaymentId, event_txid: Txid,
		confirmation_status: ConfirmationStatus,
	) -> Result<FundingStatusUpdate, Error> {
		// The caller's wallet-level lock keeps the candidate history stable while we await its
		// read. The funding-type gate, the candidate lookup, and the write then share the payment
		// store's mutation lock: against a separate payment `get`, a classification merging in
		// between would have its `tx_type` and contribution figures clobbered by this stale
		// snapshot.
		let pending_payment = self.pending_payment_store.get(&payment_id).await?;
		let mut outcome = FundingStatusUpdate::NotFunding;
		let mut handled = None;
		self.payment_store
			.mutate(&payment_id, |existing| {
				let payment = existing?;
				let (current_txid, tx_type) = match &payment.kind {
					PaymentKind::Onchain {
						txid,
						tx_type:
							tx_type @ Some(
								TransactionType::Funding { .. }
								| TransactionType::InteractiveFunding { .. },
							),
						..
					} => (*txid, tx_type.clone()),
					_ => return None,
				};
				// Adopt the event's txid only when the transaction is part of this payment's
				// funding history: its current txid or a classified candidate. A conflicting
				// transaction that is neither — a close also spends the funding outpoint — must
				// not overwrite the record.
				let owns_event_tx = event_txid == current_txid
					|| pending_payment.as_ref().is_some_and(|p| p.candidate(event_txid).is_some());
				if !owns_event_tx {
					outcome = FundingStatusUpdate::Foreign;
					return None;
				}
				// Report the figures of the candidate that actually confirmed, which need not be
				// the last one broadcast (an earlier, lower-fee candidate may win) and may carry
				// no figures at all (`None`) for a round we didn't contribute to. (`direction` is
				// invariant across a splice's candidates and cannot be changed through the store
				// anyway.)
				let mut target = payment.clone();
				if let Some(candidate) =
					pending_payment.as_ref().and_then(|p| p.candidate(event_txid))
				{
					target.amount_msat = candidate.amount_msat;
					target.fee_paid_msat = candidate.fee_paid_msat;
				}
				target.kind =
					PaymentKind::Onchain { txid: event_txid, status: confirmation_status, tx_type };

				// Merge through the update machinery so its rules (e.g. which fields a merge may
				// touch) keep applying, and skip the write when nothing changed.
				let mut merged = payment.clone();
				if merged.update(target.to_update()) {
					handled = Some(merged.clone());
					Some(merged)
				} else {
					handled = Some(payment.clone());
					None
				}
			})
			.await?;
		let Some(payment) = handled else {
			return Ok(outcome);
		};
		// Mirror the refreshed confirmation status onto the pending entry: `ChainTipChanged`
		// graduates by reading the pending entry's details, so it must see the new status. This is
		// the same dual-write the default `TxConfirmed` path performs; an empty conflicting-txids
		// list leaves any stored conflicts intact (the update treats absent as "unchanged").
		if payment.status == PaymentStatus::Pending {
			self.upsert_pending_payment(payment, Vec::new()).await?;
		}
		Ok(FundingStatusUpdate::Applied)
	}

	#[allow(deprecated)]
	pub(crate) async fn bump_fee_rbf(
		&self, payment_id: PaymentId, fee_rate: Option<FeeRate>, cur_anchor_reserve_sats: u64,
	) -> Result<Txid, Error> {
		let payment = self.payment_store.get(&payment_id).await?.ok_or_else(|| {
			log_error!(self.logger, "Payment {} not found in payment store", payment_id);
			Error::InvalidPaymentId
		})?;

		// Funding transactions (channel opens and splices) are driven by LDK's funding/splice
		// lifecycle, not the on-chain wallet. Replacing one via on-chain RBF would broadcast a
		// transaction LDK isn't tracking (and, for splices, can't sign). Fee-bumping a pending
		// splice goes through `bump_channel_funding_fee` instead.
		if let PaymentKind::Onchain {
			tx_type:
				Some(TransactionType::Funding { .. } | TransactionType::InteractiveFunding { .. }),
			..
		} = &payment.kind
		{
			log_error!(
				self.logger,
				"Cannot RBF funding payment {} via bump_fee_rbf; use bump_channel_funding_fee instead",
				payment_id,
			);
			return Err(Error::InvalidPaymentId);
		}

		if let PaymentKind::Onchain { status, .. } = &payment.kind {
			match status {
				ConfirmationStatus::Confirmed { .. } => {
					log_error!(
						self.logger,
						"Transaction {} is already confirmed and cannot be replaced via RBF",
						payment_id
					);
					return Err(Error::InvalidPaymentId);
				},
				ConfirmationStatus::Unconfirmed => {},
			}
		}

		if payment.direction != PaymentDirection::Outbound {
			log_error!(
				self.logger,
				"Cannot RBF payment {}: only outbound payments can be replaced",
				payment_id
			);
			return Err(Error::InvalidPaymentId);
		}

		let txid = match &payment.kind {
			PaymentKind::Onchain { txid, .. } => *txid,
			_ => {
				log_error!(
					self.logger,
					"Payment {} is not an on-chain payment, cannot be replaced via RBF",
					payment_id
				);
				return Err(Error::InvalidPaymentId);
			},
		};

		let mut locked_persister = self.persister.lock().await;
		let mut locked_wallet = self.inner.lock().expect("lock");

		debug_assert!(
			locked_wallet.tx_details(txid).is_some(),
			"Transaction {} expected in wallet but not found",
			txid,
		);
		let old_tx = locked_wallet
			.tx_details(txid)
			.ok_or_else(|| {
				log_error!(self.logger, "Transaction {} not found in wallet", txid);
				Error::InvalidPaymentId
			})?
			.tx
			.deref()
			.clone();

		let old_fee_rate = locked_wallet.calculate_fee_rate(&old_tx).map_err(|e| {
			log_error!(self.logger, "Failed to calculate fee rate of transaction {}: {}", txid, e);
			Error::WalletOperationFailed
		})?;

		// BIP 125 requires the replacement to pay a higher fee rate than the original.
		// The minimum increase is the incremental relay fee.
		let min_required_fee_rate_sat_per_kwu =
			old_fee_rate.to_sat_per_kwu() + INCREMENTAL_RELAY_FEE_SAT_PER_1000_WEIGHT as u64;

		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let estimated_fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);

		// Use the higher of minimum RBF requirement or current network estimate
		let final_fee_rate_sat_per_kwu =
			min_required_fee_rate_sat_per_kwu.max(estimated_fee_rate.to_sat_per_kwu());
		let final_fee_rate =
			fee_rate.unwrap_or_else(|| FeeRate::from_sat_per_kwu(final_fee_rate_sat_per_kwu));

		let mut psbt = {
			let mut builder = locked_wallet.build_fee_bump(txid).map_err(|e| {
				log_error!(self.logger, "BDK fee bump failed for {}: {:?}", txid, e);
				match e {
					BuildFeeBumpError::TransactionNotFound(_) => Error::InvalidPaymentId,
					BuildFeeBumpError::TransactionConfirmed(_) => {
						log_error!(self.logger, "Payment {} is already confirmed", payment_id);
						Error::InvalidPaymentId
					},
					BuildFeeBumpError::IrreplaceableTransaction(_) => {
						Error::OnchainTxCreationFailed
					},
					BuildFeeBumpError::FeeRateUnavailable => Error::FeerateEstimationUpdateFailed,
					BuildFeeBumpError::UnknownUtxo(_) => Error::OnchainTxCreationFailed,
					BuildFeeBumpError::InvalidOutputIndex(_) => Error::OnchainTxCreationFailed,
				}
			})?;

			builder.fee_rate(final_fee_rate);

			match builder.finish() {
				Ok(psbt) => Ok(psbt),
				Err(CreateTxError::FeeRateTooLow { required: required_fee_rate }) => {
					if fee_rate.is_some() {
						log_error!(
							self.logger,
							"Provided fee rate {} is too low for RBF fee bump of txid {}, required minimum fee rate: {}",
							fee_rate.expect("fee rate is set"),
							txid,
							required_fee_rate
						);
						return Err(Error::InvalidFeeRate);
					}

					log_info!(self.logger, "BDK requires higher fee rate: {}", required_fee_rate);

					// BDK may require a higher fee rate than our estimate due to
					// differences in UTXO selection or transaction weight calculations.
					// We cap the retry at 1.5x our target fee rate as a safety bound
					// to avoid overpaying.
					let max_allowed_fee_rate = FeeRate::from_sat_per_kwu(
						final_fee_rate_sat_per_kwu.saturating_mul(3).saturating_div(2),
					);
					if required_fee_rate > max_allowed_fee_rate {
						log_error!( self.logger, "BDK required fee rate {} exceeds sanity cap {} (1.5x our estimate) for tx {}", required_fee_rate, max_allowed_fee_rate, txid );
						return Err(Error::InvalidFeeRate);
					}

					let mut builder = locked_wallet.build_fee_bump(txid).map_err(|e| {
						log_error!(self.logger, "BDK fee bump retry failed for {}: {:?}", txid, e);
						Error::InvalidFeeRate
					})?;

					builder.fee_rate(required_fee_rate);
					builder.finish().map_err(|e| {
						log_error!(
							self.logger,
							"Failed to finish PSBT with required fee rate: {:?}",
							e
						);
						Error::InvalidFeeRate
					})
				},
				Err(e) => {
					log_error!(self.logger, "Failed to create fee bump PSBT: {:?}", e);
					Err(Error::InvalidFeeRate)
				},
			}?
		};

		let old_fee_sats = locked_wallet
			.calculate_fee(&old_tx)
			.map_err(|e| {
				log_error!(self.logger, "Failed to calculate fee of transaction {}: {}", txid, e);
				Error::WalletOperationFailed
			})?
			.to_sat();
		let replacement_fee_sats = locked_wallet
			.calculate_fee(&psbt.unsigned_tx)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to calculate fee of replacement transaction for {}: {}",
					txid,
					e
				);
				Error::WalletOperationFailed
			})?
			.to_sat();
		let additional_fee_sats = replacement_fee_sats.saturating_sub(old_fee_sats);
		let balance = locked_wallet.balance();
		let spendable_amount_sats =
			self.get_balances_inner(balance, cur_anchor_reserve_sats).map(|(_, s)| s).unwrap_or(0);
		if spendable_amount_sats < additional_fee_sats {
			log_error!(
				self.logger,
				"Unable to bump fee due to insufficient reserve-preserving funds. \
					Available: {}sats, required additional fee: {}sats, reserve: {}sats",
				spendable_amount_sats,
				additional_fee_sats,
				cur_anchor_reserve_sats,
			);
			return Err(Error::InsufficientFunds);
		}

		match locked_wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					log_error!(self.logger, "Failed to finalize signing for fee bump of {}", txid);
					return Err(Error::OnchainTxCreationFailed);
				}
			},
			Err(err) => {
				log_error!(
					self.logger,
					"Failed to sign fee bump transaction for {}: {}",
					txid,
					err
				);
				return Err(err.into());
			},
		}

		let fee_bumped_tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract fee bump transaction for {}: {}", txid, e);
			e
		})?;

		let new_txid = fee_bumped_tx.compute_txid();

		let new_payment = self.create_payment_from_tx(
			&locked_wallet,
			new_txid,
			payment.id,
			&fee_bumped_tx,
			PaymentStatus::Pending,
			ConfirmationStatus::Unconfirmed,
		);

		let change_set = locked_wallet.take_staged().unwrap_or_default();
		drop(locked_wallet);
		locked_persister.persist_changeset(change_set).await.map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet after fee bump of {}: {}", txid, e);
			Error::PersistenceFailed
		})?;

		self.payment_store.insert_or_update(new_payment.clone()).await?;
		self.upsert_pending_payment(new_payment, Vec::new()).await?;

		self.broadcaster.broadcast_unclassified_transaction(fee_bumped_tx);

		log_info!(self.logger, "RBF successful: replaced {} with {}", txid, new_txid);

		Ok(new_txid)
	}
}

struct LocalStakeAggregate {
	amount_msat: Option<u64>,
	fee_paid_msat: Option<u64>,
	direction: PaymentDirection,
}

/// Aggregates our net stake across the channels of a single [`FundingCandidate`] by summing each
/// channel's signed [`FundingContribution::net_value`]. Returns no amount if we contributed to none
/// of them.
fn aggregate_local_stakes(candidate: &FundingCandidate) -> LocalStakeAggregate {
	let mut net_stake = SignedAmount::ZERO;
	let mut fee = Amount::ZERO;
	let mut have_contribution = false;
	for channel in &candidate.channels {
		if let Some(contribution) = channel.contribution.as_ref() {
			have_contribution = true;
			net_stake += contribution.net_value();
			// `estimated_fee` is our per-contributor share, so summing across channels is correct.
			fee += contribution.estimated_fee();
		}
	}
	if !have_contribution {
		return LocalStakeAggregate {
			amount_msat: None,
			fee_paid_msat: None,
			direction: PaymentDirection::Outbound,
		};
	}
	// Direction is from our on-chain wallet's perspective: a positive net stake funds the channel
	// (Outbound), while a negative one is a splice-out that returns funds to the wallet (Inbound).
	let direction = if net_stake >= SignedAmount::ZERO {
		PaymentDirection::Outbound
	} else {
		PaymentDirection::Inbound
	};
	LocalStakeAggregate {
		amount_msat: Some(net_stake.unsigned_abs().to_sat() * 1000),
		fee_paid_msat: Some(fee.to_sat() * 1000),
		direction,
	}
}

/// Whether `entry` is the funding payment of a splice into `channel_id`.
fn tracks_channel(entry: &PendingPaymentDetails, channel_id: ChannelId) -> bool {
	match entry.details().map(|details| &details.kind) {
		Some(PaymentKind::Onchain {
			tx_type: Some(TransactionType::InteractiveFunding { channels }),
			..
		}) => channels.iter().any(|channel| channel.channel_id == channel_id),
		_ => false,
	}
}

/// Lists a channel's pending splice rounds that have a transaction — the negotiated predecessors
/// and the round awaiting signatures, in LDK's order, each with this node's contribution to it —
/// as the [`FundingCandidate`]s LDK hands the broadcaster for the round, for recording the round
/// when signing it. A contribution still queued behind the pending rounds has no transaction and
/// is left out; a channel with no pending splice yields nothing.
pub(crate) fn funding_candidates(
	details: Option<&SpliceDetails>, counterparty_node_id: PublicKey, channel_id: ChannelId,
) -> Vec<FundingCandidate> {
	details
		.map(|details| details.candidates.as_slice())
		.unwrap_or(&[])
		.iter()
		.filter_map(|candidate| {
			let txid = round_txid(candidate)?;
			Some(FundingCandidate {
				txid,
				channels: vec![ChannelFunding {
					counterparty_node_id,
					channel_id,
					purpose: FundingPurpose::Splice,
					contribution: candidate.contribution.clone(),
				}],
			})
		})
		.collect()
}

/// The transaction of a pending splice round, once it has one: a negotiated round's, or the
/// round awaiting signatures'.
fn round_txid(candidate: &SpliceCandidateDetails) -> Option<Txid> {
	match &candidate.status {
		SpliceCandidateStatus::Negotiated { txid, .. }
		| SpliceCandidateStatus::AwaitingSignatures { txid, .. } => Some(*txid),
		_ => None,
	}
}

/// The splice rounds LDK holds for a channel, as [`Wallet::drop_abandoned_splice_rounds`] takes
/// them: the pending rounds with a transaction, as [`funding_candidates`] lists them, and the
/// channel's current funding. A zero-conf splice is promoted to the funding as soon as
/// `splice_locked` is exchanged, before its transaction confirms, so it leaves the pending rounds
/// while its record may still await the `SpliceNegotiated` event that marks it broadcast.
pub(crate) fn held_splice_rounds(
	details: Option<&SpliceDetails>, funding_txo: Option<LdkOutPoint>,
) -> Vec<Txid> {
	let mut held: Vec<Txid> = details
		.map(|details| details.candidates.as_slice())
		.unwrap_or(&[])
		.iter()
		.filter_map(round_txid)
		.collect();
	held.extend(funding_txo.map(|funding| funding.txid));
	held
}

/// The splice rounds LDK still holds for a closed channel, as
/// [`Wallet::drop_abandoned_splice_rounds`] takes them: the channel's last funding — which a
/// zero-conf splice may have become before its transaction confirmed — and every transaction the
/// channel's monitor still watches. The channel manager forgets a pending round with the channel
/// and reports no failed negotiation for one awaiting the counterparty's signatures, but the
/// monitor keeps watching every pending round the counterparty's `commitment_signed` reached, until
/// a sibling locks or the close matures, and our signatures cannot have left the node before that
/// message: such a round may yet confirm and is left to wallet sync or `DiscardFunding` to resolve,
/// while a round the monitor never watched never had our signatures released. The watched
/// transactions also include the funding and whatever spent it on chain, which no recorded round
/// is. A funding the channel moved on from before it confirmed — a zero-conf splice a later splice
/// built on — is held by neither and can confirm still; the funding payments keep such rounds
/// themselves (see [`Wallet::resolve_promoted_splice_round`]).
pub(crate) fn closed_channel_held_rounds(
	funding_txo: Option<LdkOutPoint>, watched_txids: impl IntoIterator<Item = Txid>,
) -> Vec<Txid> {
	let mut held: Vec<Txid> = funding_txo.map(|funding| funding.txid).into_iter().collect();
	for txid in watched_txids {
		if !held.contains(&txid) {
			held.push(txid);
		}
	}
	held
}

/// The occasion on which [`Wallet::fail_funding_payments_without_held_round_locked`] resolves a
/// channel's funding payments by the rounds LDK holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FundingResolution {
	/// The channel closed.
	Close,
	/// LDK promoted the given splice round to the channel's funding.
	Promotion(Txid),
}

/// The outcome of [`Wallet::fail_unconfirmed_funding_payment_locked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FundingPaymentFailure {
	/// The payment was failed and its pending entry removed.
	Failed,
	/// The payment was failed already — by a pass whose entry removal was lost to a crash — and
	/// only the lingering entry was removed.
	EntryRemoved,
	/// The record no longer waits on the transaction; nothing was touched.
	MovedOn,
}

/// Generates a fresh funding-record [`PaymentId`] from the OS entropy source. A funding record's id
/// carries no meaning beyond uniqueness: the record is found through its transaction history
/// ([`Wallet::find_payment_by_txid`]), never re-derived from a txid.
fn random_payment_id() -> PaymentId {
	let mut bytes = [0u8; 32];
	getrandom::fill(&mut bytes).expect("getrandom failed");
	PaymentId(bytes)
}

/// The outcome of [`Wallet::apply_funding_status_update_locked`].
enum FundingStatusUpdate {
	/// The event's transaction belongs to the funding payment; its refreshed confirmation status
	/// was applied (or was already current).
	Applied,
	/// The resolved payment is not a classified funding payment; the caller's default on-chain
	/// handling applies under the resolved id.
	NotFunding,
	/// The event's transaction is not part of the funding payment's history — e.g. a close
	/// spending the same funding outpoint — so the funding record must not adopt it; the caller
	/// should record the transaction under its own txid-derived id.
	Foreign,
}

impl Listen for Wallet {
	fn filtered_block_connected(
		&self, _header: &bitcoin::block::Header,
		_txdata: &lightning::chain::transaction::TransactionData, _height: u32,
	) {
		debug_assert!(false, "Syncing filtered blocks is currently not supported");
		// As far as we can tell this would be a no-op anyways as we don't have to tell BDK about
		// the header chain of intermediate blocks. According to the BDK team, it's sufficient to
		// only connect full blocks starting from the last point of disagreement.
	}

	fn block_connected(&self, block: &bitcoin::Block, height: u32) {
		self.runtime.block_on(async {
			let mut locked_persister = self.persister.lock().await;
			let events = {
				let mut locked_wallet = self.inner.lock().expect("lock");

				let pre_checkpoint = locked_wallet.latest_checkpoint();
				if pre_checkpoint.height() != height - 1
					|| pre_checkpoint.hash() != block.header.prev_blockhash
				{
					log_debug!(
						self.logger,
						"Detected reorg while applying a connected block to on-chain wallet: new block with hash {} at height {}",
						block.header.block_hash(),
						height
					);
				}

				// In order to be able to reliably calculate fees the `Wallet` needs access to the previous
				// ouput data. To this end, we here insert any ouputs of transactions that LDK is intersted
				// in (e.g., funding transaction ouputs) into the wallet's transaction graph when we see
				// them, so it is reliably able to calculate fees for subsequent spends.
				//
				// FIXME: technically, we should also do this for mempool transactions. However, at the
				// current time fixing the edge case doesn't seem worth the additional conplexity /
				// additional overhead..
				let registered_txids = self.chain_source.registered_txids();
				for tx in &block.txdata {
					let txid = tx.compute_txid();
					if registered_txids.contains(&txid) {
						for (vout, txout) in tx.output.iter().enumerate() {
							let outpoint = OutPoint { txid, vout: vout as u32 };
							locked_wallet.insert_txout(outpoint, txout.clone());
						}
					}
				}

				match locked_wallet.apply_block_events(block, height) {
					Ok(events) => events,
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to apply connected block to on-chain wallet: {}",
							e
						);
						return;
					},
				}
			};

			if let Err(e) = self.update_payment_store(events).await {
				log_error!(self.logger, "Failed to update payment store: {}", e);
				return;
			}

			let change_set = self.inner.lock().expect("lock").take_staged().unwrap_or_default();
			if let Err(e) = locked_persister.persist_changeset(change_set).await {
				log_error!(self.logger, "Failed to persist on-chain wallet: {}", e);
				return;
			}
		});
	}

	fn blocks_disconnected(&self, _fork_point_block: BlockLocator) {
		// This is a no-op as we don't have to tell BDK about disconnections. According to the BDK
		// team, it's sufficient in case of a reorg to always connect blocks starting from the last
		// point of disagreement.
	}
}

impl WalletSource for Wallet {
	fn list_confirmed_utxos<'a>(
		&'a self,
	) -> impl Future<Output = Result<Vec<Utxo>, ()>> + Send + 'a {
		async move { self.list_confirmed_utxos_inner() }
	}

	fn get_change_script<'a>(&'a self) -> impl Future<Output = Result<ScriptBuf, ()>> + Send + 'a {
		async move { self.get_change_script_inner().await }
	}

	fn get_prevtx<'a>(
		&'a self, outpoint: OutPoint,
	) -> impl Future<Output = Result<Transaction, ()>> + Send + 'a {
		async move {
			let locked_wallet = self.inner.lock().expect("lock");
			locked_wallet
				.tx_details(outpoint.txid)
				.map(|tx_details| tx_details.tx.deref().clone())
				.ok_or_else(|| {
					log_error!(
						self.logger,
						"Failed to get previous transaction for {}",
						outpoint.txid
					);
				})
		}
	}

	fn sign_psbt<'a>(
		&'a self, psbt: Psbt,
	) -> impl Future<Output = Result<Transaction, ()>> + Send + 'a {
		async move { self.sign_psbt_inner(psbt) }
	}
}

// Anchor bumping uses LdkWallet for coin selection, which wraps a WalletSource to implement
// CoinSelectionSource. Splicing uses this implementation of coin selection instead.
impl CoinSelectionSource for Wallet {
	fn select_confirmed_utxos<'a>(
		&'a self, claim_id: Option<ClaimId>, must_spend: Vec<Input>, must_pay_to: &'a [TxOut],
		target_feerate_sat_per_1000_weight: u32, _max_tx_weight: u64,
	) -> impl Future<Output = Result<CoinSelection, ()>> + Send + 'a {
		debug_assert!(claim_id.is_none());
		let fee_rate = FeeRate::from_sat_per_kwu(target_feerate_sat_per_1000_weight as u64);
		async move { self.select_confirmed_utxos(must_spend, must_pay_to, fee_rate).await }
	}

	fn sign_psbt<'a>(
		&'a self, psbt: Psbt,
	) -> impl Future<Output = Result<Transaction, ()>> + Send + 'a {
		debug_assert!(false);
		async move { self.sign_psbt_inner(psbt) }
	}
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub(crate) struct WalletKeysManager {
	inner: KeysManager,
	wallet: Arc<Wallet>,
	logger: Arc<Logger>,
}

impl WalletKeysManager {
	/// Constructs a `WalletKeysManager` that overrides the destination and shutdown scripts.
	///
	/// See [`KeysManager::new`] for more information on `seed`, `starting_time_secs`, and
	/// `starting_time_nanos`.
	pub fn new(
		seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32, wallet: Arc<Wallet>,
		logger: Arc<Logger>,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos, true);
		Self { inner, wallet, logger }
	}

	pub fn sign_message(&self, msg: &[u8]) -> String {
		message_signing::sign(msg, &self.inner.get_node_secret_key())
	}

	pub fn get_node_secret_key(&self) -> SecretKey {
		self.inner.get_node_secret_key()
	}

	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		message_signing::verify(msg, sig, pkey)
	}
}

impl NodeSigner for WalletKeysManager {
	fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
		self.inner.get_node_id(recipient)
	}

	fn ecdh(
		&self, recipient: Recipient, other_key: &PublicKey, tweak: Option<&Scalar>,
	) -> Result<SharedSecret, ()> {
		self.inner.ecdh(recipient, other_key, tweak)
	}

	fn get_expanded_key(&self) -> ExpandedKey {
		self.inner.get_expanded_key()
	}

	fn get_peer_storage_key(&self) -> PeerStorageKey {
		self.inner.get_peer_storage_key()
	}

	fn get_receive_auth_key(&self) -> lightning::sign::ReceiveAuthKey {
		self.inner.get_receive_auth_key()
	}

	fn sign_invoice(
		&self, invoice: &RawBolt11Invoice, recipient: Recipient,
	) -> Result<RecoverableSignature, ()> {
		self.inner.sign_invoice(invoice, recipient)
	}

	fn sign_gossip_message(&self, msg: UnsignedGossipMessage<'_>) -> Result<Signature, ()> {
		self.inner.sign_gossip_message(msg)
	}

	fn sign_bolt12_invoice(
		&self, invoice: &lightning::offers::invoice::UnsignedBolt12Invoice,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice(invoice)
	}
	fn sign_message(&self, msg: &[u8]) -> Result<String, ()> {
		self.inner.sign_message(msg)
	}
}

impl OutputSpender for WalletKeysManager {
	/// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
	fn spend_spendable_outputs(
		&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>,
		change_destination_script: ScriptBuf, feerate_sat_per_1000_weight: u32,
		locktime: Option<LockTime>, secp_ctx: &Secp256k1<All>,
	) -> Result<Transaction, ()> {
		self.inner.spend_spendable_outputs(
			descriptors,
			outputs,
			change_destination_script,
			feerate_sat_per_1000_weight,
			locktime,
			secp_ctx,
		)
	}
}

impl EntropySource for WalletKeysManager {
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl SignerProvider for WalletKeysManager {
	type EcdsaSigner = InMemorySigner;

	fn generate_channel_keys_id(&self, inbound: bool, user_channel_id: u128) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, user_channel_id)
	}

	fn derive_channel_signer(&self, channel_keys_id: [u8; 32]) -> Self::EcdsaSigner {
		self.inner.derive_channel_signer(channel_keys_id)
	}

	fn get_destination_script(&self, _channel_keys_id: [u8; 32]) -> Result<ScriptBuf, ()> {
		// LDK may invoke this callback on a runtime worker thread while holding channel locks.
		// It must not block on the runtime, or the runtime can deadlock.
		let address = self.wallet.pop_pooled_address().ok_or_else(|| {
			log_error!(self.logger, "Failed to retrieve a destination script: address pool empty");
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		// LDK may invoke this callback on a runtime worker thread while holding channel locks.
		// It must not block on the runtime, or the runtime can deadlock.
		let address = self.wallet.pop_pooled_address().ok_or_else(|| {
			log_error!(self.logger, "Failed to retrieve a shutdown script: address pool empty");
		})?;

		match address.witness_program() {
			Some(program) => ShutdownScript::new_witness_program(&program).map_err(|e| {
				log_error!(self.logger, "Invalid shutdown script: {:?}", e);
			}),
			_ => {
				log_error!(
					self.logger,
					"Tried to use a non-witness address. This must never happen."
				);
				panic!("Tried to use a non-witness address. This must never happen.");
			},
		}
	}
}

impl ChangeDestinationSource for WalletKeysManager {
	fn get_change_destination_script<'a>(
		&'a self,
	) -> impl Future<Output = Result<ScriptBuf, ()>> + Send + 'a {
		async move {
			self.wallet
				.get_new_internal_address()
				.await
				.map_err(|e| {
					log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
				})
				.map(|addr| addr.script_pubkey())
				.map_err(|_| ())
		}
	}
}

/// Convert LDK's `Input::satisfaction_weight` to the value BDK's
/// [`bdk_wallet::TxBuilder::add_foreign_utxo`] expects.
///
/// LDK and BDK disagree on what `satisfaction_weight` includes for a SegWit input. LDK
/// treats it as the full weight of the spent input's `script_sig` and `witness` *each
/// with their lengths included* — i.e., the empty `script_sig` length byte (4 WU) and
/// the witness-elements-count varint (1 WU) are part of the value. BDK adds
/// `TxIn::default().segwit_weight()` internally, which already accounts for those same
/// 5 WU (an empty TxIn has a 1-byte empty `script_sig` length and a 1-byte empty
/// witness-count varint). Passing LDK's value directly to BDK therefore double-counts
/// 5 WU per foreign input, which inflates BDK's fee estimate and ultimately funnels the
/// surplus into the new funding output during splice negotiation.
fn ldk_to_bdk_satisfaction_weight(ldk_satisfaction_weight: u64) -> Weight {
	const EMPTY_SCRIPT_SIG_WEIGHT: u64 =
		1 /* empty script_sig length byte */ * WITNESS_SCALE_FACTOR as u64;
	const EMPTY_WITNESS_COUNT_WEIGHT: u64 = 1 /* witness elements count varint */;
	Weight::from_wu(
		ldk_satisfaction_weight
			.saturating_sub(EMPTY_SCRIPT_SIG_WEIGHT + EMPTY_WITNESS_COUNT_WEIGHT),
	)
}

/// Builds the payment-store update for a freshly classified funding payment. `details` describes
/// the actively broadcast candidate, but when the record already confirmed a *different*
/// candidate — wallet sync saw it win before this classification ran — the update instead carries
/// the confirmed candidate's txid and figures from the candidate history, mirroring what
/// [`Wallet::apply_funding_status_update_locked`] reports when confirmation arrives after
/// classification.
///
/// `current` is the record as observed inside the payment store's `mutate` critical section — its
/// sole caller, [`Wallet::persist_funding_payment_locked`], builds and applies the update within
/// one closure — so the candidate choice cannot go stale against a concurrent confirmation before
/// the update lands. [`PaymentDetails::update`]'s confirmed-figures rule still arbitrates which
/// figures may land on the record.
fn funding_reclassification_update(
	details: PaymentDetails, candidates: &[FundingTxCandidate], current: Option<&PaymentDetails>,
) -> PaymentDetailsUpdate {
	// A funding-typed classification of a record already classified as interactive funding is a
	// downgrade, not news: LDK re-broadcasts a promoted-but-unconfirmed splice through its
	// generic funding path, where the figures are wallet-view rather than contribution-derived.
	// Keep the record as classified; wallet-sync events own its confirmation state.
	//
	// TODO(https://git.rust-bitcoin.org/lightningdevkit/rust-lightning/issues/4878): The
	// re-typed re-broadcasts are upstream behavior that should be fixed in `rust-lightning`:
	// the re-offer ought to keep its `InteractiveFunding` classification, or not recur at all.
	// `zero_conf_splice_in_funding_rebroadcast_canary` pins the current behavior via the
	// arrival log in `classify_funding`; when it fails against a newer LDK, re-evaluate
	// whether this guard still sees traffic.
	if let (
		Some(PaymentKind::Onchain {
			tx_type: Some(TransactionType::InteractiveFunding { .. }),
			..
		}),
		PaymentKind::Onchain { tx_type: Some(TransactionType::Funding { .. }), .. },
	) = (current.map(|payment| &payment.kind), &details.kind)
	{
		return PaymentDetailsUpdate::new(details.id);
	}

	let mut update = PaymentDetailsUpdate::funding_reclassification(details);
	if let Some(PaymentKind::Onchain {
		txid: confirmed_txid,
		status: ConfirmationStatus::Confirmed { .. },
		..
	}) = current.map(|payment| &payment.kind)
	{
		if update.txid != Some(*confirmed_txid) {
			if let Some(candidate) = candidates.iter().find(|c| c.txid == *confirmed_txid) {
				update.txid = Some(candidate.txid);
				update.amount_msat = Some(candidate.amount_msat);
				update.fee_paid_msat = Some(candidate.fee_paid_msat);
			}
		}
	}
	update
}

#[cfg(all(test, any(feature = "chain-esplora", feature = "chain-electrum")))]
mod tests {
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
	use std::time::Duration;

	use bdk_chain::{BlockId, ConfirmationBlockTime};
	use bdk_wallet::Wallet as BdkWallet;
	use bitcoin::hashes::Hash;
	use bitcoin::Network;
	use lightning::io;
	use lightning::ln::funding::FundingContribution;
	use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};

	use super::*;
	#[cfg(all(not(feature = "chain-esplora"), feature = "chain-electrum"))]
	use crate::config::ElectrumSyncConfig;
	#[cfg(feature = "chain-esplora")]
	use crate::config::EsploraSyncConfig;
	use crate::config::PAYMENT_CACHE_CAPACITY;
	use crate::io::test_utils::InMemoryStore;
	use crate::io::{
		BDK_WALLET_ADDRESS_POOL_KEY, BDK_WALLET_ADDRESS_POOL_PRIMARY_NAMESPACE,
		BDK_WALLET_ADDRESS_POOL_SECONDARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use crate::payment::pending_payment_store::{
		test_funding_contribution_with_outputs, test_funding_contribution_with_parts,
	};
	use crate::types::{DynStore, DynStoreWrapper};
	use crate::{NodeMetrics, PersistedNodeMetrics};

	const EXTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/0/*)";
	const INTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/1/*)";

	/// An in-memory store whose writes can be made to fail on demand, counting the failures so
	/// tests can wait for a write to have actually failed rather than guessing with a sleep.
	#[derive(Clone)]
	struct FailSwitchStore {
		inner: Arc<InMemoryStore>,
		fail_writes: Arc<AtomicBool>,
		failed_writes: Arc<AtomicUsize>,
		/// When set, only writes to this primary namespace fail while `fail_writes` is on.
		failing_namespace: Option<String>,
	}

	impl FailSwitchStore {
		fn new() -> Self {
			Self {
				inner: Arc::new(InMemoryStore::new()),
				fail_writes: Arc::new(AtomicBool::new(false)),
				failed_writes: Arc::new(AtomicUsize::new(0)),
				failing_namespace: None,
			}
		}

		/// Like [`Self::new`], but only writes to `primary_namespace` fail.
		fn failing_only(primary_namespace: &str) -> Self {
			Self { failing_namespace: Some(primary_namespace.to_string()), ..Self::new() }
		}
	}

	impl KVStore for FailSwitchStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let inner = Arc::clone(&self.inner);
			let fail_writes = Arc::clone(&self.fail_writes);
			let failed_writes = Arc::clone(&self.failed_writes);
			let may_fail =
				self.failing_namespace.as_deref().map_or(true, |ns| ns == primary_namespace);
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				if may_fail && fail_writes.load(Ordering::Acquire) {
					failed_writes.fetch_add(1, Ordering::AcqRel);
					return Err(io::Error::new(io::ErrorKind::Other, "writes disabled"));
				}
				KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for FailSwitchStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			PaginatedKVStore::list_paginated(
				&*self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	/// Constructs a `Wallet` around the given store, either creating a fresh BDK wallet or
	/// loading the one the store already holds.
	async fn new_test_wallet(store: Arc<DynStore>, load_existing: bool) -> Arc<Wallet> {
		let logger = Arc::new(Logger::new_log_facade());
		let mut config = Config::default();
		config.network = Network::Regtest;
		let config = Arc::new(config);

		let mut wallet_persister =
			KVStoreWalletPersister::new(Arc::clone(&store), Arc::clone(&logger));
		let bdk_wallet = if load_existing {
			BdkWallet::load()
				.descriptor(KeychainKind::External, Some(EXTERNAL_DESCRIPTOR))
				.descriptor(KeychainKind::Internal, Some(INTERNAL_DESCRIPTOR))
				.extract_keys()
				.check_network(Network::Regtest)
				.load_wallet_async(&mut wallet_persister)
				.await
				.unwrap()
				.unwrap()
		} else {
			BdkWallet::create(EXTERNAL_DESCRIPTOR, INTERNAL_DESCRIPTOR)
				.network(Network::Regtest)
				.create_wallet_async(&mut wallet_persister)
				.await
				.unwrap()
		};

		let fee_estimator = Arc::new(OnchainFeeEstimator::new());
		let broadcaster = Arc::new(Broadcaster::new(Arc::clone(&logger)));
		let node_metrics = Arc::new(PersistedNodeMetrics::new(NodeMetrics::default()));
		#[cfg(feature = "chain-esplora")]
		let (chain_source, _) = ChainSource::new_esplora(
			"http://localhost:1".to_string(),
			HashMap::new(),
			EsploraSyncConfig::default(),
			Arc::clone(&fee_estimator),
			Arc::clone(&broadcaster),
			Arc::clone(&store),
			Arc::clone(&config),
			Arc::clone(&logger),
			node_metrics,
		)
		.unwrap();
		#[cfg(all(not(feature = "chain-esplora"), feature = "chain-electrum"))]
		let (chain_source, _) = ChainSource::new_electrum(
			"tcp://localhost:1".to_string(),
			ElectrumSyncConfig::default(),
			Arc::clone(&fee_estimator),
			Arc::clone(&broadcaster),
			Arc::clone(&store),
			Arc::clone(&config),
			Arc::clone(&logger),
			node_metrics,
		);
		let payment_store = Arc::new(PaymentStore::new(
			Vec::new(),
			KeepLeastRecentlyUsed::new(PAYMENT_CACHE_CAPACITY),
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
			Arc::clone(&store),
			Arc::clone(&logger),
		));
		let pending_payment_store = Arc::new(PendingPaymentStore::new(
			Vec::new(),
			KeepAllEntries,
			PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
			PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
			Arc::clone(&store),
			Arc::clone(&logger),
		));
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());

		let persisted_pool_indices = persist::read_address_pool(&*store, &*logger).await.unwrap();

		Arc::new(Wallet::new(
			bdk_wallet,
			wallet_persister,
			persisted_pool_indices,
			broadcaster,
			fee_estimator,
			Arc::new(chain_source),
			payment_store,
			runtime,
			config,
			logger,
			pending_payment_store,
		))
	}

	fn pooled_indices(wallet: &Wallet) -> Vec<u32> {
		wallet.address_pool.lock().unwrap().available.iter().map(|(index, _)| *index).collect()
	}

	#[tokio::test]
	async fn refill_publishes_addresses_only_after_their_reveal_is_persisted() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;

		wallet.refill_address_pool().await.unwrap();
		assert_eq!(pooled_indices(&wallet).len(), ADDRESS_POOL_TARGET_SIZE);

		// Simulate a handout, then make wallet writes fail: the refill must not publish the
		// address it revealed, as a crash would leave its script unwatched by incremental syncs.
		wallet.address_pool.lock().unwrap().available.pop_front().unwrap();
		fail_store.fail_writes.store(true, Ordering::Release);
		assert!(wallet.refill_address_pool().await.is_err());
		let unpersisted_index = ADDRESS_POOL_TARGET_SIZE as u32;
		let indices = pooled_indices(&wallet);
		assert_eq!(indices.len(), ADDRESS_POOL_TARGET_SIZE - 1);
		assert!(!indices.contains(&unpersisted_index));

		// Once persistence recovers, the next refill publishes the retained reveal without
		// burning another derivation index.
		fail_store.fail_writes.store(false, Ordering::Release);
		wallet.refill_address_pool().await.unwrap();
		let indices = pooled_indices(&wallet);
		assert_eq!(indices.len(), ADDRESS_POOL_TARGET_SIZE);
		assert!(indices.contains(&unpersisted_index));
		let last_revealed = wallet.inner.lock().unwrap().derivation_index(KeychainKind::External);
		assert_eq!(last_revealed, Some(unpersisted_index));
	}

	#[tokio::test]
	async fn pool_reloads_across_restarts_without_burning_indices() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));

		let (popped_address, indices_before) = {
			let wallet = new_test_wallet(Arc::clone(&store), false).await;
			wallet.refill_address_pool().await.unwrap();
			// Simulate a handout and a completed refill before the restart.
			let (_, popped_address) =
				wallet.address_pool.lock().unwrap().available.pop_front().unwrap();
			wallet.refill_address_pool().await.unwrap();
			(popped_address, pooled_indices(&wallet))
		};

		let wallet = new_test_wallet(Arc::clone(&store), true).await;
		wallet.refill_address_pool().await.unwrap();

		// The pool is rebuilt from the persisted record: the restart neither reveals fresh
		// indices (widening what incremental syncs must watch) nor re-hands-out the address
		// popped before the restart.
		assert_eq!(pooled_indices(&wallet), indices_before);
		let last_revealed = wallet.inner.lock().unwrap().derivation_index(KeychainKind::External);
		assert_eq!(last_revealed, Some(ADDRESS_POOL_TARGET_SIZE as u32));
		let pool = wallet.address_pool.lock().unwrap();
		assert!(!pool.available.iter().any(|(_, address)| *address == popped_address));
	}

	#[tokio::test]
	async fn loading_drops_pool_indices_the_wallet_never_revealed() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		{
			let wallet = new_test_wallet(Arc::clone(&store), false).await;
			wallet.refill_address_pool().await.unwrap();
		}

		// Corrupt the persisted record with an index the wallet never revealed.
		let logger = Arc::new(Logger::new_log_facade());
		let mut persister = KVStoreWalletPersister::new(Arc::clone(&store), logger);
		persister.persist_address_pool(vec![5, 100]).await.unwrap();

		let wallet = new_test_wallet(Arc::clone(&store), true).await;
		wallet.refill_address_pool().await.unwrap();

		// Index 5 was revealed before the restart and is kept; the never-revealed index 100
		// must be dropped, as no sync path would watch its script. The initial refill then
		// tops the pool back up with fresh reveals.
		let indices = pooled_indices(&wallet);
		assert_eq!(indices.len(), ADDRESS_POOL_TARGET_SIZE);
		assert!(indices.contains(&5));
		assert!(!indices.contains(&100));
	}

	#[tokio::test]
	async fn signer_provider_callbacks_fail_closed_when_pool_is_empty() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let logger = Arc::new(Logger::new_log_facade());
		let keys_manager = WalletKeysManager::new(&[7u8; 32], 42, 42, Arc::clone(&wallet), logger);

		// Before the pool is filled it is empty: the sync callbacks must fail closed rather
		// than hand out an address whose reveal was never persisted.
		assert!(keys_manager.get_destination_script([0u8; 32]).is_err());
		assert!(keys_manager.get_shutdown_scriptpubkey().is_err());

		wallet.refill_address_pool().await.unwrap();
		assert!(keys_manager.get_destination_script([0u8; 32]).is_ok());
		assert!(keys_manager.get_shutdown_scriptpubkey().is_ok());
	}

	/// An in-memory store that snapshots its full contents after every completed write, letting
	/// tests reload the wallet from any crash point.
	#[derive(Clone)]
	struct SnapshotStore {
		data: Arc<Mutex<HashMap<(String, String, String), Vec<u8>>>>,
		snapshots: Arc<Mutex<Vec<HashMap<(String, String, String), Vec<u8>>>>>,
	}

	impl SnapshotStore {
		fn new() -> Self {
			Self {
				data: Arc::new(Mutex::new(HashMap::new())),
				snapshots: Arc::new(Mutex::new(Vec::new())),
			}
		}

		fn from_contents(data: HashMap<(String, String, String), Vec<u8>>) -> Self {
			Self { data: Arc::new(Mutex::new(data)), snapshots: Arc::new(Mutex::new(Vec::new())) }
		}
	}

	impl KVStore for SnapshotStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			let res = self
				.data
				.lock()
				.unwrap()
				.get(&(
					primary_namespace.to_string(),
					secondary_namespace.to_string(),
					key.to_string(),
				))
				.cloned()
				.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"));
			async move { res }
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let mut data = self.data.lock().unwrap();
			data.insert(
				(primary_namespace.to_string(), secondary_namespace.to_string(), key.to_string()),
				buf,
			);
			self.snapshots.lock().unwrap().push(data.clone());
			async move { Ok(()) }
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let mut data = self.data.lock().unwrap();
			data.remove(&(
				primary_namespace.to_string(),
				secondary_namespace.to_string(),
				key.to_string(),
			));
			self.snapshots.lock().unwrap().push(data.clone());
			async move { Ok(()) }
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			let keys = self
				.data
				.lock()
				.unwrap()
				.keys()
				.filter(|(primary, secondary, _)| {
					primary == primary_namespace && secondary == secondary_namespace
				})
				.map(|(_, _, key)| key.clone())
				.collect::<Vec<_>>();
			async move { Ok(keys) }
		}
	}

	impl PaginatedKVStore for SnapshotStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			_page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			let keys = self
				.data
				.lock()
				.unwrap()
				.keys()
				.filter(|(primary, secondary, _)| {
					primary == primary_namespace && secondary == secondary_namespace
				})
				.map(|(_, _, key)| key.clone())
				.collect::<Vec<_>>();
			async move { Ok(PaginatedListResponse { keys, next_page_token: None }) }
		}
	}

	#[tokio::test]
	async fn pool_survives_a_crash_at_any_point_during_refill() {
		let snapshot_store = SnapshotStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(snapshot_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		// Only replay crash points from wallet creation onwards; earlier snapshots hold a
		// half-created wallet, which is the builder's concern rather than the pool's.
		let baseline = snapshot_store.snapshots.lock().unwrap().len();

		wallet.refill_address_pool().await.unwrap();
		// Simulate a handout plus the refill it schedules.
		wallet.address_pool.lock().unwrap().available.pop_front().unwrap();
		wallet.refill_address_pool().await.unwrap();
		let final_derivation =
			wallet.inner.lock().unwrap().derivation_index(KeychainKind::External).unwrap();

		// Reload the wallet from every intermediate store state. No crash point may leave the
		// pool unfillable or burn indices: a reload revealing past `final_derivation` means some
		// reveal was durable while absent from the pool record, stranding its index as
		// revealed-but-unused forever.
		let snapshots = snapshot_store.snapshots.lock().unwrap().clone();
		assert!(snapshots.len() > baseline);
		for snapshot in snapshots.into_iter().skip(baseline) {
			let store: Arc<DynStore> =
				Arc::new(DynStoreWrapper(SnapshotStore::from_contents(snapshot)));
			let wallet = new_test_wallet(Arc::clone(&store), true).await;
			wallet.refill_address_pool().await.unwrap();
			assert_eq!(pooled_indices(&wallet).len(), ADDRESS_POOL_TARGET_SIZE);
			let derivation =
				wallet.inner.lock().unwrap().derivation_index(KeychainKind::External).unwrap();
			assert!(derivation <= final_derivation);
		}
	}

	/// An in-memory store whose writes can be made to park until aborted or released,
	/// signalling when a write has entered the gate, and whose writes can be made to fail.
	#[derive(Clone)]
	struct GatedStore {
		inner: Arc<InMemoryStore>,
		gate_writes: Arc<AtomicBool>,
		fail_writes: Arc<AtomicBool>,
		write_entered: Arc<tokio::sync::Notify>,
		release: Arc<tokio::sync::Notify>,
	}

	impl GatedStore {
		fn new() -> Self {
			Self {
				inner: Arc::new(InMemoryStore::new()),
				gate_writes: Arc::new(AtomicBool::new(false)),
				fail_writes: Arc::new(AtomicBool::new(false)),
				write_entered: Arc::new(tokio::sync::Notify::new()),
				release: Arc::new(tokio::sync::Notify::new()),
			}
		}
	}

	impl KVStore for GatedStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let inner = Arc::clone(&self.inner);
			let gate_writes = Arc::clone(&self.gate_writes);
			let fail_writes = Arc::clone(&self.fail_writes);
			let write_entered = Arc::clone(&self.write_entered);
			let release = Arc::clone(&self.release);
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				if gate_writes.load(Ordering::Acquire) {
					write_entered.notify_one();
					release.notified().await;
				}
				if fail_writes.load(Ordering::Acquire) {
					return Err(io::Error::new(io::ErrorKind::Other, "write failed"));
				}
				KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for GatedStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			PaginatedKVStore::list_paginated(
				&*self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	#[tokio::test]
	async fn aborting_a_refill_mid_persist_loses_no_reveals() {
		let gated_store = GatedStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(gated_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.refill_address_pool().await.unwrap();

		// Simulate two handouts, then a refill that is aborted (as node shutdown aborts
		// cancellable tasks) while parked on its first store write.
		wallet.address_pool.lock().unwrap().available.pop_front().unwrap();
		wallet.address_pool.lock().unwrap().available.pop_front().unwrap();
		gated_store.gate_writes.store(true, Ordering::Release);
		let refill_wallet = Arc::clone(&wallet);
		let refill_task = tokio::spawn(async move {
			let _ = refill_wallet.refill_address_pool().await;
		});
		gated_store.write_entered.notified().await;
		refill_task.abort();
		assert!(refill_task.await.unwrap_err().is_cancelled());
		gated_store.gate_writes.store(false, Ordering::Release);

		// The aborted refill had already revealed replacements and taken them out of the
		// wallet's staged change set. Those reveals must survive the abort: everything a later
		// refill publishes has to be covered by persisted wallet state, or a crash would leave
		// handed-out scripts unwatched by incremental syncs.
		wallet.refill_address_pool().await.unwrap();
		let indices = pooled_indices(&wallet);
		assert_eq!(indices.len(), ADDRESS_POOL_TARGET_SIZE);
		let max_pooled = *indices.iter().max().unwrap();

		let reloaded = new_test_wallet(Arc::clone(&store), true).await;
		let persisted_last_revealed =
			reloaded.inner.lock().unwrap().derivation_index(KeychainKind::External).unwrap();
		assert!(
			persisted_last_revealed >= max_pooled,
			"pooled index {} exceeds the persisted last revealed index {}",
			max_pooled,
			persisted_last_revealed
		);
	}

	#[tokio::test]
	async fn get_new_address_pops_the_oldest_pooled_address_and_persists_the_dequeue() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.refill_address_pool().await.unwrap();

		let (front_index, front_address) =
			wallet.address_pool.lock().unwrap().available.front().cloned().unwrap();
		assert_eq!(front_index, 0);

		// The handout comes from the pool front (the oldest revealed index) rather than minting
		// a fresh index past the pool's unused tail, keeping the window of revealed-but-unused
		// scripts compact for a from-seed restore's full scan.
		let address = wallet.get_new_address().await.unwrap();
		assert_eq!(address, front_address);
		let indices = pooled_indices(&wallet);
		assert_eq!(indices.len(), ADDRESS_POOL_TARGET_SIZE);
		assert!(!indices.contains(&front_index));

		// The dequeue must be durable before the address is returned: a wallet reloaded from
		// the store may not pool (and later re-hand-out) the returned address.
		let reloaded = new_test_wallet(Arc::clone(&store), true).await;
		reloaded.refill_address_pool().await.unwrap();
		let reloaded_indices = pooled_indices(&reloaded);
		assert!(!reloaded_indices.contains(&front_index));
		assert_eq!(reloaded_indices, pooled_indices(&wallet));
	}

	#[tokio::test]
	async fn get_new_address_fails_closed_and_returns_the_address_to_the_pool() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.refill_address_pool().await.unwrap();

		let (front_index, front_address) =
			wallet.address_pool.lock().unwrap().available.front().cloned().unwrap();

		// While persistence is unavailable no address is handed out, and the popped address
		// returns to the pool front: its index is neither skipped nor left unreachable.
		fail_store.fail_writes.store(true, Ordering::Release);
		assert!(wallet.get_new_address().await.is_err());
		let (index, address) =
			wallet.address_pool.lock().unwrap().available.front().cloned().unwrap();
		assert_eq!(index, front_index);
		assert_eq!(address, front_address);
		assert_eq!(pooled_indices(&wallet).len(), ADDRESS_POOL_TARGET_SIZE);

		// Once persistence recovers, the very address the failed call popped is handed out.
		fail_store.fail_writes.store(false, Ordering::Release);
		assert_eq!(wallet.get_new_address().await.unwrap(), front_address);
	}

	#[tokio::test]
	async fn get_new_address_refills_an_empty_pool_before_handing_out() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;

		// With the pool empty and persistence down, the call must fail closed rather than hand
		// out an address whose reveal isn't durable.
		fail_store.fail_writes.store(true, Ordering::Release);
		assert!(wallet.get_new_address().await.is_err());

		// With persistence available it fills the pool inline and serves from it.
		fail_store.fail_writes.store(false, Ordering::Release);
		let address = wallet.get_new_address().await.unwrap();
		let expected = wallet.inner.lock().unwrap().peek_address(KeychainKind::External, 0).address;
		assert_eq!(address, expected);
		assert_eq!(pooled_indices(&wallet).len(), ADDRESS_POOL_TARGET_SIZE);
	}

	#[tokio::test]
	async fn get_new_address_never_reuses_across_restarts_after_an_overfull_pool() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.refill_address_pool().await.unwrap();

		// A failed handout returns the popped address to the pool while the refill retains its
		// unpublished reveal; the next successful refill then records and publishes all
		// seventeen indices, filling the pool past its target size.
		fail_store.fail_writes.store(true, Ordering::Release);
		assert!(wallet.get_new_address().await.is_err());
		fail_store.fail_writes.store(false, Ordering::Release);
		wallet.refill_address_pool().await.unwrap();
		assert!(pooled_indices(&wallet).len() > ADDRESS_POOL_TARGET_SIZE);

		// Handing out from the overfull pool must still durably exclude the returned address
		// from the pool record before returning: a wallet reloaded from the store may never
		// hand it out again.
		let address = wallet.get_new_address().await.unwrap();

		let reloaded = new_test_wallet(Arc::clone(&store), true).await;
		reloaded.refill_address_pool().await.unwrap();
		let reloaded_pool = reloaded.address_pool.lock().unwrap();
		assert!(!reloaded_pool.available.iter().any(|(_, pooled)| *pooled == address));
	}

	/// An in-memory store that can fail all writes except the address-pool record's.
	#[derive(Clone)]
	struct RecordOnlyStore {
		inner: Arc<InMemoryStore>,
		fail_non_record_writes: Arc<AtomicBool>,
	}

	impl RecordOnlyStore {
		fn new() -> Self {
			Self {
				inner: Arc::new(InMemoryStore::new()),
				fail_non_record_writes: Arc::new(AtomicBool::new(false)),
			}
		}
	}

	impl KVStore for RecordOnlyStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let inner = Arc::clone(&self.inner);
			let fail_non_record_writes = Arc::clone(&self.fail_non_record_writes);
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				if fail_non_record_writes.load(Ordering::Acquire)
					&& key != BDK_WALLET_ADDRESS_POOL_KEY
				{
					return Err(io::Error::new(io::ErrorKind::Other, "writes disabled"));
				}
				KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for RecordOnlyStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			PaginatedKVStore::list_paginated(
				&*self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	#[tokio::test]
	async fn failed_get_new_address_leaves_the_pool_record_covering_the_pool() {
		let record_store = RecordOnlyStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(record_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.refill_address_pool().await.unwrap();
		let (front_index, _) =
			wallet.address_pool.lock().unwrap().available.front().cloned().unwrap();

		// Fail everything but the pool record: the handout's record write succeeds (durably
		// excluding the popped index) while the reveal flush fails, so the call fails and the
		// address goes back into the pool. Its index must not be stranded by that partial
		// failure: a crash right here reloads the pool from the record, and a durably revealed
		// index missing from it would never be pooled or handed out again.
		record_store.fail_non_record_writes.store(true, Ordering::Release);
		assert!(wallet.get_new_address().await.is_err());
		record_store.fail_non_record_writes.store(false, Ordering::Release);

		let reloaded = new_test_wallet(Arc::clone(&store), true).await;
		reloaded.refill_address_pool().await.unwrap();
		assert!(pooled_indices(&reloaded).contains(&front_index));
	}

	#[tokio::test]
	async fn loading_survives_an_undecodable_pool_record() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		{
			let wallet = new_test_wallet(Arc::clone(&store), false).await;
			wallet.refill_address_pool().await.unwrap();
		}

		// Corrupt the record itself: the pool is a reconstructible cache, so an undecodable
		// record must not prevent the node from starting.
		KVStore::write(
			&*store,
			BDK_WALLET_ADDRESS_POOL_PRIMARY_NAMESPACE,
			BDK_WALLET_ADDRESS_POOL_SECONDARY_NAMESPACE,
			BDK_WALLET_ADDRESS_POOL_KEY,
			vec![0x00, 0xff],
		)
		.await
		.unwrap();

		let wallet = new_test_wallet(Arc::clone(&store), true).await;
		wallet.refill_address_pool().await.unwrap();
		assert_eq!(pooled_indices(&wallet).len(), ADDRESS_POOL_TARGET_SIZE);
	}

	/// An in-memory store whose pool-record writes can be made to fail while wallet-changeset
	/// writes succeed.
	#[derive(Clone)]
	struct RecordFailStore {
		inner: Arc<InMemoryStore>,
		fail_record_writes: Arc<AtomicBool>,
	}

	impl RecordFailStore {
		fn new() -> Self {
			Self {
				inner: Arc::new(InMemoryStore::new()),
				fail_record_writes: Arc::new(AtomicBool::new(false)),
			}
		}
	}

	impl KVStore for RecordFailStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let inner = Arc::clone(&self.inner);
			let fail_record_writes = Arc::clone(&self.fail_record_writes);
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				if fail_record_writes.load(Ordering::Acquire) && key == BDK_WALLET_ADDRESS_POOL_KEY
				{
					return Err(io::Error::new(io::ErrorKind::Other, "writes disabled"));
				}
				KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for RecordFailStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			PaginatedKVStore::list_paginated(
				&*self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	#[tokio::test]
	async fn crash_after_a_failed_record_write_re_derives_the_same_indices() {
		let record_store = RecordFailStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(record_store.clone()));
		{
			let wallet = new_test_wallet(Arc::clone(&store), false).await;

			// Fail only the record write: the fill's reveals must not become durable without
			// record coverage, as a crash would then leave indices that no path ever pools or
			// hands out again — permanently skipping them in the keychain.
			record_store.fail_record_writes.store(true, Ordering::Release);
			assert!(wallet.refill_address_pool().await.is_err());
		}

		record_store.fail_record_writes.store(false, Ordering::Release);
		let reloaded = new_test_wallet(Arc::clone(&store), true).await;
		reloaded.refill_address_pool().await.unwrap();
		let indices = pooled_indices(&reloaded);
		assert_eq!(indices.len(), ADDRESS_POOL_TARGET_SIZE);
		assert!(
			indices.contains(&0),
			"the failed fill's indices must be re-derived, not skipped: {:?}",
			indices
		);
	}

	#[tokio::test]
	async fn oldest_address_still_leads_the_pool_after_concurrent_failed_handouts() {
		let gated_store = GatedStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(gated_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.refill_address_pool().await.unwrap();
		let (_, oldest_address) =
			wallet.address_pool.lock().unwrap().available.front().cloned().unwrap();

		// First handout pops index 0 and parks inside its refill's record write, holding the
		// refill lock.
		gated_store.gate_writes.store(true, Ordering::Release);
		gated_store.fail_writes.store(true, Ordering::Release);
		let first_wallet = Arc::clone(&wallet);
		let first_handout = tokio::spawn(async move { first_wallet.get_new_address().await });
		gated_store.write_entered.notified().await;

		// Second handout pops index 1 while the first is parked, then queues on the refill lock.
		let second_wallet = Arc::clone(&wallet);
		let second_handout = tokio::spawn(async move { second_wallet.get_new_address().await });
		while wallet.address_pool.lock().unwrap().available.len() > ADDRESS_POOL_TARGET_SIZE - 2 {
			tokio::task::yield_now().await;
		}

		// Both handouts now fail and return their indices to the pool, completing out of pop
		// order: index 0 first, index 1 second.
		gated_store.gate_writes.store(false, Ordering::Release);
		gated_store.release.notify_one();
		assert!(first_handout.await.unwrap().is_err());
		assert!(second_handout.await.unwrap().is_err());
		gated_store.fail_writes.store(false, Ordering::Release);

		// The pushed-back indices must not swap the pool out of index order: the next handout
		// has to serve the oldest revealed index, or a lower unused index would be left sitting
		// behind a handed-out (potentially funded) one, where a from-seed restore's stop gap
		// could strand it.
		let handed_out = wallet.get_new_address().await.unwrap();
		assert_eq!(
			handed_out,
			oldest_address,
			"the oldest pooled address must be handed out first, pool: {:?}",
			pooled_indices(&wallet)
		);
	}

	/// A pass-through [`KVStore`] that parks writes to one namespace: a matching writer first
	/// signals `parked`, then waits until the test drops its `gate` write guard. Writes to every
	/// other namespace pass straight through.
	#[derive(Clone)]
	struct NamespaceGatedStore {
		inner: Arc<InMemoryStore>,
		gated_namespace: String,
		parked: Arc<tokio::sync::Notify>,
		gate: Arc<tokio::sync::RwLock<()>>,
	}

	impl NamespaceGatedStore {
		fn new(gated_namespace: &str) -> Self {
			Self {
				inner: Arc::new(InMemoryStore::new()),
				gated_namespace: gated_namespace.to_string(),
				parked: Arc::new(tokio::sync::Notify::new()),
				gate: Arc::new(tokio::sync::RwLock::new(())),
			}
		}
	}

	impl KVStore for NamespaceGatedStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let inner = Arc::clone(&self.inner);
			let gated = primary_namespace == self.gated_namespace;
			let parked = Arc::clone(&self.parked);
			let gate = Arc::clone(&self.gate);
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				if gated {
					parked.notify_one();
					let _guard = gate.read().await;
				}
				KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for NamespaceGatedStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			PaginatedKVStore::list_paginated(
				&*self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	fn dummy_tx() -> Transaction {
		Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: Vec::new(),
		}
	}

	fn confirmed_block_time(height: u32) -> ConfirmationBlockTime {
		ConfirmationBlockTime {
			block_id: BlockId { height, hash: bitcoin::BlockHash::from_byte_array([9u8; 32]) },
			confirmation_time: 100,
		}
	}

	fn interactive_funding_details(
		id: PaymentId, txid: Txid, amount_msat: Option<u64>, fee_paid_msat: Option<u64>,
	) -> PaymentDetails {
		let kind = PaymentKind::Onchain {
			txid,
			status: ConfirmationStatus::Unconfirmed,
			tx_type: Some(TransactionType::InteractiveFunding { channels: vec![] }),
		};
		PaymentDetails::new(
			id,
			kind,
			amount_msat,
			fee_paid_msat,
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	fn onchain_details(txid: Txid, status: ConfirmationStatus) -> PaymentDetails {
		PaymentDetails::new(
			PaymentId([42u8; 32]),
			PaymentKind::Onchain { txid, status, tx_type: None },
			Some(1_000_000),
			Some(500),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	fn confirmed_status() -> ConfirmationStatus {
		ConfirmationStatus::Confirmed {
			block_hash: bitcoin::BlockHash::from_byte_array([8u8; 32]),
			height: 100,
			timestamp: 1,
		}
	}

	/// Inserts `tx` into the BDK wallet as canonically confirmed at `height`, extending the
	/// local chain to that height.
	fn insert_confirmed_tx(wallet: &Wallet, tx: Transaction, height: u32) {
		let txid = tx.compute_txid();
		let mut locked = wallet.inner.lock().unwrap();
		let block =
			BlockId { height, hash: bitcoin::BlockHash::from_byte_array([height as u8; 32]) };
		let chain = locked.latest_checkpoint().insert(block);
		let mut tx_update = bdk_chain::TxUpdate::default();
		tx_update.txs = vec![Arc::new(tx)];
		tx_update.anchors =
			[(ConfirmationBlockTime { block_id: block, confirmation_time: 100 }, txid)].into();
		locked
			.apply_update(Update { tx_update, chain: Some(chain), ..Default::default() })
			.unwrap();
	}

	/// Inserts `tx` into the BDK wallet as canonically unconfirmed (seen in the mempool).
	fn insert_unconfirmed_tx(wallet: &Wallet, tx: Transaction) {
		let txid = tx.compute_txid();
		let mut locked = wallet.inner.lock().unwrap();
		let mut tx_update = bdk_chain::TxUpdate::default();
		tx_update.txs = vec![Arc::new(tx)];
		tx_update.seen_ats = [(txid, 100)].into();
		locked.apply_update(Update { tx_update, ..Default::default() }).unwrap();
	}

	/// Builds a transaction paying a wallet address, spending an outpoint derived from
	/// `input_byte` (distinct bytes yield non-conflicting transactions).
	fn wallet_paying_tx(wallet: &Wallet, input_byte: u8) -> Transaction {
		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![bitcoin::TxIn {
				previous_output: OutPoint {
					txid: Txid::from_byte_array([input_byte; 32]),
					vout: 0,
				},
				..Default::default()
			}],
			output: vec![TxOut { value: Amount::from_sat(90_000), script_pubkey }],
		}
	}

	/// A counterparty and channel for splice rounds in tests.
	fn test_counterparty_and_channel() -> (PublicKey, ChannelId) {
		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		(counterparty_node_id, ChannelId([7u8; 32]))
	}

	/// Builds one [`FundingCandidate`] per `(txid, contribution)` round of a single channel, in
	/// the given order — the shape LDK hands both the signing-time recording and the broadcaster.
	fn splice_candidates(
		counterparty_node_id: PublicKey, channel_id: ChannelId,
		rounds: &[(Txid, Option<FundingContribution>)],
	) -> Vec<FundingCandidate> {
		use lightning::chain::chaininterface::{ChannelFunding, FundingPurpose};
		rounds
			.iter()
			.map(|(txid, contribution)| FundingCandidate {
				txid: *txid,
				channels: vec![ChannelFunding {
					counterparty_node_id,
					channel_id,
					purpose: FundingPurpose::Splice,
					contribution: contribution.clone(),
				}],
			})
			.collect()
	}

	/// Marks `txid` as evicted from the mempool after it was seen, so the BDK wallet still holds
	/// the transaction but no longer considers it canonical.
	fn evict_tx(wallet: &Wallet, txid: Txid) {
		let mut locked = wallet.inner.lock().unwrap();
		let mut tx_update = bdk_chain::TxUpdate::default();
		tx_update.evicted_ats = [(txid, 101)].into();
		locked.apply_update(Update { tx_update, ..Default::default() }).unwrap();
	}

	/// A splice-out round returning `value_sat` to an external address at an estimated fee of
	/// `fee_sat`, so `value_sat + fee_sat` leaves the channel: the contribution as LDK would
	/// negotiate it, and the transaction carrying it,
	/// which also pays a wallet address so the wallet sees movement (spending an outpoint derived
	/// from `input_byte`).
	fn splice_out_round(
		wallet: &Wallet, input_byte: u8, value_sat: u64, fee_sat: u64,
	) -> (Transaction, FundingContribution) {
		let splice_out =
			TxOut { value: Amount::from_sat(value_sat), script_pubkey: ScriptBuf::new() };
		let contribution =
			test_funding_contribution_with_outputs(fee_sat, 253, std::slice::from_ref(&splice_out));
		let mut tx = wallet_paying_tx(wallet, input_byte);
		tx.output.push(splice_out);
		(tx, contribution)
	}

	/// Signing a splice round records its funding payment with the channel's full pending splice
	/// history, so a wallet sync that observes the transaction before the broadcast (the
	/// counterparty may broadcast first) resolves to the funding record through any round of that
	/// history instead of filing the round as a foreign duplicate. Only the signed round awaits
	/// broadcast; LDK broadcast the negotiated predecessor already.
	#[tokio::test]
	async fn signing_records_the_round_with_the_full_splice_history() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		// The signed round is an RBF of a counterparty-initiated round (`prior_txid`, no
		// contribution of ours), so the history LDK reports has two entries.
		let prior_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(prior_txid, None), (txid, Some(contribution))],
		);

		wallet.record_signed_funding(&tx, &candidates).await.unwrap();

		let payments = wallet.payment_store.list_page(None).await.unwrap().objects;
		assert_eq!(payments.len(), 1);
		let payment = &payments[0];
		let id = payment.id;
		assert_ne!(id, PaymentId(prior_txid.to_byte_array()));
		assert_ne!(id, PaymentId(txid.to_byte_array()));
		assert_eq!(payment.amount_msat, Some(500_300_000));
		assert_eq!(payment.fee_paid_msat, Some(300_000));
		assert_eq!(payment.direction, PaymentDirection::Inbound);
		assert_eq!(payment.status, PaymentStatus::Pending);
		match &payment.kind {
			PaymentKind::Onchain {
				txid: recorded_txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(TransactionType::InteractiveFunding { channels }),
			} => {
				assert_eq!(*recorded_txid, txid);
				assert_eq!(channels.len(), 1);
				assert_eq!(channels[0].counterparty_node_id, counterparty_node_id);
				assert_eq!(channels[0].channel_id, channel_id);
			},
			kind => panic!("unexpected kind {:?}", kind),
		}
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(
			record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(),
			vec![prior_txid, txid]
		);
		let prior = record.candidate(prior_txid).unwrap();
		assert_eq!(prior.amount_msat, None);
		assert!(!prior.awaiting_broadcast);
		let signed = record.candidate(txid).unwrap();
		assert_eq!(signed.amount_msat, Some(500_300_000));
		assert_eq!(signed.fee_paid_msat, Some(300_000));
		assert!(signed.awaiting_broadcast);
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), Some(id));
		assert_eq!(wallet.find_payment_by_txid(prior_txid).await.unwrap(), Some(id));
	}

	/// A fee bump of a round whose payment wallet sync failed — the round lost to a conflicting
	/// spend confirmed while the channel stayed open, so LDK still holds it and offers the bump —
	/// is signed with the failed round among its candidates. The failed record takes no round:
	/// nothing revisits its status, so the bump would go untracked under it. The bump gets a
	/// record of its own.
	#[tokio::test]
	async fn signing_a_bump_of_a_failed_round_gets_a_record_of_its_own() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let failed_id = wallet.find_payment_by_txid(txid).await.unwrap().expect("record");
		// Wallet sync failed the payment and removed its entry.
		wallet
			.payment_store
			.mutate(&failed_id, |existing| {
				let mut update = PaymentDetailsUpdate::new(failed_id);
				update.status = Some(PaymentStatus::Failed);
				let mut updated = existing?.clone();
				updated.update(update).then_some(updated)
			})
			.await
			.unwrap();
		wallet.pending_payment_store.remove(&failed_id).await.unwrap();

		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		wallet.record_signed_funding(&bump_tx, &bump_candidates).await.unwrap();

		let bump_id = wallet.find_payment_by_txid(bump_txid).await.unwrap().expect("a record");
		assert_ne!(bump_id, failed_id);
		let payment = wallet.payment_store.get(&bump_id).await.unwrap().expect("record");
		assert_eq!(payment.status, PaymentStatus::Pending);
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == bump_txid));
		let entry = wallet.pending_payment_store.get(&bump_id).await.unwrap().expect("entry");
		assert_eq!(entry.details(), Some(&payment));
		assert!(entry.candidate(bump_txid).expect("candidate").awaiting_broadcast);
		let failed =
			wallet.payment_store.get(&failed_id).await.unwrap().expect("the failed record stays");
		assert_eq!(failed.status, PaymentStatus::Failed);
		assert!(matches!(failed.kind, PaymentKind::Onchain { txid: t, .. } if t == txid));
		assert!(wallet.pending_payment_store.get(&failed_id).await.unwrap().is_none());
	}

	/// Once LDK reports a round recorded at signing negotiated, there is nothing to add but the
	/// broadcast itself: the round's awaiting-broadcast mark is cleared and the record left as
	/// written.
	#[tokio::test]
	async fn negotiation_of_a_signed_round_marks_it_broadcast() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let prior_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(prior_txid, None), (txid, Some(contribution))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(prior_txid).await.unwrap().expect("id");
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert!(record.candidate(txid).unwrap().awaiting_broadcast);

		wallet.record_broadcast_splice_round(channel_id, txid).await.unwrap();

		assert_eq!(wallet.payment_store.get(&id).await.unwrap(), Some(payment));
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(
			record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(),
			vec![prior_txid, txid]
		);
		assert!(!record.candidate(txid).unwrap().awaiting_broadcast);
		assert_eq!(record.candidate(txid).unwrap().amount_msat, Some(500_300_000));
	}

	/// A splice round this node contributed to is recorded when it is signed, so its broadcast has
	/// nothing left to record: classifying it writes nothing, and the round keeps awaiting the
	/// `SpliceNegotiated` event that marks it broadcast.
	#[tokio::test]
	async fn classifying_an_interactive_funding_broadcast_writes_nothing() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert!(record.candidate(txid).unwrap().awaiting_broadcast);

		fail_store.fail_writes.store(true, Ordering::Release);
		let tx_type = LdkTransactionType::InteractiveFunding { candidates };
		wallet.classify_broadcast(&tx, &tx_type).await.unwrap();
		assert_eq!(
			fail_store.failed_writes.load(Ordering::Acquire),
			0,
			"classifying a recorded round must write nothing"
		);
		assert_eq!(wallet.payment_store.get(&id).await.unwrap(), Some(payment));
		assert_eq!(wallet.pending_payment_store.get(&id).await.unwrap(), Some(record));
	}

	/// A replayed `SpliceNegotiated` event names a round already marked broadcast; nothing is
	/// written.
	#[tokio::test]
	async fn marking_a_broadcast_round_again_writes_nothing() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		wallet.record_broadcast_splice_round(channel_id, txid).await.unwrap();

		fail_store.fail_writes.store(true, Ordering::Release);
		wallet.record_broadcast_splice_round(channel_id, txid).await.unwrap();
		assert_eq!(
			fail_store.failed_writes.load(Ordering::Acquire),
			0,
			"marking a round broadcast again must produce no new write"
		);
	}

	/// A round no funding payment tracks — this node contributed nothing to it, so signing never
	/// recorded it — has no mark to clear; nothing is written.
	#[tokio::test]
	async fn marking_an_unrecorded_round_broadcast_writes_nothing() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_counterparty_node_id, channel_id) = test_counterparty_and_channel();

		fail_store.fail_writes.store(true, Ordering::Release);
		let txid = Txid::from_byte_array([0xAA; 32]);
		wallet.record_broadcast_splice_round(channel_id, txid).await.unwrap();
		assert_eq!(fail_store.failed_writes.load(Ordering::Acquire), 0);
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());
	}

	/// A replayed signing event re-offers a transaction already recorded; nothing is written.
	#[tokio::test]
	async fn signing_a_recorded_round_again_writes_nothing() {
		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();

		fail_store.fail_writes.store(true, Ordering::Release);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		assert_eq!(
			fail_store.failed_writes.load(Ordering::Acquire),
			0,
			"a replayed signing must produce no new write"
		);
	}

	/// A signed round absent from the channel's pending splice history was reset between the
	/// event's emission and its handling (the counterparty aborted): LDK will refuse the signed
	/// transaction, so nothing is recorded for it — not even when the history holds another round
	/// this node contributed to.
	#[tokio::test]
	async fn signing_skips_a_round_missing_from_the_splice_history() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let other_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(other_txid, Some(contribution))],
		);

		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		wallet.record_signed_funding(&tx, &[]).await.unwrap();
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());
		assert!(wallet.pending_payment_store.list_page(None).await.unwrap().objects.is_empty());
	}

	/// A round this node did not contribute to is not its payment: the signing-time recording
	/// declines it.
	#[tokio::test]
	async fn signing_skips_a_round_without_a_local_contribution() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, _contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(tx.compute_txid(), None)]);

		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());
		assert!(wallet.pending_payment_store.list_page(None).await.unwrap().objects.is_empty());
	}

	/// A splice-out to an external address moves no wallet funds; the signing-time recording
	/// declines it — wallet sync cannot observe it either, so there is no race to close.
	#[tokio::test]
	async fn signing_skips_a_wallet_untouched_transaction() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let splice_out =
			TxOut { value: Amount::from_sat(500_000), script_pubkey: ScriptBuf::new() };
		let contribution =
			test_funding_contribution_with_outputs(300, 253, std::slice::from_ref(&splice_out));
		let tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![bitcoin::TxIn {
				previous_output: OutPoint { txid: Txid::from_byte_array([1u8; 32]), vout: 0 },
				..Default::default()
			}],
			output: vec![splice_out],
		};
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(tx.compute_txid(), Some(contribution))],
		);

		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());
		assert!(wallet.pending_payment_store.list_page(None).await.unwrap().objects.is_empty());
	}

	/// The signing write merges LDK's history into the recorded one instead of replacing it: a
	/// recorded round LDK no longer lists survives the write, since dropping the rounds LDK
	/// abandoned is [`Wallet::drop_abandoned_splice_rounds`]'s job, once LDK reports the failure.
	#[tokio::test]
	async fn signing_merges_ldk_history_into_the_recorded_one() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let prior_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(prior_txid, None), (txid, Some(contribution))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();

		let (next_tx, next_contribution) = splice_out_round(&wallet, 2, 400_000, 700);
		let next_txid = next_tx.compute_txid();
		let next_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(prior_txid, None), (next_txid, Some(next_contribution))],
		);
		wallet.record_signed_funding(&next_tx, &next_candidates).await.unwrap();

		let payments = wallet.payment_store.list_page(None).await.unwrap().objects;
		assert_eq!(payments.len(), 1);
		let id = payments[0].id;
		assert_eq!(wallet.find_payment_by_txid(prior_txid).await.unwrap(), Some(id));
		assert!(
			matches!(&payments[0].kind, PaymentKind::Onchain { txid: t, .. } if *t == next_txid)
		);
		assert_eq!(payments[0].amount_msat, Some(400_700_000));
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(
			record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(),
			vec![prior_txid, txid, next_txid]
		);
		assert_eq!(record.candidate(txid).unwrap().amount_msat, Some(500_300_000));
		assert_eq!(record.candidate(next_txid).unwrap().amount_msat, Some(400_700_000));
	}

	/// LDK abandoned a signed first round (the counterparty aborted before the signatures were
	/// exchanged) and reports the failure: nothing was ever broadcast under the record, so it goes,
	/// leaving no payment nothing can confirm — while another channel's record is left alone.
	#[tokio::test]
	async fn dropping_an_abandoned_first_round_removes_its_record() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let other_channel_id = ChannelId([8u8; 32]);

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let (other_tx, other_contribution) = splice_out_round(&wallet, 2, 400_000, 700);
		let other_txid = other_tx.compute_txid();
		let other_candidates = splice_candidates(
			counterparty_node_id,
			other_channel_id,
			&[(other_txid, Some(other_contribution))],
		);
		wallet.record_signed_funding(&other_tx, &other_candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let other_id = wallet.find_payment_by_txid(other_txid).await.unwrap().expect("other id");

		wallet.drop_abandoned_splice_rounds(channel_id, &[]).await.unwrap();

		assert!(wallet.payment_store.get(&id).await.unwrap().is_none());
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), None);
		assert!(wallet.payment_store.get(&other_id).await.unwrap().is_some());
		assert_eq!(wallet.find_payment_by_txid(other_txid).await.unwrap(), Some(other_id));
	}

	/// LDK abandoned a signed fee bump while the round it replaces stays pending: the bump leaves
	/// the recorded history and the record tracks the original round again, figures included.
	#[tokio::test]
	async fn dropping_an_abandoned_bump_restores_the_prior_round() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");

		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		wallet.record_signed_funding(&bump_tx, &bump_candidates).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == bump_txid));

		wallet.drop_abandoned_splice_rounds(channel_id, &[txid]).await.unwrap();

		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(
			matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == txid),
			"the original round must be the actively-tracked transaction again"
		);
		assert_eq!(payment.amount_msat, Some(500_300_000));
		assert_eq!(payment.fee_paid_msat, Some(300_000));
		assert_eq!(record.details(), Some(&payment));
		assert_eq!(wallet.find_payment_by_txid(bump_txid).await.unwrap(), None);
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), Some(id));
	}

	/// A round awaiting broadcast that the wallet has nonetheless seen — the counterparty broadcast
	/// it with our signatures while LDK still waited on its own, and the channel then closed — may
	/// still confirm and keeps its place, even once evicted from the mempool: the lookup is not
	/// canonical-only.
	#[tokio::test]
	async fn dropping_keeps_a_round_the_wallet_has_seen() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		insert_unconfirmed_tx(&wallet, tx);
		evict_tx(&wallet, txid);
		assert!(wallet.inner.lock().unwrap().get_tx(txid).is_none(), "evicted: not canonical");
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");

		wallet.drop_abandoned_splice_rounds(channel_id, &[]).await.unwrap();

		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == txid));
		assert_eq!(payment.status, PaymentStatus::Pending);
	}

	/// The channel force-closed with a negotiated round unconfirmed and a fee bump of it signed
	/// but never exchanged, before wallet sync picked the negotiated round up: LDK lists neither
	/// anymore, but the negotiated round was handed to the broadcaster and may still confirm, so
	/// only the bump is dropped.
	#[tokio::test]
	async fn dropping_keeps_rounds_handed_to_the_broadcaster() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		wallet.record_broadcast_splice_round(channel_id, txid).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");

		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		wallet.record_signed_funding(&bump_tx, &bump_candidates).await.unwrap();

		wallet.drop_abandoned_splice_rounds(channel_id, &[]).await.unwrap();

		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == txid));
		assert_eq!(payment.amount_msat, Some(500_300_000));
		assert_eq!(payment.fee_paid_msat, Some(300_000));
		assert_eq!(payment.status, PaymentStatus::Pending);
	}

	/// LDK abandoned the only round this node contributed to, an RBF of a counterparty-initiated
	/// round it did not: what remains is not this node's payment, so the record goes instead of
	/// being handed to a round the wallet will never observe.
	#[tokio::test]
	async fn dropping_the_last_contributed_round_removes_the_record() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let prior_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(prior_txid, None), (txid, Some(contribution))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(prior_txid).await.unwrap().expect("id");
		assert!(wallet.payment_store.get(&id).await.unwrap().is_some());

		wallet.drop_abandoned_splice_rounds(channel_id, &[prior_txid]).await.unwrap();

		assert!(wallet.payment_store.get(&id).await.unwrap().is_none());
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), None);
	}

	/// The record moved on before the drop: wallet sync confirmed the original round while its
	/// bump awaited signatures, then LDK abandoned the bump. The confirmed record is left as it
	/// stands; only the bump leaves the recorded history.
	#[tokio::test]
	async fn dropping_leaves_a_record_that_moved_on() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");

		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		wallet.record_signed_funding(&bump_tx, &bump_candidates).await.unwrap();

		insert_confirmed_tx(&wallet, tx.clone(), 105);
		let event = WalletEvent::TxConfirmed {
			txid,
			tx: Arc::new(tx),
			block_time: confirmed_block_time(105),
			old_block_time: None,
		};
		wallet.update_payment_store(vec![event]).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(matches!(
			payment.kind,
			PaymentKind::Onchain { txid: t, status: ConfirmationStatus::Confirmed { .. }, .. }
				if t == txid
		));

		wallet.drop_abandoned_splice_rounds(channel_id, &[txid]).await.unwrap();

		assert_eq!(wallet.payment_store.get(&id).await.unwrap(), Some(payment.clone()));
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		assert_eq!(record.details(), Some(&payment));
		assert_eq!(wallet.find_payment_by_txid(bump_txid).await.unwrap(), None);
	}

	/// A removal that was cut short between the two stores — the payment record went, the pending
	/// entry stayed — is finished by the replayed drop: the entry alone still resolves the round's
	/// txid, so it is what the replayed event finds and removes.
	#[tokio::test]
	async fn a_cut_short_removal_is_finished_by_the_replayed_drop() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		wallet.payment_store.remove(&id).await.unwrap();
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_some());

		wallet.drop_abandoned_splice_rounds(channel_id, &[]).await.unwrap();

		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), None);
	}

	/// A hand-back that was cut short between the two stores — the payment record tracks the
	/// original round again, the pending entry still lists the bump and mirrors the record as it
	/// was — is finished by the replayed drop: the bump leaves the history and the entry's copy of
	/// the record catches up with the record.
	#[tokio::test]
	async fn a_cut_short_hand_back_is_finished_by_the_replayed_drop() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		wallet.record_signed_funding(&bump_tx, &bump_candidates).await.unwrap();

		// The first half of the hand-back: the payment record alone tracks the original round.
		let mut update = PaymentDetailsUpdate::new(id);
		update.txid = Some(txid);
		update.confirmation_status = Some(ConfirmationStatus::Unconfirmed);
		update.amount_msat = Some(Some(500_300_000));
		update.fee_paid_msat = Some(Some(300_000));
		wallet.payment_store.update(update).await.unwrap();
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("entry");
		assert!(matches!(
			entry.details().map(|details| &details.kind),
			Some(PaymentKind::Onchain { txid: t, .. }) if *t == bump_txid
		));

		wallet.drop_abandoned_splice_rounds(channel_id, &[txid]).await.unwrap();

		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("entry");
		assert_eq!(entry.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == txid));
		assert_eq!(payment.amount_msat, Some(500_300_000));
		assert_eq!(entry.details(), Some(&payment));
		assert_eq!(wallet.find_payment_by_txid(bump_txid).await.unwrap(), None);
	}

	/// The replayed signing event removes only the half-written record of a first round: a funding
	/// record that has graduated, and a pending on-chain record that is not a funding payment,
	/// stay as they are even though neither has a pending entry.
	#[tokio::test]
	async fn a_replayed_signing_leaves_records_that_are_not_half_written_rounds() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;

		let (tx, _) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let id = PaymentId([11u8; 32]);
		let mut graduated = interactive_funding_details(id, txid, Some(500_300_000), Some(300_000));
		graduated.status = PaymentStatus::Succeeded;
		wallet.payment_store.insert_or_update(graduated.clone()).await.unwrap();

		let (other_tx, _) = splice_out_round(&wallet, 2, 400_000, 700);
		let other_txid = other_tx.compute_txid();
		let other_id = PaymentId([12u8; 32]);
		let untyped = PaymentDetails::new(
			other_id,
			PaymentKind::Onchain {
				txid: other_txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: None,
			},
			Some(90_000_000),
			None,
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);
		wallet.payment_store.insert_or_update(untyped.clone()).await.unwrap();

		wallet.record_signed_funding(&tx, &[]).await.unwrap();
		wallet.record_signed_funding(&other_tx, &[]).await.unwrap();

		assert_eq!(wallet.payment_store.get(&id).await.unwrap(), Some(graduated));
		assert_eq!(wallet.payment_store.get(&other_id).await.unwrap(), Some(untyped));
	}

	/// The rounds LDK holds for a channel are its pending rounds with a transaction and its current
	/// funding, which a zero-conf splice becomes before its transaction confirms.
	#[test]
	fn held_splice_rounds_include_the_current_funding() {
		let pending_txid = Txid::from_byte_array([0xAA; 32]);
		let funding_txid = Txid::from_byte_array([0xBB; 32]);
		let details = SpliceDetails {
			candidates: vec![
				SpliceCandidateDetails {
					status: SpliceCandidateStatus::AwaitingSignatures {
						is_initiator: true,
						funding_feerate_sat_per_1000_weight: 253,
						new_channel_value_satoshis: 110_000,
						txid: pending_txid,
					},
					contribution: None,
				},
				SpliceCandidateDetails {
					status: SpliceCandidateStatus::WaitingOnLock,
					contribution: None,
				},
			],
			confirmed_candidate: None,
			received_splice_locked_txid: None,
		};
		let funding = LdkOutPoint { txid: funding_txid, index: 0 };

		assert_eq!(
			held_splice_rounds(Some(&details), Some(funding)),
			vec![pending_txid, funding_txid]
		);
		assert_eq!(held_splice_rounds(None, Some(funding)), vec![funding_txid]);
		assert!(held_splice_rounds(None, None).is_empty());
	}

	/// The rounds a closed channel may still see confirm are its last funding and every transaction
	/// its monitor still watches: a splice round the counterparty committed to stays watched once
	/// the channel manager has forgotten it with the channel. Without a monitor, only the funding
	/// is held.
	#[test]
	fn closed_channel_held_rounds_include_the_watched_transactions() {
		let funding_txid = Txid::from_byte_array([0xBB; 32]);
		let watched_txid = Txid::from_byte_array([0xCC; 32]);
		let funding = LdkOutPoint { txid: funding_txid, index: 0 };

		assert_eq!(
			closed_channel_held_rounds(Some(funding), [funding_txid, watched_txid]),
			vec![funding_txid, watched_txid]
		);
		assert_eq!(closed_channel_held_rounds(Some(funding), []), vec![funding_txid]);
		assert_eq!(closed_channel_held_rounds(None, [watched_txid]), vec![watched_txid]);
		assert!(closed_channel_held_rounds(None, []).is_empty());
	}

	/// The node restarted with a signed round LDK never wrote out — it stopped between LDK handing
	/// the round out for signing and its next channel manager write, and the round was committed
	/// after the last one — so LDK holds nothing for it and reports no failure: the startup sweep
	/// drops it, while a round LDK still holds stays, and so does the round of a channel LDK no
	/// longer lists, which is left to the channel's `ChannelClosed` event.
	#[tokio::test]
	async fn startup_drops_the_rounds_ldk_no_longer_holds() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let other_channel_id = ChannelId([8u8; 32]);
		let closed_channel_id = ChannelId([9u8; 32]);

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let (other_tx, other_contribution) = splice_out_round(&wallet, 2, 400_000, 700);
		let other_txid = other_tx.compute_txid();
		let other_candidates = splice_candidates(
			counterparty_node_id,
			other_channel_id,
			&[(other_txid, Some(other_contribution))],
		);
		wallet.record_signed_funding(&other_tx, &other_candidates).await.unwrap();
		let (closed_tx, closed_contribution) = splice_out_round(&wallet, 3, 300_000, 500);
		let closed_txid = closed_tx.compute_txid();
		let closed_candidates = splice_candidates(
			counterparty_node_id,
			closed_channel_id,
			&[(closed_txid, Some(closed_contribution))],
		);
		wallet.record_signed_funding(&closed_tx, &closed_candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let other_id = wallet.find_payment_by_txid(other_txid).await.unwrap().expect("other id");
		let closed_id = wallet.find_payment_by_txid(closed_txid).await.unwrap().expect("closed id");

		wallet
			.drop_splice_rounds_lost_across_restart(|channel| {
				if channel == other_channel_id {
					Some(vec![other_txid])
				} else if channel == closed_channel_id {
					None
				} else {
					Some(Vec::new())
				}
			})
			.await
			.unwrap();

		assert!(wallet.payment_store.get(&id).await.unwrap().is_none());
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
		assert!(wallet.payment_store.get(&other_id).await.unwrap().is_some());
		assert!(wallet.pending_payment_store.get(&other_id).await.unwrap().is_some());
		assert!(wallet.payment_store.get(&closed_id).await.unwrap().is_some());
		assert!(wallet.pending_payment_store.get(&closed_id).await.unwrap().is_some());
	}

	/// A record that graduated while its pending entry lingers — the entry's removal is still
	/// owed — loses the dropped round from its history but keeps the entry's pending copy of the
	/// record: the pass that cleans up lingering entries goes by that copy, and a graduated one
	/// would leave the entry behind for good.
	#[tokio::test]
	async fn dropping_leaves_the_entry_of_a_graduated_record_pending() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		wallet.record_signed_funding(&bump_tx, &bump_candidates).await.unwrap();

		let mut update = PaymentDetailsUpdate::new(id);
		update.status = Some(PaymentStatus::Succeeded);
		wallet.payment_store.update(update).await.unwrap();

		wallet.drop_abandoned_splice_rounds(channel_id, &[txid]).await.unwrap();

		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert_eq!(payment.status, PaymentStatus::Succeeded);
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == bump_txid));
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("entry");
		assert_eq!(entry.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		assert_eq!(entry.details().map(|details| details.status), Some(PaymentStatus::Pending));
	}

	/// The signing write failed between its two stores and the rollback failed as well, leaving
	/// the payment record without its pending entry; the round was then reset. The replayed
	/// signing event, finding the round gone, drops the half-written record — and leaves a fully
	/// recorded round to the negotiation-failure handling.
	#[tokio::test]
	async fn a_replayed_signing_drops_the_half_written_record_of_a_reset_round() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let id = PaymentId([9u8; 32]);
		let half_written = interactive_funding_details(id, txid, Some(500_300_000), Some(300_000));
		wallet.payment_store.insert_or_update(half_written).await.unwrap();
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), Some(id));

		wallet.record_signed_funding(&tx, &[]).await.unwrap();
		assert!(wallet.payment_store.get(&id).await.unwrap().is_none());
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), None);

		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		wallet.record_signed_funding(&tx, &[]).await.unwrap();
		assert!(wallet.payment_store.get(&id).await.unwrap().is_some());
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_some());
	}

	/// The signing write fails between its two stores — the payment record lands, the pending
	/// entry does not — so the payment store is put back as it was, and the replayed event
	/// records the round in full once the store recovers instead of building on a half-written
	/// record.
	#[tokio::test]
	async fn a_failed_first_round_signing_write_leaves_no_half_written_record() {
		let fail_store =
			FailSwitchStore::failing_only(PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE);
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);

		fail_store.fail_writes.store(true, Ordering::Release);
		assert!(wallet.record_signed_funding(&tx, &candidates).await.is_err());
		assert_eq!(fail_store.failed_writes.load(Ordering::Acquire), 1);
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), None);

		fail_store.fail_writes.store(false, Ordering::Release);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("payment");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid: t, .. } if t == txid));
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
	}

	/// The same failure while signing a fee bump: the record is put back to the original round,
	/// figures included, rather than left pointing at a bump the pending entry knows nothing of.
	#[tokio::test]
	async fn a_failed_bump_signing_write_restores_the_prior_round() {
		let fail_store =
			FailSwitchStore::failing_only(PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE);
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();

		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution.clone()))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		let prior = wallet.payment_store.get(&id).await.unwrap().expect("payment");

		let (bump_tx, bump_contribution) = splice_out_round(&wallet, 2, 499_000, 700);
		let bump_txid = bump_tx.compute_txid();
		let bump_candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(txid, Some(contribution)), (bump_txid, Some(bump_contribution))],
		);
		fail_store.fail_writes.store(true, Ordering::Release);
		assert!(wallet.record_signed_funding(&bump_tx, &bump_candidates).await.is_err());
		assert_eq!(fail_store.failed_writes.load(Ordering::Acquire), 1);

		assert_eq!(wallet.payment_store.get(&id).await.unwrap(), Some(prior));
		let record = wallet.pending_payment_store.get(&id).await.unwrap().expect("record");
		assert_eq!(record.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![txid]);
		assert_eq!(wallet.find_payment_by_txid(bump_txid).await.unwrap(), None);
	}

	/// The candidates handed to the signing-time recording are the channel's pending splice
	/// rounds that have a transaction — negotiated predecessors and the round awaiting
	/// signatures, in LDK's order, each with this node's contribution to it. A contribution
	/// still queued behind the pending rounds has no transaction and is left out.
	#[test]
	fn funding_candidates_list_the_rounds_with_a_transaction() {
		use lightning::chain::chaininterface::FundingPurpose;
		use lightning::ln::channel_state::{
			SpliceCandidateDetails, SpliceCandidateStatus, SpliceDetails,
		};

		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let prior_txid = Txid::from_byte_array([9u8; 32]);
		let signing_txid = Txid::from_byte_array([10u8; 32]);
		let contribution = test_funding_contribution_with_outputs(0, 253, &[]);
		let details = SpliceDetails {
			candidates: vec![
				SpliceCandidateDetails {
					contribution: None,
					status: SpliceCandidateStatus::Negotiated {
						txid: prior_txid,
						new_channel_value_satoshis: 100_000,
					},
				},
				SpliceCandidateDetails {
					contribution: Some(contribution.clone()),
					status: SpliceCandidateStatus::AwaitingSignatures {
						is_initiator: true,
						funding_feerate_sat_per_1000_weight: 253,
						new_channel_value_satoshis: 110_000,
						txid: signing_txid,
					},
				},
				SpliceCandidateDetails {
					contribution: Some(test_funding_contribution_with_outputs(0, 500, &[])),
					status: SpliceCandidateStatus::WaitingOnLock,
				},
			],
			confirmed_candidate: None,
			received_splice_locked_txid: None,
		};

		let candidates = funding_candidates(Some(&details), counterparty_node_id, channel_id);

		assert_eq!(candidates.len(), 2);
		assert_eq!(candidates[0].txid, prior_txid);
		assert_eq!(candidates[0].channels.len(), 1);
		assert_eq!(candidates[0].channels[0].contribution, None);
		assert_eq!(candidates[1].txid, signing_txid);
		assert_eq!(candidates[1].channels.len(), 1);
		assert_eq!(candidates[1].channels[0].counterparty_node_id, counterparty_node_id);
		assert_eq!(candidates[1].channels[0].channel_id, channel_id);
		assert_eq!(candidates[1].channels[0].purpose, FundingPurpose::Splice);
		assert_eq!(candidates[1].channels[0].contribution, Some(contribution));

		assert!(funding_candidates(None, counterparty_node_id, channel_id).is_empty());
	}

	#[test]
	fn funding_reclassification_update_substitutes_the_confirmed_candidate() {
		let confirmed_txid = Txid::from_byte_array([1u8; 32]);
		let active_txid = Txid::from_byte_array([2u8; 32]);
		let candidates = vec![
			FundingTxCandidate {
				txid: confirmed_txid,
				amount_msat: Some(2_000_000),
				fee_paid_msat: Some(999),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: active_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
				awaiting_broadcast: false,
			},
		];
		let details = onchain_details(active_txid, ConfirmationStatus::Unconfirmed);

		// The record confirmed an earlier candidate: the update reports that candidate, not the
		// active one.
		let current = onchain_details(confirmed_txid, confirmed_status());
		let update = funding_reclassification_update(details.clone(), &candidates, Some(&current));
		assert_eq!(update.txid, Some(confirmed_txid));
		assert_eq!(update.amount_msat, Some(Some(2_000_000)));
		assert_eq!(update.fee_paid_msat, Some(Some(999)));

		// A confirmed candidate we did not contribute to still substitutes, with empty figures —
		// the same figures a confirmation arriving after classification would report.
		let uncontributed = vec![FundingTxCandidate {
			txid: confirmed_txid,
			amount_msat: None,
			fee_paid_msat: None,
			awaiting_broadcast: false,
		}];
		let update =
			funding_reclassification_update(details.clone(), &uncontributed, Some(&current));
		assert_eq!(update.txid, Some(confirmed_txid));
		assert_eq!(update.amount_msat, Some(None));
		assert_eq!(update.fee_paid_msat, Some(None));
	}

	#[test]
	fn funding_reclassification_update_keeps_the_active_candidate() {
		let active_txid = Txid::from_byte_array([2u8; 32]);
		let candidates = vec![FundingTxCandidate {
			txid: active_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details = onchain_details(active_txid, ConfirmationStatus::Unconfirmed);

		// No record yet: the update describes the active candidate.
		let update = funding_reclassification_update(details.clone(), &candidates, None);
		assert_eq!(update.txid, Some(active_txid));
		assert_eq!(update.amount_msat, Some(Some(1_000_000)));

		// An unconfirmed record: still the active candidate (RBF rotation).
		let unconfirmed =
			onchain_details(Txid::from_byte_array([1u8; 32]), ConfirmationStatus::Unconfirmed);
		let update =
			funding_reclassification_update(details.clone(), &candidates, Some(&unconfirmed));
		assert_eq!(update.txid, Some(active_txid));

		// The record confirmed the active candidate itself: nothing to substitute.
		let current = onchain_details(active_txid, confirmed_status());
		let update = funding_reclassification_update(details.clone(), &candidates, Some(&current));
		assert_eq!(update.txid, Some(active_txid));
		assert_eq!(update.amount_msat, Some(Some(1_000_000)));

		// A confirmed txid outside the candidate history (e.g. the record is an unrelated
		// same-id payment): fall back to the active candidate; `PaymentDetails::update` keeps
		// the confirmed figures in place on mismatch.
		let foreign = onchain_details(Txid::from_byte_array([9u8; 32]), confirmed_status());
		let update = funding_reclassification_update(details, &candidates, Some(&foreign));
		assert_eq!(update.txid, Some(active_txid));
	}

	/// A funding-typed (re)classification of a record already classified as interactive funding
	/// carries nothing the record doesn't have — LDK re-broadcasts a promoted-but-unconfirmed
	/// splice through its generic funding path with wallet-view figures — so the update must
	/// move nothing.
	#[test]
	fn funding_reclassification_update_skips_funding_over_interactive_funding() {
		let txid = Txid::from_byte_array([1u8; 32]);
		let payment_id = PaymentId(txid.to_byte_array());
		let current = interactive_funding_details(payment_id, txid, Some(1_000_000), Some(500));

		let rebroadcast = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(TransactionType::Funding { channels: vec![] }),
			},
			Some(10_000_000),
			Some(0),
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);

		let update = funding_reclassification_update(rebroadcast, &[], Some(&current));
		let mut updated = current.clone();
		assert!(!updated.update(update), "the rebroadcast must not move the record");
		assert_eq!(updated, current);
	}

	/// Graduation must decide from the live record and write only the status: a pending-store
	/// snapshot taken before a concurrent classification landed must not roll the record's
	/// figures back when the payment graduates to `Succeeded`.
	#[tokio::test]
	async fn graduation_preserves_classified_figures() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let txid = Txid::from_byte_array([4u8; 32]);
		let payment_id = PaymentId(txid.to_byte_array());
		let confirmed = ConfirmationStatus::Confirmed {
			block_hash: bitcoin::BlockHash::from_byte_array([9u8; 32]),
			height: 5,
			timestamp: 100,
		};
		let tx_type = Some(TransactionType::InteractiveFunding { channels: vec![] });

		// The live record carries the classification: contribution-derived figures, confirmed.
		let mut recorded =
			interactive_funding_details(payment_id, txid, Some(2_000_000), Some(999));
		recorded.kind = PaymentKind::Onchain { txid, status: confirmed, tx_type: tx_type.clone() };
		recorded.latest_update_timestamp = 0;
		wallet.payment_store.insert_or_update(recorded).await.unwrap();

		// The pending entry embeds a stale snapshot: wallet-derived figures recorded before the
		// classification above landed.
		let mut stale = interactive_funding_details(payment_id, txid, Some(0), Some(0));
		stale.kind = PaymentKind::Onchain { txid, status: confirmed, tx_type };
		let entry = PendingPaymentDetails::new(stale, Vec::new(), Vec::new());
		wallet.pending_payment_store.insert_or_update(entry).await.unwrap();

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged { old_tip: block_id(9), new_tip: block_id(10) };
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Succeeded);
		assert_eq!(
			payment.amount_msat,
			Some(2_000_000),
			"graduation must not roll figures back to the snapshot's"
		);
		assert_eq!(payment.fee_paid_msat, Some(999));
		assert!(payment.latest_update_timestamp > 0, "the graduation write must timestamp");
		assert!(wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none());
	}

	/// When the live record has diverged from the pending-store snapshot — here the snapshot
	/// says Confirmed at graduation depth while the record says Unconfirmed — graduation must
	/// decline and keep the entry rather than force-writing `Succeeded` from stale state. The
	/// seeded divergence is synthetic (no current production writer downgrades a record's
	/// confirmation); the test pins the hardening that comes with deciding from the live record.
	#[tokio::test]
	async fn graduation_declines_on_diverged_record() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let txid = Txid::from_byte_array([5u8; 32]);
		let payment_id = PaymentId(txid.to_byte_array());
		let confirmed = ConfirmationStatus::Confirmed {
			block_hash: bitcoin::BlockHash::from_byte_array([9u8; 32]),
			height: 5,
			timestamp: 100,
		};

		// The live record is Unconfirmed...
		let recorded = interactive_funding_details(payment_id, txid, Some(2_000_000), Some(999));
		wallet.payment_store.insert_or_update(recorded).await.unwrap();

		// ...while the pending entry's snapshot claims a graduation-deep confirmation.
		let mut snapshot =
			interactive_funding_details(payment_id, txid, Some(2_000_000), Some(999));
		snapshot.kind = PaymentKind::Onchain {
			txid,
			status: confirmed,
			tx_type: Some(TransactionType::InteractiveFunding { channels: vec![] }),
		};
		let entry = PendingPaymentDetails::new(snapshot, Vec::new(), Vec::new());
		wallet.pending_payment_store.insert_or_update(entry).await.unwrap();

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged { old_tip: block_id(9), new_tip: block_id(10) };
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(
			payment.status,
			PaymentStatus::Pending,
			"a diverged snapshot must not force-graduate the record"
		);
		assert!(matches!(
			payment.kind,
			PaymentKind::Onchain { status: ConfirmationStatus::Unconfirmed, .. }
		));
		assert!(
			wallet.pending_payment_store.get(&payment_id).await.unwrap().is_some(),
			"the entry must survive for future events to drive"
		);
	}

	/// A middle RBF candidate must map back to the funding record: it is neither the record's
	/// id (derived from the first candidate), nor its current txid (the active candidate), nor
	/// in `conflicting_txids` (it never got a `TxReplaced` event of its own).
	#[tokio::test]
	async fn find_payment_by_txid_maps_candidate_txids() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let txid1 = Txid::from_byte_array([1u8; 32]);
		let txid2 = Txid::from_byte_array([2u8; 32]);
		let txid3 = Txid::from_byte_array([3u8; 32]);
		let payment_id = PaymentId(txid1.to_byte_array());
		let candidates = vec![
			FundingTxCandidate {
				txid: txid1,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: txid2,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(600),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: txid3,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(700),
				awaiting_broadcast: false,
			},
		];
		let details = interactive_funding_details(payment_id, txid3, Some(1_000_000), Some(700));
		let entry = PendingPaymentDetails::new(details, Vec::new(), candidates);
		wallet.pending_payment_store.insert_or_update(entry).await.unwrap();

		// The first candidate resolves via the txid-derived id and the active candidate via the
		// record's current txid; the middle one must resolve through the candidate history.
		assert_eq!(wallet.find_payment_by_txid(txid1).await.unwrap(), Some(payment_id));
		assert_eq!(wallet.find_payment_by_txid(txid3).await.unwrap(), Some(payment_id));
		assert_eq!(wallet.find_payment_by_txid(txid2).await.unwrap(), Some(payment_id));
	}

	/// A graduated funding record has no pending entry — graduation removes it — so its txid must
	/// resolve through the payment store itself. Without that fallback, a funding-typed broadcast
	/// classified after graduation (e.g. LDK re-broadcasting a promoted 0conf splice whose
	/// confirmation landed while the node was offline) would miss the record and create a duplicate
	/// under a fresh id.
	#[tokio::test]
	async fn find_payment_by_txid_resolves_graduated_records() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let txid = Txid::from_byte_array([6u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let mut graduated =
			interactive_funding_details(payment_id, txid, Some(1_000_000), Some(500));
		graduated.kind = PaymentKind::Onchain {
			txid,
			status: confirmed_status(),
			tx_type: Some(TransactionType::InteractiveFunding { channels: vec![] }),
		};
		graduated.status = PaymentStatus::Succeeded;
		wallet.payment_store.insert_or_update(graduated).await.unwrap();

		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), Some(payment_id));
	}

	/// A cooperative close conflicts with a pending splice's funding transaction — both spend the
	/// pre-splice funding outpoint — so sync records the close among the splice record's
	/// conflicting txids, and the close's confirmation then resolves to the splice's PaymentId.
	/// The funding record must not adopt the close's txid and confirmation as its own: the close
	/// is not a round of the splice. It must land on a record keyed by the close's own id.
	#[tokio::test]
	async fn funding_record_does_not_adopt_a_conflicting_close() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let funding_outpoint =
			bitcoin::OutPoint { txid: Txid::from_byte_array([3u8; 32]), vout: 0 };

		// The close pays the shutdown script, which is a wallet address.
		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let close_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![bitcoin::TxIn {
				previous_output: funding_outpoint,
				script_sig: bitcoin::ScriptBuf::new(),
				sequence: bitcoin::Sequence::MAX,
				witness: bitcoin::Witness::new(),
			}],
			output: vec![TxOut { value: Amount::from_sat(90_000), script_pubkey }],
		};
		let close_txid = close_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let candidates = vec![FundingTxCandidate {
			txid: splice_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();

		// Sync saw the close double-spend the splice's funding transaction.
		wallet
			.pending_payment_store
			.update(PendingPaymentDetailsUpdate {
				id: payment_id,
				payment_update: None,
				conflicting_txids: Some(vec![close_txid]),
				candidates: Vec::new(),
				splice_intent: None,
			})
			.await
			.unwrap();

		let event = WalletEvent::TxConfirmed {
			txid: close_txid,
			tx: Arc::new(close_tx),
			block_time: confirmed_block_time(5),
			old_block_time: None,
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let funding = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		match &funding.kind {
			PaymentKind::Onchain { txid, status, tx_type } => {
				assert_eq!(*txid, splice_txid, "the record must not adopt the close's txid");
				assert!(matches!(status, ConfirmationStatus::Unconfirmed));
				assert!(matches!(tx_type, Some(TransactionType::InteractiveFunding { .. })));
			},
			kind => panic!("unexpected kind {:?}", kind),
		}
		assert_eq!(funding.amount_msat, Some(1_000_000));
		assert_eq!(funding.fee_paid_msat, Some(500));

		let close = wallet
			.payment_store
			.get(&PaymentId(close_txid.to_byte_array()))
			.await
			.unwrap()
			.unwrap();
		match &close.kind {
			PaymentKind::Onchain { txid, status, .. } => {
				assert_eq!(*txid, close_txid);
				assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			},
			kind => panic!("unexpected kind {:?}", kind),
		}
	}

	/// Continues the story above: once the conflicting close confirms through the anti-reorg
	/// depth, the splice's funding transaction can never confirm — its shared input is spent for
	/// good. The record must fail rather than stay `Pending` forever, and removing the pending
	/// entry stops the dead transaction's rebroadcast on every tip change.
	#[tokio::test]
	async fn funding_payment_fails_once_a_foreign_conflict_confirms_to_depth() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let close_tx = wallet_paying_tx(&wallet, 3);
		let close_txid = close_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let candidates = vec![FundingTxCandidate {
			txid: splice_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();
		wallet
			.pending_payment_store
			.update(PendingPaymentDetailsUpdate {
				id: payment_id,
				payment_update: None,
				conflicting_txids: Some(vec![close_txid]),
				candidates: Vec::new(),
				splice_intent: None,
			})
			.await
			.unwrap();

		// The close is canonically confirmed; the splice transaction, having lost the conflict,
		// is no longer canonical (here: never inserted at all).
		insert_confirmed_tx(&wallet, close_tx, 5);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged {
			old_tip: block_id(9),
			new_tip: block_id(5 + ANTI_REORG_DELAY - 1),
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Failed);
		match &payment.kind {
			PaymentKind::Onchain { txid, status, tx_type } => {
				assert_eq!(*txid, splice_txid, "failing must not adopt the conflict's txid");
				assert!(matches!(status, ConfirmationStatus::Unconfirmed));
				assert!(matches!(tx_type, Some(TransactionType::InteractiveFunding { .. })));
			},
			kind => panic!("unexpected kind {:?}", kind),
		}
		assert_eq!(payment.amount_msat, Some(1_000_000));
		assert_eq!(payment.fee_paid_msat, Some(500));
		assert!(
			wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none(),
			"the entry must go so the dead transaction stops being rebroadcast"
		);
	}

	/// A confirmed conflict that is one of the record's own candidates is RBF resolution, not a
	/// loss: classification adopts it into the record, so the failure pass must leave the record
	/// alone.
	#[tokio::test]
	async fn funding_payment_survives_a_confirmed_conflict_that_is_a_candidate() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let bumped_tx = wallet_paying_tx(&wallet, 3);
		let bumped_txid = bumped_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let candidates = vec![
			FundingTxCandidate {
				txid: splice_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: bumped_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(600),
				awaiting_broadcast: false,
			},
		];
		let details =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();
		wallet
			.pending_payment_store
			.update(PendingPaymentDetailsUpdate {
				id: payment_id,
				payment_update: None,
				conflicting_txids: Some(vec![bumped_txid]),
				candidates: Vec::new(),
				splice_intent: None,
			})
			.await
			.unwrap();

		insert_confirmed_tx(&wallet, bumped_tx, 5);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged {
			old_tip: block_id(9),
			new_tip: block_id(5 + ANTI_REORG_DELAY - 1),
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Pending);
		assert!(
			wallet.pending_payment_store.get(&payment_id).await.unwrap().is_some(),
			"the entry must survive for classification to adopt the confirmed candidate"
		);
	}

	/// A foreign conflict that has confirmed but not yet through the anti-reorg depth may still
	/// be reorged out, letting the funding transaction confirm after all; the record must stay
	/// pending until the conflict's confirmation is final.
	#[tokio::test]
	async fn funding_payment_survives_a_foreign_conflict_short_of_depth() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let close_tx = wallet_paying_tx(&wallet, 3);
		let close_txid = close_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let candidates = vec![FundingTxCandidate {
			txid: splice_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();
		wallet
			.pending_payment_store
			.update(PendingPaymentDetailsUpdate {
				id: payment_id,
				payment_update: None,
				conflicting_txids: Some(vec![close_txid]),
				candidates: Vec::new(),
				splice_intent: None,
			})
			.await
			.unwrap();

		insert_confirmed_tx(&wallet, close_tx, 5);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged {
			old_tip: block_id(9),
			new_tip: block_id(5 + ANTI_REORG_DELAY - 2),
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Pending);
		assert!(wallet.pending_payment_store.get(&payment_id).await.unwrap().is_some());
	}

	/// A conflict may double-spend only one round of the negotiation — e.g. it shares an input
	/// with an RBF attempt but not with the original candidate. While any candidate is still
	/// canonical it can still confirm, so the record must stay pending.
	#[tokio::test]
	async fn funding_payment_survives_while_a_candidate_can_still_confirm() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let conflict_tx = wallet_paying_tx(&wallet, 3);
		let conflict_txid = conflict_tx.compute_txid();
		// A live candidate: spends a different outpoint, so the conflict didn't kill it.
		let live_candidate_tx = wallet_paying_tx(&wallet, 4);
		let live_candidate_txid = live_candidate_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let candidates = vec![
			FundingTxCandidate {
				txid: splice_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: live_candidate_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(600),
				awaiting_broadcast: false,
			},
		];
		let details =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();
		wallet
			.pending_payment_store
			.update(PendingPaymentDetailsUpdate {
				id: payment_id,
				payment_update: None,
				conflicting_txids: Some(vec![conflict_txid]),
				candidates: Vec::new(),
				splice_intent: None,
			})
			.await
			.unwrap();

		insert_confirmed_tx(&wallet, conflict_tx, 5);
		insert_unconfirmed_tx(&wallet, live_candidate_tx);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged {
			old_tip: block_id(9),
			new_tip: block_id(5 + ANTI_REORG_DELAY - 1),
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Pending);
		assert!(
			wallet.pending_payment_store.get(&payment_id).await.unwrap().is_some(),
			"a candidate can still confirm, so the record must stay pending"
		);
	}

	/// The failure write pair is record first, entry second: a crash in between leaves a
	/// `Failed` record with a lingering entry. The next tip pass must finish the job — remove
	/// the entry without disturbing the record.
	#[tokio::test]
	async fn a_failed_funding_payment_with_a_lingering_entry_is_cleaned_up() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let close_tx = wallet_paying_tx(&wallet, 3);
		let close_txid = close_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let mut recorded =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		recorded.status = PaymentStatus::Failed;
		recorded.latest_update_timestamp = 7;
		wallet.payment_store.insert_or_update(recorded).await.unwrap();

		// The entry embeds the pre-failure snapshot, as a crash between the two writes leaves it.
		let snapshot =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		let candidates = vec![FundingTxCandidate {
			txid: splice_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let entry = PendingPaymentDetails::new(snapshot, vec![close_txid], candidates);
		wallet.pending_payment_store.insert_or_update(entry).await.unwrap();

		insert_confirmed_tx(&wallet, close_tx, 5);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged {
			old_tip: block_id(9),
			new_tip: block_id(5 + ANTI_REORG_DELAY - 1),
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert_eq!(payment.latest_update_timestamp, 7, "the repair pass must not rewrite");
		assert!(
			wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none(),
			"the lingering entry must be removed"
		);
	}

	/// A crash between the failure's record write and its entry removal loses the wallet
	/// changeset too, so the restart's catch-up sync replays the same events: `TxReplaced` for
	/// the dead funding transaction resolves through the lingering entry to the already-`Failed`
	/// record. Re-embedding that record would stamp `Failed` into the entry and hide it from the
	/// pending listing that repairs it; the replay must instead finish the interrupted removal.
	#[tokio::test]
	async fn replayed_replacement_finishes_an_interrupted_failure() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let close_tx = wallet_paying_tx(&wallet, 3);
		let close_txid = close_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let mut recorded =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		recorded.status = PaymentStatus::Failed;
		recorded.latest_update_timestamp = 7;
		wallet.payment_store.insert_or_update(recorded).await.unwrap();

		let snapshot =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		let candidates = vec![FundingTxCandidate {
			txid: splice_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let entry = PendingPaymentDetails::new(snapshot, vec![close_txid], candidates);
		wallet.pending_payment_store.insert_or_update(entry).await.unwrap();

		insert_confirmed_tx(&wallet, close_tx, 5);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let events = vec![
			WalletEvent::TxReplaced {
				txid: splice_txid,
				tx: Arc::new(dummy_tx()),
				conflicts: vec![(0, close_txid)],
			},
			WalletEvent::ChainTipChanged {
				old_tip: block_id(9),
				new_tip: block_id(5 + ANTI_REORG_DELAY - 1),
			},
		];
		wallet.update_payment_store(events).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert_eq!(payment.latest_update_timestamp, 7, "the replay must not rewrite the record");
		assert!(
			wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none(),
			"the replay must finish the interrupted entry removal"
		);
	}

	/// A funding record's id is anchored to its first candidate's txid. Once the payment settles
	/// and its entry is removed, a wallet event for that candidate no longer resolves through the
	/// candidate history — the fallback keys it by its own txid, colliding with the record's id.
	/// Recording the event there would merge a fresh wallet-view `Pending` payment into the
	/// terminal record; such events must be skipped.
	#[tokio::test]
	async fn candidate_event_does_not_resurrect_a_settled_funding_payment() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		// The record's id derives from the first candidate r1; its txid rotated to the RBF round
		// r2. The payment failed and its pending entry is gone.
		let r1 = Txid::from_byte_array([2u8; 32]);
		let r2 = Txid::from_byte_array([4u8; 32]);
		let payment_id = PaymentId(r1.to_byte_array());
		let mut recorded = interactive_funding_details(payment_id, r2, Some(1_000_000), Some(600));
		recorded.status = PaymentStatus::Failed;
		recorded.latest_update_timestamp = 7;
		wallet.payment_store.insert_or_update(recorded).await.unwrap();

		// r1 reappears in the mempool after the failure...
		let event =
			WalletEvent::TxUnconfirmed { txid: r1, tx: Arc::new(dummy_tx()), old_block_time: None };
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Failed, "the record must not resurrect");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid, .. } if txid == r2));
		assert_eq!(payment.latest_update_timestamp, 7);
		assert!(wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none());

		// ...and even confirms: the record settled as `Failed` and must stay that way.
		let event = WalletEvent::TxConfirmed {
			txid: r1,
			tx: Arc::new(dummy_tx()),
			block_time: confirmed_block_time(5),
			old_block_time: None,
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Failed, "the record must not resurrect");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid, .. } if txid == r2));
		assert_eq!(payment.latest_update_timestamp, 7);
		assert!(wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none());
	}

	/// The failure transition must apply regardless of the payment's direction: a splice-out
	/// records as `Inbound` (funds return to the wallet) and dies to a conflicting close the
	/// same way an outbound one does.
	#[tokio::test]
	async fn inbound_funding_payment_fails_once_a_foreign_conflict_confirms_to_depth() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let close_tx = wallet_paying_tx(&wallet, 3);
		let close_txid = close_tx.compute_txid();

		let splice_txid = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId([21u8; 32]);
		let candidates = vec![FundingTxCandidate {
			txid: splice_txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let mut details =
			interactive_funding_details(payment_id, splice_txid, Some(1_000_000), Some(500));
		details.direction = PaymentDirection::Inbound;
		wallet.persist_funding_payment(details, candidates).await.unwrap();
		wallet
			.pending_payment_store
			.update(PendingPaymentDetailsUpdate {
				id: payment_id,
				payment_update: None,
				conflicting_txids: Some(vec![close_txid]),
				candidates: Vec::new(),
				splice_intent: None,
			})
			.await
			.unwrap();

		insert_confirmed_tx(&wallet, close_tx, 5);

		let block_id =
			|height| BlockId { height, hash: bitcoin::BlockHash::from_byte_array([7u8; 32]) };
		let event = WalletEvent::ChainTipChanged {
			old_tip: block_id(9),
			new_tip: block_id(5 + ANTI_REORG_DELAY - 1),
		};
		wallet.update_payment_store(vec![event]).await.unwrap();

		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(wallet.pending_payment_store.get(&payment_id).await.unwrap().is_none());
	}

	/// A funding-typed broadcast that doesn't touch the on-chain wallet must not be recorded.
	/// LDK re-broadcasts a promoted-but-unconfirmed 0conf splice through its generic funding
	/// path, so a splice the signing-time recording deliberately declined — no local
	/// contribution, or none of the moved funds are the wallet's — would otherwise come back as
	/// a spurious zero-amount record that nothing ever confirms.
	#[tokio::test]
	async fn funding_broadcast_without_wallet_activity_is_not_recorded() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let channels = vec![(counterparty_node_id, ChannelId([7u8; 32]))];
		let tx_type = TransactionType::Funding { channels: vec![] };

		// No inputs or outputs involve the wallet: nothing to record.
		wallet.classify_funding(&dummy_tx(), &channels, tx_type.clone()).await.unwrap();
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());
		assert!(wallet.pending_payment_store.list_filter(|_| true).await.is_empty());

		// A computable fee is not wallet participation. The wallet can resolve a splice's shared
		// input whenever the previous funding transaction touched it (e.g. it funded the original
		// channel open), so it derives the splice's fee even when no wallet funds move.
		let prev_funding_outpoint = OutPoint { txid: Txid::from_byte_array([8u8; 32]), vout: 0 };
		wallet.inner.lock().unwrap().insert_txout(
			prev_funding_outpoint,
			TxOut { value: Amount::from_sat(100_000), script_pubkey: ScriptBuf::new() },
		);
		let splice_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![bitcoin::TxIn {
				previous_output: prev_funding_outpoint,
				..Default::default()
			}],
			output: vec![TxOut {
				value: Amount::from_sat(99_000),
				script_pubkey: ScriptBuf::new(),
			}],
		};
		wallet.classify_funding(&splice_tx, &channels, tx_type.clone()).await.unwrap();
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());

		// Control: a funding transaction the wallet participates in is still recorded.
		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let funded_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey }],
		};
		wallet.classify_funding(&funded_tx, &channels, tx_type).await.unwrap();
		let payments = wallet.payment_store.list_page(None).await.unwrap().objects;
		assert_eq!(payments.len(), 1);
		match &payments[0].kind {
			PaymentKind::Onchain { txid, .. } => assert_eq!(*txid, funded_tx.compute_txid()),
			kind => panic!("unexpected kind {:?}", kind),
		}
	}

	/// A funding record's PaymentId is generated at record creation instead of being derived from a
	/// txid: a replaceable transaction's txid is no stable identity for the record. Every lookup
	/// resolves the record through its txid history (current txid, candidates, conflicts) rather
	/// than re-deriving the id, so nothing may rely on the id and the txid coinciding.
	#[tokio::test]
	async fn funding_record_is_keyed_by_a_generated_id() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let channels = vec![(counterparty_node_id, ChannelId([7u8; 32]))];
		let tx_type = TransactionType::Funding { channels: vec![] };

		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let funded_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey }],
		};
		let txid = funded_tx.compute_txid();
		wallet.classify_funding(&funded_tx, &channels, tx_type).await.unwrap();

		let payments = wallet.payment_store.list_page(None).await.unwrap().objects;
		assert_eq!(payments.len(), 1);
		let record = &payments[0];
		assert_ne!(record.id, PaymentId(txid.to_byte_array()), "the id must not be the txid");
		match &record.kind {
			PaymentKind::Onchain { txid: kind_txid, .. } => assert_eq!(*kind_txid, txid),
			kind => panic!("unexpected kind {:?}", kind),
		}
		// The pending entry shares the id, and txid lookups resolve to the record.
		assert!(wallet.pending_payment_store.get(&record.id).await.unwrap().is_some());
		assert_eq!(wallet.find_payment_by_txid(txid).await.unwrap(), Some(record.id));
	}

	/// A funding transaction classified again — e.g. a 0conf splice re-broadcast through LDK's
	/// generic funding path after a restart — must resolve to the record's generated id rather
	/// than create a second record for the same transaction.
	#[tokio::test]
	async fn funding_rebroadcast_resolves_to_the_generated_id() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let channels = vec![(counterparty_node_id, ChannelId([7u8; 32]))];

		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let funded_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey }],
		};
		let txid = funded_tx.compute_txid();

		// The interactive-funding record written when the round was signed, keyed by a generated
		// id.
		let payment_id = PaymentId([42u8; 32]);
		let candidates = vec![FundingTxCandidate {
			txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details = interactive_funding_details(payment_id, txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();

		// The re-typed rebroadcast comes back through the generic funding path.
		wallet
			.classify_funding(&funded_tx, &channels, TransactionType::Funding { channels: vec![] })
			.await
			.unwrap();

		let payments = wallet.payment_store.list_page(None).await.unwrap().objects;
		assert_eq!(payments.len(), 1, "the rebroadcast must not create a second record");
		assert_eq!(payments[0].id, payment_id);
		// The interactive classification and contribution figures survive the generic
		// wallet-view update (`funding_reclassification_update` declines the downgrade).
		assert!(matches!(
			payments[0].kind,
			PaymentKind::Onchain { tx_type: Some(TransactionType::InteractiveFunding { .. }), .. }
		));
		assert_eq!(payments[0].amount_msat, Some(1_000_000));
	}

	/// LDK re-broadcasts a promoted-but-unconfirmed 0conf splice through its generic funding
	/// path: same txid, but typed as a plain funding transaction with wallet-view figures and no
	/// contribution metadata. The rebroadcast must not overwrite the contribution-derived
	/// figures or the interactive-funding classification — neither while the record is
	/// unconfirmed nor once it confirmed under that same txid, where updates naming the
	/// confirmed txid may otherwise move figures.
	#[tokio::test]
	async fn funding_rebroadcast_keeps_interactive_funding_classification() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(store, false).await;

		// The rebroadcast passes the wallet-activity guard: a splice-in funds the new channel
		// output partly from the wallet, so the wallet sees movement.
		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey }],
		};
		let txid = tx.compute_txid();
		let payment_id = PaymentId(txid.to_byte_array());

		let candidates = vec![FundingTxCandidate {
			txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details = interactive_funding_details(payment_id, txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();

		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let channels = vec![(counterparty_node_id, ChannelId([7u8; 32]))];
		let tx_type = TransactionType::Funding { channels: vec![] };

		async fn assert_unchanged(wallet: &Wallet, payment_id: PaymentId, confirmed: bool) {
			let payments = wallet.payment_store.list_page(None).await.unwrap().objects;
			assert_eq!(payments.len(), 1, "the rebroadcast must not mint a second record");
			let payment = &payments[0];
			assert_eq!(payment.id, payment_id);
			assert_eq!(payment.amount_msat, Some(1_000_000));
			assert_eq!(payment.fee_paid_msat, Some(500));
			match &payment.kind {
				PaymentKind::Onchain {
					status,
					tx_type: Some(TransactionType::InteractiveFunding { .. }),
					..
				} => assert_eq!(matches!(status, ConfirmationStatus::Confirmed { .. }), confirmed),
				kind => panic!("unexpected kind {:?}", kind),
			}
		}

		wallet.classify_funding(&tx, &channels, tx_type.clone()).await.unwrap();
		assert_unchanged(&wallet, payment_id, false).await;

		// Confirm the record, then replay the rebroadcast: a monitor-update completion can race
		// wallet sync around confirmation.
		let event = WalletEvent::TxConfirmed {
			txid,
			tx: Arc::new(tx.clone()),
			block_time: confirmed_block_time(5),
			old_block_time: None,
		};
		wallet.update_payment_store(vec![event]).await.unwrap();
		wallet.classify_funding(&tx, &channels, tx_type).await.unwrap();
		assert_unchanged(&wallet, payment_id, true).await;
	}

	/// A funding broadcast whose classification fails must be retried, not dropped: no timer
	/// re-broadcasts a funding transaction, so a dropped package would keep the funding off-chain
	/// until LDK re-hands it when the channel next resumes. The record is written before the
	/// broadcast so that the confirmation refreshes it rather than minting an untyped record that
	/// the retried classification types only once it lands.
	#[tokio::test]
	async fn failed_funding_classification_is_retried_not_dropped() {
		use lightning::chain::chaininterface::BroadcasterInterface;

		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.broadcaster.set_wallet(Arc::downgrade(&wallet));

		// Run the production broadcast-queue loop. The broadcast itself fails fast against the
		// fixture's unroutable Esplora server, which is irrelevant here: the record is written
		// during classification, before the broadcast attempt.
		let (stop_sender, stop_receiver) = tokio::sync::watch::channel(());
		let chain_source = Arc::clone(&wallet.chain_source);
		let loop_task = tokio::spawn(async move {
			chain_source.continuously_process_broadcast_queue(stop_receiver).await
		});

		// A funding transaction paying the wallet passes the wallet-activity guard, so its
		// classification reaches the payment-store write.
		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey }],
		};
		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();

		// Queue the broadcast while payment persistence is failing.
		fail_store.fail_writes.store(true, Ordering::Release);
		wallet.broadcaster.broadcast_transactions(&[(
			&tx,
			LdkTransactionType::Funding {
				channels: vec![(counterparty_node_id, ChannelId([7u8; 32]))],
			},
		)]);

		// Wait until the loop has actually failed a classification write; re-enabling writes
		// before the first attempt would let the first attempt succeed and the test pass
		// without any retry happening. A failed classification must not leave a partial
		// record behind.
		let mut failed_writes = 0;
		for _ in 0..100 {
			tokio::time::sleep(Duration::from_millis(100)).await;
			failed_writes = fail_store.failed_writes.load(Ordering::Acquire);
			if failed_writes > 0 {
				break;
			}
		}
		assert!(failed_writes > 0, "classification never attempted a payment-store write");
		assert!(wallet.payment_store.list_page(None).await.unwrap().objects.is_empty());

		// Once writes recover, the package must still be alive to classify.
		fail_store.fail_writes.store(false, Ordering::Release);
		let mut recorded = Vec::new();
		for _ in 0..100 {
			tokio::time::sleep(Duration::from_millis(100)).await;
			recorded = wallet.payment_store.list_page(None).await.unwrap().objects;
			if !recorded.is_empty() {
				break;
			}
		}
		assert!(
			!recorded.is_empty(),
			"the failed classification was never retried; the package was dropped"
		);
		assert_eq!(recorded.len(), 1);
		assert!(matches!(
			recorded[0].kind,
			PaymentKind::Onchain { tx_type: Some(TransactionType::Funding { .. }), .. }
		));

		stop_sender.send(()).unwrap();
		loop_task.await.unwrap();
	}

	/// A package awaiting a classification retry must die when the node stops. When the retry
	/// was a detached task, it outlived the broadcast loop: its re-send into the still-open
	/// queue succeeded after `stop()`, so a later `start()` would classify and broadcast the
	/// stale package.
	#[tokio::test]
	async fn failed_classification_retry_dies_at_stop() {
		use lightning::chain::chaininterface::BroadcasterInterface;

		let fail_store = FailSwitchStore::new();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(fail_store.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		wallet.broadcaster.set_wallet(Arc::downgrade(&wallet));

		let (stop_sender, stop_receiver) = tokio::sync::watch::channel(());
		let chain_source = Arc::clone(&wallet.chain_source);
		let loop_task = tokio::spawn(async move {
			chain_source.continuously_process_broadcast_queue(stop_receiver).await
		});

		let script_pubkey = wallet
			.inner
			.lock()
			.unwrap()
			.reveal_next_address(KeychainKind::External)
			.address
			.script_pubkey();
		let tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: Vec::new(),
			output: vec![TxOut { value: Amount::from_sat(10_000), script_pubkey }],
		};
		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();

		// Queue the broadcast while payment persistence is failing and wait for the loop to
		// fail a classification attempt, leaving a retry pending.
		fail_store.fail_writes.store(true, Ordering::Release);
		wallet.broadcaster.broadcast_transactions(&[(
			&tx,
			LdkTransactionType::Funding {
				channels: vec![(counterparty_node_id, ChannelId([7u8; 32]))],
			},
		)]);
		let mut failed_writes = 0;
		for _ in 0..100 {
			tokio::time::sleep(Duration::from_millis(100)).await;
			failed_writes = fail_store.failed_writes.load(Ordering::Acquire);
			if failed_writes > 0 {
				break;
			}
		}
		assert!(failed_writes > 0, "classification never attempted a payment-store write");

		// Stop the node with the retry still pending, then bring the loop back up with
		// working persistence, as a stop()/start() cycle would.
		stop_sender.send(()).unwrap();
		loop_task.await.unwrap();
		fail_store.fail_writes.store(false, Ordering::Release);

		let (stop_sender, stop_receiver) = tokio::sync::watch::channel(());
		let chain_source = Arc::clone(&wallet.chain_source);
		let loop_task = tokio::spawn(async move {
			chain_source.continuously_process_broadcast_queue(stop_receiver).await
		});

		// Watch well past the retry delay: the package from before the stop must not be
		// classified or broadcast by the restarted loop.
		for _ in 0..40 {
			tokio::time::sleep(Duration::from_millis(100)).await;
			assert!(
				wallet.payment_store.list_page(None).await.unwrap().objects.is_empty(),
				"a package from before stop() resurfaced after restart"
			);
		}

		stop_sender.send(()).unwrap();
		loop_task.await.unwrap();
	}

	/// Barrier test, classification-first ordering: wallet sync's confirmation handling must
	/// wait for classification's two-store write pair. Classification is parked between its
	/// payment-store and pending-store writes (the torn window) and only then is the
	/// confirmation of the replacement candidate dispatched; unless the sync arm holds the
	/// cross-store lock from payment-id resolution onwards, it resolves the id against the
	/// still-missing pending index and mints a duplicate record keyed by the event txid.
	#[tokio::test]
	async fn funding_confirmation_waits_for_classification() {
		let gated = NamespaceGatedStore::new(PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE);
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(gated.clone()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;

		let txid1 = Txid::from_byte_array([1u8; 32]);
		let txid2 = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId(txid1.to_byte_array());
		let candidates = vec![
			FundingTxCandidate {
				txid: txid1,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: txid2,
				amount_msat: Some(2_000_000),
				fee_paid_msat: Some(999),
				awaiting_broadcast: false,
			},
		];
		let details = interactive_funding_details(payment_id, txid2, Some(2_000_000), Some(999));

		// Hold the gate so classification parks on its pending-store write: the payment record
		// is persisted, the pending entry is not — the torn window a concurrent confirmation
		// must not observe.
		let gate_guard = gated.gate.write().await;
		let classification = tokio::spawn({
			let wallet = Arc::clone(&wallet);
			let candidates = candidates.clone();
			async move { wallet.persist_funding_payment(details, candidates).await }
		});
		gated.parked.notified().await;

		// Only now dispatch the confirmation of the candidate that won.
		let event = WalletEvent::TxConfirmed {
			txid: txid2,
			tx: Arc::new(dummy_tx()),
			block_time: confirmed_block_time(5),
			old_block_time: None,
		};
		let sync = tokio::spawn({
			let wallet = Arc::clone(&wallet);
			async move { wallet.update_payment_store(vec![event]).await }
		});

		// Liveness sanity only (both pre- and post-fix stall here): while classification is
		// parked, no second record may have been committed.
		tokio::time::sleep(Duration::from_millis(250)).await;
		let payment_keys = KVStore::list(
			&*store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();
		assert!(payment_keys.len() <= 1);

		drop(gate_guard);
		classification.await.unwrap().unwrap();
		sync.await.unwrap().unwrap();

		// Both writers converge on the classified record: the confirmation refreshes it in
		// place with the confirmed candidate's figures rather than minting a second record
		// keyed by the event txid.
		let payment_keys = KVStore::list(
			&*store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();
		assert_eq!(payment_keys.len(), 1, "the confirmation must not mint a duplicate record");
		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.id, payment_id);
		assert_eq!(payment.amount_msat, Some(2_000_000));
		assert_eq!(payment.fee_paid_msat, Some(999));
		match &payment.kind {
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Confirmed { .. },
				tx_type: Some(TransactionType::InteractiveFunding { .. }),
			} => assert_eq!(*txid, txid2),
			kind => panic!("unexpected kind {:?}", kind),
		}
	}

	/// Barrier test, sync-first ordering: classification must wait for wallet sync's complete
	/// decision-plus-write sequence. Wallet sync is parked inside its generic-fallback window —
	/// past the funding-status check that found no record, before its writes — by holding the
	/// BDK wallet lock the fallback needs. Unless the sync arm holds the cross-store lock
	/// across that window, classification lands in between and the fallback's stale merge
	/// overwrites the contribution-derived figures with wallet-derived ones.
	#[tokio::test(flavor = "multi_thread")]
	async fn funding_classification_waits_for_wallet_sync() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;

		let txid = Txid::from_byte_array([3u8; 32]);
		let payment_id = PaymentId(txid.to_byte_array());
		let candidates = vec![FundingTxCandidate {
			txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
			awaiting_broadcast: false,
		}];
		let details = interactive_funding_details(payment_id, txid, Some(1_000_000), Some(500));

		// Park wallet sync inside its fallback window: the TxUnconfirmed arm reads no wallet
		// state before that point, so it passes the funding-status check (no record exists yet)
		// and then blocks on the wallet lock held here. The sleeps give the tasks time to reach
		// their parking spots; they make the pre-fix failure deterministic, while the fixed
		// code converges to the same final state under any arrival order.
		let inner_guard = wallet.inner.lock().unwrap();
		let sync = tokio::spawn({
			let wallet = Arc::clone(&wallet);
			let event =
				WalletEvent::TxUnconfirmed { txid, tx: Arc::new(dummy_tx()), old_block_time: None };
			async move { wallet.update_payment_store(vec![event]).await }
		});
		tokio::time::sleep(Duration::from_millis(250)).await;

		let classification = tokio::spawn({
			let wallet = Arc::clone(&wallet);
			let candidates = candidates.clone();
			async move { wallet.persist_funding_payment(details, candidates).await }
		});
		tokio::time::sleep(Duration::from_millis(250)).await;

		drop(inner_guard);
		sync.await.unwrap().unwrap();
		classification.await.unwrap().unwrap();

		// Both writers converge on one record carrying the classification: the generic
		// fallback must not clobber the contribution-derived figures with its wallet-derived
		// view of the transaction.
		let payment_keys = KVStore::list(
			&*store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();
		assert_eq!(payment_keys.len(), 1);
		let payment = wallet.payment_store.get(&payment_id).await.unwrap().unwrap();
		assert_eq!(payment.id, payment_id);
		assert_eq!(
			payment.amount_msat,
			Some(1_000_000),
			"wallet sync's fallback must not overwrite contribution figures"
		);
		assert_eq!(payment.fee_paid_msat, Some(500));
		assert!(matches!(
			&payment.kind,
			PaymentKind::Onchain { tx_type: Some(TransactionType::InteractiveFunding { .. }), .. }
		));
	}
	/// A previous transaction with a P2WPKH output at index 0 for a contribution input to spend;
	/// `seed` varies the output script, and with it the txid.
	fn test_prevtx(seed: u8) -> Transaction {
		Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![bitcoin::TxIn::default()],
			output: vec![TxOut {
				value: Amount::from_sat(10_000),
				script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([seed; 20])),
			}],
		}
	}

	/// Records `rounds` as their signing did — the last round signed, the others negotiated
	/// before — then marks the signed round as broadcast, as its `SpliceNegotiated` event would.
	/// Returns the record's id.
	async fn record_broadcast_rounds(
		wallet: &Wallet, tx: &Transaction, rounds: &[(Txid, Option<FundingContribution>)],
	) -> PaymentId {
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let candidates = splice_candidates(counterparty_node_id, channel_id, rounds);
		wallet.record_signed_funding(tx, &candidates).await.unwrap();
		wallet.record_broadcast_splice_round(channel_id, tx.compute_txid()).await.unwrap();
		wallet.find_payment_by_txid(tx.compute_txid()).await.unwrap().expect("recorded")
	}

	/// The close finds no round of ours held — the channel closed on a commitment transaction and
	/// the monitor watches the round no longer — so the only round's payment is failed and its
	/// entry removed. The record keeps describing the round.
	#[tokio::test]
	async fn closing_without_a_round_of_ours_held_fails_the_payment() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let id = record_broadcast_rounds(&wallet, &tx, &[(txid, Some(contribution))]).await;

		wallet.resolve_closed_channel_splice_rounds(channel_id, &[]).await.unwrap();

		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(matches!(
			payment.kind,
			PaymentKind::Onchain { txid: recorded, status: ConfirmationStatus::Unconfirmed, .. }
				if recorded == txid
		));
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// LDK promoted a round of ours and discarded the counterparty's round it replaced with the
	/// promotion, so the payment stays as it is, the promotion recorded and the discarded round
	/// still in its history.
	#[tokio::test]
	async fn promoting_a_round_of_ours_keeps_its_payment() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let counterparty_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let rounds = [(counterparty_txid, None), (txid, Some(contribution))];
		let id = record_broadcast_rounds(&wallet, &tx, &rounds).await;

		wallet.resolve_promoted_splice_round(channel_id, txid, Some(&[txid])).await.unwrap();

		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert_eq!(entry.candidates().len(), 2);
		assert_eq!(entry.locked_rounds(), &[txid]);
	}

	/// LDK promoted a sibling this node did not contribute to — the counterparty's round locked on
	/// a channel that stays open, and the channel manager holds it as the funding and no pending
	/// round by the time the event is handled — so no round of ours can confirm anymore and the
	/// payment is failed, although the channel holds a round of the splice. The channel's monitor,
	/// updated only later, may still watch our round; it is not consulted. The record keeps
	/// describing our round.
	#[tokio::test]
	async fn promoting_a_round_not_ours_fails_the_payment() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let counterparty_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let rounds = [(counterparty_txid, None), (txid, Some(contribution))];
		let id = record_broadcast_rounds(&wallet, &tx, &rounds).await;

		wallet
			.resolve_promoted_splice_round(
				channel_id,
				counterparty_txid,
				Some(&[counterparty_txid]),
			)
			.await
			.unwrap();

		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(matches!(
			payment.kind,
			PaymentKind::Onchain { txid: recorded, status: ConfirmationStatus::Unconfirmed, .. }
				if recorded == txid
		));
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// A round of ours nothing had broadcast when the counterparty's round locked — our
	/// signatures were never exchanged — is dropped with the promotion, and its record with it,
	/// rather than failed: no transaction of ours ever existed to fail a payment for.
	#[tokio::test]
	async fn promoting_a_round_drops_a_round_nothing_broadcast() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let counterparty_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(counterparty_txid, None), (txid, Some(contribution))],
		);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		assert!(wallet.payment_store.get(&id).await.unwrap().is_some(), "the round was recorded");

		wallet
			.resolve_promoted_splice_round(
				channel_id,
				counterparty_txid,
				Some(&[counterparty_txid]),
			)
			.await
			.unwrap();

		assert!(wallet.payment_store.get(&id).await.unwrap().is_none());
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// Failing the payment writes the record before it removes the entry; a replay after the
	/// removal was lost finds the record failed already and finishes the removal.
	#[tokio::test]
	async fn promoting_a_round_finishes_a_failure_cut_short() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let counterparty_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let rounds = [(counterparty_txid, None), (txid, Some(contribution))];
		let id = record_broadcast_rounds(&wallet, &tx, &rounds).await;
		wallet
			.payment_store
			.mutate(&id, |existing| {
				let mut update = PaymentDetailsUpdate::new(id);
				update.status = Some(PaymentStatus::Failed);
				let mut updated = existing?.clone();
				updated.update(update).then_some(updated)
			})
			.await
			.unwrap();
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_some());

		wallet
			.resolve_promoted_splice_round(
				channel_id,
				counterparty_txid,
				Some(&[counterparty_txid]),
			)
			.await
			.unwrap();

		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// A promotion reported for a channel the manager no longer lists — the channel closed before
	/// the event was handled — records the round and leaves the payments to the close, which
	/// resolves them by what the monitor holds: nothing of ours here, the promoted round being the
	/// counterparty's, so the payment is failed then. Recording the counterparty's round does not
	/// keep it.
	#[tokio::test]
	async fn promoting_a_round_on_an_unlisted_channel_records_it_alone() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let counterparty_txid = Txid::from_byte_array([0xAA; 32]);
		let (tx, contribution) = splice_out_round(&wallet, 1, 500_000, 300);
		let txid = tx.compute_txid();
		let rounds = [(counterparty_txid, None), (txid, Some(contribution))];
		let id = record_broadcast_rounds(&wallet, &tx, &rounds).await;

		wallet.resolve_promoted_splice_round(channel_id, counterparty_txid, None).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert_eq!(entry.locked_rounds(), &[counterparty_txid]);

		wallet
			.resolve_closed_channel_splice_rounds(channel_id, &[counterparty_txid])
			.await
			.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// A payment whose round LDK promoted before is kept when a later splice's round is promoted
	/// — the round can still confirm, the later one descending from it — while the later round's
	/// payment is kept for the round LDK holds. The close after that keeps both as well.
	#[tokio::test]
	async fn a_later_promotion_keeps_a_payment_whose_round_locked_before() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (first_tx, first) = splice_in_round(&wallet, 1);
		let first_txid = first_tx.compute_txid();
		let first_id =
			record_broadcast_rounds(&wallet, &first_tx, &[(first_txid, Some(first))]).await;
		wallet
			.resolve_promoted_splice_round(channel_id, first_txid, Some(&[first_txid]))
			.await
			.unwrap();

		let (second_tx, second) = splice_in_round(&wallet, 2);
		let second_txid = second_tx.compute_txid();
		let second_id =
			record_broadcast_rounds(&wallet, &second_tx, &[(second_txid, Some(second))]).await;
		wallet
			.resolve_promoted_splice_round(channel_id, second_txid, Some(&[second_txid]))
			.await
			.unwrap();

		for (id, locked) in [(first_id, first_txid), (second_id, second_txid)] {
			let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
			assert_eq!(payment.status, PaymentStatus::Pending);
			let entry =
				wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
			assert_eq!(entry.locked_rounds(), &[locked]);
		}

		wallet.resolve_closed_channel_splice_rounds(channel_id, &[second_txid]).await.unwrap();
		for id in [first_id, second_id] {
			let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
			assert_eq!(payment.status, PaymentStatus::Pending);
			assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_some());
		}
	}

	/// A fee bump nothing broadcast is dropped when the round it was to replace is promoted — the
	/// counterparty's `splice_locked` for the round arrived as the bump was signed — and the
	/// record is handed back to the promoted round, figures included, with the promotion recorded.
	#[tokio::test]
	async fn promoting_a_round_drops_an_abandoned_bump_and_hands_the_record_back() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let (first_tx, first) = splice_out_round(&wallet, 1, 500_000, 300);
		let (bump_tx, bump) = splice_out_round(&wallet, 2, 500_000, 600);
		let (first_txid, bump_txid) = (first_tx.compute_txid(), bump_tx.compute_txid());
		let id =
			record_broadcast_rounds(&wallet, &first_tx, &[(first_txid, Some(first.clone()))]).await;
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record exists");
		let first_figures = (payment.amount_msat, payment.fee_paid_msat);
		let candidates = splice_candidates(
			counterparty_node_id,
			channel_id,
			&[(first_txid, Some(first)), (bump_txid, Some(bump))],
		);
		wallet.record_signed_funding(&bump_tx, &candidates).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record exists");
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid, .. } if txid == bump_txid));
		assert_ne!((payment.amount_msat, payment.fee_paid_msat), first_figures);

		wallet
			.resolve_promoted_splice_round(channel_id, first_txid, Some(&[first_txid]))
			.await
			.unwrap();

		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		assert!(matches!(payment.kind, PaymentKind::Onchain { txid, .. } if txid == first_txid));
		assert_eq!((payment.amount_msat, payment.fee_paid_msat), first_figures);
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert_eq!(entry.candidates().iter().map(|c| c.txid).collect::<Vec<_>>(), vec![first_txid]);
		assert_eq!(entry.locked_rounds(), &[first_txid]);
	}

	/// A zero-conf splice round of ours locked before its transaction confirmed and a later splice
	/// built on it, so at the close the monitor holds the later round as the funding and watches
	/// neither. The promotion LDK reported keeps the payment: the round can still confirm, the
	/// later round descending from it. Reporting the promotion again — a replayed `ChannelReady` —
	/// records it once and keeps the payment, and reporting one for a round no funding payment
	/// holds records nothing and keeps the payment for the round recorded before.
	#[tokio::test]
	async fn closing_keeps_a_payment_whose_round_locked() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (tx, contribution) = splice_in_round(&wallet, 1);
		let txid = tx.compute_txid();
		let id = record_broadcast_rounds(&wallet, &tx, &[(txid, Some(contribution))]).await;
		let later_funding_txid = Txid::from_byte_array([0xF1; 32]);
		for locked in [txid, txid, later_funding_txid] {
			wallet
				.resolve_promoted_splice_round(channel_id, locked, Some(&[locked]))
				.await
				.unwrap();
		}
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert_eq!(entry.locked_rounds(), &[txid]);

		wallet
			.resolve_closed_channel_splice_rounds(channel_id, &[later_funding_txid])
			.await
			.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_some(), "the entry stays");
	}

	/// A promoted round whose `SpliceNegotiated` event is still unhandled when the channel closes
	/// is not taken back as abandoned: LDK broadcast it as the signatures were exchanged, before it
	/// locked.
	#[tokio::test]
	async fn closing_keeps_a_locked_round_whose_negotiation_event_is_unhandled() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (counterparty_node_id, channel_id) = test_counterparty_and_channel();
		let (tx, contribution) = splice_in_round(&wallet, 1);
		let txid = tx.compute_txid();
		let candidates =
			splice_candidates(counterparty_node_id, channel_id, &[(txid, Some(contribution))]);
		wallet.record_signed_funding(&tx, &candidates).await.unwrap();
		let id = wallet.find_payment_by_txid(txid).await.unwrap().expect("id");
		wallet.resolve_promoted_splice_round(channel_id, txid, Some(&[txid])).await.unwrap();

		let later_funding_txid = Txid::from_byte_array([0xF1; 32]);
		wallet
			.resolve_closed_channel_splice_rounds(channel_id, &[later_funding_txid])
			.await
			.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert!(entry.candidate(txid).is_some_and(|round| round.awaiting_broadcast));
	}

	/// A splice-in round spending output 0 of `test_prevtx(seed)`: the contribution as LDK would
	/// negotiate it, its input its only part, and the transaction carrying it, which also pays a
	/// wallet address so the wallet sees movement. Rounds with distinct seeds have distinct parts,
	/// as a fee bump that had to select other inputs has.
	fn splice_in_round(wallet: &Wallet, seed: u8) -> (Transaction, FundingContribution) {
		let prevtx = test_prevtx(seed);
		let contribution = test_funding_contribution_with_parts(
			300,
			253,
			std::slice::from_ref(&prevtx),
			&[],
			None,
		);
		(wallet_paying_tx(wallet, seed), contribution)
	}

	/// Both broadcast rounds of ours were discarded while the channel manager still listed the
	/// channel — the monitor's events reached the handler ahead of the channel's close — and an
	/// event for a listed channel only drops the rounds nothing broadcast, so the payment is left.
	/// The close that follows finds no round of ours the monitor watches and fails it.
	#[tokio::test]
	async fn rounds_discarded_while_the_channel_is_listed_fail_at_close() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (first_tx, first) = splice_in_round(&wallet, 1);
		let (bump_tx, bump) = splice_in_round(&wallet, 2);
		let (first_txid, bump_txid) = (first_tx.compute_txid(), bump_tx.compute_txid());
		let rounds = [(first_txid, Some(first)), (bump_txid, Some(bump))];
		let id = record_broadcast_rounds(&wallet, &bump_tx, &rounds).await;
		let funding_txid = Txid::from_byte_array([0xF0; 32]);
		// The listed channel's pending rounds and funding, as LDK still reports them.
		let held = [first_txid, bump_txid, funding_txid];
		for _ in 0..2 {
			wallet.drop_abandoned_splice_rounds(channel_id, &held).await.unwrap();
		}
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert_eq!(entry.candidates().len(), 2);

		// At the close the monitor has settled on the funding and watches neither round.
		wallet.resolve_closed_channel_splice_rounds(channel_id, &[funding_txid]).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(matches!(
			payment.kind,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(TransactionType::InteractiveFunding { .. }),
			} if txid == bump_txid
		));
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// The close leaves a payment alone while the monitor watches a round of ours in its record:
	/// the round may yet confirm, and wallet sync or the monitor's `DiscardFunding` resolves it.
	#[tokio::test]
	async fn closing_keeps_a_payment_whose_round_the_monitor_watches() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (first_tx, first) = splice_in_round(&wallet, 1);
		let (bump_tx, bump) = splice_in_round(&wallet, 2);
		let (first_txid, bump_txid) = (first_tx.compute_txid(), bump_tx.compute_txid());
		let rounds = [(first_txid, Some(first)), (bump_txid, Some(bump))];
		let id = record_broadcast_rounds(&wallet, &bump_tx, &rounds).await;
		let funding_txid = Txid::from_byte_array([0xF0; 32]);
		wallet
			.resolve_closed_channel_splice_rounds(channel_id, &[funding_txid, bump_txid])
			.await
			.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Pending);
		let entry = wallet.pending_payment_store.get(&id).await.unwrap().expect("the entry stays");
		assert_eq!(entry.candidates().len(), 2);
	}

	/// The close does not touch a payment that no longer waits on an unconfirmed round: one whose
	/// round confirmed keeps its state, and the entry a graduation cut short left behind is left
	/// to the replayed graduation.
	#[tokio::test]
	async fn closing_leaves_a_confirmed_payment_alone() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (tx, contribution) = splice_in_round(&wallet, 1);
		let txid = tx.compute_txid();
		let id = record_broadcast_rounds(&wallet, &tx, &[(txid, Some(contribution))]).await;
		let confirmed = ConfirmationStatus::Confirmed {
			block_hash: bitcoin::BlockHash::all_zeros(),
			height: 100,
			timestamp: 1_700_000_000,
		};
		wallet
			.payment_store
			.mutate(&id, |existing| {
				let mut updated = existing?.clone();
				if let PaymentKind::Onchain { status, .. } = &mut updated.kind {
					*status = confirmed;
				}
				updated.status = PaymentStatus::Succeeded;
				Some(updated)
			})
			.await
			.unwrap();
		wallet.resolve_closed_channel_splice_rounds(channel_id, &[]).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Succeeded);
		assert!(matches!(
			payment.kind,
			PaymentKind::Onchain { status: ConfirmationStatus::Confirmed { .. }, .. }
		));
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_some());
	}

	/// Failing the payment writes the record before it removes the entry; the close replayed after
	/// the removal was lost finds the record failed already and finishes the removal.
	#[tokio::test]
	async fn closing_finishes_a_failure_cut_short() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (tx, contribution) = splice_in_round(&wallet, 1);
		let txid = tx.compute_txid();
		let id = record_broadcast_rounds(&wallet, &tx, &[(txid, Some(contribution))]).await;
		wallet
			.payment_store
			.mutate(&id, |existing| {
				let mut update = PaymentDetailsUpdate::new(id);
				update.status = Some(PaymentStatus::Failed);
				let mut updated = existing?.clone();
				updated.update(update).then_some(updated)
			})
			.await
			.unwrap();
		wallet.resolve_closed_channel_splice_rounds(channel_id, &[]).await.unwrap();
		let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
		assert_eq!(payment.status, PaymentStatus::Failed);
		assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
	}

	/// The close resolves every record of the channel — two splices signed under different
	/// first-candidate ids, as two negotiations from the same coins are — each by the rounds the
	/// monitor holds: nothing of ours here, so both are failed.
	#[tokio::test]
	async fn closing_resolves_every_record_of_the_channel() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let wallet = new_test_wallet(Arc::clone(&store), false).await;
		let (_, channel_id) = test_counterparty_and_channel();
		let (first_tx, contribution) = splice_in_round(&wallet, 1);
		let (second_tx, _) = splice_in_round(&wallet, 2);
		let (first_txid, second_txid) = (first_tx.compute_txid(), second_tx.compute_txid());
		let first_id = record_broadcast_rounds(
			&wallet,
			&first_tx,
			&[(first_txid, Some(contribution.clone()))],
		)
		.await;
		let second_id =
			record_broadcast_rounds(&wallet, &second_tx, &[(second_txid, Some(contribution))])
				.await;
		let funding_txid = Txid::from_byte_array([0xF0; 32]);
		wallet.resolve_closed_channel_splice_rounds(channel_id, &[funding_txid]).await.unwrap();
		for id in [first_id, second_id] {
			let payment = wallet.payment_store.get(&id).await.unwrap().expect("the record stays");
			assert_eq!(payment.status, PaymentStatus::Failed);
			assert!(wallet.pending_payment_store.get(&id).await.unwrap().is_none());
		}
	}
}
