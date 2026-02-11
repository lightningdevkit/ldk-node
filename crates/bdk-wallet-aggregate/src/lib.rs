// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! A library that aggregates multiple BDK wallets into a single logical wallet
//! with unified balance, UTXO management, transaction building, and signing.
//!
//! # Overview
//!
//! [`AggregateWallet`] wraps a *primary* BDK wallet and zero or more
//! *secondary* wallets, keyed by a user-defined type `K` (e.g. an address-type
//! enum).  The primary wallet is used for generating new addresses and change
//! outputs; secondary wallets are monitored for existing funds and their UTXOs
//! participate in transaction construction and signing.

pub mod rbf;
pub mod signing;
pub mod types;
pub mod utxo;

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_wallet::event::WalletEvent;
use bdk_wallet::{Balance, KeychainKind, LocalOutput, PersistedWallet, Update, WalletPersister};
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash as _;
use bitcoin::psbt::Psbt;
use bitcoin::{
	Address, Amount, Block, BlockHash, FeeRate, OutPoint, Script, ScriptBuf, Transaction, Txid,
	WPubkeyHash,
};
pub use types::{CoinSelectionAlgorithm, Error, UtxoPsbtInfo};

/// A wallet aggregator that presents multiple BDK wallets as a single logical
/// wallet.
///
/// Generic over:
/// * `K` – the key type used to identify individual wallets (e.g. an
///   `AddressType` enum).
/// * `P` – the BDK `WalletPersister` implementation.
pub struct AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	wallets: HashMap<K, PersistedWallet<P>>,
	persisters: HashMap<K, P>,
	primary: K,
}

/// Delegates to the primary wallet.
impl<K, P> Deref for AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	type Target = PersistedWallet<P>;

	fn deref(&self) -> &Self::Target {
		self.wallets.get(&self.primary).expect("Primary wallet must always exist")
	}
}

impl<K, P> DerefMut for AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	fn deref_mut(&mut self) -> &mut Self::Target {
		self.wallets.get_mut(&self.primary).expect("Primary wallet must always exist")
	}
}

impl<K, P> AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
	P::Error: std::fmt::Display,
{
	/// Create a new aggregate wallet.
	///
	/// * `primary_wallet` / `primary_persister` – the wallet used for new
	///   address generation and change outputs.
	/// * `primary_key` – the key identifying the primary wallet.
	/// * `additional_wallets` – secondary wallets to monitor and use for
	///   transaction construction.
	pub fn new(
		primary_wallet: PersistedWallet<P>, primary_persister: P, primary_key: K,
		additional_wallets: Vec<(K, PersistedWallet<P>, P)>,
	) -> Self {
		let mut wallets = HashMap::new();
		let mut persisters = HashMap::new();

		wallets.insert(primary_key, primary_wallet);
		persisters.insert(primary_key, primary_persister);

		for (key, wallet, persister) in additional_wallets {
			wallets.insert(key, wallet);
			persisters.insert(key, persister);
		}

		Self { wallets, persisters, primary: primary_key }
	}

	// ─── Accessors ──────────────────────────────────────────────────────

	/// The primary wallet key.
	pub fn primary_key(&self) -> K {
		self.primary
	}

	/// All loaded wallet keys (primary + monitored).
	pub fn loaded_keys(&self) -> Vec<K> {
		self.wallets.keys().copied().collect()
	}

	/// Immutable access to the underlying wallet map.
	pub fn wallets(&self) -> &HashMap<K, PersistedWallet<P>> {
		&self.wallets
	}

	/// Mutable access to the underlying wallet map.
	pub fn wallets_mut(&mut self) -> &mut HashMap<K, PersistedWallet<P>> {
		&mut self.wallets
	}

	/// Immutable access to the persister map.
	pub fn persisters(&self) -> &HashMap<K, P> {
		&self.persisters
	}

	/// Mutable access to the underlying persister map.
	pub fn persisters_mut(&mut self) -> &mut HashMap<K, P> {
		&mut self.persisters
	}

	/// Get a reference to a specific wallet.
	pub fn wallet(&self, key: &K) -> Option<&PersistedWallet<P>> {
		self.wallets.get(key)
	}

	/// Get a mutable reference to a specific wallet.
	pub fn wallet_mut(&mut self, key: &K) -> Option<&mut PersistedWallet<P>> {
		self.wallets.get_mut(key)
	}

	/// Get a reference to the primary wallet.
	pub fn primary_wallet(&self) -> &PersistedWallet<P> {
		self.wallets.get(&self.primary).expect("Primary wallet must always exist")
	}

	/// Get a mutable reference to the primary wallet.
	pub fn primary_wallet_mut(&mut self) -> &mut PersistedWallet<P> {
		self.wallets.get_mut(&self.primary).expect("Primary wallet must always exist")
	}

	/// Get a mutable reference to the primary persister.
	pub fn primary_persister_mut(&mut self) -> &mut P {
		self.persisters.get_mut(&self.primary).expect("Primary persister must always exist")
	}

	// ─── Balance ────────────────────────────────────────────────────────

	/// Aggregate balance across all wallets.
	pub fn balance(&self) -> Balance {
		let mut total = Balance::default();
		for wallet in self.wallets.values() {
			let balance = wallet.balance();
			total.confirmed += balance.confirmed;
			total.trusted_pending += balance.trusted_pending;
			total.untrusted_pending += balance.untrusted_pending;
			total.immature += balance.immature;
		}
		total
	}

	/// Balance for a single wallet identified by key.
	pub fn balance_for(&self, key: &K) -> Result<Balance, Error> {
		self.wallets.get(key).map(|w| w.balance()).ok_or(Error::WalletNotFound)
	}

	// ─── UTXO Listing ───────────────────────────────────────────────────

	/// List all unspent outputs across every wallet.
	pub fn list_unspent(&self) -> Vec<LocalOutput> {
		self.wallets.values().flat_map(|w| w.list_unspent()).collect()
	}

	/// List confirmed unspent outputs across all wallets.
	///
	/// Only returns UTXOs whose creating transaction is confirmed in at
	/// least one wallet.
	pub fn list_confirmed_unspent(&self) -> Vec<LocalOutput> {
		let mut confirmed_txids = HashSet::new();
		for wallet in self.wallets.values() {
			for t in wallet.transactions().filter(|t| t.chain_position.is_confirmed()) {
				confirmed_txids.insert(t.tx_node.txid);
			}
		}

		self.list_unspent()
			.into_iter()
			.filter(|u| confirmed_txids.contains(&u.outpoint.txid))
			.collect()
	}

	/// Derive the inner `WPubkeyHash` for a P2SH-wrapped P2WPKH UTXO.
	///
	/// For NestedSegwit wallets (BIP-49) the descriptor is `Sh(Wpkh(...))`.
	/// This method finds the wallet that owns `utxo`, derives the script at
	/// the UTXO's derivation index, and extracts the 20-byte witness
	/// program hash from the inner redeemScript (`OP_0 <20-byte-wpkh>`).
	///
	/// Returns `None` if the UTXO is not owned by any wallet or if the
	/// inner script cannot be parsed as P2WPKH.
	pub fn derive_wpkh_for_p2sh(&self, utxo: &LocalOutput) -> Option<WPubkeyHash> {
		for wallet in self.wallets.values() {
			if wallet.get_utxo(utxo.outpoint).is_some() {
				let descriptor = wallet.public_descriptor(utxo.keychain);
				if let Ok(derived_desc) = descriptor.at_derivation_index(utxo.derivation_index) {
					// For Sh(Wpkh(..)) descriptors, `explicit_script()` gives
					// the inner P2WPKH redeemScript: OP_0 <20-byte-wpkh>.
					if let Ok(explicit) = derived_desc.explicit_script() {
						if explicit.len() == 22
							&& explicit.as_bytes()[0] == 0x00
							&& explicit.as_bytes()[1] == 0x14
						{
							return WPubkeyHash::from_slice(&explicit.as_bytes()[2..22]).ok();
						}
					}
				}
				break;
			}
		}
		None
	}

	// ─── Transaction Lookup ─────────────────────────────────────────────

	/// Find which wallet key contains a transaction.
	pub fn find_wallet_for_tx(&self, txid: Txid) -> Option<K> {
		self.wallets.iter().find_map(|(key, wallet)| wallet.get_tx(txid).map(|_| *key))
	}

	/// Find a transaction across all wallets.
	pub fn find_tx(&self, txid: Txid) -> Option<Transaction> {
		for wallet in self.wallets.values() {
			if let Some(tx_node) = wallet.get_tx(txid) {
				return Some((*tx_node.tx_node.tx).clone());
			}
		}
		None
	}

	/// Check whether a transaction is confirmed in any wallet.
	pub fn is_tx_confirmed(&self, txid: &Txid) -> bool {
		for wallet in self.wallets.values() {
			if let Some(tx_node) = wallet.get_tx(*txid) {
				if tx_node.chain_position.is_confirmed() {
					return true;
				}
			}
		}
		false
	}

	/// Aggregated sent and received amounts for a transaction across all wallets.
	pub fn sent_and_received(&self, txid: Txid) -> Option<(u64, u64)> {
		let tx = self.find_tx(txid)?;
		let mut total_sent = 0u64;
		let mut total_received = 0u64;
		for wallet in self.wallets.values() {
			if wallet.get_tx(txid).is_some() {
				let (sent, received) = wallet.sent_and_received(&tx);
				total_sent += sent.to_sat();
				total_received += received.to_sat();
			}
		}
		Some((total_sent, total_received))
	}

	/// Collect all cached transactions from all wallets, deduplicated by txid.
	pub fn cached_txs(&self) -> Vec<Arc<Transaction>> {
		let mut seen = HashSet::new();
		self.wallets
			.values()
			.flat_map(|w| w.tx_graph().full_txs())
			.filter(|tx_node| seen.insert(tx_node.txid))
			.map(|tx_node| tx_node.tx)
			.collect()
	}

	/// Collect all unconfirmed transaction IDs across wallets (deduplicated).
	pub fn unconfirmed_txids(&self) -> Vec<Txid> {
		let mut seen = HashSet::new();
		self.wallets
			.values()
			.flat_map(|w| {
				w.transactions()
					.filter(|t| t.chain_position.is_unconfirmed())
					.map(|t| t.tx_node.txid)
			})
			.filter(|txid| seen.insert(*txid))
			.collect()
	}

	/// All transaction IDs across all wallets.
	pub fn all_txids(&self) -> Vec<Txid> {
		self.wallets.values().flat_map(|w| w.transactions().map(|wtx| wtx.tx_node.txid)).collect()
	}

	// ─── Chain Tip ──────────────────────────────────────────────────────

	/// The latest checkpoint from the primary wallet, returned as
	/// `(block_hash, height)`.
	pub fn current_best_block(&self) -> (BlockHash, u32) {
		let checkpoint = self.primary_wallet().latest_checkpoint();
		(checkpoint.hash(), checkpoint.height())
	}

	// ─── Address Generation ─────────────────────────────────────────────

	/// Generate a new receiving address from the primary wallet.
	pub fn new_address(&mut self) -> Result<Address, Error> {
		let key = self.primary;
		self.new_address_for(&key)
	}

	/// Generate a new receiving address for a specific wallet.
	pub fn new_address_for(&mut self, key: &K) -> Result<Address, Error> {
		let wallet = self.wallets.get_mut(key).ok_or(Error::WalletNotFound)?;
		let persister = self.persisters.get_mut(key).ok_or(Error::PersisterNotFound)?;

		let address_info = wallet.reveal_next_address(KeychainKind::External);
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet for {:?}: {}", key, e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
	}

	/// Generate a new internal (change) address from the primary wallet.
	pub fn new_internal_address(&mut self) -> Result<Address, Error> {
		let primary = self.primary;
		let wallet = self.wallets.get_mut(&primary).ok_or(Error::WalletNotFound)?;
		let persister = self.persisters.get_mut(&primary).ok_or(Error::PersisterNotFound)?;

		let address_info = wallet.next_unused_address(KeychainKind::Internal);
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
	}

	// ─── Transaction Cancellation ───────────────────────────────────────

	/// Cancel a transaction in all wallets that know about it.
	pub fn cancel_tx(&mut self, tx: &Transaction) -> Result<(), Error> {
		for (key, wallet) in self.wallets.iter_mut() {
			wallet.cancel_tx(tx);
			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}
		Ok(())
	}

	// ─── Fee Calculation ────────────────────────────────────────────────

	/// Calculate the fee of a PSBT by summing input values and subtracting
	/// output values.
	pub fn calculate_fee_from_psbt(&self, psbt: &Psbt) -> Result<u64, Error> {
		let mut total_input_value = 0u64;

		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			if let Some(psbt_input) = psbt.inputs.get(i) {
				if let Some(witness_utxo) = &psbt_input.witness_utxo {
					total_input_value += witness_utxo.value.to_sat();
				} else if let Some(non_witness_tx) = &psbt_input.non_witness_utxo {
					if let Some(txout) =
						non_witness_tx.output.get(txin.previous_output.vout as usize)
					{
						total_input_value += txout.value.to_sat();
					} else {
						return Err(Error::OnchainTxCreationFailed);
					}
				} else {
					let mut found = false;
					for wallet in self.wallets.values() {
						if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
							total_input_value += local_utxo.txout.value.to_sat();
							found = true;
							break;
						}
					}
					if !found {
						return Err(Error::OnchainTxCreationFailed);
					}
				}
			} else {
				let mut found = false;
				for wallet in self.wallets.values() {
					if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
						total_input_value += local_utxo.txout.value.to_sat();
						found = true;
						break;
					}
				}
				if !found {
					return Err(Error::OnchainTxCreationFailed);
				}
			}
		}

		let total_output_value: u64 =
			psbt.unsigned_tx.output.iter().map(|txout| txout.value.to_sat()).sum();

		Ok(total_input_value.saturating_sub(total_output_value))
	}

	/// Calculate fee from PSBT with fallback to primary wallet calculation.
	pub fn calculate_fee_with_fallback(&self, psbt: &Psbt) -> Result<u64, Error> {
		self.calculate_fee_from_psbt(psbt).or_else(|_| {
			self.primary_wallet()
				.calculate_fee(&psbt.unsigned_tx)
				.map(|f| f.to_sat())
				.map_err(|_| Error::OnchainTxCreationFailed)
		})
	}

	/// Calculate the drain amount from a PSBT using sent_and_received across
	/// all wallets. Returns the net outgoing amount (sent - received) in sats.
	///
	/// For not-yet-broadcast PSBTs the transaction won't be in any wallet's
	/// tx-graph, so we compute sent/received directly from the unsigned tx
	/// against every wallet's script set.
	pub fn drain_amount_from_psbt(&self, psbt: &Psbt) -> u64 {
		let mut total_sent = Amount::ZERO;
		let mut total_received = Amount::ZERO;
		for wallet in self.wallets.values() {
			let (s, r) = wallet.sent_and_received(&psbt.unsigned_tx);
			total_sent += s;
			total_received += r;
		}
		total_sent.to_sat().saturating_sub(total_received.to_sat())
	}

	// ─── Sync ───────────────────────────────────────────────────────────

	/// Build sync requests for a specific wallet.
	#[allow(clippy::type_complexity)]
	pub fn wallet_sync_request(
		&self, key: &K,
	) -> Result<(FullScanRequest<KeychainKind>, SyncRequest<(KeychainKind, u32)>), Error> {
		let wallet = self.wallets.get(key).ok_or(Error::WalletNotFound)?;
		let full_scan = wallet.start_full_scan().build();
		let incremental_sync = wallet.start_sync_with_revealed_spks().build();
		Ok((full_scan, incremental_sync))
	}

	/// Apply a chain update to the primary wallet.
	///
	/// Returns the wallet events and a list of all transaction IDs in the
	/// primary wallet (so the caller can update its payment store).
	pub fn apply_update(
		&mut self, update: impl Into<Update>,
	) -> Result<(Vec<WalletEvent>, Vec<Txid>), Error> {
		let update = update.into();
		let primary = self.primary;
		let wallet = self.wallets.get_mut(&primary).ok_or(Error::WalletNotFound)?;

		let events = wallet.apply_update_events(update).map_err(|e| {
			log::error!("Failed to apply update to primary wallet: {}", e);
			Error::WalletOperationFailed
		})?;

		let persister = self.persisters.get_mut(&primary).ok_or(Error::PersisterNotFound)?;
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist primary wallet: {}", e);
			Error::PersistenceFailed
		})?;

		let txids: Vec<Txid> = wallet.transactions().map(|wtx| wtx.tx_node.txid).collect();

		Ok((events, txids))
	}

	/// Apply a chain update to a specific wallet.
	///
	/// Returns the wallet events and a list of all transaction IDs in that
	/// wallet.
	pub fn apply_update_to_wallet(
		&mut self, key: K, update: impl Into<Update>,
	) -> Result<(Vec<WalletEvent>, Vec<Txid>), Error> {
		let update = update.into();
		let wallet = self.wallets.get_mut(&key).ok_or(Error::WalletNotFound)?;

		let events = wallet.apply_update_events(update).map_err(|e| {
			log::error!("Failed to apply update to wallet {:?}: {}", key, e);
			Error::WalletOperationFailed
		})?;

		let persister = self.persisters.get_mut(&key).ok_or(Error::PersisterNotFound)?;
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet {:?}: {}", key, e);
			Error::PersistenceFailed
		})?;

		let txids: Vec<Txid> = wallet.transactions().map(|wtx| wtx.tx_node.txid).collect();

		Ok((events, txids))
	}

	/// Apply mempool (unconfirmed) transactions to all wallets.
	pub fn apply_mempool_txs(
		&mut self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		for (key, wallet) in self.wallets.iter_mut() {
			wallet.apply_unconfirmed_txs(unconfirmed_txs.clone());
			wallet.apply_evicted_txs(evicted_txids.clone());

			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}
		Ok(())
	}

	/// Apply a connected block to all wallets and persist.
	///
	/// Returns the set of all transaction IDs across all wallets.
	pub fn apply_block(&mut self, block: &Block, height: u32) -> Result<HashSet<Txid>, Error> {
		let pre_checkpoint = self.primary_wallet().latest_checkpoint();
		if height > 0
			&& (pre_checkpoint.height() != height - 1
				|| pre_checkpoint.hash() != block.header.prev_blockhash)
		{
			log::debug!("Detected reorg while applying connected block at height {}", height);
		}

		for (key, wallet) in self.wallets.iter_mut() {
			wallet.apply_block(block, height).map_err(|e| {
				log::error!("Failed to apply connected block to wallet {:?}: {}", key, e);
				Error::WalletOperationFailed
			})?;

			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}

		let mut all_txids = HashSet::new();
		for wallet in self.wallets.values() {
			for wtx in wallet.transactions() {
				all_txids.insert(wtx.tx_node.txid);
			}
		}

		Ok(all_txids)
	}

	// ─── UTXO Preparation Helpers ───────────────────────────────────────

	/// Prepare local outputs for cross-wallet PSBT building.
	pub fn prepare_utxos_for_psbt(
		&self, utxos: &[LocalOutput],
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		utxo::prepare_utxos_for_psbt(utxos, &self.wallets, &self.primary)
	}

	/// Prepare outpoints for cross-wallet PSBT building.
	pub fn prepare_outpoints_for_psbt(
		&self, outpoints: &[OutPoint],
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		utxo::prepare_outpoints_for_psbt(outpoints, &self.wallets, &self.primary)
	}

	// ─── Signing ────────────────────────────────────────────────────────

	/// Sign an unsigned transaction using all wallets that own inputs.
	pub fn sign_owned_inputs(&mut self, unsigned_tx: Transaction) -> Result<Transaction, Error> {
		signing::sign_owned_inputs(unsigned_tx, &mut self.wallets)
	}

	/// Sign a PSBT using all wallets that own inputs.
	pub fn sign_psbt_all(&mut self, psbt: Psbt) -> Result<Transaction, Error> {
		signing::sign_psbt_all_wallets(psbt, &mut self.wallets)
	}

	// ─── Cross-wallet RBF ───────────────────────────────────────────────

	/// Build a cross-wallet RBF replacement, adding extra inputs if needed.
	///
	/// 1. Tries adjusting the change output only (no new inputs).
	/// 2. If that is insufficient, selects minimum additional UTXOs from all
	///    wallets and retries.
	fn build_cross_wallet_rbf_with_fallback(
		&mut self, original_tx: &Transaction, new_fee_rate: FeeRate,
	) -> Result<Transaction, Error> {
		// Attempt 1: change-only bump.
		match rbf::build_cross_wallet_rbf(&mut self.wallets, original_tx, new_fee_rate, &[]) {
			Ok(tx) => return Ok(tx),
			Err(Error::InsufficientFunds) => {},
			Err(e) => return Err(e),
		}

		// Attempt 2: select minimum additional UTXOs.
		let original_fee = self.calculate_tx_fee(original_tx)?;
		let new_fee = new_fee_rate.fee_wu(original_tx.weight()).unwrap_or(Amount::ZERO);
		let fee_increase = new_fee.checked_sub(original_fee).unwrap_or(Amount::ZERO);

		// Change value absorbable from the original tx.
		let change_value: Amount = original_tx
			.output
			.iter()
			.filter(|out| {
				self.wallets.values().any(|w| {
					w.list_unspent().any(|u| {
						u.keychain == KeychainKind::Internal
							&& u.txout.script_pubkey == out.script_pubkey
					})
				})
			})
			.map(|out| out.value)
			.sum();

		let deficit = fee_increase.checked_sub(change_value).unwrap_or(Amount::ZERO);

		// Collect UTXOs from all wallets, excluding those already in the
		// original tx and outputs created by the original tx.
		let original_outpoints: HashSet<OutPoint> =
			original_tx.input.iter().map(|i| i.previous_output).collect();
		let original_txid = original_tx.compute_txid();

		let available: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !original_outpoints.contains(&u.outpoint))
			.filter(|u| u.outpoint.txid != original_txid)
			.collect();

		if available.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		let drain_script =
			self.primary_wallet().peek_address(KeychainKind::Internal, 0).address.script_pubkey();

		let selected = utxo::select_utxos_with_algorithm(
			deficit.to_sat(),
			available,
			new_fee_rate,
			CoinSelectionAlgorithm::BranchAndBound,
			&drain_script,
			&[],
			&self.wallets,
		)?;

		let extra_infos = self.prepare_outpoints_for_psbt(&selected)?;
		if extra_infos.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		rbf::build_cross_wallet_rbf(&mut self.wallets, original_tx, new_fee_rate, &extra_infos)
	}

	/// Look up an input value across local wallets.
	pub fn get_input_value(&self, outpoint: &OutPoint) -> Result<u64, Error> {
		rbf::get_input_value(outpoint, &self.wallets)
	}

	// ─── Coin Selection ─────────────────────────────────────────────────

	/// Run coin selection across all wallets.
	pub fn select_utxos(
		&self, target_amount: u64, available_utxos: Vec<LocalOutput>, fee_rate: FeeRate,
		algorithm: CoinSelectionAlgorithm, drain_script: &Script, excluded_outpoints: &[OutPoint],
	) -> Result<Vec<OutPoint>, Error> {
		utxo::select_utxos_with_algorithm(
			target_amount,
			available_utxos,
			fee_rate,
			algorithm,
			drain_script,
			excluded_outpoints,
			&self.wallets,
		)
	}

	// ─── Fee Calculation ─────────────────────────────────────────────────

	/// Calculate the fee of a transaction by looking up input values across
	/// all wallets and subtracting total output values.
	///
	/// Tries BDK's native `calculate_fee` on each wallet first (works
	/// reliably for transactions that were built by that wallet), then
	/// falls back to manual input-value lookup across all wallets.
	pub fn calculate_tx_fee(&self, tx: &Transaction) -> Result<Amount, Error> {
		// Fast path: BDK knows the fee for transactions it built.
		for wallet in self.wallets.values() {
			if let Ok(fee) = wallet.calculate_fee(tx) {
				return Ok(fee);
			}
		}

		// Slow path: manually look up each input value.
		let mut total_input = 0u64;
		for txin in &tx.input {
			total_input += self.get_input_value(&txin.previous_output)?;
		}
		let total_output: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
		Ok(Amount::from_sat(total_input.saturating_sub(total_output)))
	}

	// ─── High-Level Helpers ─────────────────────────────────────────────

	/// Return prepared PSBT info for all non-primary UTXOs, suitable for
	/// adding as foreign inputs to a transaction built on the primary wallet.
	///
	/// `excluded_txids` allows the caller to filter out UTXOs belonging to
	/// specific transactions (e.g. channel funding transactions).
	pub fn non_primary_foreign_utxos(
		&self, excluded_txids: &HashSet<Txid>,
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		let primary_outpoints: HashSet<OutPoint> =
			self.primary_wallet().list_unspent().map(|u| u.outpoint).collect();

		let non_primary: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !primary_outpoints.contains(&u.outpoint))
			.filter(|u| !excluded_txids.contains(&u.outpoint.txid))
			.collect();

		if non_primary.is_empty() {
			return Ok(Vec::new());
		}

		self.prepare_utxos_for_psbt(&non_primary)
	}

	/// Select the minimum set of non-primary foreign UTXOs needed to cover
	/// `deficit`, using the given coin selection algorithm.
	///
	/// * `segwit_only` – if `true`, excludes bare P2PKH UTXOs (includes
	///   P2SH-P2WPKH / NestedSegwit since those carry witness data).
	/// * `algorithm` – the coin selection strategy to use.
	///
	/// Returns prepared `UtxoPsbtInfo` entries ready to add as foreign
	/// inputs, or an empty vec if no non-primary UTXOs are available.
	pub fn select_non_primary_foreign_utxos(
		&self, deficit: Amount, fee_rate: FeeRate, excluded_txids: &HashSet<Txid>,
		segwit_only: bool, algorithm: CoinSelectionAlgorithm,
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		let primary_outpoints: HashSet<OutPoint> =
			self.primary_wallet().list_unspent().map(|u| u.outpoint).collect();

		let non_primary: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !primary_outpoints.contains(&u.outpoint))
			.filter(|u| !excluded_txids.contains(&u.outpoint.txid))
			.filter(|u| {
				!segwit_only
					|| u.txout.script_pubkey.witness_version().is_some()
					|| u.txout.script_pubkey.is_p2sh()
			})
			.collect();

		if non_primary.is_empty() {
			return Ok(Vec::new());
		}

		let drain_script =
			self.primary_wallet().peek_address(KeychainKind::Internal, 0).address.script_pubkey();

		let selected_outpoints = utxo::select_utxos_with_algorithm(
			deficit.to_sat(),
			non_primary,
			fee_rate,
			algorithm,
			&drain_script,
			&[],
			&self.wallets,
		)?;

		self.prepare_outpoints_for_psbt(&selected_outpoints)
	}

	/// Build a transaction using unified coin selection across all eligible
	/// wallets.
	///
	/// Pools UTXOs from every wallet that passes `utxo_filter`, runs coin
	/// selection once on the combined set, then builds a PSBT on the
	/// `build_key` wallet with primary UTXOs as native inputs and foreign
	/// UTXOs via `add_foreign_utxo`. Signs with all wallets and persists.
	#[allow(deprecated, clippy::too_many_arguments)]
	fn build_tx_unified(
		&mut self, build_key: &K, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate,
		locktime: Option<LockTime>, excluded_txids: &HashSet<Txid>,
		algorithm: CoinSelectionAlgorithm, utxo_filter: impl Fn(&LocalOutput) -> bool,
	) -> Result<Psbt, Error> {
		let all_utxos: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !excluded_txids.contains(&u.outpoint.txid))
			.filter(&utxo_filter)
			.collect();

		if all_utxos.is_empty() {
			return Err(Error::NoSpendableOutputs);
		}

		let drain_script = self
			.wallet(build_key)
			.ok_or(Error::WalletNotFound)?
			.peek_address(KeychainKind::Internal, 0)
			.address
			.script_pubkey();

		let selected = utxo::select_utxos_with_algorithm(
			amount.to_sat(),
			all_utxos,
			fee_rate,
			algorithm,
			&drain_script,
			&[],
			&self.wallets,
		)?;

		let infos = self.prepare_outpoints_for_psbt(&selected)?;
		if infos.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		let w = self.wallets.get_mut(build_key).ok_or(Error::WalletNotFound)?;
		let mut b = w.build_tx();
		b.add_recipient(output_script, amount).fee_rate(fee_rate);
		if let Some(lt) = locktime {
			b.nlocktime(lt);
		}
		utxo::add_utxos_to_tx_builder(&mut b, &infos)?;
		// BDK must not add extra UTXOs — we already selected everything.
		b.manually_selected_only();
		b.finish().map_err(|e| {
			log::error!("Failed to build tx with unified selection: {}", e);
			Error::InsufficientFunds
		})
	}

	/// Build a channel-funding transaction using unified coin selection
	/// across all wallets. Only SegWit-compatible UTXOs are included
	/// (Legacy/P2PKH excluded) as required by BOLT 2.
	#[allow(deprecated)]
	pub fn build_and_sign_funding_tx(
		&mut self, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate, locktime: LockTime,
	) -> Result<Transaction, Error> {
		let psbt = self.build_tx_unified(
			&self.primary.clone(),
			output_script,
			amount,
			fee_rate,
			Some(locktime),
			&HashSet::new(),
			CoinSelectionAlgorithm::BranchAndBound,
			|u| {
				u.txout.script_pubkey.witness_version().is_some() || u.txout.script_pubkey.is_p2sh()
			},
		)?;

		let tx = self.sign_psbt_all(psbt)?;
		self.persist_all()?;
		Ok(tx)
	}

	/// Build a channel-funding transaction when the primary wallet is not
	/// eligible (e.g. Legacy). Picks the highest-balance wallet from those
	/// passing `filter` as the PSBT builder, then runs unified coin
	/// selection across all eligible wallets.
	#[allow(deprecated)]
	pub fn build_and_sign_tx_with_best_wallet(
		&mut self, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate, locktime: LockTime,
		filter: impl Fn(&K) -> bool,
	) -> Result<Transaction, Error> {
		let mut matching_keys: Vec<K> =
			self.wallets.keys().filter(|k| filter(k)).cloned().collect();

		if matching_keys.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		matching_keys.sort_by(|a, b| {
			let bal_a = self
				.balance_for(a)
				.map(|bal| bal.confirmed.to_sat() + bal.trusted_pending.to_sat())
				.unwrap_or(0);
			let bal_b = self
				.balance_for(b)
				.map(|bal| bal.confirmed.to_sat() + bal.trusted_pending.to_sat())
				.unwrap_or(0);
			bal_b.cmp(&bal_a)
		});

		let build_key = matching_keys[0];

		let psbt = self.build_tx_unified(
			&build_key,
			output_script,
			amount,
			fee_rate,
			Some(locktime),
			&HashSet::new(),
			CoinSelectionAlgorithm::BranchAndBound,
			|u| {
				u.txout.script_pubkey.witness_version().is_some() || u.txout.script_pubkey.is_p2sh()
			},
		)?;

		let tx = self.sign_psbt_all(psbt)?;
		self.persist_all()?;
		Ok(tx)
	}

	/// Build a PSBT using unified coin selection across all wallets.
	/// Returns the **unsigned** PSBT — the caller is responsible for
	/// signing and persisting.
	#[allow(deprecated)]
	pub fn build_psbt_with_cross_wallet_fallback(
		&mut self, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate,
		excluded_txids: &HashSet<Txid>, algorithm: CoinSelectionAlgorithm,
	) -> Result<Psbt, Error> {
		self.build_tx_unified(
			&self.primary.clone(),
			output_script,
			amount,
			fee_rate,
			None,
			excluded_txids,
			algorithm,
			|_| true,
		)
	}

	/// Aggregate balance across wallets whose keys satisfy `filter`.
	pub fn balance_filtered(&self, filter: impl Fn(&K) -> bool) -> Balance {
		let mut total = Balance::default();
		for (key, wallet) in &self.wallets {
			if filter(key) {
				let balance = wallet.balance();
				total.confirmed += balance.confirmed;
				total.trusted_pending += balance.trusted_pending;
				total.untrusted_pending += balance.untrusted_pending;
				total.immature += balance.immature;
			}
		}
		total
	}

	/// Build and sign a transaction that drains ALL wallets (primary +
	/// monitored) to a single destination script.
	///
	/// Uses `drain_wallet()` for the primary wallet and adds non-primary
	/// UTXOs as foreign inputs.  BDK's drain calculation then accounts for
	/// the full input value (primary + foreign) minus fees.
	pub fn build_and_sign_drain(
		&mut self, destination: ScriptBuf, fee_rate: FeeRate,
	) -> Result<Transaction, Error> {
		// Collect non-primary UTXOs first (immutable borrow).
		let non_primary_infos = self.non_primary_foreign_utxos(&HashSet::new())?;

		let primary = self.primary;
		let wallet = self.wallets.get_mut(&primary).ok_or(Error::WalletNotFound)?;

		// Check primary wallet has at least something to drain.
		if wallet.balance().total() == Amount::ZERO && non_primary_infos.is_empty() {
			return Err(Error::NoSpendableOutputs);
		}

		let mut builder = wallet.build_tx();

		// drain_wallet() tells BDK to include all primary UTXOs.
		builder.drain_wallet();
		builder.drain_to(destination);
		builder.fee_rate(fee_rate);

		// Add non-primary UTXOs as foreign inputs.
		for info in &non_primary_infos {
			builder
				.add_foreign_utxo_with_sequence(
					info.outpoint,
					info.psbt_input.clone(),
					info.weight,
					bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
				)
				.map_err(|e| {
					log::error!("Failed to add foreign UTXO {:?} for drain: {}", info.outpoint, e);
					Error::OnchainTxCreationFailed
				})?;
		}

		let psbt = builder.finish().map_err(|e| {
			log::error!("Failed to build drain transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		self.sign_psbt_all(psbt)
	}

	// ─── CPFP ──────────────────────────────────────────────────────────

	/// Build a CPFP (Child-Pays-For-Parent) transaction.
	///
	/// Finds spendable outputs of `parent_txid` across all wallets, builds
	/// a child transaction at `fee_rate` that spends those outputs to
	/// `destination_script`.  If `destination_script` is `None` the
	/// primary wallet's next internal address is used.
	///
	/// Returns `(signed_child_tx, parent_fee, parent_fee_rate)`.
	pub fn build_cpfp(
		&mut self, parent_txid: Txid, fee_rate: FeeRate, destination_script: Option<ScriptBuf>,
	) -> Result<(Transaction, Amount, FeeRate), Error> {
		// Find and validate the parent transaction.
		let parent_tx = self.find_tx(parent_txid).ok_or(Error::TransactionNotFound)?;
		if self.is_tx_confirmed(&parent_txid) {
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Calculate parent fee.
		let parent_fee = self.calculate_tx_fee(&parent_tx)?;
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// Find spendable outputs from this transaction across ALL wallets.
		let utxos: Vec<LocalOutput> =
			self.list_unspent().into_iter().filter(|u| u.outpoint.txid == parent_txid).collect();
		if utxos.is_empty() {
			return Err(Error::NoSpendableOutputs);
		}

		// Classify UTXOs as primary or foreign for tx building.
		let utxo_infos = self.prepare_utxos_for_psbt(&utxos)?;

		// Determine destination.
		let drain_script = match destination_script {
			Some(s) => s,
			None => {
				let wallet = self.primary_wallet_mut();
				wallet.next_unused_address(KeychainKind::Internal).address.script_pubkey()
			},
		};

		// Build the child transaction on the primary wallet.
		let wallet = self.primary_wallet_mut();
		let mut tx_builder = wallet.build_tx();

		utxo::add_utxos_to_tx_builder(&mut tx_builder, &utxo_infos)
			.map_err(|_| Error::OnchainTxCreationFailed)?;

		tx_builder.fee_rate(fee_rate);
		tx_builder.drain_to(drain_script);
		tx_builder.manually_selected_only();

		let psbt = tx_builder.finish().map_err(|e| {
			log::error!("Failed to create CPFP transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		let tx = self.sign_psbt_all(psbt)?;
		Ok((tx, parent_fee, parent_fee_rate))
	}

	/// Calculate an appropriate CPFP child fee rate given a parent txid and
	/// a `target_fee_rate` that the caller wants the package to achieve.
	///
	/// Returns the recommended child fee rate.  If the parent already meets
	/// the target, returns `target + 1 sat/vB`.
	pub fn calculate_cpfp_fee_rate(
		&self, parent_txid: Txid, target_fee_rate: FeeRate,
	) -> Result<FeeRate, Error> {
		let parent_tx = self.find_tx(parent_txid).ok_or(Error::TransactionNotFound)?;
		if self.is_tx_confirmed(&parent_txid) {
			return Err(Error::TransactionAlreadyConfirmed);
		}

		let parent_fee = self.calculate_tx_fee(&parent_tx)?;
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// If parent already meets target, return slightly higher rate.
		if parent_fee_rate >= target_fee_rate {
			return Ok(FeeRate::from_sat_per_kwu(parent_fee_rate.to_sat_per_kwu() + 250));
		}

		// Estimate child size: conservative 1-in/1-out (~120 vB).
		let estimated_child_vbytes: u64 = 120;
		let parent_vbytes = parent_tx.weight().to_vbytes_ceil();

		let parent_fee_deficit = (target_fee_rate.to_sat_per_vb_ceil()
			- parent_fee_rate.to_sat_per_vb_ceil())
			* parent_vbytes;
		let base_child_fee = target_fee_rate.to_sat_per_vb_ceil() * estimated_child_vbytes;
		let total_child_fee = base_child_fee + parent_fee_deficit;
		let child_fee_rate_sat_vb = total_child_fee / estimated_child_vbytes;

		Ok(FeeRate::from_sat_per_vb(child_fee_rate_sat_vb)
			.unwrap_or(FeeRate::from_sat_per_kwu(child_fee_rate_sat_vb * 250)))
	}

	// ─── RBF ────────────────────────────────────────────────────────────

	/// Returns `true` if all inputs of the transaction belong to the primary wallet.
	pub fn is_primary_only_tx(&self, txid: Txid) -> bool {
		let primary = self.primary_wallet();
		if primary.get_tx(txid).is_none() {
			return false;
		}
		let tx = match self.find_tx(txid) {
			Some(tx) => tx,
			None => return false,
		};
		tx.input.iter().all(|txin| {
			// Fast path: primary still has the UTXO (unspent).
			if primary.get_utxo(txin.previous_output).is_some() {
				return true;
			}
			// Slow path: the UTXO was already spent (by this tx).  Check
			// whether the output script belongs to the primary wallet.
			if let Some(tx_node) = primary.get_tx(txin.previous_output.txid) {
				if let Some(txout) =
					tx_node.tx_node.tx.output.get(txin.previous_output.vout as usize)
				{
					return primary.is_mine(txout.script_pubkey.clone());
				}
			}
			false
		})
	}

	/// Build an RBF replacement transaction.
	///
	/// This is a high-level method that:
	/// 1. Finds the original transaction across all wallets.
	/// 2. Validates that it is unconfirmed and the new fee rate is higher.
	/// 3. If the original tx was primary-only, tries BDK's built-in fee
	///    bump and falls back to adding foreign inputs if the primary wallet
	///    has insufficient funds.
	/// 4. If the original tx was cross-wallet, uses manual RBF construction.
	/// 5. Signs the replacement with all wallets.
	///
	/// Returns the signed replacement transaction and the original fee
	/// (for caller logging).
	pub fn build_rbf(
		&mut self, txid: Txid, new_fee_rate: FeeRate,
	) -> Result<(Transaction, Amount), Error> {
		// Find original transaction.
		let original_tx = self.find_tx(txid).ok_or(Error::TransactionNotFound)?;

		// Must not be confirmed.
		if self.is_tx_confirmed(&txid) {
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Calculate original fee.
		let original_fee = self.calculate_tx_fee(&original_tx)?;
		let original_fee_rate = original_fee / original_tx.weight();

		// New fee rate must be higher.
		if new_fee_rate <= original_fee_rate {
			return Err(Error::InvalidFeeRate);
		}

		// Build the replacement.
		let is_primary = self.is_primary_only_tx(txid);

		let tx = if is_primary {
			self.build_rbf_primary_with_fallback(txid, new_fee_rate)?
		} else {
			self.build_cross_wallet_rbf_with_fallback(&original_tx, new_fee_rate)?
		};

		Ok((tx, original_fee))
	}

	/// Try a primary-only RBF first; if the primary wallet doesn't have
	/// enough funds for the higher fee, fall back to adding non-primary
	/// UTXOs as additional inputs.
	fn build_rbf_primary_with_fallback(
		&mut self, txid: Txid, fee_rate: FeeRate,
	) -> Result<Transaction, Error> {
		// Attempt 1: primary-only fee bump.
		let primary_result: Result<Psbt, Error> = {
			let wallet = self.wallets.get_mut(&self.primary).ok_or(Error::WalletNotFound)?;
			match wallet.build_fee_bump(txid) {
				Ok(mut builder) => {
					builder.fee_rate(fee_rate);
					builder.finish().map_err(|e| {
						log::debug!("Primary-only RBF failed: {}", e);
						Error::InsufficientFunds
					})
				},
				Err(e) => {
					log::debug!("build_fee_bump failed: {}", e);
					Err(Error::OnchainTxCreationFailed)
				},
			}
		}; // mutable wallet borrow released here

		match primary_result {
			Ok(psbt) => self.sign_psbt_all(psbt),
			Err(_) => {
				// Attempt 2: select minimum non-primary UTXOs for the fee deficit.
				let original_tx = self.find_tx(txid).ok_or(Error::TransactionNotFound)?;
				let original_fee = self.calculate_tx_fee(&original_tx)?;
				let new_fee = fee_rate.fee_wu(original_tx.weight()).unwrap_or(Amount::ZERO);
				let fee_increase = new_fee.checked_sub(original_fee).unwrap_or(Amount::ZERO);

				// Wallet-owned outputs (change) in the original tx can absorb
				// part of the fee increase, reducing what foreign UTXOs must cover.
				let absorbable: Amount = original_tx
					.output
					.iter()
					.filter(|out| self.is_mine(out.script_pubkey.clone()))
					.map(|out| out.value)
					.sum();
				// No explicit buffer needed: BDK's coin selection already
				// accounts for each candidate's input weight cost.
				let deficit = fee_increase.checked_sub(absorbable).unwrap_or(Amount::ZERO);

				let non_primary_infos = self.select_non_primary_foreign_utxos(
					deficit,
					fee_rate,
					&HashSet::new(),
					false,
					CoinSelectionAlgorithm::BranchAndBound,
				)?;
				if non_primary_infos.is_empty() {
					return Err(Error::InsufficientFunds);
				}

				let wallet = self.wallets.get_mut(&self.primary).ok_or(Error::WalletNotFound)?;
				let mut builder = wallet.build_fee_bump(txid).map_err(|e| {
					log::error!("Failed to create fee bump builder (with foreign): {}", e);
					Error::OnchainTxCreationFailed
				})?;
				builder.fee_rate(fee_rate);

				for info in &non_primary_infos {
					builder
						.add_foreign_utxo_with_sequence(
							info.outpoint,
							info.psbt_input.clone(),
							info.weight,
							bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
						)
						.map_err(|e| {
							log::error!("Failed to add foreign UTXO {:?}: {}", info.outpoint, e);
							Error::OnchainTxCreationFailed
						})?;
				}

				let psbt = builder.finish().map_err(|e| {
					log::error!("Failed to build RBF with additional inputs: {}", e);
					Error::OnchainTxCreationFailed
				})?;

				self.sign_psbt_all(psbt)
			},
		}
	}

	// ─── Persistence ────────────────────────────────────────────────────

	/// Persist all wallets.
	pub fn persist_all(&mut self) -> Result<(), Error> {
		for (key, wallet) in self.wallets.iter_mut() {
			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}
		Ok(())
	}

	/// Check if an output belongs to any wallet.
	pub fn is_mine(&self, script_pubkey: ScriptBuf) -> bool {
		self.wallets.values().any(|w| w.is_mine(script_pubkey.clone()))
	}
}
