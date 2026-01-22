// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use bitcoin::{BlockHash, FeeRate, Network, OutPoint, Transaction, Txid};
use lightning::chain::chaininterface::ConfirmationTarget as LdkConfirmationTarget;
use lightning::chain::{BestBlock, Listen};
use lightning::util::ser::Writeable;
use lightning_block_sync::gossip::UtxoSource;
use lightning_block_sync::http::{HttpEndpoint, JsonResponse};
use lightning_block_sync::init::{synchronize_listeners, validate_best_block_header};
use lightning_block_sync::poll::{ChainPoller, ChainTip, ValidatedBlockHeader};
use lightning_block_sync::rest::RestClient;
use lightning_block_sync::rpc::{RpcClient, RpcError};
use lightning_block_sync::{
	BlockData, BlockHeaderData, BlockSource, BlockSourceError, BlockSourceErrorKind, Cache,
	SpvClient,
};
use serde::Serialize;

use super::WalletSyncStatus;
use crate::config::{
	BitcoindRestClientConfig, Config, FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, TX_BROADCAST_TIMEOUT_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	ConfirmationTarget, OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

const CHAIN_POLLING_INTERVAL_SECS: u64 = 2;
const CHAIN_POLLING_TIMEOUT_SECS: u64 = 10;

pub(super) struct BitcoindChainSource {
	api_client: Arc<BitcoindClient>,
	header_cache: tokio::sync::Mutex<BoundedHeaderCache>,
	latest_chain_tip: RwLock<Option<ValidatedBlockHeader>>,
	wallet_polling_status: Mutex<WalletSyncStatus>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	kv_store: Arc<DynStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl BitcoindChainSource {
	pub(crate) fn new_rpc(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let api_client = Arc::new(BitcoindClient::new_rpc(
			rpc_host.clone(),
			rpc_port.clone(),
			rpc_user.clone(),
			rpc_password.clone(),
		));

		let header_cache = tokio::sync::Mutex::new(BoundedHeaderCache::new());
		let latest_chain_tip = RwLock::new(None);
		let wallet_polling_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			api_client,
			header_cache,
			latest_chain_tip,
			wallet_polling_status,
			fee_estimator,
			kv_store,
			config,
			logger: Arc::clone(&logger),
			node_metrics,
		}
	}

	pub(crate) fn new_rest(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		rest_client_config: BitcoindRestClientConfig, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let api_client = Arc::new(BitcoindClient::new_rest(
			rest_client_config.rest_host,
			rest_client_config.rest_port,
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
		));

		let header_cache = tokio::sync::Mutex::new(BoundedHeaderCache::new());
		let latest_chain_tip = RwLock::new(None);
		let wallet_polling_status = Mutex::new(WalletSyncStatus::Completed);

		Self {
			api_client,
			header_cache,
			latest_chain_tip,
			wallet_polling_status,
			fee_estimator,
			kv_store,
			config,
			logger: Arc::clone(&logger),
			node_metrics,
		}
	}

	pub(super) fn as_utxo_source(&self) -> UtxoSourceClient {
		self.api_client.utxo_source()
	}

	pub(super) async fn continuously_sync_wallets(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) {
		// First register for the wallet polling status to make sure `Node::sync_wallets` calls
		// wait on the result before proceeding.
		{
			let mut status_lock = self.wallet_polling_status.lock().unwrap();
			if status_lock.register_or_subscribe_pending_sync().is_some() {
				debug_assert!(false, "Sync already in progress. This should never happen.");
			}
		}

		log_info!(
			self.logger,
			"Starting initial synchronization of chain listeners. This might take a while..",
		);

		let mut backoff = CHAIN_POLLING_INTERVAL_SECS;
		const MAX_BACKOFF_SECS: u64 = 300;

		loop {
			// if the stop_sync_sender has been dropped, we should just exit
			if stop_sync_receiver.has_changed().unwrap_or(true) {
				log_trace!(self.logger, "Stopping initial chain sync.");
				return;
			}

			let channel_manager_best_block_hash = channel_manager.current_best_block().block_hash;
			let sweeper_best_block_hash = output_sweeper.current_best_block().block_hash;
			let onchain_wallet_best_block_hash = onchain_wallet.current_best_block().block_hash;

			let mut chain_listeners = vec![
				(onchain_wallet_best_block_hash, &*onchain_wallet as &(dyn Listen + Send + Sync)),
				(channel_manager_best_block_hash, &*channel_manager as &(dyn Listen + Send + Sync)),
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
				.flat_map(|channel_id| chain_monitor.get_monitor(*channel_id))
				.map(|m| m.current_best_block())
				.min_by_key(|b| b.height)
				.map(|b| b.block_hash)
			{
				chain_listeners.push((
					worst_channel_monitor_block_hash,
					&*chain_monitor as &(dyn Listen + Send + Sync),
				));
			}

			let mut locked_header_cache = self.header_cache.lock().await;
			let now = SystemTime::now();
			match synchronize_listeners(
				self.api_client.as_ref(),
				self.config.network,
				&mut *locked_header_cache,
				chain_listeners.clone(),
			)
			.await
			{
				Ok(chain_tip) => {
					{
						log_info!(
							self.logger,
							"Finished synchronizing listeners in {}ms",
							now.elapsed().unwrap().as_millis()
						);
						*self.latest_chain_tip.write().unwrap() = Some(chain_tip);
						let unix_time_secs_opt =
							SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
						let mut locked_node_metrics = self.node_metrics.write().unwrap();
						locked_node_metrics.latest_lightning_wallet_sync_timestamp =
							unix_time_secs_opt;
						locked_node_metrics.latest_onchain_wallet_sync_timestamp =
							unix_time_secs_opt;
						write_node_metrics(&*locked_node_metrics, &*self.kv_store, &*self.logger)
							.unwrap_or_else(|e| {
								log_error!(self.logger, "Failed to persist node metrics: {}", e);
							});
					}
					break;
				},

				Err(e) => {
					log_error!(self.logger, "Failed to synchronize chain listeners: {:?}", e);
					if e.kind() == BlockSourceErrorKind::Transient {
						log_info!(
									self.logger,
									"Transient error syncing chain listeners: {:?}. Retrying in {} seconds.",
									e,
									backoff
								);
						// Sleep with stop signal check to allow immediate shutdown
						tokio::select! {
							biased;
							_ = stop_sync_receiver.changed() => {
								log_trace!(
									self.logger,
									"Stopping initial chain sync.",
								);
								return;
							}
							_ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
						}
						backoff = std::cmp::min(backoff * 2, MAX_BACKOFF_SECS);
					} else {
						log_error!(
									self.logger,
									"Persistent error syncing chain listeners: {:?}. Retrying in {} seconds.",
									e,
									MAX_BACKOFF_SECS
								);
						// Sleep with stop signal check to allow immediate shutdown
						tokio::select! {
							biased;
							_ = stop_sync_receiver.changed() => {
								log_trace!(
									self.logger,
									"Stopping initial chain sync during backoff.",
								);
								return;
							}
							_ = tokio::time::sleep(Duration::from_secs(MAX_BACKOFF_SECS)) => {}
						}
					}
				},
			}
		}

		// Now propagate the initial result to unblock waiting subscribers.
		self.wallet_polling_status.lock().unwrap().propagate_result_to_subscribers(Ok(()));

		let mut chain_polling_interval =
			tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
		chain_polling_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		let mut fee_rate_update_interval =
			tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
		// When starting up, we just blocked on updating, so skip the first tick.
		fee_rate_update_interval.reset();
		fee_rate_update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		log_info!(self.logger, "Starting continuous polling for chain updates.");

		// Start the polling loop.
		let mut last_best_block_hash = None;
		loop {
			tokio::select! {
				biased;
				_ = stop_sync_receiver.changed() => {
					log_trace!(
						self.logger,
						"Stopping polling for new chain data.",
					);
					return;
				}
				_ = chain_polling_interval.tick() => {
					tokio::select! {
						biased;
						_ = stop_sync_receiver.changed() => {
							log_trace!(
								self.logger,
								"Stopping polling for new chain data.",
							);
							return;
						}
						_ = self.poll_and_update_listeners(
							Arc::clone(&onchain_wallet),
							Arc::clone(&channel_manager),
							Arc::clone(&chain_monitor),
							Arc::clone(&output_sweeper)
						) => {}
					}
				}
				_ = fee_rate_update_interval.tick() => {
					if last_best_block_hash != Some(channel_manager.current_best_block().block_hash) {
						tokio::select! {
							biased;
							_ = stop_sync_receiver.changed() => {
								log_trace!(
									self.logger,
									"Stopping polling for new chain data.",
								);
								return;
							}
							update_res = self.update_fee_rate_estimates() => {
								if update_res.is_ok() {
									last_best_block_hash = Some(channel_manager.current_best_block().block_hash);
								}
							}
						}
					}
				}
			}
		}
	}

	pub(super) async fn poll_best_block(&self) -> Result<BestBlock, Error> {
		self.poll_chain_tip().await.map(|tip| tip.to_best_block())
	}

	async fn poll_chain_tip(&self) -> Result<ValidatedBlockHeader, Error> {
		let validate_res = tokio::time::timeout(
			Duration::from_secs(CHAIN_POLLING_TIMEOUT_SECS),
			validate_best_block_header(self.api_client.as_ref()),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Failed to poll for chain data: {:?}", e);
			Error::TxSyncTimeout
		})?;

		match validate_res {
			Ok(tip) => {
				*self.latest_chain_tip.write().unwrap() = Some(tip);
				Ok(tip)
			},
			Err(e) => {
				log_error!(self.logger, "Failed to poll for chain data: {:?}", e);
				return Err(Error::TxSyncFailed);
			},
		}
	}

	pub(super) async fn poll_and_update_listeners(
		&self, onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.wallet_polling_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};

		if let Some(mut sync_receiver) = receiver_res {
			log_debug!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet polling result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet polling result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		let res = self
			.poll_and_update_listeners_inner(
				onchain_wallet,
				channel_manager,
				chain_monitor,
				output_sweeper,
			)
			.await;

		self.wallet_polling_status.lock().unwrap().propagate_result_to_subscribers(res);

		res
	}

	async fn poll_and_update_listeners_inner(
		&self, onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let latest_chain_tip_opt = self.latest_chain_tip.read().unwrap().clone();
		let chain_tip =
			if let Some(tip) = latest_chain_tip_opt { tip } else { self.poll_chain_tip().await? };

		let mut locked_header_cache = self.header_cache.lock().await;
		let chain_poller = ChainPoller::new(Arc::clone(&self.api_client), self.config.network);
		let chain_listener = ChainListener {
			onchain_wallet: Arc::clone(&onchain_wallet),
			channel_manager: Arc::clone(&channel_manager),
			chain_monitor: Arc::clone(&chain_monitor),
			output_sweeper,
		};
		let mut spv_client =
			SpvClient::new(chain_tip, chain_poller, &mut *locked_header_cache, &chain_listener);

		let now = SystemTime::now();
		match spv_client.poll_best_tip().await {
			Ok((ChainTip::Better(tip), true)) => {
				log_trace!(
					self.logger,
					"Finished polling best tip in {}ms",
					now.elapsed().unwrap().as_millis()
				);
				*self.latest_chain_tip.write().unwrap() = Some(tip);
			},
			Ok(_) => {},
			Err(e) => {
				log_error!(self.logger, "Failed to poll for chain data: {:?}", e);
				return Err(Error::TxSyncFailed);
			},
		}

		let cur_height = channel_manager.current_best_block().height;

		let now = SystemTime::now();
		let bdk_unconfirmed_txids = onchain_wallet.get_unconfirmed_txids();
		match self
			.api_client
			.get_updated_mempool_transactions(cur_height, bdk_unconfirmed_txids)
			.await
		{
			Ok((unconfirmed_txs, evicted_txids)) => {
				log_trace!(
					self.logger,
					"Finished polling mempool of size {} and {} evicted transactions in {}ms",
					unconfirmed_txs.len(),
					evicted_txids.len(),
					now.elapsed().unwrap().as_millis()
				);
				onchain_wallet.apply_mempool_txs(unconfirmed_txs, evicted_txids).unwrap_or_else(
					|e| {
						log_error!(self.logger, "Failed to apply mempool transactions: {:?}", e);
					},
				);
			},
			Err(e) => {
				log_error!(self.logger, "Failed to poll for mempool transactions: {:?}", e);
				return Err(Error::TxSyncFailed);
			},
		}

		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		let mut locked_node_metrics = self.node_metrics.write().unwrap();
		locked_node_metrics.latest_lightning_wallet_sync_timestamp = unix_time_secs_opt;
		locked_node_metrics.latest_onchain_wallet_sync_timestamp = unix_time_secs_opt;

		write_node_metrics(&*locked_node_metrics, &*self.kv_store, &*self.logger)?;

		Ok(())
	}

	pub(super) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		macro_rules! get_fee_rate_update {
			($estimation_fut:expr) => {{
				let update_res = tokio::time::timeout(
					Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
					$estimation_fut,
				)
				.await
				.map_err(|e| {
					log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
					Error::FeerateEstimationUpdateTimeout
				})?;
				update_res
			}};
		}
		let confirmation_targets = get_all_conf_targets();

		let mut new_fee_rate_cache = HashMap::with_capacity(10);
		let now = Instant::now();
		for target in confirmation_targets {
			let fee_rate_update_res = match target {
				ConfirmationTarget::Lightning(
					LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
				) => {
					let estimation_fut = self.api_client.get_mempool_minimum_fee_rate();
					get_fee_rate_update!(estimation_fut)
				},
				ConfirmationTarget::Lightning(LdkConfirmationTarget::MaximumFeeEstimate) => {
					let num_blocks = get_num_block_defaults_for_target(target);
					let estimation_mode = FeeRateEstimationMode::Conservative;
					let estimation_fut =
						self.api_client.get_fee_estimate_for_target(num_blocks, estimation_mode);
					get_fee_rate_update!(estimation_fut)
				},
				ConfirmationTarget::Lightning(LdkConfirmationTarget::UrgentOnChainSweep) => {
					let num_blocks = get_num_block_defaults_for_target(target);
					let estimation_mode = FeeRateEstimationMode::Conservative;
					let estimation_fut =
						self.api_client.get_fee_estimate_for_target(num_blocks, estimation_mode);
					get_fee_rate_update!(estimation_fut)
				},
				_ => {
					// Otherwise, we default to economical block-target estimate.
					let num_blocks = get_num_block_defaults_for_target(target);
					let estimation_mode = FeeRateEstimationMode::Economical;
					let estimation_fut =
						self.api_client.get_fee_estimate_for_target(num_blocks, estimation_mode);
					get_fee_rate_update!(estimation_fut)
				},
			};

			let fee_rate = match (fee_rate_update_res, self.config.network) {
				(Ok(rate), _) => rate,
				(Err(e), Network::Bitcoin) => {
					// Strictly fail on mainnet.
					log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
					return Err(Error::FeerateEstimationUpdateFailed);
				},
				(Err(e), n) if n == Network::Regtest || n == Network::Signet => {
					// On regtest/signet we just fall back to the usual 1 sat/vb == 250
					// sat/kwu default.
					log_error!(
								self.logger,
								"Failed to retrieve fee rate estimates: {}. Falling back to default of 1 sat/vb.",
								e,
							);
					FeeRate::from_sat_per_kwu(250)
				},
				(Err(e), _) => {
					// On testnet `estimatesmartfee` can be unreliable so we just skip in
					// case of a failure, which will have us falling back to defaults.
					log_error!(
						self.logger,
						"Failed to retrieve fee rate estimates: {}. Falling back to defaults.",
						e,
					);
					return Ok(());
				},
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

		if self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache) {
			// We only log if the values changed, as it might be very spammy otherwise.
			log_debug!(
				self.logger,
				"Fee rate cache update finished in {}ms.",
				now.elapsed().as_millis()
			);
		}

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
		if package.len() == 1 {
			let tx = &package[0];
			let txid = tx.compute_txid();
			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
				self.api_client.broadcast_transaction(tx),
			);
			match timeout_fut.await {
				Ok(res) => match res {
					Ok(id) => {
						debug_assert_eq!(id, txid);
						log_trace!(self.logger, "Successfully broadcast transaction {}", txid);
					},
					Err(e) => {
						log_error!(self.logger, "Failed to broadcast transaction {}: {}", txid, e);
						log_trace!(
							self.logger,
							"Failed broadcast transaction bytes: {}",
							log_bytes!(tx.encode())
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
						log_bytes!(tx.encode())
					);
				},
			}
		} else if package.len() > 1 {
			let txids: Vec<_> = package.iter().map(|tx| tx.compute_txid()).collect();
			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
				self.api_client.submit_package(&package),
			);
			match timeout_fut.await {
				Ok(res) => match res {
					Ok(res) => {
						// TODO: We'd like to debug assert here the txids, but we sometimes
						// get 0 txids back
						log_trace!(self.logger, "Package broadcast result {}", res);
					},
					Err(e) => {
						log_error!(self.logger, "Failed to broadcast package {:?}: {}", txids, e);
						log_trace!(self.logger, "Failed broadcast package bytes:");
						for tx in package {
							log_trace!(self.logger, "{}", log_bytes!(tx.encode()));
						}
					},
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to broadcast package due to timeout {:?}: {}",
						txids,
						e
					);
					log_trace!(self.logger, "Failed broadcast package bytes:");
					for tx in package {
						log_trace!(self.logger, "{}", log_bytes!(tx.encode()));
					}
				},
			}
		}
	}
}

#[derive(Clone)]
pub(crate) enum UtxoSourceClient {
	Rpc(Arc<RpcClient>),
	Rest(Arc<RestClient>),
}

impl std::ops::Deref for UtxoSourceClient {
	type Target = Self;
	fn deref(&self) -> &Self {
		self
	}
}

impl BlockSource for UtxoSourceClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a BlockHash, height_hint: Option<u32>,
	) -> impl Future<Output = Result<BlockHeaderData, BlockSourceError>> + 'a {
		async move {
			match self {
				Self::Rpc(client) => client.get_header(header_hash, height_hint).await,
				Self::Rest(client) => client.get_header(header_hash, height_hint).await,
			}
		}
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a BlockHash,
	) -> impl Future<Output = Result<BlockData, BlockSourceError>> + 'a {
		async move {
			match self {
				Self::Rpc(client) => client.get_block(header_hash).await,
				Self::Rest(client) => client.get_block(header_hash).await,
			}
		}
	}

	fn get_best_block<'a>(
		&'a self,
	) -> impl Future<Output = Result<(BlockHash, Option<u32>), BlockSourceError>> + 'a {
		async move {
			match self {
				Self::Rpc(client) => client.get_best_block().await,
				Self::Rest(client) => client.get_best_block().await,
			}
		}
	}
}

impl UtxoSource for UtxoSourceClient {
	fn get_block_hash_by_height<'a>(
		&'a self, block_height: u32,
	) -> impl Future<Output = Result<BlockHash, BlockSourceError>> + 'a {
		async move {
			match self {
				Self::Rpc(client) => client.get_block_hash_by_height(block_height).await,
				Self::Rest(client) => client.get_block_hash_by_height(block_height).await,
			}
		}
	}

	fn is_output_unspent<'a>(
		&'a self, outpoint: OutPoint,
	) -> impl Future<Output = Result<bool, BlockSourceError>> + 'a {
		async move {
			match self {
				Self::Rpc(client) => client.is_output_unspent(outpoint).await,
				Self::Rest(client) => client.is_output_unspent(outpoint).await,
			}
		}
	}
}

pub enum BitcoindClient {
	Rpc {
		rpc_client: Arc<RpcClient>,
		latest_mempool_timestamp: AtomicU64,
		mempool_entries_cache: tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
	},
	Rest {
		rest_client: Arc<RestClient>,
		rpc_client: Arc<RpcClient>,
		latest_mempool_timestamp: AtomicU64,
		mempool_entries_cache: tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
	},
}

impl BitcoindClient {
	/// Creates a new RPC API client for the chain interactions with Bitcoin Core.
	pub(crate) fn new_rpc(host: String, port: u16, rpc_user: String, rpc_password: String) -> Self {
		let http_endpoint = endpoint(host, port);
		let rpc_credentials = rpc_credentials(rpc_user, rpc_password);

		let rpc_client = Arc::new(RpcClient::new(&rpc_credentials, http_endpoint));

		let latest_mempool_timestamp = AtomicU64::new(0);

		let mempool_entries_cache = tokio::sync::Mutex::new(HashMap::new());
		let mempool_txs_cache = tokio::sync::Mutex::new(HashMap::new());
		Self::Rpc { rpc_client, latest_mempool_timestamp, mempool_entries_cache, mempool_txs_cache }
	}

	/// Creates a new, primarily REST API client for the chain interactions
	/// with Bitcoin Core.
	///
	/// Aside the required REST host and port, we provide RPC configuration
	/// options for necessary calls not supported by the REST interface.
	pub(crate) fn new_rest(
		rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16, rpc_user: String,
		rpc_password: String,
	) -> Self {
		let rest_endpoint = endpoint(rest_host, rest_port).with_path("/rest".to_string());
		let rest_client = Arc::new(RestClient::new(rest_endpoint));

		let rpc_endpoint = endpoint(rpc_host, rpc_port);
		let rpc_credentials = rpc_credentials(rpc_user, rpc_password);
		let rpc_client = Arc::new(RpcClient::new(&rpc_credentials, rpc_endpoint));

		let latest_mempool_timestamp = AtomicU64::new(0);

		let mempool_entries_cache = tokio::sync::Mutex::new(HashMap::new());
		let mempool_txs_cache = tokio::sync::Mutex::new(HashMap::new());

		Self::Rest {
			rest_client,
			rpc_client,
			latest_mempool_timestamp,
			mempool_entries_cache,
			mempool_txs_cache,
		}
	}

	fn utxo_source(&self) -> UtxoSourceClient {
		match self {
			Self::Rpc { rpc_client, .. } => UtxoSourceClient::Rpc(Arc::clone(&rpc_client)),
			Self::Rest { rest_client, .. } => UtxoSourceClient::Rest(Arc::clone(&rest_client)),
		}
	}

	/// Broadcasts the provided transaction.
	pub(crate) async fn broadcast_transaction(&self, tx: &Transaction) -> std::io::Result<Txid> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::broadcast_transaction_inner(Arc::clone(rpc_client), tx).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// Bitcoin Core's REST interface does not support broadcasting transactions
				// so we use the RPC client.
				Self::broadcast_transaction_inner(Arc::clone(rpc_client), tx).await
			},
		}
	}

	async fn broadcast_transaction_inner(
		rpc_client: Arc<RpcClient>, tx: &Transaction,
	) -> std::io::Result<Txid> {
		let tx_serialized = bitcoin::consensus::encode::serialize_hex(tx);
		let tx_json = serde_json::json!(tx_serialized);
		rpc_client.call_method::<Txid>("sendrawtransaction", &[tx_json]).await
	}

	/// Submits the provided package
	pub(crate) async fn submit_package(&self, package: &[Transaction]) -> std::io::Result<String> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::submit_package_inner(Arc::clone(rpc_client), package).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// Bitcoin Core's REST interface does not support submitting packages
				// so we use the RPC client.
				Self::submit_package_inner(Arc::clone(rpc_client), package).await
			},
		}
	}

	async fn submit_package_inner(
		rpc_client: Arc<RpcClient>, package: &[Transaction],
	) -> std::io::Result<String> {
		let package_serialized: Vec<_> =
			package.iter().map(|tx| bitcoin::consensus::encode::serialize_hex(tx)).collect();
		let package_json = serde_json::json!(package_serialized);
		rpc_client
			.call_method::<serde_json::Value>("submitpackage", &[package_json])
			.await
			.map(|value| value.to_string())
	}

	/// Retrieve the fee estimate needed for a transaction to begin
	/// confirmation within the provided `num_blocks`.
	pub(crate) async fn get_fee_estimate_for_target(
		&self, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_fee_estimate_for_target_inner(
					Arc::clone(rpc_client),
					num_blocks,
					estimation_mode,
				)
				.await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// We rely on the internal RPC client to make this call, as this
				// operation is not supported by Bitcoin Core's REST interface.
				Self::get_fee_estimate_for_target_inner(
					Arc::clone(rpc_client),
					num_blocks,
					estimation_mode,
				)
				.await
			},
		}
	}

	/// Estimate the fee rate for the provided target number of blocks.
	async fn get_fee_estimate_for_target_inner(
		rpc_client: Arc<RpcClient>, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		let num_blocks_json = serde_json::json!(num_blocks);
		let estimation_mode_json = serde_json::json!(estimation_mode);
		rpc_client
			.call_method::<FeeResponse>(
				"estimatesmartfee",
				&[num_blocks_json, estimation_mode_json],
			)
			.await
			.map(|resp| resp.0)
	}

	/// Gets the mempool minimum fee rate.
	pub(crate) async fn get_mempool_minimum_fee_rate(&self) -> std::io::Result<FeeRate> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_mempool_minimum_fee_rate_rpc(Arc::clone(rpc_client)).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_mempool_minimum_fee_rate_rest(Arc::clone(rest_client)).await
			},
		}
	}

	/// Get the mempool minimum fee rate via RPC interface.
	async fn get_mempool_minimum_fee_rate_rpc(
		rpc_client: Arc<RpcClient>,
	) -> std::io::Result<FeeRate> {
		rpc_client
			.call_method::<MempoolMinFeeResponse>("getmempoolinfo", &[])
			.await
			.map(|resp| resp.0)
	}

	/// Get the mempool minimum fee rate via REST interface.
	async fn get_mempool_minimum_fee_rate_rest(
		rest_client: Arc<RestClient>,
	) -> std::io::Result<FeeRate> {
		rest_client
			.request_resource::<JsonResponse, MempoolMinFeeResponse>("mempool/info.json")
			.await
			.map(|resp| resp.0)
	}

	/// Gets the raw transaction for the provided transaction ID. Returns `None` if not found.
	pub(crate) async fn get_raw_transaction(
		&self, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_raw_transaction_rpc(Arc::clone(rpc_client), txid).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_raw_transaction_rest(Arc::clone(rest_client), txid).await
			},
		}
	}

	/// Retrieve raw transaction for provided transaction ID via the RPC interface.
	async fn get_raw_transaction_rpc(
		rpc_client: Arc<RpcClient>, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = txid.to_string();
		let txid_json = serde_json::json!(txid_hex);
		match rpc_client
			.call_method::<GetRawTransactionResponse>("getrawtransaction", &[txid_json])
			.await
		{
			Ok(resp) => Ok(Some(resp.0)),
			Err(e) => match e.into_inner() {
				Some(inner) => {
					let rpc_error_res: Result<Box<RpcError>, _> = inner.downcast();

					match rpc_error_res {
						Ok(rpc_error) => {
							// Check if it's the 'not found' error code.
							if rpc_error.code == -5 {
								Ok(None)
							} else {
								Err(std::io::Error::new(std::io::ErrorKind::Other, rpc_error))
							}
						},
						Err(_) => Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to process getrawtransaction response",
						)),
					}
				},
				None => Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to process getrawtransaction response",
				)),
			},
		}
	}

	/// Retrieve raw transaction for provided transaction ID via the REST interface.
	async fn get_raw_transaction_rest(
		rest_client: Arc<RestClient>, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = txid.to_string();
		let tx_path = format!("tx/{}.json", txid_hex);
		match rest_client
			.request_resource::<JsonResponse, GetRawTransactionResponse>(&tx_path)
			.await
		{
			Ok(resp) => Ok(Some(resp.0)),
			Err(e) => match e.kind() {
				std::io::ErrorKind::Other => {
					match e.into_inner() {
						Some(inner) => {
							let http_error_res: Result<Box<HttpError>, _> = inner.downcast();
							match http_error_res {
								Ok(http_error) => {
									// Check if it's the HTTP NOT_FOUND error code.
									if &http_error.status_code == "404" {
										Ok(None)
									} else {
										Err(std::io::Error::new(
											std::io::ErrorKind::Other,
											http_error,
										))
									}
								},
								Err(_) => {
									let error_msg =
										format!("Failed to process {} response.", tx_path);
									Err(std::io::Error::new(
										std::io::ErrorKind::Other,
										error_msg.as_str(),
									))
								},
							}
						},
						None => {
							let error_msg = format!("Failed to process {} response.", tx_path);
							Err(std::io::Error::new(std::io::ErrorKind::Other, error_msg.as_str()))
						},
					}
				},
				_ => {
					let error_msg = format!("Failed to process {} response.", tx_path);
					Err(std::io::Error::new(std::io::ErrorKind::Other, error_msg.as_str()))
				},
			},
		}
	}

	/// Retrieves the raw mempool.
	pub(crate) async fn get_raw_mempool(&self) -> std::io::Result<Vec<Txid>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_raw_mempool_rpc(Arc::clone(rpc_client)).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_raw_mempool_rest(Arc::clone(rest_client)).await
			},
		}
	}

	/// Retrieves the raw mempool via the RPC interface.
	async fn get_raw_mempool_rpc(rpc_client: Arc<RpcClient>) -> std::io::Result<Vec<Txid>> {
		let verbose_flag_json = serde_json::json!(false);
		rpc_client
			.call_method::<GetRawMempoolResponse>("getrawmempool", &[verbose_flag_json])
			.await
			.map(|resp| resp.0)
	}

	/// Retrieves the raw mempool via the REST interface.
	async fn get_raw_mempool_rest(rest_client: Arc<RestClient>) -> std::io::Result<Vec<Txid>> {
		rest_client
			.request_resource::<JsonResponse, GetRawMempoolResponse>(
				"mempool/contents.json?verbose=false",
			)
			.await
			.map(|resp| resp.0)
	}

	/// Retrieves an entry from the mempool if it exists, else return `None`.
	pub(crate) async fn get_mempool_entry(
		&self, txid: Txid,
	) -> std::io::Result<Option<MempoolEntry>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_mempool_entry_inner(Arc::clone(rpc_client), txid).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				Self::get_mempool_entry_inner(Arc::clone(rpc_client), txid).await
			},
		}
	}

	/// Retrieves the mempool entry of the provided transaction ID.
	async fn get_mempool_entry_inner(
		client: Arc<RpcClient>, txid: Txid,
	) -> std::io::Result<Option<MempoolEntry>> {
		let txid_hex = txid.to_string();
		let txid_json = serde_json::json!(txid_hex);

		match client.call_method::<GetMempoolEntryResponse>("getmempoolentry", &[txid_json]).await {
			Ok(resp) => Ok(Some(MempoolEntry { txid, time: resp.time, height: resp.height })),
			Err(e) => match e.into_inner() {
				Some(inner) => {
					let rpc_error_res: Result<Box<RpcError>, _> = inner.downcast();

					match rpc_error_res {
						Ok(rpc_error) => {
							// Check if it's the 'not found' error code.
							if rpc_error.code == -5 {
								Ok(None)
							} else {
								Err(std::io::Error::new(std::io::ErrorKind::Other, rpc_error))
							}
						},
						Err(_) => Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to process getmempoolentry response",
						)),
					}
				},
				None => Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to process getmempoolentry response",
				)),
			},
		}
	}

	pub(crate) async fn update_mempool_entries_cache(&self) -> std::io::Result<()> {
		match self {
			BitcoindClient::Rpc { mempool_entries_cache, .. } => {
				self.update_mempool_entries_cache_inner(mempool_entries_cache).await
			},
			BitcoindClient::Rest { mempool_entries_cache, .. } => {
				self.update_mempool_entries_cache_inner(mempool_entries_cache).await
			},
		}
	}

	async fn update_mempool_entries_cache_inner(
		&self, mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
	) -> std::io::Result<()> {
		let mempool_txids = self.get_raw_mempool().await?;

		let mut mempool_entries_cache = mempool_entries_cache.lock().await;
		mempool_entries_cache.retain(|txid, _| mempool_txids.contains(txid));

		if let Some(difference) = mempool_txids.len().checked_sub(mempool_entries_cache.capacity())
		{
			mempool_entries_cache.reserve(difference)
		}

		for txid in mempool_txids {
			if mempool_entries_cache.contains_key(&txid) {
				continue;
			}

			if let Some(entry) = self.get_mempool_entry(txid).await? {
				mempool_entries_cache.insert(txid, entry.clone());
			}
		}

		mempool_entries_cache.shrink_to_fit();

		Ok(())
	}

	/// Returns two `Vec`s:
	/// - mempool transactions, alongside their first-seen unix timestamps.
	/// - transactions that have been evicted from the mempool, alongside the last time they were seen absent.
	pub(crate) async fn get_updated_mempool_transactions(
		&self, best_processed_height: u32, bdk_unconfirmed_txids: Vec<Txid>,
	) -> std::io::Result<(Vec<(Transaction, u64)>, Vec<(Txid, u64)>)> {
		let mempool_txs =
			self.get_mempool_transactions_and_timestamp_at_height(best_processed_height).await?;
		let evicted_txids =
			self.get_evicted_mempool_txids_and_timestamp(bdk_unconfirmed_txids).await?;
		Ok((mempool_txs, evicted_txids))
	}

	/// Get mempool transactions, alongside their first-seen unix timestamps.
	///
	/// This method is an adapted version of `bdk_bitcoind_rpc::Emitter::mempool`. It emits each
	/// transaction only once, unless we cannot assume the transaction's ancestors are already
	/// emitted.
	pub(crate) async fn get_mempool_transactions_and_timestamp_at_height(
		&self, best_processed_height: u32,
	) -> std::io::Result<Vec<(Transaction, u64)>> {
		match self {
			BitcoindClient::Rpc {
				latest_mempool_timestamp,
				mempool_entries_cache,
				mempool_txs_cache,
				..
			} => {
				self.get_mempool_transactions_and_timestamp_at_height_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					mempool_txs_cache,
					best_processed_height,
				)
				.await
			},
			BitcoindClient::Rest {
				latest_mempool_timestamp,
				mempool_entries_cache,
				mempool_txs_cache,
				..
			} => {
				self.get_mempool_transactions_and_timestamp_at_height_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					mempool_txs_cache,
					best_processed_height,
				)
				.await
			},
		}
	}

	async fn get_mempool_transactions_and_timestamp_at_height_inner(
		&self, latest_mempool_timestamp: &AtomicU64,
		mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: &tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
		best_processed_height: u32,
	) -> std::io::Result<Vec<(Transaction, u64)>> {
		let prev_mempool_time = latest_mempool_timestamp.load(Ordering::Relaxed);
		let mut latest_time = prev_mempool_time;

		self.update_mempool_entries_cache().await?;

		let mempool_entries_cache = mempool_entries_cache.lock().await;
		let mut mempool_txs_cache = mempool_txs_cache.lock().await;
		mempool_txs_cache.retain(|txid, _| mempool_entries_cache.contains_key(txid));

		if let Some(difference) =
			mempool_entries_cache.len().checked_sub(mempool_txs_cache.capacity())
		{
			mempool_txs_cache.reserve(difference)
		}

		let mut txs_to_emit = Vec::with_capacity(mempool_entries_cache.len());
		for (txid, entry) in mempool_entries_cache.iter() {
			if entry.time > latest_time {
				latest_time = entry.time;
			}

			// Avoid emitting transactions that are already emitted if we can guarantee
			// blocks containing ancestors are already emitted. The bitcoind rpc interface
			// provides us with the block height that the tx is introduced to the mempool.
			// If we have already emitted the block of height, we can assume that all
			// ancestor txs have been processed by the receiver.
			let ancestor_within_height = entry.height <= best_processed_height;
			let is_already_emitted = entry.time <= prev_mempool_time;
			if is_already_emitted && ancestor_within_height {
				continue;
			}

			if let Some((cached_tx, cached_time)) = mempool_txs_cache.get(txid) {
				txs_to_emit.push((cached_tx.clone(), *cached_time));
				continue;
			}

			match self.get_raw_transaction(&entry.txid).await {
				Ok(Some(tx)) => {
					mempool_txs_cache.insert(entry.txid, (tx.clone(), entry.time));
					txs_to_emit.push((tx, entry.time));
				},
				Ok(None) => {
					continue;
				},
				Err(e) => return Err(e),
			};
		}

		if !txs_to_emit.is_empty() {
			latest_mempool_timestamp.store(latest_time, Ordering::Release);
		}
		Ok(txs_to_emit)
	}

	// Retrieve a list of Txids that have been evicted from the mempool.
	//
	// To this end, we first update our local mempool_entries_cache and then return all unconfirmed
	// wallet `Txid`s that don't appear in the mempool still.
	async fn get_evicted_mempool_txids_and_timestamp(
		&self, bdk_unconfirmed_txids: Vec<Txid>,
	) -> std::io::Result<Vec<(Txid, u64)>> {
		match self {
			BitcoindClient::Rpc { latest_mempool_timestamp, mempool_entries_cache, .. } => {
				Self::get_evicted_mempool_txids_and_timestamp_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					bdk_unconfirmed_txids,
				)
				.await
			},
			BitcoindClient::Rest { latest_mempool_timestamp, mempool_entries_cache, .. } => {
				Self::get_evicted_mempool_txids_and_timestamp_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					bdk_unconfirmed_txids,
				)
				.await
			},
		}
	}

	async fn get_evicted_mempool_txids_and_timestamp_inner(
		latest_mempool_timestamp: &AtomicU64,
		mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		bdk_unconfirmed_txids: Vec<Txid>,
	) -> std::io::Result<Vec<(Txid, u64)>> {
		let latest_mempool_timestamp = latest_mempool_timestamp.load(Ordering::Relaxed);
		let mempool_entries_cache = mempool_entries_cache.lock().await;
		let evicted_txids = bdk_unconfirmed_txids
			.into_iter()
			.filter(|txid| !mempool_entries_cache.contains_key(txid))
			.map(|txid| (txid, latest_mempool_timestamp))
			.collect();
		Ok(evicted_txids)
	}
}

impl BlockSource for BitcoindClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a bitcoin::BlockHash, height_hint: Option<u32>,
	) -> impl Future<Output = Result<BlockHeaderData, BlockSourceError>> + 'a {
		async move {
			match self {
				BitcoindClient::Rpc { rpc_client, .. } => {
					rpc_client.get_header(header_hash, height_hint).await
				},
				BitcoindClient::Rest { rest_client, .. } => {
					rest_client.get_header(header_hash, height_hint).await
				},
			}
		}
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a bitcoin::BlockHash,
	) -> impl Future<Output = Result<BlockData, BlockSourceError>> + 'a {
		async move {
			match self {
				BitcoindClient::Rpc { rpc_client, .. } => rpc_client.get_block(header_hash).await,
				BitcoindClient::Rest { rest_client, .. } => {
					rest_client.get_block(header_hash).await
				},
			}
		}
	}

	fn get_best_block<'a>(
		&'a self,
	) -> impl Future<Output = Result<(bitcoin::BlockHash, Option<u32>), BlockSourceError>> + 'a {
		async move {
			match self {
				BitcoindClient::Rpc { rpc_client, .. } => rpc_client.get_best_block().await,
				BitcoindClient::Rest { rest_client, .. } => rest_client.get_best_block().await,
			}
		}
	}
}

pub(crate) struct FeeResponse(pub FeeRate);

impl TryInto<FeeResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<FeeResponse> {
		if !self.0["errors"].is_null() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Other,
				self.0["errors"].to_string(),
			));
		}
		let fee_rate_btc_per_kvbyte = self.0["feerate"]
			.as_f64()
			.ok_or(std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse fee rate"))?;
		// Bitcoin Core gives us a feerate in BTC/KvB.
		// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
		let fee_rate = {
			let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvbyte * 25_000_000.0).round() as u64;
			FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
		};
		Ok(FeeResponse(fee_rate))
	}
}

pub(crate) struct MempoolMinFeeResponse(pub FeeRate);

impl TryInto<MempoolMinFeeResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<MempoolMinFeeResponse> {
		let fee_rate_btc_per_kvbyte = self.0["mempoolminfee"]
			.as_f64()
			.ok_or(std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse fee rate"))?;
		// Bitcoin Core gives us a feerate in BTC/KvB.
		// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
		let fee_rate = {
			let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvbyte * 25_000_000.0).round() as u64;
			FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
		};
		Ok(MempoolMinFeeResponse(fee_rate))
	}
}

pub(crate) struct GetRawTransactionResponse(pub Transaction);

impl TryInto<GetRawTransactionResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetRawTransactionResponse> {
		let tx = self
			.0
			.as_str()
			.ok_or(std::io::Error::new(
				std::io::ErrorKind::Other,
				"Failed to parse getrawtransaction response",
			))
			.and_then(|s| {
				bitcoin::consensus::encode::deserialize_hex(s).map_err(|_| {
					std::io::Error::new(
						std::io::ErrorKind::Other,
						"Failed to parse getrawtransaction response",
					)
				})
			})?;

		Ok(GetRawTransactionResponse(tx))
	}
}

pub struct GetRawMempoolResponse(Vec<Txid>);

impl TryInto<GetRawMempoolResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetRawMempoolResponse> {
		let res = self.0.as_array().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse getrawmempool response",
		))?;

		let mut mempool_transactions = Vec::with_capacity(res.len());

		for hex in res {
			let txid = if let Some(hex_str) = hex.as_str() {
				match hex_str.parse::<Txid>() {
					Ok(txid) => txid,
					Err(_) => {
						return Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to parse getrawmempool response",
						));
					},
				}
			} else {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getrawmempool response",
				));
			};

			mempool_transactions.push(txid);
		}

		Ok(GetRawMempoolResponse(mempool_transactions))
	}
}

pub struct GetMempoolEntryResponse {
	time: u64,
	height: u32,
}

impl TryInto<GetMempoolEntryResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetMempoolEntryResponse> {
		let res = self.0.as_object().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse getmempoolentry response",
		))?;

		let time = match res["time"].as_u64() {
			Some(time) => time,
			None => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getmempoolentry response",
				));
			},
		};

		let height = match res["height"].as_u64().and_then(|h| h.try_into().ok()) {
			Some(height) => height,
			None => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getmempoolentry response",
				));
			},
		};

		Ok(GetMempoolEntryResponse { time, height })
	}
}

/*
pub struct SubmitPackageResponse {
	package_msg: String,
	txids: Vec<Txid>,
}

impl TryInto<SubmitPackageResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<SubmitPackageResponse> {
		let package_msg = self.0["package_msg"]
			.as_str()
			.ok_or(std::io::Error::new(
				std::io::ErrorKind::Other,
				"Failed to parse submitpackage response",
			))?
			.to_string();
		let tx_results = self.0["tx-results"].as_object().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse submitpackage response",
		))?;
		let mut txids = Vec::with_capacity(tx_results.len());
		for tx_result in tx_results.values() {
			let txid_string = tx_result["txid"].as_str().ok_or(std::io::Error::new(
				std::io::ErrorKind::Other,
				"Failed to parse submitpackage response",
			))?;
			let txid: Txid = txid_string.parse().map_err(|_| {
				std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse submitpackage response",
				)
			})?;
			txids.push(txid);
		}
		Ok(SubmitPackageResponse { package_msg, txids })
	}
}
*/

#[derive(Debug, Clone)]
pub(crate) struct MempoolEntry {
	/// The transaction id
	txid: Txid,
	/// Local time transaction entered pool in seconds since 1 Jan 1970 GMT
	time: u64,
	/// Block height when transaction entered pool
	height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum FeeRateEstimationMode {
	Economical,
	Conservative,
}

const MAX_HEADER_CACHE_ENTRIES: usize = 100;

pub(crate) struct BoundedHeaderCache {
	header_map: HashMap<BlockHash, ValidatedBlockHeader>,
	recently_seen: VecDeque<BlockHash>,
}

impl BoundedHeaderCache {
	pub(crate) fn new() -> Self {
		let header_map = HashMap::new();
		let recently_seen = VecDeque::new();
		Self { header_map, recently_seen }
	}
}

impl Cache for BoundedHeaderCache {
	fn look_up(&self, block_hash: &BlockHash) -> Option<&ValidatedBlockHeader> {
		self.header_map.get(block_hash)
	}

	fn block_connected(&mut self, block_hash: BlockHash, block_header: ValidatedBlockHeader) {
		self.recently_seen.push_back(block_hash);
		self.header_map.insert(block_hash, block_header);

		if self.header_map.len() >= MAX_HEADER_CACHE_ENTRIES {
			// Keep dropping old entries until we've actually removed a header entry.
			while let Some(oldest_entry) = self.recently_seen.pop_front() {
				if self.header_map.remove(&oldest_entry).is_some() {
					break;
				}
			}
		}
	}

	fn block_disconnected(&mut self, block_hash: &BlockHash) -> Option<ValidatedBlockHeader> {
		self.recently_seen.retain(|e| e != block_hash);
		self.header_map.remove(block_hash)
	}
}

pub(crate) struct ChainListener {
	pub(crate) onchain_wallet: Arc<Wallet>,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) chain_monitor: Arc<ChainMonitor>,
	pub(crate) output_sweeper: Arc<Sweeper>,
}

impl Listen for ChainListener {
	fn filtered_block_connected(
		&self, header: &bitcoin::block::Header,
		txdata: &lightning::chain::transaction::TransactionData, height: u32,
	) {
		self.onchain_wallet.filtered_block_connected(header, txdata, height);
		self.channel_manager.filtered_block_connected(header, txdata, height);
		self.chain_monitor.filtered_block_connected(header, txdata, height);
		self.output_sweeper.filtered_block_connected(header, txdata, height);
	}
	fn block_connected(&self, block: &bitcoin::Block, height: u32) {
		self.onchain_wallet.block_connected(block, height);
		self.channel_manager.block_connected(block, height);
		self.chain_monitor.block_connected(block, height);
		self.output_sweeper.block_connected(block, height);
	}

	fn blocks_disconnected(&self, fork_point_block: lightning::chain::BestBlock) {
		self.onchain_wallet.blocks_disconnected(fork_point_block);
		self.channel_manager.blocks_disconnected(fork_point_block);
		self.chain_monitor.blocks_disconnected(fork_point_block);
		self.output_sweeper.blocks_disconnected(fork_point_block);
	}
}

pub(crate) fn rpc_credentials(rpc_user: String, rpc_password: String) -> String {
	BASE64_STANDARD.encode(format!("{}:{}", rpc_user, rpc_password))
}

pub(crate) fn endpoint(host: String, port: u16) -> HttpEndpoint {
	HttpEndpoint::for_host(host).with_port(port)
}

#[derive(Debug)]
pub struct HttpError {
	pub(crate) status_code: String,
	pub(crate) contents: Vec<u8>,
}

impl std::error::Error for HttpError {}

impl std::fmt::Display for HttpError {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		let contents = String::from_utf8_lossy(&self.contents);
		write!(f, "status_code: {}, contents: {}", self.status_code, contents)
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use bitcoin::{FeeRate, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid, Witness};
	use lightning_block_sync::http::JsonResponse;
	use proptest::arbitrary::any;
	use proptest::collection::vec;
	use proptest::{prop_assert_eq, prop_compose, proptest};
	use serde_json::json;

	use crate::chain::bitcoind::{
		FeeResponse, GetMempoolEntryResponse, GetRawMempoolResponse, GetRawTransactionResponse,
		MempoolMinFeeResponse,
	};

	prop_compose! {
		fn arbitrary_witness()(
			witness_elements in vec(vec(any::<u8>(), 0..100), 0..20)
		) -> Witness {
			let mut witness = Witness::new();
			for element in witness_elements {
				witness.push(element);
			}
			witness
		}
	}

	prop_compose! {
		fn arbitrary_txin()(
			outpoint_hash in any::<[u8; 32]>(),
			outpoint_vout in any::<u32>(),
			script_bytes in vec(any::<u8>(), 0..100),
			witness in arbitrary_witness(),
			sequence in any::<u32>()
		) -> TxIn {
			TxIn {
				previous_output: OutPoint {
					txid: Txid::from_byte_array(outpoint_hash),
					vout: outpoint_vout,
				},
				script_sig: ScriptBuf::from_bytes(script_bytes),
				sequence: bitcoin::Sequence::from_consensus(sequence),
				witness,
			}
		}
	}

	prop_compose! {
		fn arbitrary_txout()(
			value in 0u64..21_000_000_00_000_000u64,
			script_bytes in vec(any::<u8>(), 0..100)
		) -> TxOut {
			TxOut {
				value: bitcoin::Amount::from_sat(value),
				script_pubkey: ScriptBuf::from_bytes(script_bytes),
			}
		}
	}

	prop_compose! {
		fn arbitrary_transaction()(
			version in any::<i32>(),
			inputs in vec(arbitrary_txin(), 1..20),
			outputs in vec(arbitrary_txout(), 1..20),
			lock_time in any::<u32>()
		) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version(version),
				input: inputs,
				output: outputs,
				lock_time: bitcoin::absolute::LockTime::from_consensus(lock_time),
			}
		}
	}

	proptest! {
		#![proptest_config(proptest::test_runner::Config::with_cases(20))]

		#[test]
		fn prop_get_raw_mempool_response_roundtrip(txids in vec(any::<[u8;32]>(), 0..10)) {
			let txid_vec: Vec<Txid> = txids.into_iter().map(Txid::from_byte_array).collect();
			let original = GetRawMempoolResponse(txid_vec.clone());

			let json_vec: Vec<String> = txid_vec.iter().map(|t| t.to_string()).collect();
			let json_val = serde_json::Value::Array(json_vec.iter().map(|s| json!(s)).collect());

			let resp = JsonResponse(json_val);
			let decoded: GetRawMempoolResponse = resp.try_into().unwrap();

			prop_assert_eq!(original.0.len(), decoded.0.len());

			prop_assert_eq!(original.0, decoded.0);
		}

		#[test]
		fn prop_get_mempool_entry_response_roundtrip(
			time in any::<u64>(),
			height in any::<u32>()
		) {
			let json_val = json!({
				"time": time,
				"height": height
			});

			let resp = JsonResponse(json_val);
			let decoded: GetMempoolEntryResponse = resp.try_into().unwrap();

			prop_assert_eq!(decoded.time, time);
			prop_assert_eq!(decoded.height, height);
		}

		#[test]
		fn prop_get_raw_transaction_response_roundtrip(tx in arbitrary_transaction()) {
			let hex = bitcoin::consensus::encode::serialize_hex(&tx);
			let json_val = serde_json::Value::String(hex.clone());

			let resp = JsonResponse(json_val);
			let decoded: GetRawTransactionResponse = resp.try_into().unwrap();

			prop_assert_eq!(decoded.0.compute_txid(), tx.compute_txid());
			prop_assert_eq!(decoded.0.compute_wtxid(), tx.compute_wtxid());

			prop_assert_eq!(decoded.0, tx);
		}

		#[test]
		fn prop_fee_response_roundtrip(fee_rate in any::<f64>()) {
			let fee_rate = fee_rate.abs();
			let json_val = json!({
				"feerate": fee_rate,
				"errors": serde_json::Value::Null
			});

			let resp = JsonResponse(json_val);
			let decoded: FeeResponse = resp.try_into().unwrap();

			let expected = {
				let fee_rate_sat_per_kwu = (fee_rate * 25_000_000.0).round() as u64;
				FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
			};
			prop_assert_eq!(decoded.0, expected);
		}

		#[test]
		fn prop_mempool_min_fee_response_roundtrip(fee_rate in any::<f64>()) {
			let fee_rate = fee_rate.abs();
			let json_val = json!({
				"mempoolminfee": fee_rate
			});

			let resp = JsonResponse(json_val);
			let decoded: MempoolMinFeeResponse = resp.try_into().unwrap();

			let expected = {
				let fee_rate_sat_per_kwu = (fee_rate * 25_000_000.0).round() as u64;
				FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
			};
			prop_assert_eq!(decoded.0, expected);
		}

	}
}
