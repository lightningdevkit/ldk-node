// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use persist::KVStoreWalletPersister;

use crate::config::Config;
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger};

use crate::fee_estimator::{ConfirmationTarget, FeeEstimator};
use crate::payment::store::ConfirmationStatus;
use crate::payment::{PaymentDetails, PaymentDirection, PaymentStatus};
use crate::types::{ChannelManager, PaymentStore};
use crate::Error;

use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::{BestBlock, Listen};

use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeysManager, NodeSigner, OutputSpender,
	Recipient, SignerProvider, SpendableOutputDescriptor,
};

use lightning::util::message_signing;
use lightning_invoice::RawBolt11Invoice;

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_wallet::{Balance, KeychainKind, PersistedWallet, SignOptions, Update};

use bitcoin::address::NetworkUnchecked;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Signing};
use bitcoin::{
	Address, Amount, FeeRate, Network, ScriptBuf, Transaction, TxOut, Txid, WPubkeyHash,
	WitnessProgram, WitnessVersion,
};

use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

pub(crate) enum OnchainSendAmount {
	ExactRetainingReserve { amount_sats: u64, cur_anchor_reserve_sats: u64 },
	AllRetainingReserve { cur_anchor_reserve_sats: u64 },
	AllDrainingReserve,
}

pub(crate) mod persist;
pub(crate) mod ser;

pub(crate) struct Wallet<B: Deref, E: Deref, L: Deref>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	// A BDK on-chain wallet.
	inner: Mutex<PersistedWallet<KVStoreWalletPersister>>,
	persister: Mutex<KVStoreWalletPersister>,
	broadcaster: B,
	fee_estimator: E,
	payment_store: Arc<PaymentStore>,
	config: Arc<Config>,
	logger: L,
}

impl<B: Deref, E: Deref, L: Deref> Wallet<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		wallet_persister: KVStoreWalletPersister, broadcaster: B, fee_estimator: E,
		payment_store: Arc<PaymentStore>, config: Arc<Config>, logger: L,
	) -> Self {
		let inner = Mutex::new(wallet);
		let persister = Mutex::new(wallet_persister);
		Self { inner, persister, broadcaster, fee_estimator, payment_store, config, logger }
	}

	pub(crate) fn is_funding_transaction(
		&self,
		txid: &Txid,
		channel_manager: &ChannelManager
	) -> bool {
		// Check all channels (pending and confirmed) for matching funding txid
		for channel in channel_manager.list_channels() {
			if let Some(funding_txo) = channel.funding_txo {
				if funding_txo.txid == *txid {
					log_debug!(
						self.logger,
						"Transaction {} is a funding transaction for channel {}",
						txid,
						channel.channel_id
					);
					return true;
				}
			}
		}
		false
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

	pub(crate) fn current_best_block(&self) -> BestBlock {
		let checkpoint = self.inner.lock().unwrap().latest_checkpoint();
		BestBlock { block_hash: checkpoint.hash(), height: checkpoint.height() }
	}

	pub(crate) fn apply_update(&self, update: impl Into<Update>) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		match locked_wallet.apply_update(update) {
			Ok(()) => {
				let mut locked_persister = self.persister.lock().unwrap();
				locked_wallet.persist(&mut locked_persister).map_err(|e| {
					log_error!(self.logger, "Failed to persist wallet: {}", e);
					Error::PersistenceFailed
				})?;

				self.update_payment_store(&mut *locked_wallet).map_err(|e| {
					log_error!(self.logger, "Failed to update payment store: {}", e);
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

	pub(crate) fn apply_unconfirmed_txs(
		&self, unconfirmed_txs: Vec<(Transaction, u64)>,
	) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.apply_unconfirmed_txs(unconfirmed_txs);

		let mut locked_persister = self.persister.lock().unwrap();
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		Ok(())
	}

	/// Bumps the fee of an existing transaction using Replace-By-Fee (RBF).
	///
	/// This allows a previously sent transaction to be replaced with a new version
	/// that pays a higher fee. The original transaction must have been created with
	/// RBF enabled (which is the default for transactions created by LDK).
	///
	/// Returns the txid of the new transaction if successful.
	pub(crate) fn bump_fee_by_rbf(
		&self,
		txid: &Txid,
		fee_rate: FeeRate,
		channel_manager: &ChannelManager,
	) -> Result<Txid, Error> {
		// Check if this is a funding transaction
		if self.is_funding_transaction(txid, channel_manager) {
			log_error!(
				self.logger,
				"Cannot RBF transaction {}: it is a channel funding transaction",
				txid
			);
			return Err(Error::CannotRbfFundingTransaction);
		}
		let mut locked_wallet = self.inner.lock().unwrap();

		// Find the transaction in the wallet
		let tx_node = locked_wallet
			.get_tx(*txid)
			.ok_or_else(|| {
				log_error!(self.logger, "Transaction not found in wallet: {}", txid);
				Error::TransactionNotFound
			})?;

		// Check if transaction is confirmed - can't replace confirmed transactions
		if tx_node.chain_position.is_confirmed() {
			log_error!(self.logger, "Cannot replace confirmed transaction: {}", txid);
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Calculate original transaction fee and fee rate
		let original_tx = &tx_node.tx_node.tx;
		let original_fee = locked_wallet.calculate_fee(original_tx).map_err(|e| {
			log_error!(self.logger, "Failed to calculate original fee: {}", e);
			Error::WalletOperationFailed
		})?;

		// Use Bitcoin crate's built-in fee rate calculation for accuracy
		let original_fee_rate = original_fee / original_tx.weight();

		// Log detailed information for debugging
		log_info!(self.logger, "RBF Analysis for transaction {}", txid);
		log_info!(self.logger, "  Original fee: {} sats", original_fee.to_sat());
		log_info!(self.logger, "  Original weight: {} WU ({} vB)",
			original_tx.weight().to_wu(), original_tx.weight().to_vbytes_ceil());
		log_info!(self.logger, "  Original fee rate: {} sat/kwu ({} sat/vB)",
			original_fee_rate.to_sat_per_kwu(), original_fee_rate.to_sat_per_vb_ceil());
		log_info!(self.logger, "  Requested fee rate: {} sat/kwu ({} sat/vB)",
			fee_rate.to_sat_per_kwu(), fee_rate.to_sat_per_vb_ceil());

		// Essential validation: new fee rate must be higher than original
		// This prevents definite rejections by the Bitcoin network
		if fee_rate <= original_fee_rate {
			log_error!(
				self.logger,
				"RBF rejected: New fee rate ({} sat/vB) must be higher than original fee rate ({} sat/vB)",
				fee_rate.to_sat_per_vb_ceil(),
				original_fee_rate.to_sat_per_vb_ceil()
			);
			return Err(Error::InvalidFeeRate);
		}

		log_info!(
			self.logger,
			"RBF approved: Fee rate increase from {} to {} sat/vB",
			original_fee_rate.to_sat_per_vb_ceil(),
			fee_rate.to_sat_per_vb_ceil()
		);

		// Build a new transaction with higher fee using BDK's fee bump functionality
		let mut tx_builder = locked_wallet.build_fee_bump(*txid).map_err(|e| {
			log_error!(self.logger, "Failed to create fee bump builder: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		// Set the new fee rate
		tx_builder.fee_rate(fee_rate);

		// Finalize the transaction
		let mut psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created RBF PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create RBF transaction: {}", err);
				return Err(Error::OnchainTxCreationFailed);
			},
		};

		// Sign the transaction
		match locked_wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					log_error!(self.logger, "Failed to finalize RBF transaction");
					return Err(Error::OnchainTxSigningFailed);
				}
			},
			Err(err) => {
				log_error!(self.logger, "Failed to sign RBF transaction: {}", err);
				return Err(Error::OnchainTxSigningFailed);
			},
		}

		// Persist wallet changes
		let mut locked_persister = self.persister.lock().unwrap();
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Extract and broadcast the transaction
		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		self.broadcaster.broadcast_transactions(&[&tx]);

		let new_txid = tx.compute_txid();

		// Calculate and log the actual fee increase achieved
		let new_fee = locked_wallet.calculate_fee(&tx).unwrap_or(Amount::ZERO);
		let actual_fee_rate = new_fee / tx.weight();

		log_info!(
			self.logger,
			"RBF transaction created successfully!"
		);
		log_info!(
			self.logger,
			"  Original: {} ({} sat/vB, {} sats fee)",
			txid, original_fee_rate.to_sat_per_vb_ceil(), original_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Replacement: {} ({} sat/vB, {} sats fee)",
			new_txid, actual_fee_rate.to_sat_per_vb_ceil(), new_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Additional fee paid: {} sats",
			new_fee.to_sat().saturating_sub(original_fee.to_sat())
		);

		Ok(new_txid)
	}

	/// Accelerates confirmation of a transaction using Child-Pays-For-Parent (CPFP).
	///
	/// This creates a new transaction (child) that spends an output from the
	/// transaction to be accelerated (parent), with a high enough fee to pay for both.
	///
	/// Returns the txid of the child transaction if successful.
	pub(crate) fn accelerate_by_cpfp(
		&self,
		txid: &Txid,
		fee_rate: FeeRate,
		destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();

		// Find the transaction in the wallet
		let parent_tx_node = locked_wallet
			.get_tx(*txid)
			.ok_or_else(|| {
				log_error!(self.logger, "Transaction not found in wallet: {}", txid);
				Error::TransactionNotFound
			})?;

		// Check if transaction is confirmed - can't accelerate confirmed transactions
		if parent_tx_node.chain_position.is_confirmed() {
			log_error!(self.logger, "Cannot accelerate confirmed transaction: {}", txid);
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Calculate parent transaction fee and fee rate for validation
		let parent_tx = &parent_tx_node.tx_node.tx;
		let parent_fee = locked_wallet.calculate_fee(parent_tx).map_err(|e| {
			log_error!(self.logger, "Failed to calculate parent fee: {}", e);
			Error::WalletOperationFailed
		})?;

		// Use Bitcoin crate's built-in fee rate calculation for accuracy
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// Log detailed information for debugging
		log_info!(self.logger, "CPFP Analysis for transaction {}", txid);
		log_info!(self.logger, "  Parent fee: {} sats", parent_fee.to_sat());
		log_info!(self.logger, "  Parent weight: {} WU ({} vB)",
			parent_tx.weight().to_wu(), parent_tx.weight().to_vbytes_ceil());
		log_info!(self.logger, "  Parent fee rate: {} sat/kwu ({} sat/vB)",
			parent_fee_rate.to_sat_per_kwu(), parent_fee_rate.to_sat_per_vb_ceil());
		log_info!(self.logger, "  Child fee rate: {} sat/kwu ({} sat/vB)",
			fee_rate.to_sat_per_kwu(), fee_rate.to_sat_per_vb_ceil());

		// Validate that child fee rate is higher than parent (for effective acceleration)
		if fee_rate <= parent_fee_rate {
			log_info!(
				self.logger,
				"CPFP warning: Child fee rate ({} sat/vB) is not higher than parent fee rate ({} sat/vB). This may not effectively accelerate confirmation.",
				fee_rate.to_sat_per_vb_ceil(),
				parent_fee_rate.to_sat_per_vb_ceil()
			);
			// Note: We warn but don't reject - CPFP can still work in some cases
		} else {
			let acceleration_ratio = fee_rate.to_sat_per_kwu() as f64 / parent_fee_rate.to_sat_per_kwu() as f64;
			log_info!(
				self.logger,
				"CPFP acceleration: Child fee rate is {:.1}x higher than parent ({} vs {} sat/vB)",
				acceleration_ratio,
				fee_rate.to_sat_per_vb_ceil(),
				parent_fee_rate.to_sat_per_vb_ceil()
			);
		}

		// Find spendable outputs from this transaction
		let utxos = locked_wallet
			.list_unspent()
			.filter(|utxo| utxo.outpoint.txid == *txid)
			.collect::<Vec<_>>();

		if utxos.is_empty() {
			log_error!(
				self.logger,
				"No spendable outputs found for transaction: {}",
				txid
			);
			return Err(Error::NoSpendableOutputs);
		}

		log_info!(self.logger, "Found {} spendable output(s) from parent transaction", utxos.len());
		let total_input_value: u64 = utxos.iter().map(|utxo| utxo.txout.value.to_sat()).sum();
		log_info!(self.logger, "  Total input value: {} sats", total_input_value);

		// Determine where to send the funds
		let script_pubkey = match destination_address {
			Some(addr) => {
				log_info!(self.logger, "  Destination: {} (user-specified)", addr);
				// Validate the address
				self.parse_and_validate_address(self.config.network, &addr)?;
				addr.script_pubkey()
			},
			None => {
				// Create a new address to send the funds to
				let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
				log_info!(self.logger, "  Destination: {} (wallet internal address)", address_info.address);
				address_info.address.script_pubkey()
			}
		};

		// Build a transaction that spends these UTXOs
		let mut tx_builder = locked_wallet.build_tx();

		// Add the UTXOs explicitly
		for utxo in &utxos {
			match tx_builder.add_utxo(utxo.outpoint) {
				Ok(_) => {},
				Err(e) => {
					log_error!(self.logger, "Failed to add UTXO: {:?} - {}", utxo.outpoint, e);
					return Err(Error::OnchainTxCreationFailed);
				}
			}
		}

		// Set the fee rate for the child transaction
		tx_builder.fee_rate(fee_rate);

		// Drain all inputs to the destination
		tx_builder.drain_to(script_pubkey);

		// Finalize the transaction
		let mut psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created CPFP PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create CPFP transaction: {}", err);
				return Err(Error::OnchainTxCreationFailed);
			},
		};

		// Sign the transaction
		match locked_wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					log_error!(self.logger, "Failed to finalize CPFP transaction");
					return Err(Error::OnchainTxSigningFailed);
				}
			},
			Err(err) => {
				log_error!(self.logger, "Failed to sign CPFP transaction: {}", err);
				return Err(Error::OnchainTxSigningFailed);
			},
		}

		// Persist wallet changes
		let mut locked_persister = self.persister.lock().unwrap();
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Extract and broadcast the transaction
		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		self.broadcaster.broadcast_transactions(&[&tx]);

		let child_txid = tx.compute_txid();

		// Calculate and log the actual results
		let child_fee = locked_wallet.calculate_fee(&tx).unwrap_or(Amount::ZERO);
		let actual_child_fee_rate = child_fee / tx.weight();

		log_info!(
			self.logger,
			"CPFP transaction created successfully!"
		);
		log_info!(
			self.logger,
			"  Parent: {} ({} sat/vB, {} sats fee)",
			txid, parent_fee_rate.to_sat_per_vb_ceil(), parent_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Child: {} ({} sat/vB, {} sats fee)",
			child_txid, actual_child_fee_rate.to_sat_per_vb_ceil(), child_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Combined package fee rate: approximately {:.1} sat/vB",
			((parent_fee.to_sat() + child_fee.to_sat()) as f64) / ((parent_tx.weight().to_vbytes_ceil() + tx.weight().to_vbytes_ceil()) as f64)
		);

		Ok(child_txid)
	}

	/// Calculates an appropriate fee rate for a CPFP transaction to ensure
	/// the parent transaction gets confirmed within the target number of blocks.
	///
	/// Returns the fee rate that should be used for the child transaction.
	pub(crate) fn calculate_cpfp_fee_rate(
		&self,
		parent_txid: &Txid,
		urgent: bool,
	) -> Result<FeeRate, Error> {
		let locked_wallet = self.inner.lock().unwrap();

		// Get the parent transaction
		let parent_tx_node = locked_wallet
			.get_tx(*parent_txid)
			.ok_or_else(|| {
				log_error!(self.logger, "Transaction not found in wallet: {}", parent_txid);
				Error::TransactionNotFound
			})?;

		// Make sure it's not confirmed
		if parent_tx_node.chain_position.is_confirmed() {
			log_error!(self.logger, "Transaction is already confirmed: {}", parent_txid);
			return Err(Error::TransactionAlreadyConfirmed);
		}

		let parent_tx = &parent_tx_node.tx_node.tx;

		// Calculate parent fee and fee rate using accurate method
		let parent_fee = locked_wallet.calculate_fee(parent_tx).map_err(|e| {
			log_error!(self.logger, "Failed to calculate parent fee: {}", e);
			Error::WalletOperationFailed
		})?;

		// Use Bitcoin crate's built-in fee rate calculation for accuracy
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// Get current mempool fee rates from fee estimator based on urgency
		let target = if urgent {
			ConfirmationTarget::Lightning(lightning::chain::chaininterface::ConfirmationTarget::UrgentOnChainSweep)
		} else {
			ConfirmationTarget::OnchainPayment
		};

		let target_fee_rate = self.fee_estimator.estimate_fee_rate(target);

		log_info!(self.logger, "CPFP Fee Rate Calculation for transaction {}", parent_txid);
		log_info!(self.logger, "  Parent fee: {} sats", parent_fee.to_sat());
		log_info!(self.logger, "  Parent weight: {} WU ({} vB)",
			parent_tx.weight().to_wu(), parent_tx.weight().to_vbytes_ceil());
		log_info!(self.logger, "  Parent fee rate: {} sat/kwu ({} sat/vB)",
			parent_fee_rate.to_sat_per_kwu(), parent_fee_rate.to_sat_per_vb_ceil());
		log_info!(self.logger, "  Target fee rate: {} sat/kwu ({} sat/vB)",
			target_fee_rate.to_sat_per_kwu(), target_fee_rate.to_sat_per_vb_ceil());
		log_info!(self.logger, "  Urgency level: {}", if urgent { "HIGH" } else { "NORMAL" });

		// If parent fee rate is already sufficient, return a slightly higher one
		if parent_fee_rate >= target_fee_rate {
			let recommended_rate = FeeRate::from_sat_per_kwu(parent_fee_rate.to_sat_per_kwu() + 250); // +1 sat/vB
			log_info!(
				self.logger,
				"Parent fee rate is already sufficient. Recommending slightly higher rate: {} sat/vB",
				recommended_rate.to_sat_per_vb_ceil()
			);
			return Ok(recommended_rate);
		}

		// Estimate child transaction size (weight units)
		// Conservative estimate for a typical 1-input, 1-output transaction
		let estimated_child_weight_units = 480; // ~120 vbytes * 4 = 480 wu
		let estimated_child_vbytes = estimated_child_weight_units / 4;

		// Calculate the fee deficit for the parent (in sats)
		//let parent_weight_units = parent_tx.weight().to_wu();
		let parent_vbytes = parent_tx.weight().to_vbytes_ceil();
		let parent_fee_deficit = (target_fee_rate.to_sat_per_vb_ceil() - parent_fee_rate.to_sat_per_vb_ceil()) * parent_vbytes;

		// Calculate what the child needs to pay to cover both transactions
		let base_child_fee = target_fee_rate.to_sat_per_vb_ceil() * estimated_child_vbytes;
		let total_child_fee = base_child_fee + parent_fee_deficit;

		// Calculate the effective fee rate for the child
		let child_fee_rate_sat_vb = total_child_fee / estimated_child_vbytes;
		let child_fee_rate = FeeRate::from_sat_per_vb(child_fee_rate_sat_vb)
			.unwrap_or(FeeRate::from_sat_per_kwu(child_fee_rate_sat_vb * 250));

		log_info!(self.logger, "CPFP Calculation Results:");
		log_info!(self.logger, "  Parent fee deficit: {} sats", parent_fee_deficit);
		log_info!(self.logger, "  Base child fee needed: {} sats", base_child_fee);
		log_info!(self.logger, "  Total child fee needed: {} sats", total_child_fee);
		log_info!(self.logger, "  Recommended child fee rate: {} sat/vB", child_fee_rate.to_sat_per_vb_ceil());
		log_info!(self.logger, "  Combined package rate: ~{} sat/vB",
			((parent_fee.to_sat() + total_child_fee) / (parent_vbytes + estimated_child_vbytes)));

		Ok(child_fee_rate)
	}

	fn update_payment_store<'a>(
		&self, locked_wallet: &'a mut PersistedWallet<KVStoreWalletPersister>,
	) -> Result<(), Error> {
		for wtx in locked_wallet.transactions() {
			let id = PaymentId(wtx.tx_node.txid.to_byte_array());
			let txid = wtx.tx_node.txid;
			let (payment_status, confirmation_status) = match wtx.chain_position {
				bdk_chain::ChainPosition::Confirmed { anchor, .. } => {
					let confirmation_height = anchor.block_id.height;
					let cur_height = locked_wallet.latest_checkpoint().height();
					let payment_status = if cur_height >= confirmation_height + ANTI_REORG_DELAY - 1
					{
						PaymentStatus::Succeeded
					} else {
						PaymentStatus::Pending
					};
					let confirmation_status = ConfirmationStatus::Confirmed {
						block_hash: anchor.block_id.hash,
						height: confirmation_height,
						timestamp: anchor.confirmation_time,
					};
					(payment_status, confirmation_status)
				},
				bdk_chain::ChainPosition::Unconfirmed { .. } => {
					(PaymentStatus::Pending, ConfirmationStatus::Unconfirmed)
				},
			};
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
			let kind = crate::payment::PaymentKind::Onchain { txid, status: confirmation_status };
			let fee = locked_wallet.calculate_fee(&wtx.tx_node.tx).unwrap_or(Amount::ZERO);
			let (sent, received) = locked_wallet.sent_and_received(&wtx.tx_node.tx);
			let (direction, amount_msat) = if sent > received {
				let direction = PaymentDirection::Outbound;
				let amount_msat = Some(
					sent.to_sat().saturating_sub(fee.to_sat()).saturating_sub(received.to_sat())
						* 1000,
				);
				(direction, amount_msat)
			} else {
				let direction = PaymentDirection::Inbound;
				let amount_msat = Some(
					received.to_sat().saturating_sub(sent.to_sat().saturating_sub(fee.to_sat()))
						* 1000,
				);
				(direction, amount_msat)
			};

			let fee_paid_msat = Some(fee.to_sat() * 1000);

			let payment = PaymentDetails::new(
				id,
				kind,
				amount_msat,
				fee_paid_msat,
				direction,
				payment_status,
			);

			self.payment_store.insert_or_update(payment)?;
		}

		Ok(())
	}

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

	fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let mut locked_persister = self.persister.lock().unwrap();

		let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
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
				self.list_confirmed_utxos().map_or(false, |v| !v.is_empty()),
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

	fn parse_and_validate_address(
		&self, network: Network, address: &Address,
	) -> Result<Address, Error> {
		Address::<NetworkUnchecked>::from_str(address.to_string().as_str())
			.map_err(|_| Error::InvalidAddress)?
			.require_network(network)
			.map_err(|_| Error::InvalidAddress)
	}

	pub(crate) fn send_to_address(
		&self, address: &bitcoin::Address, send_amount: OnchainSendAmount,
		fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		self.parse_and_validate_address(self.config.network, &address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let tx = {
			let mut locked_wallet = self.inner.lock().unwrap();

			// Prepare the tx_builder. We properly check the reserve requirements (again) further down.
			const DUST_LIMIT_SATS: u64 = 546;
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
					let change_address_info = locked_wallet.peek_address(KeychainKind::Internal, 0);
					let balance = locked_wallet.balance();
					let spendable_amount_sats = self
						.get_balances_inner(balance, cur_anchor_reserve_sats)
						.map(|(_, s)| s)
						.unwrap_or(0);
					let tmp_tx = {
						let mut tmp_tx_builder = locked_wallet.build_tx();
						tmp_tx_builder
							.drain_wallet()
							.drain_to(address.script_pubkey())
							.add_recipient(
								change_address_info.address.script_pubkey(),
								Amount::from_sat(cur_anchor_reserve_sats),
							)
							.fee_rate(fee_rate);
						match tmp_tx_builder.finish() {
							Ok(psbt) => psbt.unsigned_tx,
							Err(err) => {
								log_error!(
									self.logger,
									"Failed to create temporary transaction: {}",
									err
								);
								return Err(err.into());
							},
						}
					};

					let estimated_tx_fee = locked_wallet.calculate_fee(&tmp_tx).map_err(|e| {
						log_error!(
							self.logger,
							"Failed to calculate fee of temporary transaction: {}",
							e
						);
						e
					})?;

					// 'cancel' the transaction to free up any used change addresses
					locked_wallet.cancel_tx(&tmp_tx);

					let estimated_spendable_amount = Amount::from_sat(
						spendable_amount_sats.saturating_sub(estimated_tx_fee.to_sat()),
					);

					if estimated_spendable_amount == Amount::ZERO {
						log_error!(self.logger,
							"Unable to send payment without infringing on Anchor reserves. Available: {}sats, estimated fee required: {}sats.",
							spendable_amount_sats,
							estimated_tx_fee,
						);
						return Err(Error::InsufficientFunds);
					}

					let mut tx_builder = locked_wallet.build_tx();
					tx_builder
						.add_recipient(address.script_pubkey(), estimated_spendable_amount)
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

		self.broadcaster.broadcast_transactions(&[&tx]);

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
}

impl<B: Deref, E: Deref, L: Deref> Listen for Wallet<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
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

		match locked_wallet.apply_block(block, height) {
			Ok(()) => {
				if let Err(e) = self.update_payment_store(&mut *locked_wallet) {
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

	fn block_disconnected(&self, _header: &bitcoin::block::Header, _height: u32) {
		// This is a no-op as we don't have to tell BDK about disconnections. According to the BDK
		// team, it's sufficient in case of a reorg to always connect blocks starting from the last
		// point of disagreement.
	}
}

impl<B: Deref, E: Deref, L: Deref> WalletSource for Wallet<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
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
							1 /* witness items */ + 1 /* schnorr sig len */ + 64, /* schnorr sig */
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

	fn get_change_script(&self) -> Result<ScriptBuf, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let mut locked_persister = self.persister.lock().unwrap();

		let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
		locked_wallet.persist(&mut locked_persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			()
		})?;
		Ok(address_info.address.script_pubkey())
	}

	fn sign_psbt(&self, mut psbt: Psbt) -> Result<Transaction, ()> {
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
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub(crate) struct WalletKeysManager<B: Deref, E: Deref, L: Deref>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	inner: KeysManager,
	wallet: Arc<Wallet<B, E, L>>,
	logger: L,
}

impl<B: Deref, E: Deref, L: Deref> WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	/// Constructs a `WalletKeysManager` that overrides the destination and shutdown scripts.
	///
	/// See [`KeysManager::new`] for more information on `seed`, `starting_time_secs`, and
	/// `starting_time_nanos`.
	pub fn new(
		seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32,
		wallet: Arc<Wallet<B, E, L>>, logger: L,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos);
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

impl<B: Deref, E: Deref, L: Deref> NodeSigner for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
		self.inner.get_node_id(recipient)
	}

	fn ecdh(
		&self, recipient: Recipient, other_key: &PublicKey, tweak: Option<&Scalar>,
	) -> Result<SharedSecret, ()> {
		self.inner.ecdh(recipient, other_key, tweak)
	}

	fn get_inbound_payment_key(&self) -> ExpandedKey {
		self.inner.get_inbound_payment_key()
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
}

impl<B: Deref, E: Deref, L: Deref> OutputSpender for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	/// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
	fn spend_spendable_outputs<C: Signing>(
		&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>,
		change_destination_script: ScriptBuf, feerate_sat_per_1000_weight: u32,
		locktime: Option<LockTime>, secp_ctx: &Secp256k1<C>,
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

impl<B: Deref, E: Deref, L: Deref> EntropySource for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl<B: Deref, E: Deref, L: Deref> SignerProvider for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	type EcdsaSigner = InMemorySigner;

	fn generate_channel_keys_id(
		&self, inbound: bool, channel_value_satoshis: u64, user_channel_id: u128,
	) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, channel_value_satoshis, user_channel_id)
	}

	fn derive_channel_signer(
		&self, channel_value_satoshis: u64, channel_keys_id: [u8; 32],
	) -> Self::EcdsaSigner {
		self.inner.derive_channel_signer(channel_value_satoshis, channel_keys_id)
	}

	fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::EcdsaSigner, DecodeError> {
		self.inner.read_chan_signer(reader)
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

impl<B: Deref, E: Deref, L: Deref> ChangeDestinationSource for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
{
	fn get_change_destination_script(&self) -> Result<ScriptBuf, ()> {
		let address = self.wallet.get_new_internal_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}
}