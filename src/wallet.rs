use crate::logger::{log_error, log_info, log_trace, Logger};

use crate::config::BDK_WALLET_SYNC_TIMEOUT_SECS;
use crate::Error;

use bitcoin::psbt::Psbt;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};

use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeyMaterial, KeysManager, NodeSigner,
	OutputSpender, Recipient, SignerProvider, SpendableOutputDescriptor,
};

use lightning::util::message_signing;

use bdk::blockchain::EsploraBlockchain;
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{Balance, FeeRate};
use bdk::{SignOptions, SyncOptions};

use bitcoin::address::{Payload, WitnessVersion};
use bitcoin::bech32::u5;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hash_types::WPubkeyHash;
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Signing};
use bitcoin::{ScriptBuf, Transaction, TxOut, Txid};

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

enum WalletSyncStatus {
	Completed,
	InProgress { subscribers: tokio::sync::broadcast::Sender<Result<(), Error>> },
}

pub struct Wallet<D, B: Deref, E: Deref, L: Deref>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	// A BDK blockchain used for wallet sync.
	blockchain: EsploraBlockchain,
	// A BDK on-chain wallet.
	inner: Mutex<bdk::Wallet<D>>,
	// A cache storing the most recently retrieved fee rate estimations.
	broadcaster: B,
	fee_estimator: E,
	// A Mutex holding the current sync status.
	sync_status: Mutex<WalletSyncStatus>,
	// TODO: Drop this workaround after BDK 1.0 upgrade.
	balance_cache: RwLock<Balance>,
	logger: L,
}

impl<D, B: Deref, E: Deref, L: Deref> Wallet<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	pub(crate) fn new(
		blockchain: EsploraBlockchain, wallet: bdk::Wallet<D>, broadcaster: B, fee_estimator: E,
		logger: L,
	) -> Self {
		let start_balance = wallet.get_balance().unwrap_or(Balance {
			immature: 0,
			trusted_pending: 0,
			untrusted_pending: 0,
			confirmed: 0,
		});

		let inner = Mutex::new(wallet);
		let sync_status = Mutex::new(WalletSyncStatus::Completed);
		let balance_cache = RwLock::new(start_balance);
		Self { blockchain, inner, broadcaster, fee_estimator, sync_status, balance_cache, logger }
	}

	pub(crate) async fn sync(&self) -> Result<(), Error> {
		if let Some(mut sync_receiver) = self.register_or_subscribe_pending_sync() {
			log_info!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		let res = {
			let wallet_lock = self.inner.lock().unwrap();

			let wallet_sync_timeout_fut = tokio::time::timeout(
				Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
				wallet_lock.sync(&self.blockchain, SyncOptions { progress: None }),
			);

			match wallet_sync_timeout_fut.await {
				Ok(res) => match res {
					Ok(()) => {
						// TODO: Drop this workaround after BDK 1.0 upgrade.
						// Update balance cache after syncing.
						if let Ok(balance) = wallet_lock.get_balance() {
							*self.balance_cache.write().unwrap() = balance;
						}
						Ok(())
					},
					Err(e) => match e {
						bdk::Error::Esplora(ref be) => match **be {
							bdk::blockchain::esplora::EsploraError::Reqwest(_) => {
								log_error!(
									self.logger,
									"Sync failed due to HTTP connection error: {}",
									e
								);
								Err(From::from(e))
							},
							_ => {
								log_error!(self.logger, "Sync failed due to Esplora error: {}", e);
								Err(From::from(e))
							},
						},
						_ => {
							log_error!(self.logger, "Wallet sync error: {}", e);
							Err(From::from(e))
						},
					},
				},
				Err(e) => {
					log_error!(self.logger, "On-chain wallet sync timed out: {}", e);
					Err(Error::WalletOperationTimeout)
				},
			}
		};

		self.propagate_result_to_subscribers(res);

		res
	}

	pub(crate) fn build_payjoin_transaction(
		&self, output_script: ScriptBuf, value_sats: u64,
	) -> Result<Psbt, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let network = locked_wallet.network();
		let fee_rate = match network {
			bitcoin::Network::Regtest => 1000.0,
			_ => self
				.fee_estimator
				.get_est_sat_per_1000_weight(ConfirmationTarget::OutputSpendingFee) as f32,
		};
		let fee_rate = FeeRate::from_sat_per_kwu(fee_rate);
		let locked_wallet = self.inner.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();
		tx_builder.add_recipient(output_script, value_sats).fee_rate(fee_rate).enable_rbf();
		let mut psbt = match tx_builder.finish() {
			Ok((psbt, _)) => {
				log_trace!(self.logger, "Created Payjoin transaction: {:?}", psbt);
				psbt
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create Payjoin transaction: {}", err);
				return Err(err.into());
			},
		};
		locked_wallet.sign(&mut psbt, SignOptions::default())?;
		Ok(psbt)
	}

	pub(crate) fn sign_payjoin_proposal(
		&self, payjoin_proposal_psbt: &mut Psbt, original_psbt: &mut Psbt,
	) -> Result<bool, Error> {
		// BDK only signs scripts that match its target descriptor by iterating through input map.
		// The BIP 78 spec makes receiver clear sender input map UTXOs, so process_response will
		// fail unless they're cleared.  A PSBT unsigned_tx.input references input OutPoints and
		// not a Script, so the sender signer must either be able to sign based on OutPoint UTXO
		// lookup or otherwise re-introduce the Script from original_psbt.  Since BDK PSBT signer
		// only checks Input map Scripts for match against its descriptor, it won't sign if they're
		// empty.  Re-add the scripts from the original_psbt in order for BDK to sign properly.
		// reference: https://github.com/bitcoindevkit/bdk-cli/pull/156#discussion_r1261300637
		let mut original_inputs =
			original_psbt.unsigned_tx.input.iter().zip(&mut original_psbt.inputs).peekable();
		for (proposed_txin, proposed_psbtin) in
			payjoin_proposal_psbt.unsigned_tx.input.iter().zip(&mut payjoin_proposal_psbt.inputs)
		{
			if let Some((original_txin, original_psbtin)) = original_inputs.peek() {
				if proposed_txin.previous_output == original_txin.previous_output {
					proposed_psbtin.witness_utxo = original_psbtin.witness_utxo.clone();
					proposed_psbtin.non_witness_utxo = original_psbtin.non_witness_utxo.clone();
					original_inputs.next();
				}
			}
		}

		let wallet = self.inner.lock().unwrap();
		let is_signed = wallet.sign(payjoin_proposal_psbt, SignOptions::default())?;
		Ok(is_signed)
	}

	pub(crate) fn create_funding_transaction(
		&self, output_script: ScriptBuf, value_sats: u64, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = FeeRate::from_sat_per_kwu(
			self.fee_estimator.get_est_sat_per_1000_weight(confirmation_target) as f32,
		);

		let locked_wallet = self.inner.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		tx_builder
			.add_recipient(output_script, value_sats)
			.fee_rate(fee_rate)
			.nlocktime(locktime)
			.enable_rbf();

		let mut psbt = match tx_builder.finish() {
			Ok((psbt, _)) => {
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

		Ok(psbt.extract_tx())
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.inner.lock().unwrap().get_address(AddressIndex::New)?;
		Ok(address_info.address)
	}

	fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info =
			self.inner.lock().unwrap().get_internal_address(AddressIndex::LastUnused)?;
		Ok(address_info.address)
	}

	pub(crate) fn get_balances(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		// TODO: Drop this workaround after BDK 1.0 upgrade.
		// We get the balance and update our cache if we can do so without blocking on the wallet
		// Mutex. Otherwise, we return a cached value.
		let balance = match self.inner.try_lock() {
			Ok(wallet_lock) => {
				// Update balance cache if we can.
				let balance = wallet_lock.get_balance()?;
				*self.balance_cache.write().unwrap() = balance.clone();
				balance
			},
			Err(_) => self.balance_cache.read().unwrap().clone(),
		};

		let (total, spendable) = (
			balance.get_total(),
			balance.get_spendable().saturating_sub(total_anchor_channels_reserve_sats),
		);

		Ok((total, spendable))
	}

	pub(crate) fn get_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		self.get_balances(total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	/// Send funds to the given address.
	///
	/// If `amount_msat_or_drain` is `None` the wallet will be drained, i.e., all available funds will be
	/// spent.
	pub(crate) fn send_to_address(
		&self, address: &bitcoin::Address, amount_msat_or_drain: Option<u64>,
	) -> Result<Txid, Error> {
		let confirmation_target = ConfirmationTarget::OutputSpendingFee;
		let fee_rate = FeeRate::from_sat_per_kwu(
			self.fee_estimator.get_est_sat_per_1000_weight(confirmation_target) as f32,
		);

		let tx = {
			let locked_wallet = self.inner.lock().unwrap();
			let mut tx_builder = locked_wallet.build_tx();

			if let Some(amount_sats) = amount_msat_or_drain {
				tx_builder
					.add_recipient(address.script_pubkey(), amount_sats)
					.fee_rate(fee_rate)
					.enable_rbf();
			} else {
				tx_builder
					.drain_wallet()
					.drain_to(address.script_pubkey())
					.fee_rate(fee_rate)
					.enable_rbf();
			}

			let mut psbt = match tx_builder.finish() {
				Ok((psbt, _)) => {
					log_trace!(self.logger, "Created PSBT: {:?}", psbt);
					psbt
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
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
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				},
			}
			psbt.extract_tx()
		};

		self.broadcaster.broadcast_transactions(&[&tx]);

		let txid = tx.txid();

		if let Some(amount_sats) = amount_msat_or_drain {
			log_info!(
				self.logger,
				"Created new transaction {} sending {}sats on-chain to address {}",
				txid,
				amount_sats,
				address
			);
		} else {
			log_info!(
				self.logger,
				"Created new transaction {} sending all available on-chain funds to address {}",
				txid,
				address
			);
		}

		Ok(txid)
	}

	fn register_or_subscribe_pending_sync(
		&self,
	) -> Option<tokio::sync::broadcast::Receiver<Result<(), Error>>> {
		let mut sync_status_lock = self.sync_status.lock().unwrap();
		match sync_status_lock.deref_mut() {
			WalletSyncStatus::Completed => {
				// We're first to register for a sync.
				let (tx, _) = tokio::sync::broadcast::channel(1);
				*sync_status_lock = WalletSyncStatus::InProgress { subscribers: tx };
				None
			},
			WalletSyncStatus::InProgress { subscribers } => {
				// A sync is in-progress, we subscribe.
				let rx = subscribers.subscribe();
				Some(rx)
			},
		}
	}

	fn propagate_result_to_subscribers(&self, res: Result<(), Error>) {
		// Send the notification to any other tasks that might be waiting on it by now.
		{
			let mut sync_status_lock = self.sync_status.lock().unwrap();
			match sync_status_lock.deref_mut() {
				WalletSyncStatus::Completed => {
					// No sync in-progress, do nothing.
					return;
				},
				WalletSyncStatus::InProgress { subscribers } => {
					// A sync is in-progress, we notify subscribers.
					if subscribers.receiver_count() > 0 {
						match subscribers.send(res) {
							Ok(_) => (),
							Err(e) => {
								debug_assert!(
									false,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
								log_error!(
									self.logger,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
							},
						}
					}
					*sync_status_lock = WalletSyncStatus::Completed;
				},
			}
		}
	}
}

impl<D, B: Deref, E: Deref, L: Deref> WalletSource for Wallet<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
		let locked_wallet = self.inner.lock().unwrap();
		let mut utxos = Vec::new();
		let confirmed_txs: Vec<bdk::TransactionDetails> = locked_wallet
			.list_transactions(false)
			.map_err(|e| {
				log_error!(self.logger, "Failed to retrieve transactions from wallet: {}", e);
			})?
			.into_iter()
			.filter(|t| t.confirmation_time.is_some())
			.collect();
		let unspent_confirmed_utxos = locked_wallet
			.list_unspent()
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to retrieve unspent transactions from wallet: {}",
					e
				);
			})?
			.into_iter()
			.filter(|u| confirmed_txs.iter().find(|t| t.txid == u.outpoint.txid).is_some());

		for u in unspent_confirmed_utxos {
			let payload = Payload::from_script(&u.txout.script_pubkey).map_err(|e| {
				log_error!(self.logger, "Failed to retrieve script payload: {}", e);
			})?;

			match payload {
				Payload::WitnessProgram(program) => match program.version() {
					WitnessVersion::V0 if program.program().len() == 20 => {
						let wpkh =
							WPubkeyHash::from_slice(program.program().as_bytes()).map_err(|e| {
								log_error!(self.logger, "Failed to retrieve script payload: {}", e);
							})?;
						let utxo = Utxo::new_v0_p2wpkh(u.outpoint, u.txout.value, &wpkh);
						utxos.push(utxo);
					},
					WitnessVersion::V1 => {
						XOnlyPublicKey::from_slice(program.program().as_bytes()).map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

						let utxo = Utxo {
							outpoint: u.outpoint,
							output: TxOut {
								value: u.txout.value,
								script_pubkey: ScriptBuf::new_witness_program(&program),
							},
							satisfaction_weight: 1 /* empty script_sig */ * WITNESS_SCALE_FACTOR as u64 +
								1 /* witness items */ + 1 /* schnorr sig len */ + 64, /* schnorr sig */
						};
						utxos.push(utxo);
					},
					_ => {
						log_error!(
							self.logger,
							"Unexpected witness version or length. Version: {}, Length: {}",
							program.version(),
							program.program().len()
						);
					},
				},
				_ => {
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
		let locked_wallet = self.inner.lock().unwrap();
		let address_info =
			locked_wallet.get_internal_address(AddressIndex::LastUnused).map_err(|e| {
				log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
			})?;

		Ok(address_info.address.script_pubkey())
	}

	fn sign_psbt(&self, mut psbt: PartiallySignedTransaction) -> Result<Transaction, ()> {
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

		Ok(psbt.extract_tx())
	}
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub struct WalletKeysManager<D, B: Deref, E: Deref, L: Deref>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	inner: KeysManager,
	wallet: Arc<Wallet<D, B, E, L>>,
	logger: L,
}

impl<D, B: Deref, E: Deref, L: Deref> WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	/// Constructs a `WalletKeysManager` that overrides the destination and shutdown scripts.
	///
	/// See [`KeysManager::new`] for more information on `seed`, `starting_time_secs`, and
	/// `starting_time_nanos`.
	pub fn new(
		seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32,
		wallet: Arc<Wallet<D, B, E, L>>, logger: L,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos);
		Self { inner, wallet, logger }
	}

	pub fn sign_message(&self, msg: &[u8]) -> Result<String, Error> {
		message_signing::sign(msg, &self.inner.get_node_secret_key())
			.or(Err(Error::MessageSigningFailed))
	}

	pub fn get_node_secret_key(&self) -> SecretKey {
		self.inner.get_node_secret_key()
	}

	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		message_signing::verify(msg, sig, pkey)
	}
}

impl<D, B: Deref, E: Deref, L: Deref> NodeSigner for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
		self.inner.get_node_id(recipient)
	}

	fn ecdh(
		&self, recipient: Recipient, other_key: &PublicKey, tweak: Option<&Scalar>,
	) -> Result<SharedSecret, ()> {
		self.inner.ecdh(recipient, other_key, tweak)
	}

	fn get_inbound_payment_key_material(&self) -> KeyMaterial {
		self.inner.get_inbound_payment_key_material()
	}

	fn sign_invoice(
		&self, hrp_bytes: &[u8], invoice_data: &[u5], recipient: Recipient,
	) -> Result<RecoverableSignature, ()> {
		self.inner.sign_invoice(hrp_bytes, invoice_data, recipient)
	}

	fn sign_gossip_message(&self, msg: UnsignedGossipMessage<'_>) -> Result<Signature, ()> {
		self.inner.sign_gossip_message(msg)
	}

	fn sign_bolt12_invoice(
		&self, invoice: &lightning::offers::invoice::UnsignedBolt12Invoice,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice(invoice)
	}

	fn sign_bolt12_invoice_request(
		&self, invoice_request: &lightning::offers::invoice_request::UnsignedInvoiceRequest,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice_request(invoice_request)
	}
}

impl<D, B: Deref, E: Deref, L: Deref> OutputSpender for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
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

impl<D, B: Deref, E: Deref, L: Deref> EntropySource for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl<D, B: Deref, E: Deref, L: Deref> SignerProvider for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
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

		match address.payload {
			Payload::WitnessProgram(program) => ShutdownScript::new_witness_program(&program)
				.map_err(|e| {
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

impl<D, B: Deref, E: Deref, L: Deref> ChangeDestinationSource for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_change_destination_script(&self) -> Result<ScriptBuf, ()> {
		let address = self.wallet.get_new_internal_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}
}
