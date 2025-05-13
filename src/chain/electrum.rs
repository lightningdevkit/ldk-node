// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::{
	Config, BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS, FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS,
	LDK_WALLET_SYNC_TIMEOUT_SECS, TX_BROADCAST_TIMEOUT_SECS,
};
use crate::error::Error;
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	ConfirmationTarget,
};
use crate::logger::{log_bytes, log_error, log_info, log_trace, LdkLogger, Logger};

use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::util::ser::Writeable;
use lightning_transaction_sync::ElectrumSyncClient;

use bdk_chain::bdk_core::spk_client::FullScanRequest as BdkFullScanRequest;
use bdk_chain::bdk_core::spk_client::FullScanResponse as BdkFullScanResponse;
use bdk_chain::bdk_core::spk_client::SyncRequest as BdkSyncRequest;
use bdk_chain::bdk_core::spk_client::SyncResponse as BdkSyncResponse;
use bdk_wallet::KeychainKind as BdkKeyChainKind;

use bdk_electrum::BdkElectrumClient;

use electrum_client::Client as ElectrumClient;
use electrum_client::ConfigBuilder as ElectrumConfigBuilder;
use electrum_client::{Batch, ElectrumApi};

use bitcoin::{FeeRate, Network, Script, Transaction, Txid};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BDK_ELECTRUM_CLIENT_BATCH_SIZE: usize = 5;
const ELECTRUM_CLIENT_NUM_RETRIES: u8 = 3;
const ELECTRUM_CLIENT_TIMEOUT_SECS: u8 = 10;

pub(crate) struct ElectrumRuntimeClient {
	electrum_client: Arc<ElectrumClient>,
	bdk_electrum_client: Arc<BdkElectrumClient<ElectrumClient>>,
	tx_sync: Arc<ElectrumSyncClient<Arc<Logger>>>,
	runtime: Arc<tokio::runtime::Runtime>,
	config: Arc<Config>,
	logger: Arc<Logger>,
}

impl ElectrumRuntimeClient {
	pub(crate) fn new(
		server_url: String, runtime: Arc<tokio::runtime::Runtime>, config: Arc<Config>,
		logger: Arc<Logger>,
	) -> Result<Self, Error> {
		let electrum_config = ElectrumConfigBuilder::new()
			.retry(ELECTRUM_CLIENT_NUM_RETRIES)
			.timeout(Some(ELECTRUM_CLIENT_TIMEOUT_SECS))
			.build();

		let electrum_client = Arc::new(
			ElectrumClient::from_config(&server_url, electrum_config.clone()).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?,
		);
		let electrum_client_2 =
			ElectrumClient::from_config(&server_url, electrum_config).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?;
		let bdk_electrum_client = Arc::new(BdkElectrumClient::new(electrum_client_2));
		let tx_sync = Arc::new(
			ElectrumSyncClient::new(server_url.clone(), Arc::clone(&logger)).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?,
		);
		Ok(Self { electrum_client, bdk_electrum_client, tx_sync, runtime, config, logger })
	}

	pub(crate) async fn sync_confirmables(
		&self, confirmables: Vec<Arc<dyn Confirm + Sync + Send>>,
	) -> Result<(), Error> {
		let now = Instant::now();

		let tx_sync = Arc::clone(&self.tx_sync);
		let spawn_fut = self.runtime.spawn_blocking(move || tx_sync.sync(confirmables));
		let timeout_fut =
			tokio::time::timeout(Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS), spawn_fut);

		let res = timeout_fut
			.await
			.map_err(|e| {
				log_error!(self.logger, "Sync of Lightning wallet timed out: {}", e);
				Error::TxSyncTimeout
			})?
			.map_err(|e| {
				log_error!(self.logger, "Sync of Lightning wallet failed: {}", e);
				Error::TxSyncFailed
			})?
			.map_err(|e| {
				log_error!(self.logger, "Sync of Lightning wallet failed: {}", e);
				Error::TxSyncFailed
			})?;

		log_info!(
			self.logger,
			"Sync of Lightning wallet finished in {}ms.",
			now.elapsed().as_millis()
		);

		Ok(res)
	}

	pub(crate) async fn get_full_scan_wallet_update(
		&self, request: BdkFullScanRequest<BdkKeyChainKind>,
		cached_txs: impl IntoIterator<Item = impl Into<Arc<Transaction>>>,
	) -> Result<BdkFullScanResponse<BdkKeyChainKind>, Error> {
		let bdk_electrum_client = Arc::clone(&self.bdk_electrum_client);
		bdk_electrum_client.populate_tx_cache(cached_txs);

		let spawn_fut = self.runtime.spawn_blocking(move || {
			bdk_electrum_client.full_scan(
				request,
				BDK_CLIENT_STOP_GAP,
				BDK_ELECTRUM_CLIENT_BATCH_SIZE,
				true,
			)
		});
		let wallet_sync_timeout_fut =
			tokio::time::timeout(Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS), spawn_fut);

		wallet_sync_timeout_fut
			.await
			.map_err(|e| {
				log_error!(self.logger, "Sync of on-chain wallet timed out: {}", e);
				Error::WalletOperationTimeout
			})?
			.map_err(|e| {
				log_error!(self.logger, "Sync of on-chain wallet failed: {}", e);
				Error::WalletOperationFailed
			})?
			.map_err(|e| {
				log_error!(self.logger, "Sync of on-chain wallet failed: {}", e);
				Error::WalletOperationFailed
			})
	}

	pub(crate) async fn get_incremental_sync_wallet_update(
		&self, request: BdkSyncRequest<(BdkKeyChainKind, u32)>,
		cached_txs: impl IntoIterator<Item = impl Into<Arc<Transaction>>>,
	) -> Result<BdkSyncResponse, Error> {
		let bdk_electrum_client = Arc::clone(&self.bdk_electrum_client);
		bdk_electrum_client.populate_tx_cache(cached_txs);

		let spawn_fut = self.runtime.spawn_blocking(move || {
			bdk_electrum_client.sync(request, BDK_ELECTRUM_CLIENT_BATCH_SIZE, true)
		});
		let wallet_sync_timeout_fut =
			tokio::time::timeout(Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS), spawn_fut);

		wallet_sync_timeout_fut
			.await
			.map_err(|e| {
				log_error!(self.logger, "Incremental sync of on-chain wallet timed out: {}", e);
				Error::WalletOperationTimeout
			})?
			.map_err(|e| {
				log_error!(self.logger, "Incremental sync of on-chain wallet failed: {}", e);
				Error::WalletOperationFailed
			})?
			.map_err(|e| {
				log_error!(self.logger, "Incremental sync of on-chain wallet failed: {}", e);
				Error::WalletOperationFailed
			})
	}

	pub(crate) async fn broadcast(&self, tx: Transaction) {
		let electrum_client = Arc::clone(&self.electrum_client);

		let txid = tx.compute_txid();
		let tx_bytes = tx.encode();

		let spawn_fut =
			self.runtime.spawn_blocking(move || electrum_client.transaction_broadcast(&tx));

		let timeout_fut =
			tokio::time::timeout(Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS), spawn_fut);

		match timeout_fut.await {
			Ok(res) => match res {
				Ok(_) => {
					log_trace!(self.logger, "Successfully broadcast transaction {}", txid);
				},
				Err(e) => {
					log_error!(self.logger, "Failed to broadcast transaction {}: {}", txid, e);
					log_trace!(
						self.logger,
						"Failed broadcast transaction bytes: {}",
						log_bytes!(tx_bytes)
					);
				},
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to broadcast transaction due to timeout {}: {}",
					txid,
					e
				);
				log_trace!(
					self.logger,
					"Failed broadcast transaction bytes: {}",
					log_bytes!(tx_bytes)
				);
			},
		}
	}

	pub(crate) async fn get_fee_rate_cache_update(
		&self,
	) -> Result<HashMap<ConfirmationTarget, FeeRate>, Error> {
		let electrum_client = Arc::clone(&self.electrum_client);

		let mut batch = Batch::default();
		let confirmation_targets = get_all_conf_targets();
		for target in confirmation_targets {
			let num_blocks = get_num_block_defaults_for_target(target);
			batch.estimate_fee(num_blocks);
		}

		let spawn_fut = self.runtime.spawn_blocking(move || electrum_client.batch_call(&batch));

		let timeout_fut = tokio::time::timeout(
			Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
			spawn_fut,
		);

		let raw_estimates_btc_kvb = timeout_fut
			.await
			.map_err(|e| {
				log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
				Error::FeerateEstimationUpdateTimeout
			})?
			.map_err(|e| {
				log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
				Error::FeerateEstimationUpdateFailed
			})?
			.map_err(|e| {
				log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
				Error::FeerateEstimationUpdateFailed
			})?;

		if raw_estimates_btc_kvb.len() != confirmation_targets.len()
			&& self.config.network == Network::Bitcoin
		{
			// Ensure we fail if we didn't receive all estimates.
			debug_assert!(false,
				"Electrum server didn't return all expected results. This is disallowed on Mainnet."
			);
			log_error!(self.logger,
				"Failed to retrieve fee rate estimates: Electrum server didn't return all expected results. This is disallowed on Mainnet."
			);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let mut new_fee_rate_cache = HashMap::with_capacity(10);
		for (target, raw_fee_rate_btc_per_kvb) in
			confirmation_targets.into_iter().zip(raw_estimates_btc_kvb.into_iter())
		{
			// Parse the retrieved serde_json::Value and fall back to 1 sat/vb (10^3 / 10^8 = 10^-5
			// = 0.00001 btc/kvb) if we fail or it yields less than that. This is mostly necessary
			// to continue on `signet`/`regtest` where we might not get estimates (or bogus
			// values).
			let fee_rate_btc_per_kvb = raw_fee_rate_btc_per_kvb
				.as_f64()
				.map_or(0.00001, |converted| converted.max(0.00001));

			// Electrum, just like Bitcoin Core, gives us a feerate in BTC/KvB.
			// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
			let fee_rate = {
				let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvb * 25_000_000.0).round() as u64;
				FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
			};

			// LDK 0.0.118 introduced changes to the `ConfirmationTarget` semantics that
			// require some post-estimation adjustments to the fee rates, which we do here.
			let adjusted_fee_rate = apply_post_estimation_adjustments(target, fee_rate);

			new_fee_rate_cache.insert(target, adjusted_fee_rate);

			log_trace!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted_fee_rate.to_sat_per_kwu(),
			);
		}

		Ok(new_fee_rate_cache)
	}
}

impl Filter for ElectrumRuntimeClient {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.tx_sync.register_tx(txid, script_pubkey)
	}
	fn register_output(&self, output: WatchedOutput) {
		self.tx_sync.register_output(output)
	}
}
