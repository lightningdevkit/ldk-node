// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bdk_chain::bdk_core::spk_client::{
	FullScanRequest as BdkFullScanRequest, FullScanResponse as BdkFullScanResponse,
	SyncRequest as BdkSyncRequest, SyncResponse as BdkSyncResponse,
};
use bdk_electrum::BdkElectrumClient;
use bdk_wallet::{KeychainKind as BdkKeyChainKind, Update as BdkUpdate};
use bitcoin::{FeeRate, Network, Script, ScriptBuf, Transaction, Txid};
use electrum_client::{
	Batch, Client as ElectrumClient, ConfigBuilder as ElectrumConfigBuilder, ElectrumApi,
};
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::util::ser::Writeable;
use lightning_transaction_sync::ElectrumSyncClient;

use super::WalletSyncStatus;
use crate::config::{
	Config, ElectrumSyncConfig, BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS,
	FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, LDK_WALLET_SYNC_TIMEOUT_SECS, TX_BROADCAST_TIMEOUT_SECS,
};
use crate::error::Error;
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	ConfirmationTarget, OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_debug, log_error, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::NodeMetrics;

const BDK_ELECTRUM_CLIENT_BATCH_SIZE: usize = 5;
const ELECTRUM_CLIENT_NUM_RETRIES: u8 = 3;
const ELECTRUM_CLIENT_TIMEOUT_SECS: u8 = 10;

pub(super) struct ElectrumChainSource {
	server_url: String,
	pub(super) sync_config: ElectrumSyncConfig,
	electrum_runtime_status: RwLock<ElectrumRuntimeStatus>,
	onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
	lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	kv_store: Arc<DynStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl ElectrumChainSource {
	pub(super) fn new(
		server_url: String, sync_config: ElectrumSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let electrum_runtime_status = RwLock::new(ElectrumRuntimeStatus::new());
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			server_url,
			sync_config,
			electrum_runtime_status,
			onchain_wallet_sync_status,
			lightning_wallet_sync_status,
			fee_estimator,
			kv_store,
			config,
			logger: Arc::clone(&logger),
			node_metrics,
		}
	}

	pub(super) fn start(&self, runtime: Arc<Runtime>) -> Result<(), Error> {
		self.electrum_runtime_status.write().unwrap().start(
			self.server_url.clone(),
			Arc::clone(&runtime),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		)
	}

	pub(super) fn stop(&self) {
		self.electrum_runtime_status.write().unwrap().stop();
	}

	pub(crate) async fn sync_onchain_wallet(
		&self, onchain_wallet: Arc<Wallet>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.onchain_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_debug!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		let res = self.sync_onchain_wallet_inner(onchain_wallet).await;

		self.onchain_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

		res
	}

	async fn sync_onchain_wallet_inner(&self, onchain_wallet: Arc<Wallet>) -> Result<(), Error> {
		let electrum_client: Arc<ElectrumRuntimeClient> =
			if let Some(client) = self.electrum_runtime_status.read().unwrap().client().as_ref() {
				Arc::clone(client)
			} else {
				debug_assert!(
					false,
					"We should have started the chain source before syncing the onchain wallet"
				);
				return Err(Error::FeerateEstimationUpdateFailed);
			};
		// If this is our first sync, do a full scan with the configured gap limit.
		// Otherwise just do an incremental sync.
		let incremental_sync =
			self.node_metrics.read().unwrap().latest_onchain_wallet_sync_timestamp.is_some();

		let apply_wallet_update =
			|update_res: Result<BdkUpdate, Error>, now: Instant| match update_res {
				Ok(update) => match onchain_wallet.apply_update(update) {
					Ok(()) => {
						log_debug!(
							self.logger,
							"{} of on-chain wallet finished in {}ms.",
							if incremental_sync { "Incremental sync" } else { "Sync" },
							now.elapsed().as_millis()
						);
						let unix_time_secs_opt =
							SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
						{
							let mut locked_node_metrics = self.node_metrics.write().unwrap();
							locked_node_metrics.latest_onchain_wallet_sync_timestamp =
								unix_time_secs_opt;
							write_node_metrics(
								&*locked_node_metrics,
								&*self.kv_store,
								&*self.logger,
							)?;
						}
						Ok(())
					},
					Err(e) => Err(e),
				},
				Err(e) => Err(e),
			};

		let cached_txs = onchain_wallet.get_cached_txs();

		let res = if incremental_sync {
			let incremental_sync_request = onchain_wallet.get_incremental_sync_request();
			let incremental_sync_fut = electrum_client
				.get_incremental_sync_wallet_update(incremental_sync_request, cached_txs);

			let now = Instant::now();
			let update_res = incremental_sync_fut.await.map(|u| u.into());
			apply_wallet_update(update_res, now)
		} else {
			let full_scan_request = onchain_wallet.get_full_scan_request();
			let full_scan_fut =
				electrum_client.get_full_scan_wallet_update(full_scan_request, cached_txs);
			let now = Instant::now();
			let update_res = full_scan_fut.await.map(|u| u.into());
			apply_wallet_update(update_res, now)
		};

		res
	}

	pub(crate) async fn sync_lightning_wallet(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.lightning_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_debug!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::TxSyncFailed
			})?;
		}

		let res =
			self.sync_lightning_wallet_inner(channel_manager, chain_monitor, output_sweeper).await;

		self.lightning_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

		res
	}

	async fn sync_lightning_wallet_inner(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let sync_cman = Arc::clone(&channel_manager);
		let sync_cmon = Arc::clone(&chain_monitor);
		let sync_sweeper = Arc::clone(&output_sweeper);
		let confirmables = vec![
			sync_cman as Arc<dyn Confirm + Sync + Send>,
			sync_cmon as Arc<dyn Confirm + Sync + Send>,
			sync_sweeper as Arc<dyn Confirm + Sync + Send>,
		];

		let electrum_client: Arc<ElectrumRuntimeClient> =
			if let Some(client) = self.electrum_runtime_status.read().unwrap().client().as_ref() {
				Arc::clone(client)
			} else {
				debug_assert!(
					false,
					"We should have started the chain source before syncing the lightning wallet"
				);
				return Err(Error::TxSyncFailed);
			};

		let res = electrum_client.sync_confirmables(confirmables).await;

		if let Ok(_) = res {
			let unix_time_secs_opt =
				SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
			{
				let mut locked_node_metrics = self.node_metrics.write().unwrap();
				locked_node_metrics.latest_lightning_wallet_sync_timestamp = unix_time_secs_opt;
				write_node_metrics(&*locked_node_metrics, &*self.kv_store, &*self.logger)?;
			}
		}

		res
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		let electrum_client: Arc<ElectrumRuntimeClient> = if let Some(client) =
			self.electrum_runtime_status.read().unwrap().client().as_ref()
		{
			Arc::clone(client)
		} else {
			debug_assert!(false, "We should have started the chain source before updating fees");
			return Err(Error::FeerateEstimationUpdateFailed);
		};

		let now = Instant::now();

		let new_fee_rate_cache = electrum_client.get_fee_rate_cache_update().await?;
		self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache);

		log_debug!(
			self.logger,
			"Fee rate cache update finished in {}ms.",
			now.elapsed().as_millis()
		);

		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		{
			let mut locked_node_metrics = self.node_metrics.write().unwrap();
			locked_node_metrics.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt;
			write_node_metrics(&*locked_node_metrics, &*self.kv_store, &*self.logger)?;
		}

		Ok(())
	}

	pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
		let electrum_client: Arc<ElectrumRuntimeClient> =
			if let Some(client) = self.electrum_runtime_status.read().unwrap().client().as_ref() {
				Arc::clone(client)
			} else {
				debug_assert!(false, "We should have started the chain source before broadcasting");
				return;
			};

		for tx in package {
			electrum_client.broadcast(tx).await;
		}
	}
}

impl Filter for ElectrumChainSource {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.electrum_runtime_status.write().unwrap().register_tx(txid, script_pubkey)
	}
	fn register_output(&self, output: lightning::chain::WatchedOutput) {
		self.electrum_runtime_status.write().unwrap().register_output(output)
	}
}

enum ElectrumRuntimeStatus {
	Started(Arc<ElectrumRuntimeClient>),
	Stopped {
		pending_registered_txs: Vec<(Txid, ScriptBuf)>,
		pending_registered_outputs: Vec<WatchedOutput>,
	},
}

impl ElectrumRuntimeStatus {
	fn new() -> Self {
		let pending_registered_txs = Vec::new();
		let pending_registered_outputs = Vec::new();
		Self::Stopped { pending_registered_txs, pending_registered_outputs }
	}

	pub(super) fn start(
		&mut self, server_url: String, runtime: Arc<Runtime>, config: Arc<Config>,
		logger: Arc<Logger>,
	) -> Result<(), Error> {
		match self {
			Self::Stopped { pending_registered_txs, pending_registered_outputs } => {
				let client = Arc::new(ElectrumRuntimeClient::new(
					server_url.clone(),
					runtime,
					config,
					logger,
				)?);

				// Apply any pending `Filter` entries
				for (txid, script_pubkey) in pending_registered_txs.drain(..) {
					client.register_tx(&txid, &script_pubkey);
				}

				for output in pending_registered_outputs.drain(..) {
					client.register_output(output)
				}

				*self = Self::Started(client);
			},
			Self::Started(_) => {
				debug_assert!(false, "We shouldn't call start if we're already started")
			},
		}
		Ok(())
	}

	pub(super) fn stop(&mut self) {
		*self = Self::new()
	}

	fn client(&self) -> Option<Arc<ElectrumRuntimeClient>> {
		match self {
			Self::Started(client) => Some(Arc::clone(&client)),
			Self::Stopped { .. } => None,
		}
	}

	fn register_tx(&mut self, txid: &Txid, script_pubkey: &Script) {
		match self {
			Self::Started(client) => client.register_tx(txid, script_pubkey),
			Self::Stopped { pending_registered_txs, .. } => {
				pending_registered_txs.push((*txid, script_pubkey.to_owned()))
			},
		}
	}

	fn register_output(&mut self, output: lightning::chain::WatchedOutput) {
		match self {
			Self::Started(client) => client.register_output(output),
			Self::Stopped { pending_registered_outputs, .. } => {
				pending_registered_outputs.push(output)
			},
		}
	}
}

struct ElectrumRuntimeClient {
	electrum_client: Arc<ElectrumClient>,
	bdk_electrum_client: Arc<BdkElectrumClient<Arc<ElectrumClient>>>,
	tx_sync: Arc<ElectrumSyncClient<Arc<Logger>>>,
	runtime: Arc<Runtime>,
	config: Arc<Config>,
	logger: Arc<Logger>,
}

impl ElectrumRuntimeClient {
	fn new(
		server_url: String, runtime: Arc<Runtime>, config: Arc<Config>, logger: Arc<Logger>,
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
		let bdk_electrum_client = Arc::new(BdkElectrumClient::new(Arc::clone(&electrum_client)));
		let tx_sync = Arc::new(
			ElectrumSyncClient::new(server_url.clone(), Arc::clone(&logger)).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?,
		);
		Ok(Self { electrum_client, bdk_electrum_client, tx_sync, runtime, config, logger })
	}

	async fn sync_confirmables(
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

		log_debug!(
			self.logger,
			"Sync of Lightning wallet finished in {}ms.",
			now.elapsed().as_millis()
		);

		Ok(res)
	}

	async fn get_full_scan_wallet_update(
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

	async fn get_incremental_sync_wallet_update(
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

	async fn broadcast(&self, tx: Transaction) {
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

	async fn get_fee_rate_cache_update(
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
