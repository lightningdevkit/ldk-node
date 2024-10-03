// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::{
	Config, BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS,
	FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, LDK_WALLET_SYNC_TIMEOUT_SECS,
	RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, TX_BROADCAST_TIMEOUT_SECS,
	WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::logger::{log_bytes, log_error, log_info, log_trace, FilesystemLogger, Logger};
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, Sweeper, Wallet};
use crate::Error;

use lightning::chain::{Confirm, Filter};
use lightning::util::ser::Writeable;

use lightning_transaction_sync::EsploraSyncClient;

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
		esplora_client: EsploraAsyncClient,
		onchain_wallet: Arc<Wallet>,
		onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
		tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
		lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
		fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>,
		config: Arc<Config>,
		logger: Arc<FilesystemLogger>,
		latest_wallet_sync_timestamp: Arc<RwLock<Option<u64>>>,
		latest_onchain_wallet_sync_timestamp: Arc<RwLock<Option<u64>>>,
		latest_fee_rate_cache_update_timestamp: Arc<RwLock<Option<u64>>>,
		latest_channel_monitor_archival_height: Arc<RwLock<Option<u32>>>,
	},
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, config: Arc<Config>, logger: Arc<FilesystemLogger>,
		latest_wallet_sync_timestamp: Arc<RwLock<Option<u64>>>,
		latest_onchain_wallet_sync_timestamp: Arc<RwLock<Option<u64>>>,
		latest_fee_rate_cache_update_timestamp: Arc<RwLock<Option<u64>>>,
		latest_channel_monitor_archival_height: Arc<RwLock<Option<u32>>>,
	) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url);
		client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);
		let esplora_client = client_builder.build_async().unwrap();
		let tx_sync =
			Arc::new(EsploraSyncClient::from_client(esplora_client.clone(), Arc::clone(&logger)));
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self::Esplora {
			esplora_client,
			onchain_wallet,
			onchain_wallet_sync_status,
			tx_sync,
			lightning_wallet_sync_status,
			fee_estimator,
			tx_broadcaster,
			config,
			logger,
			latest_wallet_sync_timestamp,
			latest_onchain_wallet_sync_timestamp,
			latest_fee_rate_cache_update_timestamp,
			latest_channel_monitor_archival_height,
		}
	}

	pub(crate) async fn continuously_sync_wallets(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) {
		match self {
			Self::Esplora { config, logger, .. } => {
				// Setup syncing intervals
				let onchain_wallet_sync_interval_secs =
					config.onchain_wallet_sync_interval_secs.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
				let mut onchain_wallet_sync_interval =
					tokio::time::interval(Duration::from_secs(onchain_wallet_sync_interval_secs));
				onchain_wallet_sync_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				let fee_rate_cache_update_interval_secs = config
					.fee_rate_cache_update_interval_secs
					.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
				let mut fee_rate_update_interval =
					tokio::time::interval(Duration::from_secs(fee_rate_cache_update_interval_secs));
				// When starting up, we just blocked on updating, so skip the first tick.
				fee_rate_update_interval.reset();
				fee_rate_update_interval
					.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

				let wallet_sync_interval_secs =
					config.wallet_sync_interval_secs.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
				let mut wallet_sync_interval =
					tokio::time::interval(Duration::from_secs(wallet_sync_interval_secs));
				wallet_sync_interval
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
						_ = wallet_sync_interval.tick() => {
							let _ = self.sync_lightning_wallet(
								Arc::clone(&channel_manager),
								Arc::clone(&chain_monitor),
								Arc::clone(&output_sweeper),
							).await;
						}
					}
				}
			},
		}
	}

	pub(crate) async fn sync_onchain_wallet(&self) -> Result<(), Error> {
		match self {
			Self::Esplora {
				esplora_client,
				onchain_wallet,
				onchain_wallet_sync_status,
				logger,
				latest_onchain_wallet_sync_timestamp,
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

				let res =
					{
						let full_scan_request = onchain_wallet.get_full_scan_request();

						let wallet_sync_timeout_fut = tokio::time::timeout(
							Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							esplora_client.full_scan(
								full_scan_request,
								BDK_CLIENT_STOP_GAP,
								BDK_CLIENT_CONCURRENCY,
							),
						);

						let now = Instant::now();
						match wallet_sync_timeout_fut.await {
							Ok(res) => match res {
								Ok(update) => match onchain_wallet.apply_update(update) {
									Ok(()) => {
										log_info!(
											logger,
											"Sync of on-chain wallet finished in {}ms.",
											now.elapsed().as_millis()
										);
										let unix_time_secs_opt = SystemTime::now()
											.duration_since(UNIX_EPOCH)
											.ok()
											.map(|d| d.as_secs());
										*latest_onchain_wallet_sync_timestamp.write().unwrap() =
											unix_time_secs_opt;
										Ok(())
									},
									Err(e) => Err(e),
								},
								Err(e) => match *e {
									esplora_client::Error::Reqwest(he) => {
										log_error!(
											logger,
											"Sync failed due to HTTP connection error: {}",
											he
										);
										Err(Error::WalletOperationFailed)
									},
									_ => {
										log_error!(logger, "Sync of on-chain wallet failed due to Esplora error: {}", e);
										Err(Error::WalletOperationFailed)
									},
								},
							},
							Err(e) => {
								log_error!(logger, "On-chain wallet sync timed out: {}", e);
								Err(Error::WalletOperationTimeout)
							},
						}
					};

				onchain_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

				res
			},
		}
	}

	pub(crate) async fn sync_lightning_wallet(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match self {
			Self::Esplora {
				tx_sync,
				lightning_wallet_sync_status,
				logger,
				latest_wallet_sync_timestamp,
				latest_channel_monitor_archival_height,
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
								*latest_wallet_sync_timestamp.write().unwrap() = unix_time_secs_opt;

								periodically_archive_fully_resolved_monitors(
									Arc::clone(&channel_manager),
									Arc::clone(&chain_monitor),
									Arc::clone(&latest_channel_monitor_archival_height),
								);
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
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		match self {
			Self::Esplora {
				esplora_client,
				fee_estimator,
				config,
				logger,
				latest_fee_rate_cache_update_timestamp,
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
				*latest_fee_rate_cache_update_timestamp.write().unwrap() = unix_time_secs_opt;

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
		}
	}
}

impl Filter for ChainSource {
	fn register_tx(&self, txid: &bitcoin::Txid, script_pubkey: &bitcoin::Script) {
		match self {
			Self::Esplora { tx_sync, .. } => tx_sync.register_tx(txid, script_pubkey),
		}
	}
	fn register_output(&self, output: lightning::chain::WatchedOutput) {
		match self {
			Self::Esplora { tx_sync, .. } => tx_sync.register_output(output),
		}
	}
}

fn periodically_archive_fully_resolved_monitors(
	channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
	latest_channel_monitor_archival_height: Arc<RwLock<Option<u32>>>,
) {
	let mut latest_archival_height_lock = latest_channel_monitor_archival_height.write().unwrap();
	let cur_height = channel_manager.current_best_block().height;
	let should_archive = latest_archival_height_lock
		.as_ref()
		.map_or(true, |h| cur_height >= h + RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL);

	if should_archive {
		chain_monitor.archive_fully_resolved_channel_monitors();
		*latest_archival_height_lock = Some(cur_height);
	}
}
