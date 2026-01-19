// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bdk_esplora::EsploraAsyncExt;
use bitcoin::{FeeRate, Network, Script, Transaction, Txid};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::util::ser::Writeable;
use lightning_transaction_sync::EsploraSyncClient;

use super::WalletSyncStatus;
use crate::config::{
	Config, EsploraSyncConfig, BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP,
	BDK_WALLET_SYNC_TIMEOUT_SECS, DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS,
	FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, LDK_WALLET_SYNC_TIMEOUT_SECS, TX_BROADCAST_TIMEOUT_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_debug, log_error, log_trace, LdkLogger, Logger};
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

pub(super) struct EsploraChainSource {
	pub(super) sync_config: EsploraSyncConfig,
	esplora_client: EsploraAsyncClient,
	onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
	tx_sync: Arc<EsploraSyncClient<Arc<Logger>>>,
	lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	kv_store: Arc<DynStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl EsploraChainSource {
	pub(crate) fn new(
		server_url: String, headers: HashMap<String, String>, sync_config: EsploraSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url);
		client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);

		for (header_name, header_value) in &headers {
			client_builder = client_builder.header(header_name, header_value);
		}

		let esplora_client = client_builder.build_async().unwrap();
		let tx_sync =
			Arc::new(EsploraSyncClient::from_client(esplora_client.clone(), Arc::clone(&logger)));

		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			sync_config,
			esplora_client,
			onchain_wallet_sync_status,
			tx_sync,
			lightning_wallet_sync_status,
			fee_estimator,
			kv_store,
			config,
			logger,
			node_metrics,
		}
	}

	pub(super) async fn sync_onchain_wallet(
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
		// If this is our first sync, do a full scan with the configured gap limit.
		// Otherwise just do an incremental sync.
		let incremental_sync =
			self.node_metrics.read().unwrap().latest_onchain_wallet_sync_timestamp.is_some();

		macro_rules! get_and_apply_wallet_update {
			($sync_future: expr) => {{
				let now = Instant::now();
				match $sync_future.await {
					Ok(res) => match res {
						Ok(update) => match onchain_wallet.apply_update(update) {
							Ok(()) => {
								log_debug!(
									self.logger,
									"{} of on-chain wallet finished in {}ms.",
									if incremental_sync { "Incremental sync" } else { "Sync" },
									now.elapsed().as_millis()
								);
								let unix_time_secs_opt = SystemTime::now()
									.duration_since(UNIX_EPOCH)
									.ok()
									.map(|d| d.as_secs());
									{
										let mut locked_node_metrics = self.node_metrics.write().unwrap();
										locked_node_metrics.latest_onchain_wallet_sync_timestamp = unix_time_secs_opt;
										write_node_metrics(
											&*locked_node_metrics,
											&*self.kv_store,
											&*self.logger
										)?;
									}
									Ok(())
							},
							Err(e) => Err(e),
						},
						Err(e) => match *e {
							esplora_client::Error::Reqwest(he) => {
								if let Some(status_code) = he.status() {
									log_error!(
										self.logger,
										"{} of on-chain wallet failed due to HTTP {} error: {}",
										if incremental_sync { "Incremental sync" } else { "Sync" },
										status_code,
										he,
									);
								} else {
									log_error!(
										self.logger,
										"{} of on-chain wallet failed due to HTTP error: {}",
										if incremental_sync { "Incremental sync" } else { "Sync" },
										he,
									);
								}
								Err(Error::WalletOperationFailed)
							},
							_ => {
								log_error!(
									self.logger,
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
							self.logger,
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
			let sync_request = onchain_wallet.get_incremental_sync_request();
			let wallet_sync_timeout_fut = tokio::time::timeout(
				Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
				self.esplora_client.sync(sync_request, BDK_CLIENT_CONCURRENCY),
			);
			get_and_apply_wallet_update!(wallet_sync_timeout_fut)
		} else {
			let full_scan_request = onchain_wallet.get_full_scan_request();
			let wallet_sync_timeout_fut = tokio::time::timeout(
				Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
				self.esplora_client.full_scan(
					full_scan_request,
					BDK_CLIENT_STOP_GAP,
					BDK_CLIENT_CONCURRENCY,
				),
			);
			get_and_apply_wallet_update!(wallet_sync_timeout_fut)
		}
	}

	pub(super) async fn sync_lightning_wallet(
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
				Error::WalletOperationFailed
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
			&*sync_cman as &(dyn Confirm + Sync + Send),
			&*sync_cmon as &(dyn Confirm + Sync + Send),
			&*sync_sweeper as &(dyn Confirm + Sync + Send),
		];

		let timeout_fut = tokio::time::timeout(
			Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS),
			self.tx_sync.sync(confirmables),
		);
		let now = Instant::now();
		match timeout_fut.await {
			Ok(res) => match res {
				Ok(()) => {
					log_debug!(
						self.logger,
						"Sync of Lightning wallet finished in {}ms.",
						now.elapsed().as_millis()
					);

					let unix_time_secs_opt =
						SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
					{
						let mut locked_node_metrics = self.node_metrics.write().unwrap();
						locked_node_metrics.latest_lightning_wallet_sync_timestamp =
							unix_time_secs_opt;
						write_node_metrics(&*locked_node_metrics, &*self.kv_store, &*self.logger)?;
					}
					Ok(())
				},
				Err(e) => {
					log_error!(self.logger, "Sync of Lightning wallet failed: {}", e);
					Err(e.into())
				},
			},
			Err(e) => {
				log_error!(self.logger, "Lightning wallet sync timed out: {}", e);
				Err(Error::TxSyncTimeout)
			},
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		let now = Instant::now();
		let estimates = tokio::time::timeout(
			Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
			self.esplora_client.get_fee_estimates(),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
			Error::FeerateEstimationUpdateTimeout
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
			Error::FeerateEstimationUpdateFailed
		})?;

		if estimates.is_empty() && self.config.network == Network::Bitcoin {
			// Ensure we fail if we didn't receive any estimates.
			log_error!(
						self.logger,
						"Failed to retrieve fee rate estimates: empty fee estimates are dissallowed on Mainnet.",
					);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let confirmation_targets = get_all_conf_targets();

		let mut new_fee_rate_cache = HashMap::with_capacity(10);
		for target in confirmation_targets {
			let num_blocks = get_num_block_defaults_for_target(target);

			// Convert the retrieved fee rate and fall back to 1 sat/vb if we fail or it
			// yields less than that. This is mostly necessary to continue on
			// `signet`/`regtest` where we might not get estimates (or bogus values).
			let converted_estimate_sat_vb =
				esplora_client::convert_fee_rate(num_blocks, estimates.clone())
					.map_or(1.0, |converted| converted.max(1.0));

			let fee_rate = FeeRate::from_sat_per_kwu((converted_estimate_sat_vb * 250.0) as u64);

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
		for tx in &package {
			let txid = tx.compute_txid();
			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
				self.esplora_client.broadcast(tx),
			);
			match timeout_fut.await {
				Ok(res) => match res {
					Ok(()) => {
						log_trace!(self.logger, "Successfully broadcast transaction {}", txid);
					},
					Err(e) => match e {
						esplora_client::Error::HttpResponse { status, message } => {
							if status == 400 {
								// Log 400 at lesser level, as this often just means bitcoind already knows the
								// transaction.
								// FIXME: We can further differentiate here based on the error
								// message which will be available with rust-esplora-client 0.7 and
								// later.
								log_trace!(
									self.logger,
									"Failed to broadcast due to HTTP connection error: {}",
									message
								);
							} else {
								log_error!(
									self.logger,
									"Failed to broadcast due to HTTP connection error: {} - {}",
									status,
									message
								);
							}
							log_trace!(
								self.logger,
								"Failed broadcast transaction bytes: {}",
								log_bytes!(tx.encode())
							);
						},
						_ => {
							log_error!(
								self.logger,
								"Failed to broadcast transaction {}: {}",
								txid,
								e
							);
							log_trace!(
								self.logger,
								"Failed broadcast transaction bytes: {}",
								log_bytes!(tx.encode())
							);
						},
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
						log_bytes!(tx.encode())
					);
				},
			}
		}
	}
}

impl Filter for EsploraChainSource {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.tx_sync.register_tx(txid, script_pubkey);
	}
	fn register_output(&self, output: WatchedOutput) {
		self.tx_sync.register_output(output);
	}
}
