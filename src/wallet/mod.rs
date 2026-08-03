// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
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
	FundingCandidate, TransactionType as LdkTransactionType,
	INCREMENTAL_RELAY_FEE_SAT_PER_1000_WEIGHT,
};
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::{BlockLocator, ClaimId, Listen};
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
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
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
	// (candidate history included) as one consistent unit: classification holds it across its
	// two-store write pair, and wallet sync's event arms hold it from payment-id resolution
	// through their last write. Without it, a confirmation landing between classification's two
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

	pub(crate) fn get_unconfirmed_txids(&self) -> Vec<Txid> {
		self.inner
			.lock()
			.expect("lock")
			.transactions()
			.filter(|t| t.chain_position.is_unconfirmed())
			.map(|t| t.tx_node.txid)
			.collect()
	}

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

					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					if self
						.apply_funding_status_update_locked(
							&guard,
							payment_id,
							txid,
							confirmation_status,
						)
						.await?
					{
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
					let pending_payments: Vec<PendingPaymentDetails> =
						self.pending_payment_store.list_filter(|p| match p.details() {
							// A pre-broadcast splice intent carries no payment yet and cannot graduate.
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
						});

					let mut unconfirmed_outbound_txids: Vec<Txid> = Vec::new();

					for payment in pending_payments {
						// The filter admits only Tracked funding payments.
						let PendingPaymentDetails::Tracked { details, .. } = payment else {
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
							} if details.direction == PaymentDirection::Outbound => {
								unconfirmed_outbound_txids.push(txid);
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

					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					if self
						.apply_funding_status_update_locked(
							&guard,
							payment_id,
							txid,
							ConfirmationStatus::Unconfirmed,
						)
						.await?
					{
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

					let Some(payment_id) = self.find_payment_by_txid(txid) else {
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
					debug_assert!(
						self.payment_store.get(&payment_id).is_some(),
						"Payment {:?} expected in store during WalletEvent::TxReplaced but not found",
						payment_id,
					);
					let payment =
						self.payment_store.get(&payment_id).ok_or(Error::InvalidPaymentId)?;
					self.upsert_pending_payment(payment, conflict_txids).await?;
				},
				WalletEvent::TxDropped { txid, tx } => {
					// See `TxConfirmed`: id resolution and the writes below must not interleave
					// with classification.
					let guard = self.funding_payment_update_lock.lock().await;

					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					if self
						.apply_funding_status_update_locked(
							&guard,
							payment_id,
							txid,
							ConfirmationStatus::Unconfirmed,
						)
						.await?
					{
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
			LdkTransactionType::InteractiveFunding { candidates } => {
				self.classify_interactive_funding(tx, candidates, tx_type.clone().into()).await
			},
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
		// including splices the interactive-funding classification deliberately declined (no
		// local contribution, or a splice-out moving no wallet funds). Recording it here would
		// mint a zero-amount payment that nothing ever confirms. Skip on the wallet-derived
		// amount alone — the condition `classify_interactive_funding` declines on; anything
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

		let payment_id = PaymentId(txid.to_byte_array());

		// A promoted-but-unconfirmed 0conf splice comes back through this generic path re-typed
		// and carrying wallet-view figures; `funding_reclassification_update` declines the
		// downgrade, leaving no trace that a re-broadcast arrived. Log the arrival so tests can
		// observe the traffic. The read cannot go stale: only the broadcast loop writes
		// interactive-funding classifications, and it runs this classification too.
		if let Some(current) = self.payment_store.get(&payment_id) {
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
		self.persist_funding_payment(details, Vec::new()).await?;
		log_debug!(
			self.logger,
			"Recorded channel-funding broadcast {} for channel {}",
			txid,
			channel_id,
		);
		Ok(())
	}

	/// Records an interactive-funding broadcast (splice, or a V2 dual-funded open) as a pending
	/// on-chain payment, tagged with its transaction type. Amount and fee are this node's share,
	/// derived from the active candidate's contributions; broadcasts we didn't contribute to, or
	/// that don't move wallet funds, are left for wallet sync.
	async fn classify_interactive_funding(
		&self, tx: &Transaction, candidates: &[FundingCandidate], tx_type: TransactionType,
	) -> Result<(), Error> {
		// `InteractiveFunding` carries the full negotiated history; the currently-broadcast
		// candidate is the last entry, earlier entries are RBF predecessors.
		let active = match candidates.last() {
			Some(c) => c,
			None => return Ok(()),
		};
		let first = match candidates.first() {
			Some(c) => c,
			None => return Ok(()),
		};

		let txid = tx.compute_txid();
		debug_assert_eq!(active.txid, txid, "broadcast tx must match the active candidate");

		let aggregate = aggregate_local_stakes(active);
		let amount_msat = match aggregate.amount_msat {
			Some(amt) => Some(amt),
			None => {
				log_trace!(
					self.logger,
					"Not recording interactive-funding broadcast {} as a payment: no local contribution",
					txid,
				);
				return Ok(());
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
				"Not recording interactive-funding broadcast {} as a payment: no wallet-level activity",
				txid,
			);
			return Ok(());
		}

		// Anchor the `PaymentId` to the first negotiated candidate so the record stays stable
		// across RBF replacements.
		let payment_id = PaymentId(first.txid.to_byte_array());

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
		self.persist_funding_payment(details, candidate_records).await?;
		log_debug!(
			self.logger,
			"Recorded interactive-funding broadcast {} ({} candidates, {} channels)",
			txid,
			candidates.len(),
			active.channels.len(),
		);
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
	async fn persist_funding_payment(
		&self, details: PaymentDetails, candidates: Vec<FundingTxCandidate>,
	) -> Result<(), Error> {
		// Hold the cross-store lock across both writes so a funding confirmation never observes
		// the record classified but the candidate history it needs still missing.
		let _guard = self.funding_payment_update_lock.lock().await;

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
		self.pending_payment_store
			.mutate(&id, |existing| {
				// The record was written above and payment records are never removed, so absence
				// means the write failed out; fall back to the fresh details.
				let recorded = self.payment_store.get(&id).unwrap_or(details);
				match existing {
					// The inserted entry embeds the post-write record rather than the fresh
					// details, so a confirmation wallet sync already recorded keeps driving
					// graduation.
					None if recorded.status == PaymentStatus::Pending => {
						Some(PendingPaymentDetails::new(recorded, Vec::new(), candidates))
					},
					// The payment already advanced beyond Pending: the graduation path removed
					// the entry and it must not be re-created.
					None => None,
					// The entry predates this classification — wallet sync recorded the
					// transaction before it was classified (its arms and this write pair
					// serialize on the cross-store lock, so nothing lands in between): merge
					// only the classification into the existing entry.
					Some(entry) => {
						let pending_update = PendingPaymentDetailsUpdate {
							id,
							payment_update: Some(update),
							conflicting_txids: None,
							candidates,
							splice_intent: None,
						};
						let mut updated = entry.clone();
						updated.update(pending_update).then_some(updated)
					},
				}
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
		self.pending_payment_store
			.mutate(&id, |existing| {
				// Only `Pending` payments belong in the pending store. Like in
				// [`Self::persist_funding_payment`], the authoritative status is re-read inside
				// the store's critical section, where it cannot go stale against graduation.
				let is_pending = self
					.payment_store
					.get(&id)
					.map_or(payment.status == PaymentStatus::Pending, |recorded| {
						recorded.status == PaymentStatus::Pending
					});
				if !is_pending {
					return None;
				}
				match existing {
					None => {
						Some(PendingPaymentDetails::new(payment, conflicting_txids, Vec::new()))
					},
					// Promote a pre-broadcast splice intent: wallet sync saw the splice
					// transaction before its broadcast-time classification recorded it. Carrying
					// the intent into the `Tracked` record makes the entry visible to txid
					// lookups while the retrier keeps the intent until the splice locks.
					Some(PendingPaymentDetails::PendingSplice { intent, .. }) => {
						Some(PendingPaymentDetails::tracked(
							payment,
							conflicting_txids,
							Vec::new(),
							Some(intent.clone()),
						))
					},
					Some(tracked @ PendingPaymentDetails::Tracked { .. }) => {
						let mut updated = tracked.clone();
						let fresh =
							PendingPaymentDetails::new(payment, conflicting_txids, Vec::new());
						updated.update(fresh.to_update()).then_some(updated)
					},
				}
			})
			.await?;
		Ok(())
	}

	fn find_payment_by_txid(&self, target_txid: Txid) -> Option<PaymentId> {
		let direct_payment_id = PaymentId(target_txid.to_byte_array());
		if self.pending_payment_store.contains_key(&direct_payment_id) {
			return Some(direct_payment_id);
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
			.first()
		{
			return Some(replaced_details.id());
		}

		None
	}

	/// If `payment_id` refers to a classified funding payment, refreshes its confirmation status
	/// and the candidate txid the event refers to, while preserving the contribution-derived
	/// amount/fee and `tx_type` that wallet sync must not recompute from its own view: the wallet's
	/// `sent`/`received` don't capture our contribution to a shared funding output. Returns `true`
	/// when it handled the payment, so the caller skips the default on-chain path. Graduation to
	/// `Succeeded` is left to `ChainTipChanged` after `ANTI_REORG_DELAY`.
	///
	/// The caller must hold [`Self::funding_payment_update_lock`] — from resolving `payment_id`
	/// through its own last write, not just across this call — so that classification's two-store
	/// write pair cannot interleave with the caller's decision sequence. The `_guard` parameter
	/// serves as a reminder of that contract.
	async fn apply_funding_status_update_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, payment_id: PaymentId, event_txid: Txid,
		confirmation_status: ConfirmationStatus,
	) -> Result<bool, Error> {
		// The funding-type gate, the candidate lookup, and the write share the store's mutation
		// lock: against a separate `get`, a classification merging in between would have its
		// `tx_type` and contribution figures clobbered by this stale snapshot.
		let mut handled = None;
		self.payment_store
			.mutate(&payment_id, |existing| {
				let payment = existing?;
				let tx_type = match &payment.kind {
					PaymentKind::Onchain {
						tx_type:
							tx_type @ Some(
								TransactionType::Funding { .. }
								| TransactionType::InteractiveFunding { .. },
							),
						..
					} => tx_type.clone(),
					_ => return None,
				};
				// Report the figures of the candidate that actually confirmed, which need not be
				// the last one broadcast (an earlier, lower-fee candidate may win) and may carry
				// no figures at all (`None`) for a round we didn't contribute to. (`direction` is
				// invariant across a splice's candidates and cannot be changed through the store
				// anyway.)
				let mut target = payment.clone();
				if let Some(pending) = self.pending_payment_store.get(&payment_id) {
					if let Some(candidate) = pending.candidate(event_txid) {
						target.amount_msat = candidate.amount_msat;
						target.fee_paid_msat = candidate.fee_paid_msat;
					}
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
			return Ok(false);
		};
		// Mirror the refreshed confirmation status onto the pending entry: `ChainTipChanged`
		// graduates by reading the pending entry's details, so it must see the new status. This is
		// the same dual-write the default `TxConfirmed` path performs; an empty conflicting-txids
		// list leaves any stored conflicts intact (the update treats absent as "unchanged").
		if payment.status == PaymentStatus::Pending {
			self.upsert_pending_payment(payment, Vec::new()).await?;
		}
		Ok(true)
	}

	#[allow(deprecated)]
	pub(crate) async fn bump_fee_rbf(
		&self, payment_id: PaymentId, fee_rate: Option<FeeRate>, cur_anchor_reserve_sats: u64,
	) -> Result<Txid, Error> {
		let payment = self.payment_store.get(&payment_id).ok_or_else(|| {
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
/// sole caller, [`Wallet::persist_funding_payment`], builds and applies the update within one
/// closure — so the candidate choice cannot go stale against a concurrent confirmation before the
/// update lands. [`PaymentDetails::update`]'s confirmed-figures rule still arbitrates which
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

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::time::Duration;

	use bdk_chain::{BlockId, ConfirmationBlockTime};
	use bdk_wallet::Wallet as BdkWallet;
	use bitcoin::hashes::Hash;
	use bitcoin::Network;
	use lightning::io;
	use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};

	use super::*;
	use crate::config::EsploraSyncConfig;
	use crate::io::test_utils::InMemoryStore;
	use crate::io::{
		BDK_WALLET_ADDRESS_POOL_KEY, BDK_WALLET_ADDRESS_POOL_PRIMARY_NAMESPACE,
		BDK_WALLET_ADDRESS_POOL_SECONDARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use crate::types::{DynStore, DynStoreWrapper};
	use crate::{NodeMetrics, PersistedNodeMetrics};

	const EXTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/0/*)";
	const INTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPdy6LMhUtFHAgpocR8GC6QmwMSFpZs7h6Eziw3SpThFfczTDh5rW2krkqffa11UpX3XkeTTB2FvzZKWXqPY54Y6Rq4AQ5R8L/84'/1'/0'/1/*)";

	/// An in-memory store whose writes can be made to fail on demand.
	#[derive(Clone)]
	struct FailSwitchStore {
		inner: Arc<InMemoryStore>,
		fail_writes: Arc<AtomicBool>,
	}

	impl FailSwitchStore {
		fn new() -> Self {
			Self {
				inner: Arc::new(InMemoryStore::new()),
				fail_writes: Arc::new(AtomicBool::new(false)),
			}
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
			let primary_namespace = primary_namespace.to_string();
			let secondary_namespace = secondary_namespace.to_string();
			let key = key.to_string();
			async move {
				if fail_writes.load(Ordering::Acquire) {
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
		let payment_store = Arc::new(PaymentStore::new(
			Vec::new(),
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
			Arc::clone(&store),
			Arc::clone(&logger),
		));
		let pending_payment_store = Arc::new(PendingPaymentStore::new(
			Vec::new(),
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

	#[test]
	fn funding_reclassification_update_substitutes_the_confirmed_candidate() {
		let confirmed_txid = Txid::from_byte_array([1u8; 32]);
		let active_txid = Txid::from_byte_array([2u8; 32]);
		let candidates = vec![
			FundingTxCandidate {
				txid: confirmed_txid,
				amount_msat: Some(2_000_000),
				fee_paid_msat: Some(999),
			},
			FundingTxCandidate {
				txid: active_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
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

		let payment = wallet.payment_store.get(&payment_id).unwrap();
		assert_eq!(payment.status, PaymentStatus::Succeeded);
		assert_eq!(
			payment.amount_msat,
			Some(2_000_000),
			"graduation must not roll figures back to the snapshot's"
		);
		assert_eq!(payment.fee_paid_msat, Some(999));
		assert!(payment.latest_update_timestamp > 0, "the graduation write must timestamp");
		assert!(wallet.pending_payment_store.get(&payment_id).is_none());
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

		let payment = wallet.payment_store.get(&payment_id).unwrap();
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
			wallet.pending_payment_store.get(&payment_id).is_some(),
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
			},
			FundingTxCandidate {
				txid: txid2,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(600),
			},
			FundingTxCandidate {
				txid: txid3,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(700),
			},
		];
		let details = interactive_funding_details(payment_id, txid3, Some(1_000_000), Some(700));
		let entry = PendingPaymentDetails::new(details, Vec::new(), candidates);
		wallet.pending_payment_store.insert_or_update(entry).await.unwrap();

		// The first candidate resolves via the txid-derived id and the active candidate via the
		// record's current txid; the middle one must resolve through the candidate history.
		assert_eq!(wallet.find_payment_by_txid(txid1), Some(payment_id));
		assert_eq!(wallet.find_payment_by_txid(txid3), Some(payment_id));
		assert_eq!(wallet.find_payment_by_txid(txid2), Some(payment_id));
	}

	/// A funding-typed broadcast that doesn't touch the on-chain wallet must not be recorded.
	/// LDK re-broadcasts a promoted-but-unconfirmed 0conf splice through its generic funding
	/// path, so a splice the interactive-funding classification deliberately declined — no local
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
		assert!(wallet.payment_store.list_filter(|_| true).is_empty());
		assert!(wallet.pending_payment_store.list_filter(|_| true).is_empty());

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
		assert!(wallet.payment_store.list_filter(|_| true).is_empty());

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
		let payments = wallet.payment_store.list_filter(|_| true);
		assert_eq!(payments.len(), 1);
		assert_eq!(payments[0].id, PaymentId(funded_tx.compute_txid().to_byte_array()));
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
		}];
		let details = interactive_funding_details(payment_id, txid, Some(1_000_000), Some(500));
		wallet.persist_funding_payment(details, candidates).await.unwrap();

		let counterparty_node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let channels = vec![(counterparty_node_id, ChannelId([7u8; 32]))];
		let tx_type = TransactionType::Funding { channels: vec![] };

		let assert_unchanged = |confirmed: bool| {
			let payments = wallet.payment_store.list_filter(|_| true);
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
		};

		wallet.classify_funding(&tx, &channels, tx_type.clone()).await.unwrap();
		assert_unchanged(false);

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
		assert_unchanged(true);
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
		let wallet = new_test_wallet(store, false).await;

		let txid1 = Txid::from_byte_array([1u8; 32]);
		let txid2 = Txid::from_byte_array([2u8; 32]);
		let payment_id = PaymentId(txid1.to_byte_array());
		let candidates = vec![
			FundingTxCandidate {
				txid: txid1,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
			},
			FundingTxCandidate {
				txid: txid2,
				amount_msat: Some(2_000_000),
				fee_paid_msat: Some(999),
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
		assert!(wallet.payment_store.list_filter(|_| true).len() <= 1);

		drop(gate_guard);
		classification.await.unwrap().unwrap();
		sync.await.unwrap().unwrap();

		// Both writers converge on the classified record: the confirmation refreshes it in
		// place with the confirmed candidate's figures rather than minting a second record
		// keyed by the event txid.
		let payments = wallet.payment_store.list_filter(|_| true);
		assert_eq!(payments.len(), 1, "the confirmation must not mint a duplicate record");
		let payment = &payments[0];
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
		let wallet = new_test_wallet(store, false).await;

		let txid = Txid::from_byte_array([3u8; 32]);
		let payment_id = PaymentId(txid.to_byte_array());
		let candidates = vec![FundingTxCandidate {
			txid,
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(500),
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
		let payments = wallet.payment_store.list_filter(|_| true);
		assert_eq!(payments.len(), 1);
		let payment = &payments[0];
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
}
