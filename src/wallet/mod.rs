// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_wallet::descriptor::ExtendedDescriptor;
use bdk_wallet::event::WalletEvent;
#[allow(deprecated)]
use bdk_wallet::SignOptions;
use bdk_wallet::{Balance, KeychainKind, PersistedWallet, Update};
use bitcoin::address::NetworkUnchecked;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::{self, Psbt};
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{All, PublicKey, Scalar, Secp256k1, SecretKey};
use bitcoin::{
	Address, Amount, FeeRate, OutPoint, ScriptBuf, Transaction, TxOut, Txid, WPubkeyHash, Weight,
	WitnessProgram, WitnessVersion,
};
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::{BestBlock, Listen};
use lightning::events::bump_transaction::{Input, Utxo, WalletSource};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingTxInput;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::ln::script::ShutdownScript;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeysManager, NodeSigner, OutputSpender,
	PeerStorageKey, Recipient, SignerProvider, SpendableOutputDescriptor,
};
use lightning::util::message_signing;
use lightning_invoice::RawBolt11Invoice;
use persist::KVStoreWalletPersister;

use crate::config::Config;
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::payment::store::ConfirmationStatus;
use crate::payment::{
	PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus, PendingPaymentDetails,
};
use crate::types::{Broadcaster, PaymentStore, PendingPaymentStore};
use crate::{ChainSource, Error};

pub(crate) enum OnchainSendAmount {
	ExactRetainingReserve { amount_sats: u64, cur_anchor_reserve_sats: u64 },
	AllRetainingReserve { cur_anchor_reserve_sats: u64 },
	AllDrainingReserve,
}

pub(crate) mod persist;
pub(crate) mod ser;

const DUST_LIMIT_SATS: u64 = 546;

pub(crate) struct Wallet {
	// A BDK on-chain wallet.
	inner: Mutex<PersistedWallet<KVStoreWalletPersister>>,
	persister: Mutex<KVStoreWalletPersister>,
	broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	chain_source: Arc<ChainSource>,
	payment_store: Arc<PaymentStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	pending_payment_store: Arc<PendingPaymentStore>,
}

impl Wallet {
	pub(crate) fn new(
		wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		wallet_persister: KVStoreWalletPersister, broadcaster: Arc<Broadcaster>,
		fee_estimator: Arc<OnchainFeeEstimator>, chain_source: Arc<ChainSource>,
		payment_store: Arc<PaymentStore>, config: Arc<Config>, logger: Arc<Logger>,
		pending_payment_store: Arc<PendingPaymentStore>,
	) -> Self {
		let inner = Mutex::new(wallet);
		let persister = Mutex::new(wallet_persister);
		Self {
			inner,
			persister,
			broadcaster,
			fee_estimator,
			chain_source,
			payment_store,
			config,
			logger,
			pending_payment_store,
		}
	}

	pub(crate) fn get_full_scan_request(&self) -> FullScanRequest<KeychainKind> {
		self.inner.lock().unwrap().start_full_scan().build()
	}

	pub(crate) fn get_incremental_sync_request(&self) -> SyncRequest<(KeychainKind, u32)> {
		self.inner.lock().unwrap().start_sync_with_revealed_spks().build()
	}

	pub(crate) fn get_cached_txs(&self) -> Vec<Arc<Transaction>> {
		self.inner.lock().unwrap().tx_graph().full_txs().map(|tx_node| tx_node.tx).collect()
	}

	pub(crate) fn get_unconfirmed_txids(&self) -> Vec<Txid> {
		self.inner
			.lock()
			.unwrap()
			.transactions()
			.filter(|t| t.chain_position.is_unconfirmed())
			.map(|t| t.tx_node.txid)
			.collect()
	}

	pub(crate) fn current_best_block(&self) -> BestBlock {
		let checkpoint = self.inner.lock().unwrap().latest_checkpoint();
		BestBlock { block_hash: checkpoint.hash(), height: checkpoint.height() }
	}

	pub(crate) fn apply_update(&self, update: impl Into<Update>) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		match locked_wallet.apply_update_events(update) {
			Ok(events) => {
				self.update_payment_store(&mut *locked_wallet, events).map_err(|e| {
					log_error!(self.logger, "Failed to update payment store: {}", e);
					Error::PersistenceFailed
				})?;

				let mut locked_persister = self.persister.lock().unwrap();
				locked_wallet.persist(&mut locked_persister).map_err(|e| {
					log_error!(self.logger, "Failed to persist wallet: {}", e);
					Error::PersistenceFailed
				})?;

				Ok(())
			},
			Err(e) => {
				log_error!(self.logger, "Sync failed due to chain connection error: {}", e);
				Err(Error::WalletOperationFailed)
			},
		}
	}

	pub(crate) fn apply_mempool_txs(
		&self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		if unconfirmed_txs.is_empty() && evicted_txids.is_empty() {
			return Ok(());
		}

		let mut locked_wallet = self.inner.lock().unwrap();

		let chain_tip1 = locked_wallet.latest_checkpoint().block_id();
		let wallet_txs1 = locked_wallet
			.transactions()
			.map(|wtx| (wtx.tx_node.txid, (wtx.tx_node.tx.clone(), wtx.chain_position)))
			.collect::<std::collections::BTreeMap<
				Txid,
				(Arc<Transaction>, bdk_chain::ChainPosition<bdk_chain::ConfirmationBlockTime>),
			>>();

		locked_wallet.apply_unconfirmed_txs(unconfirmed_txs);
		locked_wallet.apply_evicted_txs(evicted_txids);

		let chain_tip2 = locked_wallet.latest_checkpoint().block_id();
		let wallet_txs2 = locked_wallet
			.transactions()
			.map(|wtx| (wtx.tx_node.txid, (wtx.tx_node.tx.clone(), wtx.chain_position)))
			.collect::<std::collections::BTreeMap<
				Txid,
				(Arc<Transaction>, bdk_chain::ChainPosition<bdk_chain::ConfirmationBlockTime>),
			>>();

		let events =
			wallet_events(&mut *locked_wallet, chain_tip1, chain_tip2, wallet_txs1, wallet_txs2);

		self.update_payment_store(&mut *locked_wallet, events).map_err(|e| {
			log_error!(self.logger, "Failed to update payment store: {}", e);
			Error::PersistenceFailed
		})?;

		let mut locked_persister = self.persister.lock().unwrap();
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		Ok(())
	}

	fn update_payment_store<'a>(
		&self, locked_wallet: &'a mut PersistedWallet<KVStoreWalletPersister>,
		mut events: Vec<WalletEvent>,
	) -> Result<(), Error> {
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
					let cur_height = locked_wallet.latest_checkpoint().height();
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

					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					let payment = self.create_payment_from_tx(
						locked_wallet,
						txid,
						payment_id,
						&tx,
						payment_status,
						confirmation_status,
					);

					let pending_payment =
						self.create_pending_payment_from_tx(payment.clone(), Vec::new());

					self.payment_store.insert_or_update(payment)?;
					self.pending_payment_store.insert_or_update(pending_payment)?;
				},
				WalletEvent::ChainTipChanged { new_tip, .. } => {
					// Get all payments that are Pending with Confirmed status
					let pending_payments: Vec<PendingPaymentDetails> =
						self.pending_payment_store.list_filter(|p| {
							p.details.status == PaymentStatus::Pending
								&& matches!(
									p.details.kind,
									PaymentKind::Onchain {
										status: ConfirmationStatus::Confirmed { .. },
										..
									}
								)
						});

					for mut payment in pending_payments {
						if let PaymentKind::Onchain {
							status: ConfirmationStatus::Confirmed { height, .. },
							..
						} = payment.details.kind
						{
							let payment_id = payment.details.id;
							if new_tip.height >= height + ANTI_REORG_DELAY - 1 {
								payment.details.status = PaymentStatus::Succeeded;
								self.payment_store.insert_or_update(payment.details)?;
								self.pending_payment_store.remove(&payment_id)?;
							}
						}
					}
				},
				WalletEvent::TxUnconfirmed { txid, tx, old_block_time: None } => {
					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					let payment = self.create_payment_from_tx(
						locked_wallet,
						txid,
						payment_id,
						&tx,
						PaymentStatus::Pending,
						ConfirmationStatus::Unconfirmed,
					);
					let pending_payment =
						self.create_pending_payment_from_tx(payment.clone(), Vec::new());
					self.payment_store.insert_or_update(payment)?;
					self.pending_payment_store.insert_or_update(pending_payment)?;
				},
				WalletEvent::TxReplaced { txid, conflicts, tx, .. } => {
					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));

					// Collect all conflict txids
					let conflict_txids: Vec<Txid> =
						conflicts.iter().map(|(_, conflict_txid)| *conflict_txid).collect();

					let payment = self.create_payment_from_tx(
						locked_wallet,
						txid,
						payment_id,
						&tx,
						PaymentStatus::Pending,
						ConfirmationStatus::Unconfirmed,
					);
					let pending_payment_details = self
						.create_pending_payment_from_tx(payment.clone(), conflict_txids.clone());

					self.pending_payment_store.insert_or_update(pending_payment_details)?;
				},
				WalletEvent::TxDropped { txid, tx } => {
					let payment_id = self
						.find_payment_by_txid(txid)
						.unwrap_or_else(|| PaymentId(txid.to_byte_array()));
					let payment = self.create_payment_from_tx(
						locked_wallet,
						txid,
						payment_id,
						&tx,
						PaymentStatus::Pending,
						ConfirmationStatus::Unconfirmed,
					);
					let pending_payment =
						self.create_pending_payment_from_tx(payment.clone(), Vec::new());
					self.payment_store.insert_or_update(payment)?;
					self.pending_payment_store.insert_or_update(pending_payment)?;
				},
				_ => {
					continue;
				},
			};
		}

		Ok(())
	}

	#[allow(deprecated)]
	pub(crate) fn create_funding_transaction(
		&self, output_script: ScriptBuf, amount: Amount, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);

		let mut locked_wallet = self.inner.lock().unwrap();
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

		let mut locked_persister = self.persister.lock().unwrap();
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			e
		})?;

		Ok(tx)
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let mut locked_persister = self.persister.lock().unwrap();

		let address_info = locked_wallet.reveal_next_address(KeychainKind::External);
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
	}

	pub(crate) fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let mut locked_persister = self.persister.lock().unwrap();

		let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
	}

	pub(crate) fn cancel_tx(&self, tx: &Transaction) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let mut locked_persister = self.persister.lock().unwrap();

		locked_wallet.cancel_tx(tx);
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		Ok(())
	}

	pub(crate) fn get_balances(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let balance = self.inner.lock().unwrap().balance();

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
			let weight = Weight::from_wu(input.satisfaction_weight);
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
		let mut locked_wallet = self.inner.lock().unwrap();

		// Use a dummy P2WSH script (34 bytes) to match the size of a real funding output.
		let dummy_p2wsh_script = ScriptBuf::new().to_p2wsh();

		let (max_amount, tmp_psbt) = self.get_max_drain_amount(
			&mut locked_wallet,
			dummy_p2wsh_script,
			cur_anchor_reserve_sats,
			fee_rate,
			None,
		)?;

		locked_wallet.cancel_tx(&tmp_psbt.unsigned_tx);

		Ok(max_amount)
	}

	/// Returns the maximum amount available for splicing into an existing channel, accounting for
	/// on-chain fees and anchor reserves, along with the wallet UTXOs to use as inputs.
	pub(crate) fn get_max_splice_in_amount(
		&self, shared_input: Input, shared_output_script: ScriptBuf, cur_anchor_reserve_sats: u64,
		fee_rate: FeeRate,
	) -> Result<(u64, Vec<FundingTxInput>), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();

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

		let inputs = tmp_psbt
			.unsigned_tx
			.input
			.iter()
			.filter(|txin| txin.previous_output != shared_input.outpoint)
			.filter_map(|txin| {
				locked_wallet
					.tx_details(txin.previous_output.txid)
					.map(|tx_details| tx_details.tx.deref().clone())
					.map(|prevtx| FundingTxInput::new_p2wpkh(prevtx, txin.previous_output.vout))
			})
			.collect::<Result<Vec<_>, ()>>()
			.map_err(|_| {
				log_error!(self.logger, "Failed to collect wallet UTXOs for splice");
				Error::ChannelSplicingFailed
			})?;

		locked_wallet.cancel_tx(&tmp_psbt.unsigned_tx);

		Ok((splice_amount, inputs))
	}

	pub(crate) fn parse_and_validate_address(&self, address: &Address) -> Result<Address, Error> {
		Address::<NetworkUnchecked>::from_str(address.to_string().as_str())
			.map_err(|_| Error::InvalidAddress)?
			.require_network(self.config.network)
			.map_err(|_| Error::InvalidAddress)
	}

	#[allow(deprecated)]
	pub(crate) fn send_to_address(
		&self, address: &bitcoin::Address, send_amount: OnchainSendAmount,
		fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let tx = {
			let mut locked_wallet = self.inner.lock().unwrap();

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

					locked_wallet.cancel_tx(&tmp_psbt.unsigned_tx);

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

			let mut locked_persister = self.persister.lock().unwrap();
			locked_wallet.persist(&mut locked_persister).map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;

			psbt.extract_tx().map_err(|e| {
				log_error!(self.logger, "Failed to extract transaction: {}", e);
				e
			})?
		};

		self.broadcaster.broadcast_transactions(&[(
			&tx,
			lightning::chain::chaininterface::TransactionType::Sweep { channels: vec![] },
		)]);

		let txid = tx.compute_txid();

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

	pub(crate) fn select_confirmed_utxos(
		&self, must_spend: Vec<Input>, must_pay_to: &[TxOut], fee_rate: FeeRate,
	) -> Result<Vec<FundingTxInput>, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
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
			let weight = Weight::from_wu(input.satisfaction_weight);
			tx_builder.add_foreign_utxo(input.outpoint, psbt_input, weight).map_err(|_| ())?;
		}

		for output in must_pay_to {
			tx_builder.add_recipient(output.script_pubkey.clone(), output.value);
		}

		tx_builder.fee_rate(fee_rate);
		tx_builder.exclude_unconfirmed();

		tx_builder
			.finish()
			.map_err(|e| {
				log_error!(self.logger, "Failed to select confirmed UTXOs: {}", e);
			})?
			.unsigned_tx
			.input
			.iter()
			.filter(|txin| must_spend.iter().all(|input| input.outpoint != txin.previous_output))
			.filter_map(|txin| {
				locked_wallet
					.tx_details(txin.previous_output.txid)
					.map(|tx_details| tx_details.tx.deref().clone())
					.map(|prevtx| FundingTxInput::new_p2wpkh(prevtx, txin.previous_output.vout))
			})
			.collect::<Result<Vec<_>, ()>>()
	}

	fn list_confirmed_utxos_inner(&self) -> Result<Vec<Utxo>, ()> {
		let locked_wallet = self.inner.lock().unwrap();
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
	fn get_change_script_inner(&self) -> Result<ScriptBuf, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let mut locked_persister = self.persister.lock().unwrap();

		let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			()
		})?;
		Ok(address_info.address.script_pubkey())
	}

	#[allow(deprecated)]
	pub(crate) fn sign_owned_inputs(&self, unsigned_tx: Transaction) -> Result<Transaction, ()> {
		let locked_wallet = self.inner.lock().unwrap();

		let mut psbt = Psbt::from_unsigned_tx(unsigned_tx).map_err(|e| {
			log_error!(self.logger, "Failed to construct PSBT: {}", e);
		})?;
		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			if let Some(utxo) = locked_wallet.get_utxo(txin.previous_output) {
				debug_assert!(!utxo.is_spent);
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
		let locked_wallet = self.inner.lock().unwrap();

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

		let kind = PaymentKind::Onchain { txid, status: confirmation_status };

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

		PaymentDetails::new(
			payment_id,
			kind,
			amount_msat,
			Some(fee_sat * 1000),
			direction,
			payment_status,
		)
	}

	fn create_pending_payment_from_tx(
		&self, payment: PaymentDetails, conflicting_txids: Vec<Txid>,
	) -> PendingPaymentDetails {
		PendingPaymentDetails::new(payment, conflicting_txids)
	}

	fn find_payment_by_txid(&self, target_txid: Txid) -> Option<PaymentId> {
		let direct_payment_id = PaymentId(target_txid.to_byte_array());
		if self.pending_payment_store.contains_key(&direct_payment_id) {
			return Some(direct_payment_id);
		}

		if let Some(replaced_details) = self
			.pending_payment_store
			.list_filter(|p| p.conflicting_txids.contains(&target_txid))
			.first()
		{
			return Some(replaced_details.details.id);
		}

		None
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
		let mut locked_wallet = self.inner.lock().unwrap();

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
			Ok(events) => {
				if let Err(e) = self.update_payment_store(&mut *locked_wallet, events) {
					log_error!(self.logger, "Failed to update payment store: {}", e);
					return;
				}
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to apply connected block to on-chain wallet: {}",
					e
				);
				return;
			},
		};

		let mut locked_persister = self.persister.lock().unwrap();
		match locked_wallet.persist(&mut locked_persister) {
			Ok(_) => (),
			Err(e) => {
				log_error!(self.logger, "Failed to persist on-chain wallet: {}", e);
				return;
			},
		};
	}

	fn blocks_disconnected(&self, _fork_point_block: BestBlock) {
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
		async move { self.get_change_script_inner() }
	}

	fn sign_psbt<'a>(
		&'a self, psbt: Psbt,
	) -> impl Future<Output = Result<Transaction, ()>> + Send + 'a {
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
		let address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		let address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
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
				.map_err(|e| {
					log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
				})
				.map(|addr| addr.script_pubkey())
				.map_err(|_| ())
		}
	}
}

// FIXME/TODO: This is copied-over from bdk_wallet and only used to generate `WalletEvent`s after
// applying mempool transactions. We should drop this when BDK offers to generate events for
// mempool transactions natively.
pub(crate) fn wallet_events(
	wallet: &mut bdk_wallet::Wallet, chain_tip1: bdk_chain::BlockId,
	chain_tip2: bdk_chain::BlockId,
	wallet_txs1: std::collections::BTreeMap<
		Txid,
		(Arc<Transaction>, bdk_chain::ChainPosition<bdk_chain::ConfirmationBlockTime>),
	>,
	wallet_txs2: std::collections::BTreeMap<
		Txid,
		(Arc<Transaction>, bdk_chain::ChainPosition<bdk_chain::ConfirmationBlockTime>),
	>,
) -> Vec<WalletEvent> {
	let mut events: Vec<WalletEvent> = Vec::new();

	if chain_tip1 != chain_tip2 {
		events.push(WalletEvent::ChainTipChanged { old_tip: chain_tip1, new_tip: chain_tip2 });
	}

	wallet_txs2.iter().for_each(|(txid2, (tx2, cp2))| {
		if let Some((tx1, cp1)) = wallet_txs1.get(txid2) {
			assert_eq!(tx1.compute_txid(), *txid2);
			match (cp1, cp2) {
				(
					bdk_chain::ChainPosition::Unconfirmed { .. },
					bdk_chain::ChainPosition::Confirmed { anchor, .. },
				) => {
					events.push(WalletEvent::TxConfirmed {
						txid: *txid2,
						tx: tx2.clone(),
						block_time: *anchor,
						old_block_time: None,
					});
				},
				(
					bdk_chain::ChainPosition::Confirmed { anchor, .. },
					bdk_chain::ChainPosition::Unconfirmed { .. },
				) => {
					events.push(WalletEvent::TxUnconfirmed {
						txid: *txid2,
						tx: tx2.clone(),
						old_block_time: Some(*anchor),
					});
				},
				(
					bdk_chain::ChainPosition::Confirmed { anchor: anchor1, .. },
					bdk_chain::ChainPosition::Confirmed { anchor: anchor2, .. },
				) => {
					if *anchor1 != *anchor2 {
						events.push(WalletEvent::TxConfirmed {
							txid: *txid2,
							tx: tx2.clone(),
							block_time: *anchor2,
							old_block_time: Some(*anchor1),
						});
					}
				},
				(
					bdk_chain::ChainPosition::Unconfirmed { .. },
					bdk_chain::ChainPosition::Unconfirmed { .. },
				) => {
					// do nothing if still unconfirmed
				},
			}
		} else {
			match cp2 {
				bdk_chain::ChainPosition::Confirmed { anchor, .. } => {
					events.push(WalletEvent::TxConfirmed {
						txid: *txid2,
						tx: tx2.clone(),
						block_time: *anchor,
						old_block_time: None,
					});
				},
				bdk_chain::ChainPosition::Unconfirmed { .. } => {
					events.push(WalletEvent::TxUnconfirmed {
						txid: *txid2,
						tx: tx2.clone(),
						old_block_time: None,
					});
				},
			}
		}
	});

	// find tx that are no longer canonical
	wallet_txs1.iter().for_each(|(txid1, (tx1, _))| {
		if !wallet_txs2.contains_key(txid1) {
			let conflicts = wallet.tx_graph().direct_conflicts(tx1).collect::<Vec<_>>();
			if !conflicts.is_empty() {
				events.push(WalletEvent::TxReplaced { txid: *txid1, tx: tx1.clone(), conflicts });
			} else {
				events.push(WalletEvent::TxDropped { txid: *txid1, tx: tx1.clone() });
			}
		}
	});

	events
}
