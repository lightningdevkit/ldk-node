// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod bitcoind_rpc;

use crate::chain::bitcoind_rpc::{
	BitcoindRpcClient, BoundedHeaderCache, ChainListener, FeeRateEstimationMode,
};
use crate::config::{
	Config, EsploraSyncConfig, BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP,
	BDK_WALLET_SYNC_TIMEOUT_SECS, FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, LDK_WALLET_SYNC_TIMEOUT_SECS,
	RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, TX_BROADCAST_TIMEOUT_SECS,
	WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	ConfirmationTarget, OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_error, log_info, log_trace, FilesystemLogger, Logger};
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

use lightning::chain::chaininterface::ConfirmationTarget as LdkConfirmationTarget;
use lightning::chain::{Confirm, Filter, Listen};
use lightning::util::ser::Writeable;

use lightning_transaction_sync::EsploraSyncClient;

use lightning_block_sync::init::{synchronize_listeners, validate_best_block_header};
use lightning_block_sync::poll::{ChainPoller, ChainTip, ValidatedBlockHeader};
use lightning_block_sync::SpvClient;

use bdk_esplora::EsploraAsyncExt;

use esplora_client::AsyncClient as EsploraAsyncClient;

use bitcoin::{FeeRate, Network};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The default Esplora server we're using.
pub(crate) const DEFAULT_ESPLORA_SERVER_URL: &str = "https://blockstream.info/api";

// The default Esplora client timeout we're using.
pub(crate) const DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS: u64 = 10;

const CHAIN_POLLING_INTERVAL_SECS: u64 = 1;

pub(crate) enum WalletSyncStatus {
	Completed,
	InProgress { subscribers: tokio::sync::broadcast::Sender<Result<(), Error>> },
}

impl WalletSyncStatus {
	fn register_or_subscribe_pending_sync(
		&mut self,
	) -> Option<tokio::sync::broadcast::Receiver<Result<(), Error>>> {
		match self {
			WalletSyncStatus::Completed => {
				// We're first to register for a sync.
				let (tx, _) = tokio::sync::broadcast::channel(1);
				*self = WalletSyncStatus::InProgress { subscribers: tx };
				None
			},
			WalletSyncStatus::InProgress { subscribers } => {
				// A sync is in-progress, we subscribe.
				let rx = subscribers.subscribe();
				Some(rx)
			},
		}
	}

	fn propagate_result_to_subscribers(&mut self, res: Result<(), Error>) {
		// Send the notification to any other tasks that might be waiting on it by now.
		{
			match self {
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
							},
						}
					}
					*self = WalletSyncStatus::Completed;
				},
			}
		}
	}
}

pub(crate) enum ChainSource {
	Esplora {
		sync_config: EsploraSyncConfig,
		esplora_client: EsploraAsyncClient,
		onchain_wallet: Arc<Wallet>,
		onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
		tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
		lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
		fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>,
		config: Arc<Config>,
		logger: Arc<FilesystemLogger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	},
	BitcoindRpc {
		bitcoind_rpc_client: Arc<BitcoindRpcClient>,
		header_cache: tokio::sync::Mutex<BoundedHeaderCache>,
		latest_chain_tip: RwLock<Option<ValidatedBlockHeader>>,
		onchain_wallet: Arc<Wallet>,
		wallet_polling_status: Mutex<WalletSyncStatus>,
		fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>,
		config: Arc<Config>,
		logger: Arc<FilesystemLogger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	},
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, sync_config: EsploraSyncConfig, onchain_wallet: Arc<Wallet>,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<FilesystemLogger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url);
		client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);
		let esplora_client = client_builder.build_async().unwrap();
		let tx_sync =
			Arc::new(EsploraSyncClient::from_client(esplora_client.clone(), Arc::clone(&logger)));
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self::Esplora {
			sync_config,
			esplora_client,
			onchain_wallet,
			onchain_wallet_sync_status,
			tx_sync,
			lightning_wallet_sync_status,
			fee_estimator,
			tx_broadcaster,
			kv_store,
			config,
			logger,
			node_metrics,
		}
	}

	pub(crate) fn new_bitcoind_rpc(
		host: String, port: u16, rpc_user: String, rpc_password: String,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<FilesystemLogger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let bitcoind_rpc_client =
			Arc::new(BitcoindRpcClient::new(host, port, rpc_user, rpc_password));
		let header_cache = tokio::sync::Mutex::new(BoundedHeaderCache::new());
		let latest_chain_tip = RwLock::new(None);
		let wallet_polling_status = Mutex::new(WalletSyncStatus::Completed);
		Self::BitcoindRpc {
			bitcoind_rpc_client,
			header_cache,
			latest_chain_tip,
			onchain_wallet,
			wallet_polling_status,
			fee_estimator,
			tx_broadcaster,
			kv_store,
			config,
			logger,
			node_metrics,
		}
	}

	pub(crate) async fn continuously_sync_wallets(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) {
		match self {
			Self::Esplora { sync_config, logger, .. } => {
				// Setup syncing intervals
				let onchain_wallet_sync_interval_secs = sync_config
					.onchain_wallet_sync_interval_secs
					.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
				let mut onchain_wallet_sync_interval =
					tokio::time::interval(Duration::from_secs(onchain_wallet_sync_interval_secs));
				onchain_wallet_sync_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				let fee_rate_cache_update_interval_secs = sync_config
					.fee_rate_cache_update_interval_secs
					.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
				let mut fee_rate_update_interval =
					tokio::time::interval(Duration::from_secs(fee_rate_cache_update_interval_secs));
				// When starting up, we just blocked on updating, so skip the first tick.
				fee_rate_update_interval.reset();
				fee_rate_update_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				let lightning_wallet_sync_interval_secs = sync_config
					.lightning_wallet_sync_interval_secs
					.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
				let mut lightning_wallet_sync_interval =
					tokio::time::interval(Duration::from_secs(lightning_wallet_sync_interval_secs));
				lightning_wallet_sync_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				// Start the syncing loop.
				loop {
					tokio::select! {
						_ = stop_sync_receiver.changed() => {
							log_trace!(
								logger,
								"Stopping background syncing on-chain wallet.",
							);
							return;
						}
						_ = onchain_wallet_sync_interval.tick() => {
							let _ = self.sync_onchain_wallet().await;
						}
						_ = fee_rate_update_interval.tick() => {
							let _ = self.update_fee_rate_estimates().await;
						}
						_ = lightning_wallet_sync_interval.tick() => {
							let _ = self.sync_lightning_wallet(
								Arc::clone(&channel_manager),
								Arc::clone(&chain_monitor),
								Arc::clone(&output_sweeper),
							).await;
						}
					}
				}
			},
			Self::BitcoindRpc {
				bitcoind_rpc_client,
				header_cache,
				latest_chain_tip,
				onchain_wallet,
				wallet_polling_status,
				kv_store,
				config,
				logger,
				node_metrics,
				..
			} => {
				// First register for the wallet polling status to make sure `Node::sync_wallets` calls
				// wait on the result before proceeding.
				{
					let mut status_lock = wallet_polling_status.lock().unwrap();
					if status_lock.register_or_subscribe_pending_sync().is_some() {
						debug_assert!(false, "Sync already in progress. This should never happen.");
					}
				}

				let channel_manager_best_block_hash =
					channel_manager.current_best_block().block_hash;
				let sweeper_best_block_hash = output_sweeper.current_best_block().block_hash;
				let onchain_wallet_best_block_hash = onchain_wallet.current_best_block().block_hash;

				let mut chain_listeners = vec![
					(
						onchain_wallet_best_block_hash,
						&**onchain_wallet as &(dyn Listen + Send + Sync),
					),
					(
						channel_manager_best_block_hash,
						&*channel_manager as &(dyn Listen + Send + Sync),
					),
					(sweeper_best_block_hash, &*output_sweeper as &(dyn Listen + Send + Sync)),
				];

				// TODO: Eventually we might want to see if we can synchronize `ChannelMonitor`s
				// before giving them to `ChainMonitor` it the first place. However, this isn't
				// trivial as we load them on initialization (in the `Builder`) and only gain
				// network access during `start`. For now, we just make sure we get the worst known
				// block hash and sychronize them via `ChainMonitor`.
				if let Some(worst_channel_monitor_block_hash) = chain_monitor
					.list_monitors()
					.iter()
					.flat_map(|(txo, _)| chain_monitor.get_monitor(*txo))
					.map(|m| m.current_best_block())
					.min_by_key(|b| b.height)
					.map(|b| b.block_hash)
				{
					chain_listeners.push((
						worst_channel_monitor_block_hash,
						&*chain_monitor as &(dyn Listen + Send + Sync),
					));
				}

				loop {
					let mut locked_header_cache = header_cache.lock().await;
					match synchronize_listeners(
						bitcoind_rpc_client.as_ref(),
						config.network,
						&mut *locked_header_cache,
						chain_listeners.clone(),
					)
					.await
					{
						Ok(chain_tip) => {
							{
								*latest_chain_tip.write().unwrap() = Some(chain_tip);
								let unix_time_secs_opt = SystemTime::now()
									.duration_since(UNIX_EPOCH)
									.ok()
									.map(|d| d.as_secs());
								let mut locked_node_metrics = node_metrics.write().unwrap();
								locked_node_metrics.latest_lightning_wallet_sync_timestamp =
									unix_time_secs_opt;
								locked_node_metrics.latest_onchain_wallet_sync_timestamp =
									unix_time_secs_opt;
								write_node_metrics(
									&*locked_node_metrics,
									Arc::clone(&kv_store),
									Arc::clone(&logger),
								)
								.unwrap_or_else(|e| {
									log_error!(logger, "Failed to persist node metrics: {}", e);
								});
							}
							break;
						},

						Err(e) => {
							log_error!(logger, "Failed to synchronize chain listeners: {:?}", e);
							tokio::time::sleep(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS))
								.await;
						},
					}
				}

				// Now propagate the initial result to unblock waiting subscribers.
				wallet_polling_status.lock().unwrap().propagate_result_to_subscribers(Ok(()));

				let mut chain_polling_interval =
					tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
				chain_polling_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				let mut fee_rate_update_interval =
					tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
				// When starting up, we just blocked on updating, so skip the first tick.
				fee_rate_update_interval.reset();
				fee_rate_update_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				// Start the polling loop.
				loop {
					tokio::select! {
						_ = stop_sync_receiver.changed() => {
							log_trace!(
								logger,
								"Stopping polling for new chain data.",
							);
							return;
						}
						_ = chain_polling_interval.tick() => {
							let _ = self.poll_and_update_listeners(Arc::clone(&channel_manager), Arc::clone(&chain_monitor), Arc::clone(&output_sweeper)).await;
						}
						_ = fee_rate_update_interval.tick() => {
							let _ = self.update_fee_rate_estimates().await;
						}
					}
				}
			},
		}
	}

	// Synchronize the onchain wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_onchain_wallet(&self) -> Result<(), Error> {
		match self {
			Self::Esplora {
				esplora_client,
				onchain_wallet,
				onchain_wallet_sync_status,
				kv_store,
				logger,
				node_metrics,
				..
			} => {
				let receiver_res = {
					let mut status_lock = onchain_wallet_sync_status.lock().unwrap();
					status_lock.register_or_subscribe_pending_sync()
				};
				if let Some(mut sync_receiver) = receiver_res {
					log_info!(logger, "Sync in progress, skipping.");
					return sync_receiver.recv().await.map_err(|e| {
						debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
						log_error!(logger, "Failed to receive wallet sync result: {:?}", e);
						Error::WalletOperationFailed
					})?;
				}

				let res = {
					// If this is our first sync, do a full scan with the configured gap limit.
					// Otherwise just do an incremental sync.
					let incremental_sync =
						node_metrics.read().unwrap().latest_onchain_wallet_sync_timestamp.is_some();

					macro_rules! get_and_apply_wallet_update {
						($sync_future: expr) => {{
							let now = Instant::now();
							match $sync_future.await {
								Ok(res) => match res {
									Ok(update) => match onchain_wallet.apply_update(update) {
										Ok(()) => {
											log_info!(
												logger,
												"{} of on-chain wallet finished in {}ms.",
												if incremental_sync { "Incremental sync" } else { "Sync" },
												now.elapsed().as_millis()
												);
											let unix_time_secs_opt = SystemTime::now()
												.duration_since(UNIX_EPOCH)
												.ok()
												.map(|d| d.as_secs());
											{
												let mut locked_node_metrics = node_metrics.write().unwrap();
												locked_node_metrics.latest_onchain_wallet_sync_timestamp = unix_time_secs_opt;
												write_node_metrics(&*locked_node_metrics, Arc::clone(&kv_store), Arc::clone(&logger))?;
											}
											Ok(())
										},
										Err(e) => Err(e),
									},
									Err(e) => match *e {
										esplora_client::Error::Reqwest(he) => {
											log_error!(
												logger,
												"{} of on-chain wallet failed due to HTTP connection error: {}",
												if incremental_sync { "Incremental sync" } else { "Sync" },
												he
												);
											Err(Error::WalletOperationFailed)
										},
										_ => {
											log_error!(
												logger,
												"{} of on-chain wallet failed due to Esplora error: {}",
												if incremental_sync { "Incremental sync" } else { "Sync" },
												e
											);
											Err(Error::WalletOperationFailed)
										},
									},
								},
								Err(e) => {
									log_error!(
										logger,
										"{} of on-chain wallet timed out: {}",
										if incremental_sync { "Incremental sync" } else { "Sync" },
										e
									);
									Err(Error::WalletOperationTimeout)
								},
							}
						}}
					}

					if incremental_sync {
						let full_scan_request = onchain_wallet.get_full_scan_request();
						let wallet_sync_timeout_fut = tokio::time::timeout(
							Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							esplora_client.full_scan(
								full_scan_request,
								BDK_CLIENT_STOP_GAP,
								BDK_CLIENT_CONCURRENCY,
							),
						);
						get_and_apply_wallet_update!(wallet_sync_timeout_fut)
					} else {
						let sync_request = onchain_wallet.get_incremental_sync_request();
						let wallet_sync_timeout_fut = tokio::time::timeout(
							Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							esplora_client.sync(sync_request, BDK_CLIENT_CONCURRENCY),
						);
						get_and_apply_wallet_update!(wallet_sync_timeout_fut)
					}
				};

				onchain_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

				res
			},
			Self::BitcoindRpc { .. } => {
				// In BitcoindRpc mode we sync lightning and onchain wallet in one go by via
				// `ChainPoller`. So nothing to do here.
				unreachable!("Onchain wallet will be synced via chain polling")
			},
		}
	}

	// Synchronize the Lightning wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_lightning_wallet(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match self {
			Self::Esplora {
				tx_sync,
				lightning_wallet_sync_status,
				kv_store,
				logger,
				node_metrics,
				..
			} => {
				let sync_cman = Arc::clone(&channel_manager);
				let sync_cmon = Arc::clone(&chain_monitor);
				let sync_sweeper = Arc::clone(&output_sweeper);
				let confirmables = vec![
					&*sync_cman as &(dyn Confirm + Sync + Send),
					&*sync_cmon as &(dyn Confirm + Sync + Send),
					&*sync_sweeper as &(dyn Confirm + Sync + Send),
				];

				let receiver_res = {
					let mut status_lock = lightning_wallet_sync_status.lock().unwrap();
					status_lock.register_or_subscribe_pending_sync()
				};
				if let Some(mut sync_receiver) = receiver_res {
					log_info!(logger, "Sync in progress, skipping.");
					return sync_receiver.recv().await.map_err(|e| {
						debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
						log_error!(logger, "Failed to receive wallet sync result: {:?}", e);
						Error::WalletOperationFailed
					})?;
				}
				let res = {
					let timeout_fut = tokio::time::timeout(
						Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS),
						tx_sync.sync(confirmables),
					);
					let now = Instant::now();
					match timeout_fut.await {
						Ok(res) => match res {
							Ok(()) => {
								log_info!(
									logger,
									"Sync of Lightning wallet finished in {}ms.",
									now.elapsed().as_millis()
								);

								let unix_time_secs_opt = SystemTime::now()
									.duration_since(UNIX_EPOCH)
									.ok()
									.map(|d| d.as_secs());
								{
									let mut locked_node_metrics = node_metrics.write().unwrap();
									locked_node_metrics.latest_lightning_wallet_sync_timestamp =
										unix_time_secs_opt;
									write_node_metrics(
										&*locked_node_metrics,
										Arc::clone(&kv_store),
										Arc::clone(&logger),
									)?;
								}

								periodically_archive_fully_resolved_monitors(
									Arc::clone(&channel_manager),
									Arc::clone(&chain_monitor),
									Arc::clone(&kv_store),
									Arc::clone(&logger),
									Arc::clone(&node_metrics),
								)?;
								Ok(())
							},
							Err(e) => {
								log_error!(logger, "Sync of Lightning wallet failed: {}", e);
								Err(e.into())
							},
						},
						Err(e) => {
							log_error!(logger, "Lightning wallet sync timed out: {}", e);
							Err(Error::TxSyncTimeout)
						},
					}
				};

				lightning_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

				res
			},
			Self::BitcoindRpc { .. } => {
				// In BitcoindRpc mode we sync lightning and onchain wallet in one go by via
				// `ChainPoller`. So nothing to do here.
				unreachable!("Lightning wallet will be synced via chain polling")
			},
		}
	}

	pub(crate) async fn poll_and_update_listeners(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match self {
			Self::Esplora { .. } => {
				// In Esplora mode we sync lightning and onchain wallets via
				// `sync_onchain_wallet` and `sync_lightning_wallet`. So nothing to do here.
				unreachable!("Listeners will be synced via transction-based syncing")
			},
			Self::BitcoindRpc {
				bitcoind_rpc_client,
				header_cache,
				latest_chain_tip,
				onchain_wallet,
				wallet_polling_status,
				kv_store,
				config,
				logger,
				node_metrics,
				..
			} => {
				let receiver_res = {
					let mut status_lock = wallet_polling_status.lock().unwrap();
					status_lock.register_or_subscribe_pending_sync()
				};

				if let Some(mut sync_receiver) = receiver_res {
					log_info!(logger, "Sync in progress, skipping.");
					return sync_receiver.recv().await.map_err(|e| {
						debug_assert!(false, "Failed to receive wallet polling result: {:?}", e);
						log_error!(logger, "Failed to receive wallet polling result: {:?}", e);
						Error::WalletOperationFailed
					})?;
				}

				let latest_chain_tip_opt = latest_chain_tip.read().unwrap().clone();
				let chain_tip = if let Some(tip) = latest_chain_tip_opt {
					tip
				} else {
					match validate_best_block_header(bitcoind_rpc_client.as_ref()).await {
						Ok(tip) => {
							*latest_chain_tip.write().unwrap() = Some(tip);
							tip
						},
						Err(e) => {
							log_error!(logger, "Failed to poll for chain data: {:?}", e);
							let res = Err(Error::TxSyncFailed);
							wallet_polling_status
								.lock()
								.unwrap()
								.propagate_result_to_subscribers(res);
							return res;
						},
					}
				};

				let mut locked_header_cache = header_cache.lock().await;
				let chain_poller =
					ChainPoller::new(Arc::clone(&bitcoind_rpc_client), config.network);
				let chain_listener = ChainListener {
					onchain_wallet: Arc::clone(&onchain_wallet),
					channel_manager: Arc::clone(&channel_manager),
					chain_monitor,
					output_sweeper,
				};
				let mut spv_client = SpvClient::new(
					chain_tip,
					chain_poller,
					&mut *locked_header_cache,
					&chain_listener,
				);
				let mut chain_polling_interval =
					tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
				chain_polling_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				match spv_client.poll_best_tip().await {
					Ok((ChainTip::Better(tip), true)) => {
						*latest_chain_tip.write().unwrap() = Some(tip);
					},
					Ok(_) => {},
					Err(e) => {
						log_error!(logger, "Failed to poll for chain data: {:?}", e);
						let res = Err(Error::TxSyncFailed);
						wallet_polling_status.lock().unwrap().propagate_result_to_subscribers(res);
						return res;
					},
				}

				let res = {
					let unix_time_secs_opt =
						SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
					let mut locked_node_metrics = node_metrics.write().unwrap();
					locked_node_metrics.latest_lightning_wallet_sync_timestamp = unix_time_secs_opt;
					locked_node_metrics.latest_onchain_wallet_sync_timestamp = unix_time_secs_opt;
					write_node_metrics(
						&*locked_node_metrics,
						Arc::clone(&kv_store),
						Arc::clone(&logger),
					)
					.map_err(|e| {
						log_error!(logger, "Failed to persist node metrics: {}", e);
						Error::PersistenceFailed
					})
				};

				wallet_polling_status.lock().unwrap().propagate_result_to_subscribers(res);

				res
			},
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		match self {
			Self::Esplora {
				esplora_client,
				fee_estimator,
				config,
				kv_store,
				logger,
				node_metrics,
				..
			} => {
				let now = Instant::now();
				let estimates = tokio::time::timeout(
					Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
					esplora_client.get_fee_estimates(),
				)
				.await
				.map_err(|e| {
					log_error!(logger, "Updating fee rate estimates timed out: {}", e);
					Error::FeerateEstimationUpdateTimeout
				})?
				.map_err(|e| {
					log_error!(logger, "Failed to retrieve fee rate estimates: {}", e);
					Error::FeerateEstimationUpdateFailed
				})?;

				if estimates.is_empty() && config.network == Network::Bitcoin {
					// Ensure we fail if we didn't receive any estimates.
					log_error!(
						logger,
						"Failed to retrieve fee rate estimates: empty fee estimates are dissallowed on Mainnet.",
					);
					return Err(Error::FeerateEstimationUpdateFailed);
				}

				let confirmation_targets = get_all_conf_targets();

				let mut new_fee_rate_cache = HashMap::with_capacity(10);
				for target in confirmation_targets {
					let num_blocks = get_num_block_defaults_for_target(target);

					let converted_estimate_sat_vb =
						esplora_client::convert_fee_rate(num_blocks, estimates.clone()).map_err(
							|e| {
								log_error!(
									logger,
									"Failed to convert fee rate estimates for {:?}: {}",
									target,
									e
								);
								Error::FeerateEstimationUpdateFailed
							},
						)?;

					let fee_rate =
						FeeRate::from_sat_per_kwu((converted_estimate_sat_vb * 250.0) as u64);

					// LDK 0.0.118 introduced changes to the `ConfirmationTarget` semantics that
					// require some post-estimation adjustments to the fee rates, which we do here.
					let adjusted_fee_rate = apply_post_estimation_adjustments(target, fee_rate);

					new_fee_rate_cache.insert(target, adjusted_fee_rate);

					log_trace!(
						logger,
						"Fee rate estimation updated for {:?}: {} sats/kwu",
						target,
						adjusted_fee_rate.to_sat_per_kwu(),
					);
				}

				fee_estimator.set_fee_rate_cache(new_fee_rate_cache);

				log_info!(
					logger,
					"Fee rate cache update finished in {}ms.",
					now.elapsed().as_millis()
				);
				let unix_time_secs_opt =
					SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
				{
					let mut locked_node_metrics = node_metrics.write().unwrap();
					locked_node_metrics.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt;
					write_node_metrics(
						&*locked_node_metrics,
						Arc::clone(&kv_store),
						Arc::clone(&logger),
					)?;
				}

				Ok(())
			},
			Self::BitcoindRpc {
				bitcoind_rpc_client,
				fee_estimator,
				kv_store,
				logger,
				node_metrics,
				..
			} => {
				macro_rules! get_fee_rate_update {
					($estimation_fut: expr) => {{
						tokio::time::timeout(
							Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
							$estimation_fut,
						)
						.await
						.map_err(|e| {
							log_error!(logger, "Updating fee rate estimates timed out: {}", e);
							Error::FeerateEstimationUpdateTimeout
						})?
						.map_err(|e| {
							log_error!(logger, "Failed to retrieve fee rate estimates: {}", e);
							Error::FeerateEstimationUpdateFailed
						})?
					}};
				}
				let confirmation_targets = get_all_conf_targets();

				let mut new_fee_rate_cache = HashMap::with_capacity(10);
				let now = Instant::now();
				for target in confirmation_targets {
					let fee_rate = match target {
						ConfirmationTarget::Lightning(
							LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
						) => {
							let estimation_fut = bitcoind_rpc_client.get_mempool_minimum_fee_rate();
							get_fee_rate_update!(estimation_fut)
						},
						ConfirmationTarget::Lightning(
							LdkConfirmationTarget::MaximumFeeEstimate,
						) => {
							let num_blocks = get_num_block_defaults_for_target(target);
							let estimation_mode = FeeRateEstimationMode::Conservative;
							let estimation_fut = bitcoind_rpc_client
								.get_fee_estimate_for_target(num_blocks, estimation_mode);
							get_fee_rate_update!(estimation_fut)
						},
						ConfirmationTarget::Lightning(
							LdkConfirmationTarget::UrgentOnChainSweep,
						) => {
							let num_blocks = get_num_block_defaults_for_target(target);
							let estimation_mode = FeeRateEstimationMode::Conservative;
							let estimation_fut = bitcoind_rpc_client
								.get_fee_estimate_for_target(num_blocks, estimation_mode);
							get_fee_rate_update!(estimation_fut)
						},
						_ => {
							// Otherwise, we default to economical block-target estimate.
							let num_blocks = get_num_block_defaults_for_target(target);
							let estimation_mode = FeeRateEstimationMode::Economical;
							let estimation_fut = bitcoind_rpc_client
								.get_fee_estimate_for_target(num_blocks, estimation_mode);
							get_fee_rate_update!(estimation_fut)
						},
					};

					// LDK 0.0.118 introduced changes to the `ConfirmationTarget` semantics that
					// require some post-estimation adjustments to the fee rates, which we do here.
					let adjusted_fee_rate = apply_post_estimation_adjustments(target, fee_rate);

					new_fee_rate_cache.insert(target, adjusted_fee_rate);

					log_trace!(
						logger,
						"Fee rate estimation updated for {:?}: {} sats/kwu",
						target,
						adjusted_fee_rate.to_sat_per_kwu(),
					);
				}

				if fee_estimator.set_fee_rate_cache(new_fee_rate_cache) {
					// We only log if the values changed, as it might be very spammy otherwise.
					log_info!(
						logger,
						"Fee rate cache update finished in {}ms.",
						now.elapsed().as_millis()
					);
				}

				let unix_time_secs_opt =
					SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
				{
					let mut locked_node_metrics = node_metrics.write().unwrap();
					locked_node_metrics.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt;
					write_node_metrics(
						&*locked_node_metrics,
						Arc::clone(&kv_store),
						Arc::clone(&logger),
					)?;
				}

				Ok(())
			},
		}
	}

	pub(crate) async fn process_broadcast_queue(&self) {
		match self {
			Self::Esplora { esplora_client, tx_broadcaster, logger, .. } => {
				let mut receiver = tx_broadcaster.get_broadcast_queue().await;
				while let Some(next_package) = receiver.recv().await {
					for tx in &next_package {
						let txid = tx.compute_txid();
						let timeout_fut = tokio::time::timeout(
							Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
							esplora_client.broadcast(tx),
						);
						match timeout_fut.await {
							Ok(res) => match res {
								Ok(()) => {
									log_trace!(
										logger,
										"Successfully broadcast transaction {}",
										txid
									);
								},
								Err(e) => match e {
									esplora_client::Error::Reqwest(err) => {
										if err.status() == reqwest::StatusCode::from_u16(400).ok() {
											// Ignore 400, as this just means bitcoind already knows the
											// transaction.
											// FIXME: We can further differentiate here based on the error
											// message which will be available with rust-esplora-client 0.7 and
											// later.
										} else {
											log_error!(
												logger,
												"Failed to broadcast due to HTTP connection error: {}",
												err
											);
										}
										log_trace!(
											logger,
											"Failed broadcast transaction bytes: {}",
											log_bytes!(tx.encode())
										);
									},
									_ => {
										log_error!(
											logger,
											"Failed to broadcast transaction {}: {}",
											txid,
											e
										);
										log_trace!(
											logger,
											"Failed broadcast transaction bytes: {}",
											log_bytes!(tx.encode())
										);
									},
								},
							},
							Err(e) => {
								log_error!(
									logger,
									"Failed to broadcast transaction due to timeout {}: {}",
									txid,
									e
								);
								log_trace!(
									logger,
									"Failed broadcast transaction bytes: {}",
									log_bytes!(tx.encode())
								);
							},
						}
					}
				}
			},
			Self::BitcoindRpc { bitcoind_rpc_client, tx_broadcaster, logger, .. } => {
				// While it's a bit unclear when we'd be able to lean on Bitcoin Core >v28
				// features, we should eventually switch to use `submitpackage` via the
				// `rust-bitcoind-json-rpc` crate rather than just broadcasting individual
				// transactions.
				let mut receiver = tx_broadcaster.get_broadcast_queue().await;
				while let Some(next_package) = receiver.recv().await {
					for tx in &next_package {
						let txid = tx.compute_txid();
						let timeout_fut = tokio::time::timeout(
							Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
							bitcoind_rpc_client.broadcast_transaction(tx),
						);
						match timeout_fut.await {
							Ok(res) => match res {
								Ok(id) => {
									debug_assert_eq!(id, txid);
									log_trace!(
										logger,
										"Successfully broadcast transaction {}",
										txid
									);
								},
								Err(e) => {
									log_error!(
										logger,
										"Failed to broadcast transaction {}: {}",
										txid,
										e
									);
									log_trace!(
										logger,
										"Failed broadcast transaction bytes: {}",
										log_bytes!(tx.encode())
									);
								},
							},
							Err(e) => {
								log_error!(
									logger,
									"Failed to broadcast transaction due to timeout {}: {}",
									txid,
									e
								);
								log_trace!(
									logger,
									"Failed broadcast transaction bytes: {}",
									log_bytes!(tx.encode())
								);
							},
						}
					}
				}
			},
		}
	}
}

impl Filter for ChainSource {
	fn register_tx(&self, txid: &bitcoin::Txid, script_pubkey: &bitcoin::Script) {
		match self {
			Self::Esplora { tx_sync, .. } => tx_sync.register_tx(txid, script_pubkey),
			Self::BitcoindRpc { .. } => (),
		}
	}
	fn register_output(&self, output: lightning::chain::WatchedOutput) {
		match self {
			Self::Esplora { tx_sync, .. } => tx_sync.register_output(output),
			Self::BitcoindRpc { .. } => (),
		}
	}
}

fn periodically_archive_fully_resolved_monitors(
	channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
	kv_store: Arc<DynStore>, logger: Arc<FilesystemLogger>, node_metrics: Arc<RwLock<NodeMetrics>>,
) -> Result<(), Error> {
	let mut locked_node_metrics = node_metrics.write().unwrap();
	let cur_height = channel_manager.current_best_block().height;
	let should_archive = locked_node_metrics
		.latest_channel_monitor_archival_height
		.as_ref()
		.map_or(true, |h| cur_height >= h + RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL);

	if should_archive {
		chain_monitor.archive_fully_resolved_channel_monitors();
		locked_node_metrics.latest_channel_monitor_archival_height = Some(cur_height);
		write_node_metrics(&*locked_node_metrics, kv_store, logger)?;
	}
	Ok(())
}
