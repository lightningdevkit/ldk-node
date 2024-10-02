// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::{
	Config, BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS,
	FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, LDK_WALLET_SYNC_TIMEOUT_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::logger::{log_error, log_info, log_trace, FilesystemLogger, Logger};
use crate::types::Wallet;
use crate::Error;

use lightning::chain::{Confirm, Filter};

use lightning_transaction_sync::EsploraSyncClient;

use bdk_esplora::EsploraAsyncExt;

use esplora_client::AsyncClient as EsploraAsyncClient;

use bitcoin::{FeeRate, Network};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
		config: Arc<Config>,
		logger: Arc<FilesystemLogger>,
	},
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		config: Arc<Config>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url.clone());
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
			config,
			logger,
		}
	}

	pub(crate) async fn sync_onchain_wallet(&self) -> Result<(), Error> {
		match self {
			Self::Esplora {
				esplora_client,
				onchain_wallet,
				onchain_wallet_sync_status,
				logger,
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
					let full_scan_request = onchain_wallet.get_full_scan_request();

					let wallet_sync_timeout_fut = tokio::time::timeout(
						Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
						esplora_client.full_scan(
							full_scan_request,
							BDK_CLIENT_STOP_GAP,
							BDK_CLIENT_CONCURRENCY,
						),
					);

					match wallet_sync_timeout_fut.await {
						Ok(res) => match res {
							Ok(update) => onchain_wallet.apply_update(update),
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
									log_error!(logger, "Sync failed due to Esplora error: {}", e);
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
		&self, confirmables: Vec<&(dyn Confirm + Send + Sync)>,
	) -> Result<(), Error> {
		match self {
			Self::Esplora { tx_sync, lightning_wallet_sync_status, logger, .. } => {
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
					match timeout_fut.await {
						Ok(res) => res.map_err(|_| Error::TxSyncFailed),
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
			Self::Esplora { esplora_client, fee_estimator, config, logger, .. } => {
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
				Ok(())
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
