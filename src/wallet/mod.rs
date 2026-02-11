// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
pub use bdk_wallet::coin_selection::CoinSelectionAlgorithm as BdkCoinSelectionAlgorithm;
use bdk_wallet::event::WalletEvent;
use bdk_wallet::{Balance, KeychainKind, LocalOutput, PersistedWallet, Update};
use bdk_wallet_aggregate::AggregateWallet;
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
	Address, Amount, FeeRate, OutPoint, PubkeyHash, Script, ScriptBuf, Transaction, TxOut, Txid,
	WPubkeyHash, Weight, WitnessProgram, WitnessVersion,
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

use crate::config::{AddressType, Config};
use crate::event::{TxInput, TxOutput};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::payment::store::ConfirmationStatus;
use crate::payment::{PaymentDetails, PaymentDirection, PaymentStatus};
use crate::types::{Broadcaster, ChannelManager, PaymentStore};
use crate::Error;

// Minimum economical output value (dust limit)
const DUST_LIMIT_SATS: u64 = 546;

#[derive(Clone, Copy)]
pub(crate) enum OnchainSendAmount {
	ExactRetainingReserve { amount_sats: u64, cur_anchor_reserve_sats: u64 },
	AllRetainingReserve { cur_anchor_reserve_sats: u64 },
	AllDrainingReserve,
}

/// Available coin selection algorithms
#[derive(Debug, Clone, Copy)]
pub enum CoinSelectionAlgorithm {
	/// Branch and bound algorithm (tries to find exact match)
	BranchAndBound,
	/// Select largest UTXOs first
	LargestFirst,
	/// Select oldest UTXOs first
	OldestFirst,
	/// Select UTXOs randomly
	SingleRandomDraw,
}

pub(crate) mod persist;
pub(crate) mod ser;

pub(crate) struct Wallet {
	inner: Mutex<AggregateWallet<AddressType, KVStoreWalletPersister>>,
	broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	payment_store: Arc<PaymentStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
}

impl Wallet {
	pub(crate) fn new(
		wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		wallet_persister: KVStoreWalletPersister,
		additional_wallets: Vec<(
			AddressType,
			PersistedWallet<KVStoreWalletPersister>,
			KVStoreWalletPersister,
		)>,
		broadcaster: Arc<Broadcaster>, fee_estimator: Arc<OnchainFeeEstimator>,
		payment_store: Arc<PaymentStore>, config: Arc<Config>, logger: Arc<Logger>,
	) -> Self {
		let aggregate =
			AggregateWallet::new(wallet, wallet_persister, config.address_type, additional_wallets);
		let inner = Mutex::new(aggregate);
		Self { inner, broadcaster, fee_estimator, payment_store, config, logger }
	}

	pub(crate) fn is_funding_transaction(
		&self, txid: &Txid, channel_manager: &ChannelManager,
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

	pub(crate) fn estimate_fee_rate(&self, target: ConfirmationTarget) -> FeeRate {
		self.fee_estimator.estimate_fee_rate(target)
	}

	pub(crate) fn get_full_scan_request(&self) -> FullScanRequest<KeychainKind> {
		self.inner.lock().unwrap().start_full_scan().build()
	}

	pub(crate) fn get_incremental_sync_request(&self) -> SyncRequest<(KeychainKind, u32)> {
		self.inner.lock().unwrap().start_sync_with_revealed_spks().build()
	}

	pub(crate) fn get_wallet_sync_request(
		&self, address_type: AddressType,
	) -> Result<
		(FullScanRequest<KeychainKind>, SyncRequest<(KeychainKind, u32)>),
		bdk_wallet_aggregate::Error,
	> {
		self.inner.lock().unwrap().wallet_sync_request(&address_type)
	}

	pub(crate) fn get_cached_txs(&self) -> Vec<Arc<Transaction>> {
		self.inner.lock().unwrap().cached_txs()
	}

	pub(crate) fn get_unconfirmed_txids(&self) -> Vec<Txid> {
		self.inner.lock().unwrap().unconfirmed_txids()
	}

	pub(crate) fn is_tx_confirmed(&self, txid: &Txid) -> bool {
		self.inner.lock().unwrap().is_tx_confirmed(txid)
	}

	pub(crate) fn current_best_block(&self) -> BestBlock {
		let checkpoint = self.inner.lock().unwrap().latest_checkpoint();
		BestBlock { block_hash: checkpoint.hash(), height: checkpoint.height() }
	}

	// Get a drain script for change outputs.
	pub(crate) fn get_drain_script(&self) -> Result<ScriptBuf, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let change_address = locked_wallet.peek_address(KeychainKind::Internal, 0);
		Ok(change_address.address.script_pubkey())
	}

	/// Returns the list of all loaded address types (primary + monitored).
	pub(crate) fn get_loaded_address_types(&self) -> Vec<AddressType> {
		self.inner.lock().unwrap().loaded_keys()
	}

	pub(crate) fn apply_update(
		&self, update: impl Into<Update>,
	) -> Result<Vec<WalletEvent>, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		match locked_wallet.apply_update(update) {
			Ok((events, _txids)) => {
				self.update_payment_store(&locked_wallet).map_err(|e| {
					log_error!(self.logger, "Failed to update payment store: {}", e);
					Error::PersistenceFailed
				})?;

				Ok(events)
			},
			Err(e) => {
				log_error!(self.logger, "Sync failed due to chain connection error: {}", e);
				Err(Error::WalletOperationFailed)
			},
		}
	}

	pub(crate) fn apply_update_for_address_type(
		&self, address_type: AddressType, update: impl Into<Update>,
	) -> Result<Vec<WalletEvent>, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		match locked_wallet.apply_update_to_wallet(address_type, update) {
			Ok((events, _txids)) => {
				self.update_payment_store(&locked_wallet).map_err(|e| {
					log_error!(self.logger, "Failed to update payment store: {}", e);
					Error::PersistenceFailed
				})?;
				Ok(events)
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to apply update for address type {:?}: {}",
					address_type,
					e
				);
				Err(Error::WalletOperationFailed)
			},
		}
	}

	pub(crate) fn apply_mempool_txs(
		&self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.apply_mempool_txs(unconfirmed_txs, evicted_txids).map_err(|e| {
			log_error!(self.logger, "Failed to apply mempool txs: {}", e);
			Error::PersistenceFailed
		})
	}

	// Bumps the fee of an existing transaction using Replace-By-Fee (RBF).
	// Returns the txid of the new transaction if successful.
	pub(crate) fn bump_fee_by_rbf(
		&self, txid: &Txid, fee_rate: FeeRate, channel_manager: &ChannelManager,
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

		let (tx, original_fee) = locked_wallet.build_rbf(*txid, fee_rate).map_err(|e| match e {
			bdk_wallet_aggregate::Error::TransactionNotFound => {
				log_error!(self.logger, "Transaction not found in any wallet: {}", txid);
				Error::TransactionNotFound
			},
			bdk_wallet_aggregate::Error::TransactionAlreadyConfirmed => {
				log_error!(self.logger, "Cannot replace confirmed transaction: {}", txid);
				Error::TransactionAlreadyConfirmed
			},
			bdk_wallet_aggregate::Error::InvalidFeeRate => {
				log_error!(self.logger, "RBF rejected: new fee rate is not higher");
				Error::InvalidFeeRate
			},
			bdk_wallet_aggregate::Error::InsufficientFunds => {
				log_error!(self.logger, "Insufficient funds for RBF fee bump of {}", txid);
				Error::InsufficientFunds
			},
			bdk_wallet_aggregate::Error::UtxoNotFoundLocally(outpoint) => {
				log_error!(
					self.logger,
					"Cannot calculate fee for RBF of {}: input UTXO {} not found locally. \
					 Try syncing the wallet first.",
					txid,
					outpoint,
				);
				Error::OnchainTxCreationFailed
			},
			other => {
				log_error!(self.logger, "Failed to build RBF for {}: {}", txid, other);
				Error::OnchainTxCreationFailed
			},
		})?;

		// Persist wallet changes
		locked_wallet.persist_all().map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Extract and broadcast the transaction
		self.broadcaster.broadcast_transactions(&[&tx]);

		let new_txid = tx.compute_txid();

		// Calculate and log the actual fee increase achieved
		let new_fee = locked_wallet.calculate_tx_fee(&tx).unwrap_or(Amount::ZERO);
		let actual_fee_rate = new_fee / tx.weight();

		log_info!(self.logger, "RBF transaction created successfully!");
		log_info!(self.logger, "  Original: {} ({} sats fee)", txid, original_fee.to_sat());
		log_info!(
			self.logger,
			"  Replacement: {} ({} sat/vB, {} sats fee)",
			new_txid,
			actual_fee_rate.to_sat_per_vb_ceil(),
			new_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Additional fee paid: {} sats",
			new_fee.to_sat().saturating_sub(original_fee.to_sat())
		);

		Ok(new_txid)
	}

	// Accelerates confirmation of a transaction using Child-Pays-For-Parent (CPFP).
	// Returns the txid of the child transaction if successful.
	pub(crate) fn accelerate_by_cpfp(
		&self, txid: &Txid, fee_rate: FeeRate, destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		let destination_script = match destination_address {
			Some(ref addr) => {
				self.parse_and_validate_address(addr)?;
				Some(addr.script_pubkey())
			},
			None => None,
		};

		let mut locked_wallet = self.inner.lock().unwrap();

		let (tx, parent_fee, parent_fee_rate) =
			locked_wallet.build_cpfp(*txid, fee_rate, destination_script).map_err(|e| {
				log_error!(self.logger, "Failed to build CPFP for {}: {}", txid, e);
				match e {
					bdk_wallet_aggregate::Error::TransactionNotFound => Error::TransactionNotFound,
					bdk_wallet_aggregate::Error::TransactionAlreadyConfirmed => {
						Error::TransactionAlreadyConfirmed
					},
					bdk_wallet_aggregate::Error::NoSpendableOutputs => Error::NoSpendableOutputs,
					_ => Error::OnchainTxCreationFailed,
				}
			})?;

		// Persist wallet changes
		locked_wallet.persist_all().map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Extract and broadcast the transaction
		self.broadcaster.broadcast_transactions(&[&tx]);

		let child_txid = tx.compute_txid();

		// Calculate and log the actual results
		let child_fee = locked_wallet.calculate_tx_fee(&tx).unwrap_or(Amount::ZERO);
		let actual_child_fee_rate = child_fee / tx.weight();

		log_info!(self.logger, "CPFP transaction created successfully!");
		log_info!(
			self.logger,
			"  Parent: {} ({} sat/vB, {} sats fee)",
			txid,
			parent_fee_rate.to_sat_per_vb_ceil(),
			parent_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Child: {} ({} sat/vB, {} sats fee)",
			child_txid,
			actual_child_fee_rate.to_sat_per_vb_ceil(),
			child_fee.to_sat()
		);

		Ok(child_txid)
	}

	// Calculates an appropriate fee rate for a CPFP transaction.
	pub(crate) fn calculate_cpfp_fee_rate(
		&self, parent_txid: &Txid, urgent: bool,
	) -> Result<FeeRate, Error> {
		let target = if urgent {
			ConfirmationTarget::Lightning(
				lightning::chain::chaininterface::ConfirmationTarget::MaximumFeeEstimate,
			)
		} else {
			ConfirmationTarget::OnchainPayment
		};
		let target_fee_rate = self.fee_estimator.estimate_fee_rate(target);

		let locked_wallet = self.inner.lock().unwrap();
		locked_wallet.calculate_cpfp_fee_rate(*parent_txid, target_fee_rate).map_err(|e| {
			log_error!(self.logger, "Failed to calculate CPFP fee rate for {}: {}", parent_txid, e);
			match e {
				bdk_wallet_aggregate::Error::TransactionNotFound => Error::TransactionNotFound,
				bdk_wallet_aggregate::Error::TransactionAlreadyConfirmed => {
					Error::TransactionAlreadyConfirmed
				},
				_ => Error::WalletOperationFailed,
			}
		})
	}

	pub(crate) fn update_payment_store_for_all_transactions(&self) -> Result<(), Error> {
		let locked_wallet = self.inner.lock().unwrap();
		self.update_payment_store(&locked_wallet)
	}

	fn update_payment_store(
		&self, locked_wallet: &AggregateWallet<AddressType, KVStoreWalletPersister>,
	) -> Result<(), Error> {
		let mut seen_txids = std::collections::HashSet::new();
		let cur_height = locked_wallet.latest_checkpoint().height();

		for wallet in locked_wallet.wallets().values() {
			for wtx in wallet.transactions() {
				let txid = wtx.tx_node.txid;
				if !seen_txids.insert(txid) {
					continue;
				}

				let id = PaymentId(txid.to_byte_array());
				let (payment_status, confirmation_status) = match wtx.chain_position {
					bdk_chain::ChainPosition::Confirmed { anchor, .. } => {
						let confirmation_height = anchor.block_id.height;
						let payment_status =
							if cur_height >= confirmation_height + ANTI_REORG_DELAY - 1 {
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
				let kind =
					crate::payment::PaymentKind::Onchain { txid, status: confirmation_status };

				let fee = locked_wallet.calculate_tx_fee(&wtx.tx_node.tx).unwrap_or(Amount::ZERO);
				let (sent_sat, received_sat) =
					locked_wallet.sent_and_received(txid).unwrap_or((0, 0));
				let fee_sat = fee.to_sat();

				let (direction, amount_msat) = if sent_sat > received_sat {
					let direction = PaymentDirection::Outbound;
					let amount_msat =
						Some(sent_sat.saturating_sub(fee_sat).saturating_sub(received_sat) * 1000);
					(direction, amount_msat)
				} else {
					let direction = PaymentDirection::Inbound;
					let amount_msat =
						Some(received_sat.saturating_sub(sent_sat.saturating_sub(fee_sat)) * 1000);
					(direction, amount_msat)
				};

				let fee_paid_msat = Some(fee_sat * 1000);

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
		let primary_type = locked_wallet.primary_key();

		let tx = if primary_type == AddressType::Legacy {
			log_info!(
				self.logger,
				"Primary is Legacy, using best SegWit wallet for channel funding"
			);
			locked_wallet.build_and_sign_tx_with_best_wallet(
				output_script,
				amount,
				fee_rate,
				locktime,
				|k| *k != AddressType::Legacy,
			)
		} else {
			locked_wallet.build_and_sign_funding_tx(output_script, amount, fee_rate, locktime)
		};

		tx.map_err(|e| {
			log_error!(self.logger, "Failed to create funding transaction: {}", e);
			match e {
				bdk_wallet_aggregate::Error::InsufficientFunds => Error::InsufficientFunds,
				bdk_wallet_aggregate::Error::OnchainTxSigningFailed => {
					Error::OnchainTxSigningFailed
				},
				bdk_wallet_aggregate::Error::PersistenceFailed => Error::PersistenceFailed,
				_ => Error::OnchainTxCreationFailed,
			}
		})
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_address().map_err(|e| {
			log_error!(self.logger, "Failed to get new address: {}", e);
			Error::WalletOperationFailed
		})
	}

	pub(crate) fn get_new_address_for_type(
		&self, address_type: AddressType,
	) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_address_for(&address_type).map_err(|e| {
			log_error!(self.logger, "Failed to get new address for type {:?}: {}", address_type, e);
			Error::WalletOperationFailed
		})
	}

	// Returns a native witness address for Lightning channel scripts.
	// Falls back to a loaded NativeSegwit/Taproot wallet if the primary is not one.
	pub(crate) fn get_new_witness_address(&self) -> Result<bitcoin::Address, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let primary = locked_wallet.primary_key();

		if primary.is_native_witness() {
			drop(locked_wallet);
			return self.get_new_address();
		}

		let witness_key = locked_wallet.loaded_keys().into_iter().find(|k| k.is_native_witness());
		drop(locked_wallet);

		match witness_key {
			Some(key) => self.get_new_address_for_type(key),
			None => {
				log_error!(self.logger, "No native witness wallet loaded for Lightning operations");
				Err(Error::WalletOperationFailed)
			},
		}
	}

	pub(crate) fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_internal_address().map_err(|e| {
			log_error!(self.logger, "Failed to get new internal address: {}", e);
			Error::WalletOperationFailed
		})
	}

	pub(crate) fn cancel_tx(&self, tx: &Transaction) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.cancel_tx(tx).map_err(|e| {
			log_error!(self.logger, "Failed to cancel transaction: {}", e);
			Error::PersistenceFailed
		})
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

		self.get_balances_inner(&balance, total_anchor_channels_reserve_sats)
	}

	pub(crate) fn get_balance_for_address_type(
		&self, address_type: AddressType,
	) -> Result<(u64, u64), Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let balance = locked_wallet.balance_for(&address_type).map_err(|e| {
			log_error!(self.logger, "Failed to get balance for {:?}: {}", address_type, e);
			Error::WalletOperationFailed
		})?;

		self.get_balances_inner(&balance, 0)
	}

	fn get_balances_inner(
		&self, balance: &Balance, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let spendable_base = if self.config.include_untrusted_pending_in_spendable {
			balance.trusted_spendable().to_sat() + balance.untrusted_pending.to_sat()
		} else {
			balance.trusted_spendable().to_sat()
		};

		let (total, spendable) = (
			balance.total().to_sat(),
			spendable_base.saturating_sub(total_anchor_channels_reserve_sats),
		);

		Ok((total, spendable))
	}

	pub(crate) fn get_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		self.get_balances(total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	pub(crate) fn get_witness_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let balance = locked_wallet.balance_filtered(|k| *k != AddressType::Legacy);
		self.get_balances_inner(&balance, total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	// Get transaction details including inputs, outputs, and net amount.
	// Returns None if the transaction is not found in any wallet.
	pub(crate) fn get_tx_details(&self, txid: &Txid) -> Option<(i64, Vec<TxInput>, Vec<TxOutput>)> {
		let locked_wallet = self.inner.lock().unwrap();
		let (sent_sat, received_sat) = locked_wallet.sent_and_received(*txid)?;
		let tx = locked_wallet.find_tx(*txid)?;
		let net_amount = received_sat as i64 - sent_sat as i64;

		let inputs: Vec<TxInput> = tx.input.iter().map(TxInput::from_tx_input).collect();

		let outputs: Vec<TxOutput> = tx
			.output
			.iter()
			.enumerate()
			.map(|(index, tx_output)| {
				TxOutput::from_tx_output(tx_output, index as u32, self.config.network)
			})
			.collect();

		Some((net_amount, inputs, outputs))
	}

	pub(crate) fn parse_and_validate_address(&self, address: &Address) -> Result<Address, Error> {
		Address::<NetworkUnchecked>::from_str(address.to_string().as_str())
			.map_err(|_| Error::InvalidAddress)?
			.require_network(self.config.network)
			.map_err(|_| Error::InvalidAddress)
	}

	// Returns all UTXOs that are safe to spend (excluding channel funding transactions).
	pub fn get_spendable_utxos(
		&self, channel_manager: &ChannelManager,
	) -> Result<Vec<LocalOutput>, Error> {
		let locked_wallet = self.inner.lock().unwrap();

		let all_utxos: Vec<LocalOutput> = locked_wallet.list_unspent();
		let total_count = all_utxos.len();

		// Filter out channel funding transactions
		let spendable_utxos: Vec<LocalOutput> = all_utxos
			.into_iter()
			.filter(|utxo| {
				// Check if this UTXO's transaction is a channel funding transaction
				if self.is_funding_transaction(&utxo.outpoint.txid, channel_manager) {
					log_debug!(
						self.logger,
						"Filtering out UTXO {:?} as it's part of a channel funding transaction",
						utxo.outpoint
					);
					false
				} else {
					true
				}
			})
			.collect();

		log_debug!(
			self.logger,
			"Found {} spendable UTXOs out of {} total UTXOs",
			spendable_utxos.len(),
			total_count
		);

		Ok(spendable_utxos)
	}

	// Select UTXOs using a specific coin selection algorithm.
	// Returns selected UTXOs that meet the target amount plus fees, excluding channel funding txs.
	pub fn select_utxos_with_algorithm(
		&self, target_amount: u64, available_utxos: Vec<LocalOutput>, fee_rate: FeeRate,
		algorithm: CoinSelectionAlgorithm, drain_script: &Script, channel_manager: &ChannelManager,
	) -> Result<Vec<OutPoint>, Error> {
		let excluded_outpoints: Vec<OutPoint> = available_utxos
			.iter()
			.filter(|utxo| self.is_funding_transaction(&utxo.outpoint.txid, channel_manager))
			.map(|utxo| utxo.outpoint)
			.collect();

		let locked_wallet = self.inner.lock().unwrap();
		let algo = match algorithm {
			CoinSelectionAlgorithm::BranchAndBound => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::BranchAndBound
			},
			CoinSelectionAlgorithm::LargestFirst => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::LargestFirst
			},
			CoinSelectionAlgorithm::OldestFirst => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::OldestFirst
			},
			CoinSelectionAlgorithm::SingleRandomDraw => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::SingleRandomDraw
			},
		};

		locked_wallet
			.select_utxos(
				target_amount,
				available_utxos,
				fee_rate,
				algo,
				drain_script,
				&excluded_outpoints,
			)
			.map_err(|e| {
				log_error!(self.logger, "Coin selection failed: {}", e);
				Error::CoinSelectionFailed
			})
	}

	// Helper that builds a transaction PSBT with shared logic for send_to_address
	// and calculate_transaction_fee.
	// Supports cross-wallet spending: UTXOs from non-primary wallets are added as
	// foreign inputs when the primary wallet alone has insufficient funds.
	fn build_transaction_psbt(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: FeeRate,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<(Psbt, MutexGuard<'_, AggregateWallet<AddressType, KVStoreWalletPersister>>), Error>
	{
		let mut locked_wallet = self.inner.lock().unwrap();

		let all_utxos = locked_wallet.list_unspent();

		// Validate and check UTXOs if provided
		if let Some(ref outpoints) = utxos_to_spend {
			let all_utxo_set: std::collections::HashSet<_> =
				all_utxos.iter().map(|u| u.outpoint).collect();

			for outpoint in outpoints {
				if !all_utxo_set.contains(outpoint) {
					log_error!(self.logger, "UTXO {:?} not found in any wallet", outpoint);
					return Err(Error::WalletOperationFailed);
				}
				if self.is_funding_transaction(&outpoint.txid, channel_manager) {
					log_error!(
						self.logger,
						"UTXO {:?} is part of a channel funding transaction and cannot be spent",
						outpoint
					);
					return Err(Error::WalletOperationFailed);
				}
			}

			// Calculate total value of selected UTXOs
			let selected_value: u64 = all_utxos
				.iter()
				.filter(|u| outpoints.contains(&u.outpoint))
				.map(|u| u.txout.value.to_sat())
				.sum();

			// For exact amounts, ensure we have enough value
			if let OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } = send_amount {
				// Calculate a fee buffer based on fee rate
				// Assume a typical tx with 1 input and 2 outputs (~200 vbytes)
				let typical_tx_weight = Weight::from_vb(200).expect("Valid weight");
				let fee_buffer =
					fee_rate.fee_wu(typical_tx_weight).expect("Valid fee calculation").to_sat();
				// Use at least 1000 sats as minimum buffer
				let min_fee_buffer = fee_buffer.max(1000);
				let min_required = amount_sats.saturating_add(min_fee_buffer);
				if selected_value < min_required {
					log_error!(
						self.logger,
						"Selected UTXOs have insufficient value. Have: {}sats, Need at least: {}sats",
						selected_value,
						min_required
					);
					return Err(Error::InsufficientFunds);
				}
			}

			log_debug!(
				self.logger,
				"Using {} manually selected UTXOs with total value: {}sats",
				outpoints.len(),
				selected_value
			);
		}

		let funding_txids: std::collections::HashSet<Txid> = all_utxos
			.iter()
			.filter(|u| self.is_funding_transaction(&u.outpoint.txid, channel_manager))
			.map(|u| u.outpoint.txid)
			.collect();

		let non_primary_utxo_infos: Option<Vec<bdk_wallet_aggregate::UtxoPsbtInfo>> =
			match (&utxos_to_spend, send_amount) {
				(Some(_), _) => None,
				(None, OnchainSendAmount::AllDrainingReserve)
				| (None, OnchainSendAmount::AllRetainingReserve { .. }) => locked_wallet
					.non_primary_foreign_utxos(&funding_txids)
					.ok()
					.filter(|v| !v.is_empty()),
				(None, OnchainSendAmount::ExactRetainingReserve { .. }) => None,
			};

		let manual_utxo_infos: Option<Vec<bdk_wallet_aggregate::UtxoPsbtInfo>> =
			if let Some(ref outpoints) = utxos_to_spend {
				Some(locked_wallet.prepare_outpoints_for_psbt(outpoints).map_err(|e| {
					log_error!(self.logger, "Failed to prepare manually selected UTXOs: {}", e);
					Error::WalletOperationFailed
				})?)
			} else {
				None
			};

		let aggregate_balance = locked_wallet.balance();
		let primary = locked_wallet.primary_wallet_mut();

		// Prepare the tx_builder. We properly check the reserve requirements (again) further down.
		let mut tx_builder = match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
				let mut tx_builder = primary.build_tx();
				let amount = Amount::from_sat(amount_sats);
				tx_builder.add_recipient(address.script_pubkey(), amount).fee_rate(fee_rate);
				tx_builder
			},
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
				if cur_anchor_reserve_sats > DUST_LIMIT_SATS =>
			{
				let change_address_info = primary.peek_address(KeychainKind::Internal, 0);
				let spendable_amount_sats = self
					.get_balances_inner(&aggregate_balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				let tmp_tx = {
					let mut tmp_tx_builder = primary.build_tx();
					tmp_tx_builder
						.drain_wallet()
						.drain_to(address.script_pubkey())
						.add_recipient(
							change_address_info.address.script_pubkey(),
							Amount::from_sat(cur_anchor_reserve_sats),
						)
						.fee_rate(fee_rate);

					// Add manual UTXOs to temporary transaction if specified
					if let Some(ref outpoints) = utxos_to_spend {
						for outpoint in outpoints {
							tmp_tx_builder.add_utxo(*outpoint).map_err(|e| {
								log_error!(
									self.logger,
									"Failed to add UTXO {:?} to temp tx: {}",
									outpoint,
									e
								);
								Error::OnchainTxCreationFailed
							})?;
						}
						tmp_tx_builder.manually_selected_only();
					}

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

				let base_fee = primary.calculate_fee(&tmp_tx).map_err(|e| {
					log_error!(
						self.logger,
						"Failed to calculate fee of temporary transaction: {}",
						e
					);
					e
				})?;

				// 'cancel' the transaction to free up any used change addresses
				primary.cancel_tx(&tmp_tx);

				// Adjust the fee estimate for non-primary inputs that will be
				// added to the actual tx (the temp tx only used primary UTXOs).
				let extra_input_weight: u64 = non_primary_utxo_infos
					.as_ref()
					.map(|infos| infos.iter().map(|i| i.weight.to_wu()).sum::<u64>())
					.unwrap_or(0);
				let extra_input_fee = fee_rate
					.fee_wu(bitcoin::Weight::from_wu(extra_input_weight))
					.unwrap_or(Amount::ZERO);
				let estimated_tx_fee = base_fee + extra_input_fee;

				let estimated_spendable_amount = Amount::from_sat(
					spendable_amount_sats.saturating_sub(estimated_tx_fee.to_sat()),
				);

				if estimated_spendable_amount < Amount::from_sat(DUST_LIMIT_SATS) {
					log_error!(self.logger,
					"Unable to send payment without infringing on Anchor reserves. Available: {}sats, estimated fee required: {}sats.",
					spendable_amount_sats,
					estimated_tx_fee,
				);
					return Err(Error::InsufficientFunds);
				}

				let mut tx_builder = primary.build_tx();
				tx_builder
					.add_recipient(address.script_pubkey(), estimated_spendable_amount)
					.fee_absolute(estimated_tx_fee);
				tx_builder
			},
			OnchainSendAmount::AllDrainingReserve
			| OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats: _ } => {
				let mut tx_builder = primary.build_tx();
				tx_builder.drain_wallet().drain_to(address.script_pubkey()).fee_rate(fee_rate);
				tx_builder
			},
		};

		if let Some(ref utxo_infos) = manual_utxo_infos {
			bdk_wallet_aggregate::utxo::add_utxos_to_tx_builder(&mut tx_builder, utxo_infos)
				.map_err(|e| {
					log_error!(self.logger, "Failed to add manually selected UTXOs: {}", e);
					Error::OnchainTxCreationFailed
				})?;
			tx_builder.manually_selected_only();
		}

		if let Some(ref infos) = non_primary_utxo_infos {
			bdk_wallet_aggregate::utxo::add_utxos_to_tx_builder(&mut tx_builder, infos).map_err(
				|e| {
					log_error!(self.logger, "Failed to add cross-wallet UTXOs: {}", e);
					Error::OnchainTxCreationFailed
				},
			)?;
		}

		let psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				let can_retry =
					matches!(send_amount, OnchainSendAmount::ExactRetainingReserve { .. })
						&& manual_utxo_infos.is_none()
						&& non_primary_utxo_infos.is_none();

				if can_retry {
					let amount_sats = match send_amount {
						OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => amount_sats,
						_ => unreachable!(),
					};
					locked_wallet
						.build_psbt_with_cross_wallet_fallback(
							address.script_pubkey(),
							Amount::from_sat(amount_sats),
							fee_rate,
							&funding_txids,
							bdk_wallet_aggregate::CoinSelectionAlgorithm::BranchAndBound,
						)
						.map_err(|e| {
							log_error!(self.logger, "Failed to create transaction: {}", e);
							match e {
								bdk_wallet_aggregate::Error::InsufficientFunds => {
									Error::InsufficientFunds
								},
								_ => Error::OnchainTxCreationFailed,
							}
						})?
				} else {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				}
			},
		};

		// Check the reserve requirements (again) and return an error if they aren't met.
		match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats } => {
				let spendable_amount_sats = self
					.get_balances_inner(&aggregate_balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				let tx_fee_sats =
					locked_wallet.calculate_fee_with_fallback(&psbt).map_err(|e| {
						log_error!(
							self.logger,
							"Failed to calculate fee of candidate transaction: {}",
							e
						);
						Error::WalletOperationFailed
					})?;
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
				let spendable_amount_sats = self
					.get_balances_inner(&aggregate_balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				let drain_amount = locked_wallet.drain_amount_from_psbt(&psbt);
				if spendable_amount_sats < drain_amount {
					log_error!(self.logger,
						"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}sats",
						spendable_amount_sats,
						drain_amount,
					);
					return Err(Error::InsufficientFunds);
				}
			},
			_ => {},
		}

		Ok((psbt, locked_wallet))
	}

	pub(crate) fn calculate_transaction_fee(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<u64, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let (psbt, locked_wallet) = self.build_transaction_psbt(
			address,
			send_amount,
			fee_rate,
			utxos_to_spend,
			channel_manager,
		)?;

		let calculated_fee = locked_wallet.calculate_fee_with_fallback(&psbt).map_err(|e| {
			log_error!(self.logger, "Failed to calculate transaction fee: {}", e);
			Error::WalletOperationFailed
		})?;

		log_info!(
			self.logger,
			"Calculated transaction fee: {}sats for sending to address {}",
			calculated_fee,
			address
		);

		Ok(calculated_fee)
	}

	#[allow(deprecated)]
	pub(crate) fn send_to_address(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<Txid, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let is_drain_all = match send_amount {
			OnchainSendAmount::AllDrainingReserve => true,
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
				cur_anchor_reserve_sats <= DUST_LIMIT_SATS
			},
			_ => false,
		};

		let tx = if is_drain_all && utxos_to_spend.is_none() {
			let mut locked_wallet = self.inner.lock().unwrap();
			let tx = locked_wallet
				.build_and_sign_drain(address.script_pubkey(), fee_rate)
				.map_err(|e| {
					log_error!(self.logger, "Failed to drain wallets: {}", e);
					Error::OnchainTxCreationFailed
				})?;
			locked_wallet.persist_all().map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;
			tx
		} else {
			let (psbt, mut locked_wallet) = self.build_transaction_psbt(
				address,
				send_amount,
				fee_rate,
				utxos_to_spend,
				channel_manager,
			)?;

			let tx = locked_wallet.sign_psbt_all(psbt).map_err(|e| {
				log_error!(self.logger, "Failed to sign transaction: {}", e);
				Error::OnchainTxSigningFailed
			})?;

			locked_wallet.persist_all().map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;
			tx
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

	pub(crate) fn select_confirmed_utxos(
		&self, must_spend: Vec<Input>, must_pay_to: &[TxOut], fee_rate: FeeRate,
	) -> Result<Vec<FundingTxInput>, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();

		// Splicing requires native witness (P2WPKH/P2TR) primary because
		// FundingTxInput only supports native witness script types.
		if !locked_wallet.primary_key().is_native_witness() {
			log_error!(
				self.logger,
				"Splicing requires a native witness primary wallet (NativeSegwit or Taproot)"
			);
			return Err(());
		}

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

		for u in locked_wallet.list_confirmed_unspent() {
			let script_pubkey = u.txout.script_pubkey.clone();
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
					let script_bytes = script_pubkey.as_bytes();
					if script_pubkey.is_p2pkh() {
						let pkh = PubkeyHash::from_slice(&script_bytes[3..23]).map_err(|e| {
							log_error!(self.logger, "Failed to extract PubkeyHash: {}", e);
						})?;
						utxos.push(Utxo::new_p2pkh(u.outpoint, u.txout.value, &pkh));
					} else if script_pubkey.is_p2sh() {
						if let Some(wpkh) = locked_wallet.derive_wpkh_for_p2sh(&u) {
							utxos.push(Utxo::new_nested_p2wpkh(u.outpoint, u.txout.value, &wpkh));
						} else {
							log_debug!(
								self.logger,
								"Skipping P2SH UTXO {:?}: could not derive inner WPubkeyHash",
								u.outpoint
							);
						}
					} else {
						log_debug!(
							self.logger,
							"Skipping non-standard non-witness UTXO {:?}",
							u.outpoint
						);
					}
				},
			}
		}

		Ok(utxos)
	}

	#[allow(deprecated)]
	fn get_change_script_inner(&self) -> Result<ScriptBuf, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_internal_address().map(|addr| addr.script_pubkey()).map_err(|e| {
			log_error!(self.logger, "Failed to get change script: {}", e);
		})
	}

	pub(crate) fn sign_owned_inputs(&self, unsigned_tx: Transaction) -> Result<Transaction, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.sign_owned_inputs(unsigned_tx).map_err(|e| {
			log_error!(self.logger, "Failed to sign transaction: {}", e);
		})
	}

	fn sign_psbt_inner(&self, psbt: Psbt) -> Result<Transaction, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.sign_psbt_all(psbt).map_err(|e| {
			log_error!(self.logger, "Failed to sign PSBT: {}", e);
		})
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
		if height > 0
			&& (pre_checkpoint.height() != height - 1
				|| pre_checkpoint.hash() != block.header.prev_blockhash)
		{
			log_debug!(
				self.logger,
				"Detected reorg while applying a connected block to on-chain wallet: new block with hash {} at height {}",
				block.header.block_hash(),
				height
			);
		}

		match locked_wallet.apply_block(block, height) {
			Ok(_all_txids) => {
				if let Err(e) = self.update_payment_store(&locked_wallet) {
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
	) -> Pin<Box<dyn Future<Output = Result<Vec<Utxo>, ()>> + Send + 'a>> {
		Box::pin(async move { self.list_confirmed_utxos_inner() })
	}

	fn get_change_script<'a>(
		&'a self,
	) -> Pin<Box<dyn Future<Output = Result<ScriptBuf, ()>> + Send + 'a>> {
		Box::pin(async move { self.get_change_script_inner() })
	}

	fn sign_psbt<'a>(
		&'a self, psbt: Psbt,
	) -> Pin<Box<dyn Future<Output = Result<Transaction, ()>> + Send + 'a>> {
		Box::pin(async move { self.sign_psbt_inner(psbt) })
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
		let address = self.wallet.get_new_witness_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new witness address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		let address = self.wallet.get_new_witness_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new witness address from wallet: {}", e);
		})?;

		match address.witness_program() {
			Some(program) => ShutdownScript::new_witness_program(&program).map_err(|e| {
				log_error!(self.logger, "Invalid shutdown script: {:?}", e);
			}),
			_ => {
				log_error!(
					self.logger,
					"get_shutdown_scriptpubkey received a non-native-witness address. \
					 This is a bug in get_new_witness_address."
				);
				Err(())
			},
		}
	}
}

impl ChangeDestinationSource for WalletKeysManager {
	fn get_change_destination_script<'a>(
		&'a self,
	) -> Pin<Box<dyn Future<Output = Result<ScriptBuf, ()>> + Send + 'a>> {
		let wallet = Arc::clone(&self.wallet);
		let logger = Arc::clone(&self.logger);
		Box::pin(async move {
			wallet
				.get_new_internal_address()
				.map_err(|e| {
					log_error!(logger, "Failed to retrieve new address from wallet: {}", e);
				})
				.map(|addr| addr.script_pubkey())
				.map_err(|_| ())
		})
	}
}
