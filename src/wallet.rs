use crate::logger::{
	log_error, log_given_level, log_internal, log_trace, FilesystemLogger, Logger,
};
use crate::Error;

use lightning::chain::chaininterface::{
	BroadcasterInterface, ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW,
};

use bdk::blockchain::{Blockchain, EsploraBlockchain};
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{FeeRate, SignOptions, SyncOptions};

use bitcoin::{Script, Transaction};

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
	fee_rate_cache: RwLock<HashMap<ConfirmationTarget, FeeRate>>,
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
		let fee_rate_cache = RwLock::new(HashMap::new());
		let tokio_runtime = RwLock::new(None);
		Self { blockchain, inner, fee_rate_cache, tokio_runtime, logger }
	}

	pub(crate) async fn sync(&self) -> Result<(), Error> {
		match self.update_fee_estimates().await {
			Ok(()) => (),
			Err(e) => {
				log_error!(self.logger, "Fee estimation error: {}", e);
				return Err(e);
			}
		}

		let sync_options = SyncOptions { progress: None };
		match self.inner.lock().unwrap().sync(&self.blockchain, sync_options).await {
			Ok(()) => Ok(()),
			Err(e) => {
				log_error!(self.logger, "Wallet sync error: {}", e);
				Err(From::from(e))
			}
		}
	}

	pub(crate) fn set_runtime(&self, tokio_runtime: Arc<tokio::runtime::Runtime>) {
		*self.tokio_runtime.write().unwrap() = Some(tokio_runtime);
	}

	pub(crate) fn drop_runtime(&self) {
		*self.tokio_runtime.write().unwrap() = None;
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
						"Fee rate estimation updated: {} sats/kwu",
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
				Err(err)?
			}
		};

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
