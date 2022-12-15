use crate::logger::{
	log_error, log_given_level, log_internal, log_trace, FilesystemLogger, Logger,
};

use crate::Error;

use lightning::chain::chaininterface::{
	BroadcasterInterface, ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW,
};

use lightning::chain::keysinterface::{
	EntropySource, InMemorySigner, KeyMaterial, KeysInterface, KeysManager, NodeSigner, Recipient,
	SignerProvider, SpendableOutputDescriptor,
};
use lightning::ln::msgs::DecodeError;
use lightning::ln::script::ShutdownScript;

use bdk::blockchain::{Blockchain, EsploraBlockchain};
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{FeeRate, SignOptions, SyncOptions};

use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::RecoverableSignature;
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Signing};
use bitcoin::{Script, Transaction, TxOut};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub struct Wallet<D>
where
	D: BatchDatabase,
{
	// A BDK blockchain used for wallet sync.
	blockchain: EsploraBlockchain,
	// A BDK on-chain wallet.
	inner: Mutex<bdk::Wallet<D>>,
	// A cache storing the most recently retrieved fee rate estimations.
	fee_rate_cache: Mutex<HashMap<ConfirmationTarget, FeeRate>>,
	tokio_runtime: RwLock<Option<Arc<tokio::runtime::Runtime>>>,
	logger: Arc<FilesystemLogger>,
}

impl<D> Wallet<D>
where
	D: BatchDatabase,
{
	pub(crate) fn new(
		blockchain: EsploraBlockchain, wallet: bdk::Wallet<D>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let inner = Mutex::new(wallet);
		let fee_rate_cache = Mutex::new(HashMap::new());
		let tokio_runtime = RwLock::new(None);
		Self { blockchain, inner, fee_rate_cache, tokio_runtime, logger }
	}

	pub(crate) async fn sync(&self) -> Result<(), Error> {
		let sync_options = SyncOptions { progress: None };
		match self.inner.lock().unwrap().sync(&self.blockchain, sync_options).await {
			Ok(()) => Ok(()),
			Err(e) => {
				log_error!(self.logger, "Wallet sync error: {}", e);
				Err(e)?
			}
		}
	}

	pub(crate) fn set_runtime(&self, tokio_runtime: Arc<tokio::runtime::Runtime>) {
		*self.tokio_runtime.write().unwrap() = Some(tokio_runtime);
	}

	pub(crate) fn drop_runtime(&self) {
		*self.tokio_runtime.write().unwrap() = None;
	}

	pub(crate) fn create_funding_transaction(
		&self, output_script: &Script, value_sats: u64, confirmation_target: ConfirmationTarget,
	) -> Result<Transaction, Error> {
		let fee_rate = self.estimate_fee_rate(confirmation_target);

		let locked_wallet = self.inner.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		tx_builder.add_recipient(output_script.clone(), value_sats).fee_rate(fee_rate).enable_rbf();

		let mut psbt = match tx_builder.finish() {
			Ok((psbt, _)) => {
				log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);
				psbt
			}
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				Err(err)?
			}
		};

		// We double-check that no inputs try to spend non-witness outputs. As we use a SegWit
		// wallet descriptor this technically shouldn't ever happen, but better safe than sorry.
		for input in &psbt.inputs {
			if input.witness_utxo.is_none() {
				log_error!(self.logger, "Tried to spend a non-witness funding output. This must not ever happen. Panicking!");
				panic!("Tried to spend a non-witness funding output. This must not ever happen.");
			}
		}

		if !locked_wallet.sign(&mut psbt, SignOptions::default())? {
			return Err(Error::FundingTxCreationFailed);
		}

		Ok(psbt.extract_tx())
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.inner.lock().unwrap().get_address(AddressIndex::New)?;
		Ok(address_info.address)
	}

	#[cfg(any(test))]
	pub(crate) fn get_balance(&self) -> Result<bdk::Balance, Error> {
		Ok(self.inner.lock().unwrap().get_balance()?)
	}

	fn estimate_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate {
		let mut locked_fee_rate_cache = self.fee_rate_cache.lock().unwrap();
		let num_blocks = num_blocks_from_conf_target(confirmation_target);

		// We'll fall back on this, if we really don't have any other information.
		let fallback_rate = fallback_fee_from_conf_target(confirmation_target);

		let locked_runtime = self.tokio_runtime.read().unwrap();
		if locked_runtime.as_ref().is_none() {
			log_error!(self.logger, "Failed to update fee rate estimation: No runtime.");
			unreachable!("Failed to broadcast transaction: No runtime.");
		}

		let est_fee_rate = tokio::task::block_in_place(move || {
			locked_runtime
				.as_ref()
				.unwrap()
				.handle()
				.block_on(async move { self.blockchain.estimate_fee(num_blocks).await })
		});

		match est_fee_rate {
			Ok(rate) => {
				locked_fee_rate_cache.insert(confirmation_target, rate);
				log_trace!(
					self.logger,
					"Fee rate estimation updated: {} sats/kwu",
					rate.fee_wu(1000)
				);
				rate
			}
			Err(e) => {
				log_error!(self.logger, "Failed to update fee rate estimation: {}", e);
				*locked_fee_rate_cache.get(&confirmation_target).unwrap_or(&fallback_rate)
			}
		}
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
		let locked_runtime = self.tokio_runtime.read().unwrap();
		if locked_runtime.as_ref().is_none() {
			log_error!(self.logger, "Failed to broadcast transaction: No runtime.");
			unreachable!("Failed to broadcast transaction: No runtime.");
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

fn num_blocks_from_conf_target(confirmation_target: ConfirmationTarget) -> usize {
	match confirmation_target {
		ConfirmationTarget::Background => 12,
		ConfirmationTarget::Normal => 6,
		ConfirmationTarget::HighPriority => 3,
	}
}

fn fallback_fee_from_conf_target(confirmation_target: ConfirmationTarget) -> FeeRate {
	let sats_kwu = match confirmation_target {
		ConfirmationTarget::Background => FEERATE_FLOOR_SATS_PER_KW,
		ConfirmationTarget::Normal => 2000,
		ConfirmationTarget::HighPriority => 5000,
	};

	FeeRate::from_sat_per_kwu(sats_kwu as f32)
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
	/// Constructs a `WalletKeysManager` that
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
			.filter(|desc| {
				if let SpendableOutputDescriptor::StaticOutput { .. } = desc {
					false
				} else {
					true
				}
			})
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
}

impl<D> NodeSigner for WalletKeysManager<D>
where
	D: BatchDatabase,
{
	fn get_node_secret(&self, recipient: Recipient) -> Result<SecretKey, ()> {
		self.inner.get_node_secret(recipient)
	}

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
}

impl<D> KeysInterface for WalletKeysManager<D> where D: BatchDatabase {}

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
