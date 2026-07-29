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
	SyncProgress as BdkSyncProgress, SyncRequest as BdkSyncRequest,
	SyncResponse as BdkSyncResponse,
};
use bdk_electrum::BdkElectrumClient;
use bdk_wallet::{KeychainKind as BdkKeyChainKind, Update as BdkUpdate};
use bitcoin::{FeeRate, Network, Script, ScriptBuf, Transaction, Txid};
use electrum_client::{
	Batch, Client as ElectrumClient, ConfigBuilder as ElectrumConfigBuilder, ElectrumApi,
};
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::log_warn;
use lightning::util::ser::Writeable;
use lightning_transaction_sync::ElectrumSyncClient;
use tokio::runtime::Handle;

use super::{periodically_archive_fully_resolved_monitors, WalletSyncStatus};
use crate::config::{
	AddressTypeRuntimeConfig, Config, ElectrumSyncConfig, BDK_CLIENT_STOP_GAP,
	BDK_ELECTRUM_CLIENT_BATCH_SIZE, BDK_WALLET_SYNC_TIMEOUT_SECS,
	DEFAULT_ELECTRUM_CONNECTION_TIMEOUT_SECS, FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS,
	LDK_WALLET_SYNC_TIMEOUT_SECS, TX_BROADCAST_TIMEOUT_SECS,
};
use crate::error::Error;
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	ConfirmationTarget, OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::NodeMetrics;

const ELECTRUM_CLIENT_NUM_RETRIES: u8 = 3;

fn effective_connection_timeout_secs(configured_timeout_secs: u64, logger: &Logger) -> u8 {
	let requested_timeout = if configured_timeout_secs == 0 {
		log_warn!(
			logger,
			"Electrum connection_timeout_secs is 0; using the safe default of {} seconds.",
			DEFAULT_ELECTRUM_CONNECTION_TIMEOUT_SECS,
		);
		DEFAULT_ELECTRUM_CONNECTION_TIMEOUT_SECS
	} else {
		configured_timeout_secs
	};
	let capped_timeout = requested_timeout.min(u8::MAX as u64) as u8;
	if capped_timeout as u64 != requested_timeout {
		log_warn!(
			logger,
			"Electrum connection_timeout_secs ({}) exceeds maximum of {}; capping to {}.",
			requested_timeout,
			u8::MAX,
			capped_timeout,
		);
	}
	capped_timeout
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FullScanSettings {
	stop_gap: usize,
	batch_size: usize,
}

const PRIMARY_WALLET_FULL_SCAN_SETTINGS: FullScanSettings =
	FullScanSettings { stop_gap: BDK_CLIENT_STOP_GAP, batch_size: BDK_ELECTRUM_CLIENT_BATCH_SIZE };

fn additional_wallet_full_scan_settings(config: &ElectrumSyncConfig) -> FullScanSettings {
	FullScanSettings {
		stop_gap: config.additional_wallet_full_scan_stop_gap.max(1) as usize,
		batch_size: config.additional_wallet_full_scan_batch_size.max(1) as usize,
	}
}

fn additional_wallet_full_scan_timeout(
	settings: FullScanSettings, keychain_count: usize,
) -> Duration {
	let request_count =
		settings.stop_gap.div_ceil(settings.batch_size).saturating_mul(keychain_count);
	let request_count = u64::try_from(request_count).unwrap_or(u64::MAX);
	Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS.saturating_add(request_count))
}

fn additional_wallet_incremental_batch_size(
	account_index: u32, full_scan_settings: FullScanSettings,
) -> usize {
	if account_index == 0 {
		BDK_ELECTRUM_CLIENT_BATCH_SIZE
	} else {
		full_scan_settings.batch_size
	}
}

fn derived_wallet_incremental_sync_timeout(
	progress: BdkSyncProgress, batch_size: usize,
) -> Duration {
	let request_count = progress
		.spks_remaining
		.div_ceil(batch_size)
		.saturating_add(progress.txids_remaining)
		.saturating_add(progress.outpoints_remaining);
	let request_count = u64::try_from(request_count).unwrap_or(u64::MAX);
	Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS.saturating_add(request_count))
}

pub(super) struct ElectrumChainSource {
	server_url: String,
	pub(super) sync_config: ElectrumSyncConfig,
	electrum_runtime_status: RwLock<ElectrumRuntimeStatus>,
	onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
	lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	pub(super) kv_store: Arc<DynStore>,
	pub(super) config: Arc<Config>,
	address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>,
	logger: Arc<Logger>,
	pub(super) node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl ElectrumChainSource {
	pub(super) fn new(
		server_url: String, sync_config: ElectrumSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
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
			address_type_runtime_config,
			logger: Arc::clone(&logger),
			node_metrics,
		}
	}

	pub(super) fn start(&self, runtime_handle: Handle) -> Result<(), Error> {
		self.electrum_runtime_status.write().unwrap().start(
			self.server_url.clone(),
			runtime_handle,
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
			self.sync_config.connection_timeout_secs,
		)
	}

	pub(super) fn stop(&self) {
		self.electrum_runtime_status.write().unwrap().stop();
	}

	pub(crate) async fn sync_onchain_wallet(
		&self, onchain_wallet: Arc<Wallet>,
	) -> super::WalletSyncOutcome {
		let receiver_res = {
			let mut status_lock = self.onchain_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_info!(self.logger, "Sync in progress, skipping.");
			match sync_receiver.recv().await {
				Ok(Ok(())) => return super::WalletSyncOutcome::new(Vec::new(), None),
				Ok(Err(e)) => return super::WalletSyncOutcome::failed(e),
				Err(e) => {
					debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
					log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
					return super::WalletSyncOutcome::failed(Error::WalletOperationFailed);
				},
			}
		}

		let outcome = self
			.sync_onchain_wallet_inner(onchain_wallet)
			.await
			.unwrap_or_else(super::WalletSyncOutcome::failed);

		self.onchain_wallet_sync_status
			.lock()
			.unwrap()
			.propagate_result_to_subscribers(outcome.result());

		outcome
	}

	async fn sync_onchain_wallet_inner(
		&self, onchain_wallet: Arc<Wallet>,
	) -> Result<super::WalletSyncOutcome, Error> {
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

		let primary_incremental =
			self.node_metrics.read().unwrap().latest_onchain_wallet_sync_timestamp.is_some();

		let additional_accounts =
			self.address_type_runtime_config.read().unwrap().additional_wallet_accounts();
		let additional_sync_requests = super::collect_additional_sync_requests(
			&additional_accounts,
			&onchain_wallet,
			&self.node_metrics,
			&self.logger,
		)?;
		let additional_full_scan_settings = additional_wallet_full_scan_settings(&self.sync_config);

		let primary_request: super::WalletSyncRequest = if primary_incremental {
			super::WalletSyncRequest::Incremental(onchain_wallet.get_incremental_sync_request())
		} else {
			super::WalletSyncRequest::FullScan(onchain_wallet.get_full_scan_request())
		};

		// Collect cached transactions once and share via Arc to avoid cloning
		// the entire Vec for each spawned task.
		let cached_txs = Arc::new(onchain_wallet.get_cached_txs());

		// Primary wallet is identified by address_type = None in the JoinSet results.
		let now = Instant::now();
		let mut join_set: tokio::task::JoinSet<(
			Option<crate::config::OnchainWalletAccount>,
			Result<BdkUpdate, Error>,
		)> = tokio::task::JoinSet::new();

		{
			let client = Arc::clone(&electrum_client);
			let txs = Arc::clone(&cached_txs);
			match primary_request {
				super::WalletSyncRequest::Incremental(req) => {
					join_set.spawn(async move {
						let result: Result<BdkUpdate, Error> = client
							.get_incremental_sync_wallet_update(
								req,
								txs.iter().cloned(),
								BDK_ELECTRUM_CLIENT_BATCH_SIZE,
								Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							)
							.await
							.map(|u| u.into());
						(None, result)
					});
				},
				super::WalletSyncRequest::FullScan(req) => {
					join_set.spawn(async move {
						let result: Result<BdkUpdate, Error> = client
							.get_full_scan_wallet_update(
								req,
								txs.iter().cloned(),
								PRIMARY_WALLET_FULL_SCAN_SETTINGS,
								Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							)
							.await
							.map(|u| u.into());
						(None, result)
					});
				},
			}
		}

		for (wallet_account, sync_req) in additional_sync_requests {
			let client = Arc::clone(&electrum_client);
			let txs = Arc::clone(&cached_txs);
			let incremental_batch_size = additional_wallet_incremental_batch_size(
				wallet_account.account_index,
				additional_full_scan_settings,
			);
			match sync_req {
				super::WalletSyncRequest::Incremental(req) => {
					let timeout = if wallet_account.account_index == 0 {
						Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS)
					} else {
						derived_wallet_incremental_sync_timeout(
							req.progress(),
							incremental_batch_size,
						)
					};
					join_set.spawn(async move {
						let result: Result<BdkUpdate, Error> = client
							.get_incremental_sync_wallet_update(
								req,
								txs.iter().cloned(),
								incremental_batch_size,
								timeout,
							)
							.await
							.map(|u| u.into());
						(Some(wallet_account), result)
					});
				},
				super::WalletSyncRequest::FullScan(req) => {
					let timeout = additional_wallet_full_scan_timeout(
						additional_full_scan_settings,
						req.keychains().len(),
					);
					join_set.spawn(async move {
						let result: Result<BdkUpdate, Error> = client
							.get_full_scan_wallet_update(
								req,
								txs.iter().cloned(),
								additional_full_scan_settings,
								timeout,
							)
							.await
							.map(|u| u.into());
						(Some(wallet_account), result)
					});
				},
			}
		}

		let mut primary_update: Option<BdkUpdate> = None;
		let mut primary_error: Option<Error> = None;
		let mut additional_results = Vec::new();
		let mut task_error = None;

		while let Some(join_result) = join_set.join_next().await {
			match join_result {
				Ok((None, Ok(update))) => {
					primary_update = Some(update);
				},
				Ok((None, Err(e))) => {
					primary_error = Some(e);
				},
				Ok((Some(wallet_account), Ok(update))) => {
					additional_results.push((wallet_account, Ok(update)));
				},
				Ok((Some(wallet_account), Err(e))) => {
					log_warn!(self.logger, "Failed to sync wallet {:?}: {}", wallet_account, e);
					additional_results.push((wallet_account, Err(e)));
				},
				Err(e) => {
					log_warn!(self.logger, "Wallet sync task panicked: {}", e);
					task_error = Some(Error::WalletOperationFailed);
				},
			};
		}

		if primary_update.is_none() && primary_error.is_none() {
			log_error!(self.logger, "Primary wallet sync task failed unexpectedly");
			primary_error = Some(Error::WalletOperationFailed);
		}
		let mut outcome = super::apply_wallet_sync_results(
			primary_update,
			primary_error,
			task_error,
			additional_results,
			&onchain_wallet,
			&self.node_metrics,
			&self.logger,
		);
		if outcome.primary_applied {
			log_info!(
				self.logger,
				"{} of primary on-chain wallet finished in {}ms.",
				if primary_incremental { "Incremental sync" } else { "Full sync" },
				now.elapsed().as_millis()
			);
		}

		if outcome.any_applied {
			if let Err(e) = onchain_wallet.update_payment_store_for_all_transactions() {
				log_error!(self.logger, "Failed to update payment store after wallet syncs: {}", e);
				outcome.error.get_or_insert(e);
			}

			let locked_node_metrics = self.node_metrics.read().unwrap();
			if let Err(e) = write_node_metrics(
				&*locked_node_metrics,
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
			) {
				log_error!(self.logger, "Failed to persist node metrics: {}", e);
			}
		}

		Ok(super::WalletSyncOutcome::new(outcome.events, outcome.error))
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
			log_info!(self.logger, "Sync in progress, skipping.");
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
				write_node_metrics(
					&*locked_node_metrics,
					Arc::clone(&self.kv_store),
					Arc::clone(&self.logger),
				)?;
			}

			periodically_archive_fully_resolved_monitors(
				Arc::clone(&channel_manager),
				Arc::clone(&chain_monitor),
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
				Arc::clone(&self.node_metrics),
			)?;
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

		log_info!(
			self.logger,
			"Fee rate cache update finished in {}ms.",
			now.elapsed().as_millis()
		);

		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		{
			let mut locked_node_metrics = self.node_metrics.write().unwrap();
			locked_node_metrics.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt;
			write_node_metrics(
				&*locked_node_metrics,
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
			)?;
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

	pub(super) async fn get_address_balance(&self, address: &bitcoin::Address) -> Option<u64> {
		let electrum_client: Arc<ElectrumRuntimeClient> =
			if let Some(client) = self.electrum_runtime_status.read().unwrap().client().as_ref() {
				Arc::clone(client)
			} else {
				return None;
			};
		electrum_client.get_address_balance(address).await
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
		&mut self, server_url: String, runtime_handle: Handle, config: Arc<Config>,
		logger: Arc<Logger>, connection_timeout_secs: u64,
	) -> Result<(), Error> {
		match self {
			Self::Stopped { pending_registered_txs, pending_registered_outputs } => {
				let client = Arc::new(ElectrumRuntimeClient::new(
					server_url.clone(),
					runtime_handle,
					config,
					logger,
					connection_timeout_secs,
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
	runtime_handle: Handle,
	config: Arc<Config>,
	logger: Arc<Logger>,
}

impl ElectrumRuntimeClient {
	fn new(
		server_url: String, runtime_handle: Handle, config: Arc<Config>, logger: Arc<Logger>,
		connection_timeout_secs: u64,
	) -> Result<Self, Error> {
		// Every socket operation needs a finite bound so a cancelled blocking job eventually
		// completes and cannot hold owned-runtime shutdown open indefinitely.
		let timeout = effective_connection_timeout_secs(connection_timeout_secs, logger.as_ref());

		let electrum_config = ElectrumConfigBuilder::new()
			.retry(ELECTRUM_CLIENT_NUM_RETRIES)
			.timeout(Some(timeout))
			.build();

		let electrum_client = Arc::new(
			ElectrumClient::from_config(&server_url, electrum_config.clone()).map_err(|e| {
				log_error!(logger, "Failed to connect to Electrum server: {}", e);
				Error::ConnectionFailed
			})?,
		);
		let bdk_electrum_client = Arc::new(BdkElectrumClient::new(Arc::clone(&electrum_client)));

		// The LDK tx-sync client needs its own TCP connection, configured with the same
		// timeout so that its blocking reads are bounded and Tokio's blocking thread pool
		// is not exhausted by threads stuck on dead sockets.
		let ldk_electrum_client =
			Arc::new(ElectrumClient::from_config(&server_url, electrum_config).map_err(|e| {
				log_error!(logger, "Failed to connect to Electrum server for tx sync: {}", e);
				Error::ConnectionFailed
			})?);
		let tx_sync = Arc::new(
			ElectrumSyncClient::from_client(ldk_electrum_client, Arc::clone(&logger)).map_err(
				|e| {
					log_error!(logger, "Failed to initialize Electrum tx sync client: {}", e);
					Error::ConnectionFailed
				},
			)?,
		);
		Ok(Self { electrum_client, bdk_electrum_client, tx_sync, runtime_handle, config, logger })
	}

	pub(crate) async fn get_address_balance(&self, address: &bitcoin::Address) -> Option<u64> {
		use electrum_client::ElectrumApi;

		let script = address.script_pubkey();
		let electrum_client = Arc::clone(&self.electrum_client);
		let script_clone = script.clone();
		let balance_result = self
			.runtime_handle
			.spawn_blocking(move || {
				electrum_client
					.script_get_balance(&script_clone)
					.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{}", e)))
			})
			.await;

		match balance_result {
			Ok(Ok(balance)) => {
				let confirmed = balance.confirmed.max(0) as u64;
				let unconfirmed = balance.unconfirmed.max(0) as u64;
				Some(confirmed + unconfirmed)
			},
			_ => None,
		}
	}

	async fn sync_confirmables(
		&self, confirmables: Vec<Arc<dyn Confirm + Sync + Send>>,
	) -> Result<(), Error> {
		let now = Instant::now();

		let tx_sync = Arc::clone(&self.tx_sync);
		let spawn_fut = self.runtime_handle.spawn_blocking(move || tx_sync.sync(confirmables));
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

	async fn get_full_scan_wallet_update(
		&self, request: BdkFullScanRequest<BdkKeyChainKind>,
		cached_txs: impl IntoIterator<Item = impl Into<Arc<Transaction>>>,
		settings: FullScanSettings, timeout: Duration,
	) -> Result<BdkFullScanResponse<BdkKeyChainKind>, Error> {
		let bdk_electrum_client = Arc::clone(&self.bdk_electrum_client);
		bdk_electrum_client.populate_tx_cache(cached_txs);

		let spawn_fut = self.runtime_handle.spawn_blocking(move || {
			bdk_electrum_client.full_scan(request, settings.stop_gap, settings.batch_size, true)
		});
		let wallet_sync_timeout_fut = tokio::time::timeout(timeout, spawn_fut);

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
		cached_txs: impl IntoIterator<Item = impl Into<Arc<Transaction>>>, batch_size: usize,
		timeout: Duration,
	) -> Result<BdkSyncResponse, Error> {
		let bdk_electrum_client = Arc::clone(&self.bdk_electrum_client);
		bdk_electrum_client.populate_tx_cache(cached_txs);

		let spawn_fut = self
			.runtime_handle
			.spawn_blocking(move || bdk_electrum_client.sync(request, batch_size, true));
		let wallet_sync_timeout_fut = tokio::time::timeout(timeout, spawn_fut);

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
			self.runtime_handle.spawn_blocking(move || electrum_client.transaction_broadcast(&tx));
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

		let spawn_fut =
			self.runtime_handle.spawn_blocking(move || electrum_client.batch_call(&batch));

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

#[cfg(test)]
mod tests {
	use std::net::TcpListener;
	use std::process::Command;
	use std::sync::mpsc::sync_channel;
	use std::thread;

	use super::*;
	use crate::runtime::Runtime;

	const RUNTIME_SELF_DROP_CHILD_ENV: &str = "LDK_NODE_ELECTRUM_RUNTIME_SELF_DROP_CHILD";

	#[test]
	fn inflight_electrum_worker_does_not_own_runtime_lifecycle() {
		if std::env::var_os(RUNTIME_SELF_DROP_CHILD_ENV).is_some() {
			run_inflight_electrum_worker_drop();
			return;
		}

		let status = Command::new(std::env::current_exe().unwrap())
			.args([
				"--exact",
				"chain::electrum::tests::inflight_electrum_worker_does_not_own_runtime_lifecycle",
				"--nocapture",
			])
			.env(RUNTIME_SELF_DROP_CHILD_ENV, "1")
			.status()
			.unwrap();

		assert!(status.success(), "in-flight Electrum worker aborted during runtime teardown");
	}

	fn run_inflight_electrum_worker_drop() {
		std::panic::set_hook(Box::new(|panic_info| {
			eprintln!("{panic_info}");
			std::process::exit(101);
		}));

		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let server_url = format!("tcp://{}", listener.local_addr().unwrap());
		let (release_server_tx, release_server_rx) = sync_channel(0);
		let server_thread = thread::spawn(move || {
			let connections = [listener.accept().unwrap().0, listener.accept().unwrap().0];
			let _ = release_server_rx.recv();
			drop(connections);
		});

		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());
		let client = Arc::new(
			ElectrumRuntimeClient::new(
				server_url,
				runtime.handle().clone(),
				Arc::new(Config::default()),
				logger,
				1,
			)
			.unwrap(),
		);
		let (worker_started_tx, worker_started_rx) = sync_channel(0);
		let (release_worker_tx, release_worker_rx) = tokio::sync::oneshot::channel();

		runtime.spawn_background_task(async move {
			worker_started_tx.send(()).unwrap();
			let _ = release_worker_rx.await;
			drop(client);
		});
		worker_started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

		let release_thread = thread::spawn(move || {
			thread::sleep(Duration::from_millis(100));
			let _ = release_worker_tx.send(());
		});

		// A runtime handle lets this drop cancel the in-flight worker from the caller thread. An
		// Arc<Runtime> stored by the Electrum client would instead defer the last runtime drop to
		// the worker itself and abort the subprocess.
		drop(runtime);
		release_thread.join().unwrap();
		let _ = release_server_tx.send(());
		server_thread.join().unwrap();
		thread::sleep(Duration::from_millis(500));
	}

	#[test]
	fn additional_full_scan_settings_do_not_change_primary_defaults() {
		let default_config = ElectrumSyncConfig::default();
		let config = ElectrumSyncConfig {
			additional_wallet_full_scan_batch_size: 100,
			additional_wallet_full_scan_stop_gap: 1_000,
			..Default::default()
		};

		assert_eq!(
			PRIMARY_WALLET_FULL_SCAN_SETTINGS,
			FullScanSettings { stop_gap: 20, batch_size: 5 }
		);
		assert_eq!(
			additional_wallet_full_scan_settings(&default_config),
			PRIMARY_WALLET_FULL_SCAN_SETTINGS
		);
		assert_eq!(
			additional_wallet_full_scan_settings(&config),
			FullScanSettings { stop_gap: 1_000, batch_size: 100 }
		);
	}

	#[test]
	fn electrum_connection_timeout_is_always_finite_and_bounded() {
		let logger = Logger::new_log_facade();

		assert_eq!(
			effective_connection_timeout_secs(0, &logger),
			DEFAULT_ELECTRUM_CONNECTION_TIMEOUT_SECS as u8
		);
		assert_eq!(effective_connection_timeout_secs(1, &logger), 1);
		assert_eq!(effective_connection_timeout_secs(300, &logger), u8::MAX);
	}

	#[test]
	fn additional_full_scan_settings_are_never_zero() {
		let config = ElectrumSyncConfig {
			additional_wallet_full_scan_batch_size: 0,
			additional_wallet_full_scan_stop_gap: 0,
			..Default::default()
		};

		assert_eq!(
			additional_wallet_full_scan_settings(&config),
			FullScanSettings { stop_gap: 1, batch_size: 1 }
		);
	}

	#[test]
	fn additional_full_scan_timeout_scales_with_request_batches() {
		assert_eq!(
			additional_wallet_full_scan_timeout(
				FullScanSettings { stop_gap: 20, batch_size: 5 },
				2,
			),
			Duration::from_secs(28)
		);
		assert_eq!(
			additional_wallet_full_scan_timeout(
				FullScanSettings { stop_gap: 1_000, batch_size: 100 },
				2,
			),
			Duration::from_secs(40)
		);
	}

	#[test]
	fn derived_incremental_sync_uses_the_configured_batch_size() {
		let settings = FullScanSettings { stop_gap: 1_000, batch_size: 100 };

		assert_eq!(additional_wallet_incremental_batch_size(0, settings), 5);
		assert_eq!(additional_wallet_incremental_batch_size(1, settings), 100);
	}

	#[test]
	fn derived_incremental_timeout_scales_with_request_batches() {
		let settings = FullScanSettings { stop_gap: 1_000, batch_size: 100 };
		let progress = BdkSyncProgress {
			spks_consumed: 0,
			spks_remaining: 2_000,
			txids_consumed: 0,
			txids_remaining: 0,
			outpoints_consumed: 0,
			outpoints_remaining: 0,
		};

		assert_eq!(
			derived_wallet_incremental_sync_timeout(progress, settings.batch_size),
			Duration::from_secs(40)
		);
	}
}
