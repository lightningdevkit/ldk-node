use crate::logger::{log_error, log_info, log_trace, FilesystemLogger, Logger};

use crate::Error;

use lightning::chain::chaininterface::{
	BroadcasterInterface, ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW,
};

use lightning::chain::keysinterface::{
	EntropySource, InMemorySigner, KeyMaterial, KeysManager, NodeSigner, Recipient, SignerProvider,
	SpendableOutputDescriptor,
};
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;

use lightning::util::message_signing;

use bdk::blockchain::{Blockchain, EsploraBlockchain};
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{FeeRate, SignOptions, SyncOptions};

use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, Signing};
use bitcoin::{Script, Transaction, TxOut, Txid};

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

pub struct Wallet<D>
where
	D: BatchDatabase,
{
	// A BDK blockchain used for wallet sync.
	blockchain: EsploraBlockchain,
	// A BDK on-chain wallet.
	inner: Mutex<bdk::Wallet<D>>,
	// A cache storing the most recently retrieved fee rate estimations.
	fee_rate_cache: RwLock<HashMap<ConfirmationTarget, FeeRate>>,
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
	sync_lock: (Mutex<()>, Condvar),
	logger: Arc<FilesystemLogger>,
}

impl<D> Wallet<D>
where
	D: BatchDatabase,
{
	pub(crate) fn new(
		blockchain: EsploraBlockchain, wallet: bdk::Wallet<D>,
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let inner = Mutex::new(wallet);
		let fee_rate_cache = RwLock::new(HashMap::new());
		let sync_lock = (Mutex::new(()), Condvar::new());
		Self { blockchain, inner, fee_rate_cache, runtime, sync_lock, logger }
	}

	pub(crate) async fn sync(&self) -> Result<(), Error> {
		let (lock, cvar) = &self.sync_lock;

		let guard = match lock.try_lock() {
			Ok(guard) => guard,
			Err(_) => {
				log_info!(self.logger, "Sync in progress, skipping.");
				let guard = cvar.wait(lock.lock().unwrap());
				drop(guard);
				cvar.notify_all();
				return Ok(());
			}
		};

		let sync_options = SyncOptions { progress: None };
		let wallet_lock = self.inner.lock().unwrap();
		let res = match wallet_lock.sync(&self.blockchain, sync_options).await {
			Ok(()) => Ok(()),
			Err(e) => match e {
				bdk::Error::Esplora(ref be) => match **be {
					bdk::blockchain::esplora::EsploraError::Reqwest(_) => {
						tokio::time::sleep(Duration::from_secs(1)).await;
						log_error!(
							self.logger,
							"Sync failed due to HTTP connection error, retrying: {}",
							e
						);
						let sync_options = SyncOptions { progress: None };
						wallet_lock
							.sync(&self.blockchain, sync_options)
							.await
							.map_err(|e| From::from(e))
					}
					_ => {
						log_error!(self.logger, "Sync failed due to Esplora error: {}", e);
						Err(From::from(e))
					}
				},
				_ => {
					log_error!(self.logger, "Wallet sync error: {}", e);
					Err(From::from(e))
				}
			},
		};

		drop(guard);
		cvar.notify_all();
		res
	}

	pub(crate) async fn update_fee_estimates(&self) -> Result<(), Error> {
		let mut locked_fee_rate_cache = self.fee_rate_cache.write().unwrap();

		let confirmation_targets = vec![
			ConfirmationTarget::Background,
			ConfirmationTarget::Normal,
			ConfirmationTarget::HighPriority,
		];
		for target in confirmation_targets {
			let num_blocks = match target {
				ConfirmationTarget::Background => 12,
				ConfirmationTarget::Normal => 6,
				ConfirmationTarget::HighPriority => 3,
			};

			let est_fee_rate = self.blockchain.estimate_fee(num_blocks).await;

			match est_fee_rate {
				Ok(rate) => {
					locked_fee_rate_cache.insert(target, rate);
					log_trace!(
						self.logger,
						"Fee rate estimation updated for {:?}: {} sats/kwu",
						target,
						rate.fee_wu(1000)
					);
				}
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to update fee rate estimation for {:?}: {}",
						target,
						e
					);
				}
			}
		}
		Ok(())
	}

	pub(crate) fn create_funding_transaction(
		&self, output_script: Script, value_sats: u64, confirmation_target: ConfirmationTarget,
	) -> Result<Transaction, Error> {
		let fee_rate = self.estimate_fee_rate(confirmation_target);

		let locked_wallet = self.inner.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		tx_builder.add_recipient(output_script, value_sats).fee_rate(fee_rate).enable_rbf();

		let mut psbt = match tx_builder.finish() {
			Ok((psbt, _)) => {
				log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);
				psbt
			}
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			}
		};

		match locked_wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					return Err(Error::OnchainTxCreationFailed);
				}
			}
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			}
		}

		Ok(psbt.extract_tx())
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.inner.lock().unwrap().get_address(AddressIndex::New)?;
		Ok(address_info.address)
	}

	pub(crate) fn get_balance(&self) -> Result<bdk::Balance, Error> {
		Ok(self.inner.lock().unwrap().get_balance()?)
	}

	/// Send funds to the given address.
	///
	/// If `amount_msat_or_drain` is `None` the wallet will be drained, i.e., all available funds will be
	/// spent.
	pub(crate) fn send_to_address(
		&self, address: &bitcoin::Address, amount_msat_or_drain: Option<u64>,
	) -> Result<Txid, Error> {
		let confirmation_target = ConfirmationTarget::Normal;
		let fee_rate = self.estimate_fee_rate(confirmation_target);

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
				}
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				}
			};

			match locked_wallet.sign(&mut psbt, SignOptions::default()) {
				Ok(finalized) => {
					if !finalized {
						return Err(Error::OnchainTxCreationFailed);
					}
				}
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				}
			}
			psbt.extract_tx()
		};

		self.broadcast_transaction(&tx);

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

	fn estimate_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate {
		let locked_fee_rate_cache = self.fee_rate_cache.read().unwrap();

		let fallback_sats_kwu = match confirmation_target {
			ConfirmationTarget::Background => FEERATE_FLOOR_SATS_PER_KW,
			ConfirmationTarget::Normal => 2000,
			ConfirmationTarget::HighPriority => 5000,
		};

		// We'll fall back on this, if we really don't have any other information.
		let fallback_rate = FeeRate::from_sat_per_kwu(fallback_sats_kwu as f32);

		*locked_fee_rate_cache.get(&confirmation_target).unwrap_or(&fallback_rate)
	}
}

impl<D> FeeEstimator for Wallet<D>
where
	D: BatchDatabase,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		(self.estimate_fee_rate(confirmation_target).fee_wu(1000) as u32)
			.max(FEERATE_FLOOR_SATS_PER_KW)
	}
}

impl<D> BroadcasterInterface for Wallet<D>
where
	D: BatchDatabase,
{
	fn broadcast_transaction(&self, tx: &Transaction) {
		let locked_runtime = self.runtime.read().unwrap();
		if locked_runtime.as_ref().is_none() {
			log_error!(self.logger, "Failed to broadcast transaction: No runtime.");
			return;
		}

		let res = tokio::task::block_in_place(move || {
			locked_runtime
				.as_ref()
				.unwrap()
				.block_on(async move { self.blockchain.broadcast(tx).await })
		});

		match res {
			Ok(_) => {}
			Err(err) => {
				log_error!(self.logger, "Failed to broadcast transaction: {}", err);
			}
		}
	}
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub struct WalletKeysManager<D>
where
	D: BatchDatabase,
{
	inner: KeysManager,
	wallet: Arc<Wallet<D>>,
}

impl<D> WalletKeysManager<D>
where
	D: BatchDatabase,
{
	/// Constructs a `WalletKeysManager` that overrides the destination and shutdown scripts.
	///
	/// See [`KeysManager::new`] for more information on `seed`, `starting_time_secs`, and
	/// `starting_time_nanos`.
	pub fn new(
		seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32, wallet: Arc<Wallet<D>>,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos);
		Self { inner, wallet }
	}

	/// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
	pub fn spend_spendable_outputs<C: Signing>(
		&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>,
		change_destination_script: Script, feerate_sat_per_1000_weight: u32,
		secp_ctx: &Secp256k1<C>,
	) -> Result<Transaction, ()> {
		let only_non_static = &descriptors
			.iter()
			.filter(|desc| !matches!(desc, SpendableOutputDescriptor::StaticOutput { .. }))
			.copied()
			.collect::<Vec<_>>();
		self.inner.spend_spendable_outputs(
			only_non_static,
			outputs,
			change_destination_script,
			feerate_sat_per_1000_weight,
			secp_ctx,
		)
	}

	pub fn sign_message(&self, msg: &[u8]) -> Result<String, Error> {
		message_signing::sign(msg, &self.inner.get_node_secret_key())
			.or(Err(Error::MessageSigningFailed))
	}

	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		message_signing::verify(msg, sig, pkey)
	}
}

impl<D> NodeSigner for WalletKeysManager<D>
where
	D: BatchDatabase,
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
}

impl<D> EntropySource for WalletKeysManager<D>
where
	D: BatchDatabase,
{
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl<D> SignerProvider for WalletKeysManager<D>
where
	D: BatchDatabase,
{
	type Signer = InMemorySigner;

	fn generate_channel_keys_id(
		&self, inbound: bool, channel_value_satoshis: u64, user_channel_id: u128,
	) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, channel_value_satoshis, user_channel_id)
	}

	fn derive_channel_signer(
		&self, channel_value_satoshis: u64, channel_keys_id: [u8; 32],
	) -> Self::Signer {
		self.inner.derive_channel_signer(channel_value_satoshis, channel_keys_id)
	}

	fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::Signer, DecodeError> {
		self.inner.read_chan_signer(reader)
	}

	fn get_destination_script(&self) -> Script {
		let address =
			self.wallet.get_new_address().expect("Failed to retrieve new address from wallet.");
		address.script_pubkey()
	}

	fn get_shutdown_scriptpubkey(&self) -> ShutdownScript {
		let address =
			self.wallet.get_new_address().expect("Failed to retrieve new address from wallet.");
		match address.payload {
			bitcoin::util::address::Payload::WitnessProgram { version, program } => {
				return ShutdownScript::new_witness_program(version, &program)
					.expect("Invalid shutdown script.");
			}
			_ => panic!("Tried to use a non-witness address. This must not ever happen."),
		}
	}
}
