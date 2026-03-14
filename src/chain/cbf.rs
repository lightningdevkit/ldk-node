// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bdk_chain::{BlockId, ConfirmationBlockTime, TxUpdate};
use bdk_wallet::Update;
use bip157::{
	BlockHash, Builder, Client, Event, Info, Requester, SyncUpdate, TrustedPeer, Warning,
};
use bitcoin::constants::SUBSIDY_HALVING_INTERVAL;
use bitcoin::{Amount, FeeRate, Network, Script, ScriptBuf, Transaction, Txid};
use lightning::chain::WatchedOutput;
use lightning::util::ser::Writeable;
use tokio::sync::{mpsc, oneshot};

use super::WalletSyncStatus;
use crate::config::{CbfSyncConfig, Config, BDK_CLIENT_STOP_GAP};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

/// Minimum fee rate: 1 sat/vB = 250 sat/kWU. Used as a floor for computed fee rates.
const MIN_FEERATE_SAT_PER_KWU: u64 = 250;

/// Number of recent blocks to look back for per-target fee rate estimation.
const FEE_RATE_LOOKBACK_BLOCKS: usize = 6;

pub(super) struct CbfChainSource {
	/// Peer addresses for sourcing compact block filters via P2P.
	peers: Vec<String>,
	/// User-provided sync configuration (timeouts, background sync intervals).
	pub(super) sync_config: CbfSyncConfig,
	/// Tracks whether the bip157 node is running and holds the command handle.
	cbf_runtime_status: Mutex<CbfRuntimeStatus>,
	/// Latest chain tip hash, updated by the background event processing task.
	latest_tip: Arc<Mutex<Option<BlockHash>>>,
	/// Scripts to match against compact block filters during a scan.
	watched_scripts: Arc<RwLock<Vec<ScriptBuf>>>,
	/// Block (height, hash) pairs where filters matched watched scripts.
	matched_block_hashes: Arc<Mutex<Vec<(u32, BlockHash)>>>,
	/// One-shot channel sender to signal filter scan completion.
	sync_completion_tx: Arc<Mutex<Option<oneshot::Sender<SyncUpdate>>>>,
	/// Filters at or below this height are skipped during incremental scans.
	filter_skip_height: Arc<AtomicU32>,
	/// Last block height reached by on-chain wallet sync, used for incremental scans.
	last_onchain_synced_height: Mutex<Option<u32>>,
	/// Deduplicates concurrent on-chain wallet sync requests.
	onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
	/// Shared fee rate estimator, updated by this chain source.
	fee_estimator: Arc<OnchainFeeEstimator>,
	/// Persistent key-value store for node metrics.
	kv_store: Arc<DynStore>,
	/// Node configuration (network, storage path, etc.).
	config: Arc<Config>,
	/// Logger instance.
	logger: Arc<Logger>,
	/// Shared node metrics (sync timestamps, etc.).
	node_metrics: Arc<RwLock<NodeMetrics>>,
}

enum CbfRuntimeStatus {
	Started { requester: Requester },
	Stopped,
}

/// Shared state passed to the background event processing task.
struct CbfEventState {
	latest_tip: Arc<Mutex<Option<BlockHash>>>,
	watched_scripts: Arc<RwLock<Vec<ScriptBuf>>>,
	matched_block_hashes: Arc<Mutex<Vec<(u32, BlockHash)>>>,
	sync_completion_tx: Arc<Mutex<Option<oneshot::Sender<SyncUpdate>>>>,
	filter_skip_height: Arc<AtomicU32>,
}

impl CbfChainSource {
	pub(crate) fn new(
		peers: Vec<String>, sync_config: CbfSyncConfig, fee_estimator: Arc<OnchainFeeEstimator>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let cbf_runtime_status = Mutex::new(CbfRuntimeStatus::Stopped);
		let latest_tip = Arc::new(Mutex::new(None));
		let watched_scripts = Arc::new(RwLock::new(Vec::new()));
		let matched_block_hashes = Arc::new(Mutex::new(Vec::new()));
		let sync_completion_tx = Arc::new(Mutex::new(None));
		let filter_skip_height = Arc::new(AtomicU32::new(0));
		let last_onchain_synced_height = Mutex::new(None);
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			peers,
			sync_config,
			cbf_runtime_status,
			latest_tip,
			watched_scripts,
			matched_block_hashes,
			sync_completion_tx,
			filter_skip_height,
			last_onchain_synced_height,
			onchain_wallet_sync_status,
			fee_estimator,
			kv_store,
			config,
			logger,
			node_metrics,
		}
	}

	/// Start the bip157 node and spawn background tasks for event processing.
	pub(crate) fn start(&self, runtime: Arc<Runtime>) {
		let mut status = self.cbf_runtime_status.lock().unwrap();
		if matches!(*status, CbfRuntimeStatus::Started { .. }) {
			debug_assert!(false, "We shouldn't call start if we're already started");
			return;
		}

		let network = self.config.network;

		let mut builder = Builder::new(network);

		// Configure data directory under the node's storage path.
		let data_dir = std::path::PathBuf::from(&self.config.storage_dir_path).join("bip157_data");
		builder = builder.data_dir(data_dir);

		// Add configured peers.
		let peers: Vec<TrustedPeer> = self
			.peers
			.iter()
			.filter_map(|peer_str| {
				peer_str.parse::<SocketAddr>().ok().map(TrustedPeer::from_socket_addr)
			})
			.collect();
		if !peers.is_empty() {
			builder = builder.add_peers(peers);
		}

		// Request witness data so segwit transactions include full witnesses,
		// required for Lightning channel operations.
		builder = builder.fetch_witness_data();

		// Increase peer response timeout from the default 5 seconds to avoid
		// disconnecting slow peers during block downloads.
		builder = builder.response_timeout(Duration::from_secs(30));

		let (node, client) = builder.build();

		let Client { requester, info_rx, warn_rx, event_rx } = client;

		// Spawn the bip157 node in the background.
		runtime.spawn_background_task(async move {
			let _ = node.run().await;
		});

		// Spawn a task to log info messages.
		let info_logger = Arc::clone(&self.logger);
		runtime
			.spawn_cancellable_background_task(Self::process_info_messages(info_rx, info_logger));

		// Spawn a task to log warning messages.
		let warn_logger = Arc::clone(&self.logger);
		runtime
			.spawn_cancellable_background_task(Self::process_warn_messages(warn_rx, warn_logger));

		// Spawn a task to process events.
		let event_state = CbfEventState {
			latest_tip: Arc::clone(&self.latest_tip),
			watched_scripts: Arc::clone(&self.watched_scripts),
			matched_block_hashes: Arc::clone(&self.matched_block_hashes),
			sync_completion_tx: Arc::clone(&self.sync_completion_tx),
			filter_skip_height: Arc::clone(&self.filter_skip_height),
		};
		let event_logger = Arc::clone(&self.logger);
		runtime.spawn_cancellable_background_task(Self::process_events(
			event_rx,
			event_state,
			event_logger,
		));

		log_info!(self.logger, "CBF chain source started.");

		*status = CbfRuntimeStatus::Started { requester };
	}

	/// Shut down the bip157 node and stop all background tasks.
	pub(crate) fn stop(&self) {
		let mut status = self.cbf_runtime_status.lock().unwrap();
		match &*status {
			CbfRuntimeStatus::Started { requester } => {
				let _ = requester.shutdown();
				log_info!(self.logger, "CBF chain source stopped.");
			},
			CbfRuntimeStatus::Stopped => {},
		}
		*status = CbfRuntimeStatus::Stopped;
	}

	async fn process_info_messages(mut info_rx: mpsc::Receiver<Info>, logger: Arc<Logger>) {
		while let Some(info) = info_rx.recv().await {
			log_debug!(logger, "CBF node info: {}", info);
		}
	}

	async fn process_warn_messages(
		mut warn_rx: mpsc::UnboundedReceiver<Warning>, logger: Arc<Logger>,
	) {
		while let Some(warning) = warn_rx.recv().await {
			log_debug!(logger, "CBF node warning: {}", warning);
		}
	}

	async fn process_events(
		mut event_rx: mpsc::UnboundedReceiver<Event>, state: CbfEventState, logger: Arc<Logger>,
	) {
		while let Some(event) = event_rx.recv().await {
			match event {
				Event::FiltersSynced(sync_update) => {
					let tip = sync_update.tip();
					*state.latest_tip.lock().unwrap() = Some(tip.hash);
					log_info!(
						logger,
						"CBF filters synced to tip: height={}, hash={}",
						tip.height,
						tip.hash,
					);
					if let Some(tx) = state.sync_completion_tx.lock().unwrap().take() {
						let _ = tx.send(sync_update);
					}
				},
				Event::Block(indexed_block) => {
					log_trace!(logger, "CBF received block at height {}", indexed_block.height,);
				},
				Event::ChainUpdate(header_changes) => {
					log_debug!(logger, "CBF chain update: {:?}", header_changes);
				},
				Event::IndexedFilter(indexed_filter) => {
					let skip_height = state.filter_skip_height.load(Ordering::Acquire);
					if skip_height > 0 && indexed_filter.height() <= skip_height {
						continue;
					}
					let scripts = state.watched_scripts.read().unwrap();
					if !scripts.is_empty() && indexed_filter.contains_any(scripts.iter()) {
						state
							.matched_block_hashes
							.lock()
							.unwrap()
							.push((indexed_filter.height(), indexed_filter.block_hash()));
					}
					log_trace!(logger, "CBF received filter at height {}", indexed_filter.height(),);
				},
			}
		}
	}

	fn requester(&self) -> Result<Requester, Error> {
		let status = self.cbf_runtime_status.lock().unwrap();
		match &*status {
			CbfRuntimeStatus::Started { requester } => Ok(requester.clone()),
			CbfRuntimeStatus::Stopped => {
				debug_assert!(
					false,
					"We should have started the chain source before using the requester"
				);
				Err(Error::ConnectionFailed)
			},
		}
	}

	/// Run a CBF filter scan: set watched scripts, trigger a rescan, wait for
	/// completion, and return the sync update along with matched block hashes.
	///
	/// When `skip_before_height` is `Some(h)`, filters at or below height `h` are
	/// skipped, making the scan incremental.
	async fn run_filter_scan(
		&self, scripts: Vec<ScriptBuf>, skip_before_height: Option<u32>,
	) -> Result<(SyncUpdate, Vec<(u32, BlockHash)>), Error> {
		let requester = self.requester()?;

		self.filter_skip_height.store(skip_before_height.unwrap_or(0), Ordering::Release);
		self.matched_block_hashes.lock().unwrap().clear();
		*self.watched_scripts.write().unwrap() = scripts;

		let (tx, rx) = oneshot::channel();
		*self.sync_completion_tx.lock().unwrap() = Some(tx);

		requester.rescan().map_err(|e| {
			log_error!(self.logger, "Failed to trigger CBF rescan: {:?}", e);
			Error::WalletOperationFailed
		})?;

		let sync_update = rx.await.map_err(|e| {
			log_error!(self.logger, "CBF sync completion channel dropped: {:?}", e);
			Error::WalletOperationFailed
		})?;

		self.filter_skip_height.store(0, Ordering::Release);
		self.watched_scripts.write().unwrap().clear();
		let matched = std::mem::take(&mut *self.matched_block_hashes.lock().unwrap());

		Ok((sync_update, matched))
	}

	/// Sync the on-chain wallet by scanning compact block filters for relevant transactions.
	pub(crate) async fn sync_onchain_wallet(
		&self, onchain_wallet: Arc<Wallet>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.onchain_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_debug!(self.logger, "On-chain wallet sync already in progress, waiting.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		let res = async {
			let requester = self.requester()?;
			let now = Instant::now();

			let scripts = onchain_wallet.get_spks_for_cbf_sync(BDK_CLIENT_STOP_GAP);
			if scripts.is_empty() {
				log_debug!(self.logger, "No wallet scripts to sync via CBF.");
				return Ok(());
			}

			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(
					self.sync_config.timeouts_config.onchain_wallet_sync_timeout_secs,
				),
				self.sync_onchain_wallet_op(requester, scripts),
			);

			let (tx_update, sync_update) = match timeout_fut.await {
				Ok(res) => res?,
				Err(e) => {
					log_error!(self.logger, "Sync of on-chain wallet timed out: {}", e);
					return Err(Error::WalletOperationTimeout);
				},
			};

			// Build chain checkpoint extending from the wallet's current tip.
			let mut cp = onchain_wallet.latest_checkpoint();
			for (height, header) in sync_update.recent_history() {
				if *height > cp.height() {
					let block_id = BlockId { height: *height, hash: header.block_hash() };
					cp = cp.push(block_id).unwrap_or_else(|old| old);
				}
			}
			let tip = sync_update.tip();
			if tip.height > cp.height() {
				let tip_block_id = BlockId { height: tip.height, hash: tip.hash };
				cp = cp.push(tip_block_id).unwrap_or_else(|old| old);
			}

			let update =
				Update { last_active_indices: BTreeMap::new(), tx_update, chain: Some(cp) };

			onchain_wallet.apply_update(update)?;

			log_debug!(
				self.logger,
				"Sync of on-chain wallet via CBF finished in {}ms.",
				now.elapsed().as_millis()
			);

			update_node_metrics_timestamp(
				&self.node_metrics,
				&*self.kv_store,
				&*self.logger,
				|m, t| {
					m.latest_onchain_wallet_sync_timestamp = t;
				},
			)?;

			Ok(())
		}
		.await;

		self.onchain_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

		res
	}

	async fn sync_onchain_wallet_op(
		&self, requester: Requester, scripts: Vec<ScriptBuf>,
	) -> Result<(TxUpdate<ConfirmationBlockTime>, SyncUpdate), Error> {
		let skip_height = *self.last_onchain_synced_height.lock().unwrap();
		let (sync_update, matched) = self.run_filter_scan(scripts, skip_height).await?;

		log_debug!(
			self.logger,
			"CBF on-chain filter scan complete: {} matching blocks found.",
			matched.len()
		);

		// Fetch matching blocks and include all their transactions.
		// The compact block filter already matched our scripts (covering both
		// created outputs and spent inputs), so we include every transaction
		// from matched blocks and let BDK determine relevance.
		let mut tx_update = TxUpdate::default();
		for (height, block_hash) in &matched {
			let indexed_block = requester.get_block(*block_hash).await.map_err(|e| {
				log_error!(self.logger, "Failed to fetch block {}: {:?}", block_hash, e);
				Error::WalletOperationFailed
			})?;
			let block = indexed_block.block;
			let block_id = BlockId { height: *height, hash: block.header.block_hash() };
			let conf_time =
				ConfirmationBlockTime { block_id, confirmation_time: block.header.time as u64 };
			for tx in &block.txdata {
				let txid = tx.compute_txid();
				tx_update.txs.push(Arc::new(tx.clone()));
				tx_update.anchors.insert((conf_time, txid));
			}
		}

		let tip = sync_update.tip();
		*self.last_onchain_synced_height.lock().unwrap() = Some(tip.height);

		Ok((tx_update, sync_update))
	}

	/// Sync the Lightning wallet by confirming channel transactions via compact block filters.
	pub(crate) async fn sync_lightning_wallet(
		&self, _channel_manager: Arc<ChannelManager>, _chain_monitor: Arc<ChainMonitor>,
		_output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		log_error!(self.logger, "Lightning wallet sync via CBF is not yet implemented.");
		Err(Error::TxSyncFailed)
	}

	/// Estimate fee rates from recent block data.
	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		let requester = self.requester()?;

		let tip_hash = match *self.latest_tip.lock().unwrap() {
			Some(hash) => hash,
			None => {
				log_debug!(self.logger, "No tip available yet for fee rate estimation, skipping.");
				return Ok(());
			},
		};

		let now = Instant::now();

		// Fetch fee rates from the last N blocks for per-target estimation.
		// We compute fee rates ourselves rather than using Requester::average_fee_rate,
		// so we can sample multiple blocks and select percentiles per confirmation target.
		let mut block_fee_rates: Vec<u64> = Vec::with_capacity(FEE_RATE_LOOKBACK_BLOCKS);
		let mut current_hash = tip_hash;

		let timeout = Duration::from_secs(
			self.sync_config.timeouts_config.fee_rate_cache_update_timeout_secs,
		);
		let fetch_start = Instant::now();

		for idx in 0..FEE_RATE_LOOKBACK_BLOCKS {
			// Check if we've exceeded the overall timeout for fee estimation.
			let remaining_timeout = timeout.saturating_sub(fetch_start.elapsed());
			if remaining_timeout.is_zero() {
				log_error!(self.logger, "Updating fee rate estimates timed out.");
				return Err(Error::FeerateEstimationUpdateTimeout);
			}

			// Fetch the block via P2P. On the first iteration, a fetch failure
			// likely means the cached tip is stale (initial sync or reorg), so
			// we clear the tip and skip gracefully instead of returning an error.
			let indexed_block =
				match tokio::time::timeout(remaining_timeout, requester.get_block(current_hash))
					.await
				{
					Ok(Ok(indexed_block)) => indexed_block,
					Ok(Err(e)) if idx == 0 => {
						log_debug!(
							self.logger,
							"Cached CBF tip {} was unavailable during fee estimation, \
							 likely due to initial sync or a reorg: {:?}",
							current_hash,
							e
						);
						*self.latest_tip.lock().unwrap() = None;
						return Ok(());
					},
					Ok(Err(e)) => {
						log_error!(
							self.logger,
							"Failed to fetch block for fee estimation: {:?}",
							e
						);
						return Err(Error::FeerateEstimationUpdateFailed);
					},
					Err(e) if idx == 0 => {
						log_debug!(
							self.logger,
							"Timed out fetching cached CBF tip {} during fee estimation, \
							 likely due to initial sync or a reorg: {}",
							current_hash,
							e
						);
						*self.latest_tip.lock().unwrap() = None;
						return Ok(());
					},
					Err(e) => {
						log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
						return Err(Error::FeerateEstimationUpdateTimeout);
					},
				};

			let height = indexed_block.height;
			let block = &indexed_block.block;
			let weight_kwu = block.weight().to_kwu_floor();

			// Compute fee rate: (coinbase_output - subsidy) / weight.
			// For blocks with zero weight (e.g. coinbase-only in regtest), use the floor rate.
			let fee_rate_sat_per_kwu = if weight_kwu == 0 {
				MIN_FEERATE_SAT_PER_KWU
			} else {
				let subsidy = block_subsidy(height);
				let revenue = block
					.txdata
					.first()
					.map(|tx| tx.output.iter().map(|o| o.value).sum())
					.unwrap_or(Amount::ZERO);
				let block_fees = revenue.checked_sub(subsidy).unwrap_or(Amount::ZERO);

				if block_fees == Amount::ZERO && self.config.network == Network::Bitcoin {
					log_error!(
						self.logger,
						"Failed to retrieve fee rate estimates: zero block fees are disallowed on Mainnet.",
					);
					return Err(Error::FeerateEstimationUpdateFailed);
				}

				(block_fees.to_sat() / weight_kwu).max(MIN_FEERATE_SAT_PER_KWU)
			};

			block_fee_rates.push(fee_rate_sat_per_kwu);
			// Walk backwards through the chain via prev_blockhash.
			if height == 0 {
				break;
			}
			current_hash = block.header.prev_blockhash;
		}

		if block_fee_rates.is_empty() {
			log_error!(self.logger, "No blocks available for fee rate estimation.");
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		block_fee_rates.sort();

		let confirmation_targets = get_all_conf_targets();
		let mut new_fee_rate_cache = HashMap::with_capacity(confirmation_targets.len());

		for target in confirmation_targets {
			let num_blocks = get_num_block_defaults_for_target(target);
			let base_fee_rate = select_fee_rate_for_target(&block_fee_rates, num_blocks);
			let adjusted_fee_rate = apply_post_estimation_adjustments(target, base_fee_rate);
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
			"Fee rate cache update finished in {}ms ({} blocks sampled).",
			now.elapsed().as_millis(),
			block_fee_rates.len(),
		);

		update_node_metrics_timestamp(
			&self.node_metrics,
			&*self.kv_store,
			&*self.logger,
			|m, t| {
				m.latest_fee_rate_cache_update_timestamp = t;
			},
		)?;

		Ok(())
	}

	/// Broadcast a package of transactions via the P2P network.
	pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
		let Ok(requester) = self.requester() else { return };

		for tx in package {
			let txid = tx.compute_txid();
			let tx_bytes = tx.encode();
			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(self.sync_config.timeouts_config.tx_broadcast_timeout_secs),
				requester.broadcast_tx(tx),
			);
			match timeout_fut.await {
				Ok(res) => match res {
					Ok(wtxid) => {
						log_trace!(
							self.logger,
							"Successfully broadcast transaction {} (wtxid: {})",
							txid,
							wtxid
						);
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to broadcast transaction {}: {:?}",
							txid,
							e
						);
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
	}

	/// Register a transaction script for Lightning channel monitoring.
	pub(crate) fn register_tx(&self, _txid: &Txid, _script_pubkey: &Script) {
		log_error!(self.logger, "CBF register_tx is not yet implemented.");
	}

	/// Register a watched output script for Lightning channel monitoring.
	pub(crate) fn register_output(&self, _output: WatchedOutput) {
		log_error!(self.logger, "CBF register_output is not yet implemented.");
	}
}

/// Record the current timestamp in a `NodeMetrics` field and persist the metrics.
fn update_node_metrics_timestamp(
	node_metrics: &RwLock<NodeMetrics>, kv_store: &DynStore, logger: &Logger,
	setter: impl FnOnce(&mut NodeMetrics, Option<u64>),
) -> Result<(), Error> {
	let unix_time_secs_opt = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
	let mut locked = node_metrics.write().unwrap();
	setter(&mut locked, unix_time_secs_opt);
	write_node_metrics(&*locked, kv_store, logger)?;
	Ok(())
}

/// Compute the block subsidy (mining reward before fees) at the given block height.
fn block_subsidy(height: u32) -> Amount {
	let halvings = height / SUBSIDY_HALVING_INTERVAL;
	if halvings >= 64 {
		return Amount::ZERO;
	}
	let base = Amount::ONE_BTC.to_sat() * 50;
	Amount::from_sat(base >> halvings)
}

/// Select a fee rate from sorted block fee rates based on confirmation urgency.
///
/// For urgent targets (1 block), uses the highest observed fee rate.
/// For medium targets (2-6 blocks), uses the 75th percentile.
/// For standard targets (7-12 blocks), uses the median.
/// For low-urgency targets (13+ blocks), uses the 25th percentile.
fn select_fee_rate_for_target(sorted_rates: &[u64], num_blocks: usize) -> FeeRate {
	if sorted_rates.is_empty() {
		return FeeRate::from_sat_per_kwu(MIN_FEERATE_SAT_PER_KWU);
	}

	let len = sorted_rates.len();
	let idx = if num_blocks <= 1 {
		len - 1
	} else if num_blocks <= 6 {
		(len * 3) / 4
	} else if num_blocks <= 12 {
		len / 2
	} else {
		len / 4
	};

	FeeRate::from_sat_per_kwu(sorted_rates[idx.min(len - 1)])
}

#[cfg(test)]
mod tests {
	use bitcoin::constants::SUBSIDY_HALVING_INTERVAL;
	use bitcoin::{Amount, FeeRate};

	use super::{block_subsidy, select_fee_rate_for_target, MIN_FEERATE_SAT_PER_KWU};
	use crate::fee_estimator::{
		apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	};

	#[test]
	fn select_fee_rate_empty_returns_floor() {
		let rate = select_fee_rate_for_target(&[], 1);
		assert_eq!(rate, FeeRate::from_sat_per_kwu(MIN_FEERATE_SAT_PER_KWU));
	}

	#[test]
	fn select_fee_rate_single_element_returns_it_for_all_buckets() {
		let rates = [5000u64];
		// Every urgency bucket should return the single available rate.
		for num_blocks in [1, 3, 6, 12, 144, 1008] {
			let rate = select_fee_rate_for_target(&rates, num_blocks);
			assert_eq!(
				rate,
				FeeRate::from_sat_per_kwu(5000),
				"num_blocks={} should return the only available rate",
				num_blocks,
			);
		}
	}

	#[test]
	fn select_fee_rate_picks_correct_percentile() {
		// 6 sorted rates: indices 0..5
		let rates = [100, 200, 300, 400, 500, 600];
		// 1-block (most urgent): highest → index 5 → 600
		assert_eq!(select_fee_rate_for_target(&rates, 1), FeeRate::from_sat_per_kwu(600));
		// 6-block (medium): 75th percentile → (6*3)/4 = 4 → 500
		assert_eq!(select_fee_rate_for_target(&rates, 6), FeeRate::from_sat_per_kwu(500));
		// 12-block (standard): median → 6/2 = 3 → 400
		assert_eq!(select_fee_rate_for_target(&rates, 12), FeeRate::from_sat_per_kwu(400));
		// 144-block (low): 25th percentile → 6/4 = 1 → 200
		assert_eq!(select_fee_rate_for_target(&rates, 144), FeeRate::from_sat_per_kwu(200));
	}

	#[test]
	fn select_fee_rate_monotonic_urgency() {
		// More urgent targets should never produce lower rates than less urgent ones.
		let rates = [250, 500, 1000, 2000, 4000, 8000];
		let urgent = select_fee_rate_for_target(&rates, 1);
		let medium = select_fee_rate_for_target(&rates, 6);
		let standard = select_fee_rate_for_target(&rates, 12);
		let low = select_fee_rate_for_target(&rates, 144);

		assert!(
			urgent >= medium,
			"urgent ({}) >= medium ({})",
			urgent.to_sat_per_kwu(),
			medium.to_sat_per_kwu()
		);
		assert!(
			medium >= standard,
			"medium ({}) >= standard ({})",
			medium.to_sat_per_kwu(),
			standard.to_sat_per_kwu()
		);
		assert!(
			standard >= low,
			"standard ({}) >= low ({})",
			standard.to_sat_per_kwu(),
			low.to_sat_per_kwu()
		);
	}

	#[test]
	fn uniform_rates_match_naive_single_rate() {
		// When all blocks have the same fee rate (like the old single-block
		// approach), every target should select that same base rate. This
		// proves the optimized multi-block approach is backwards-compatible.

		let uniform_rate = 3000u64;
		let rates = [uniform_rate; 6];
		for target in get_all_conf_targets() {
			let num_blocks = get_num_block_defaults_for_target(target);
			let optimized = select_fee_rate_for_target(&rates, num_blocks);
			let naive = FeeRate::from_sat_per_kwu(uniform_rate);
			assert_eq!(
				optimized, naive,
				"For target {:?} (num_blocks={}), optimized rate should match naive single-rate",
				target, num_blocks,
			);

			// Also verify the post-estimation adjustments produce the same
			// result for both approaches.
			let adjusted_optimized = apply_post_estimation_adjustments(target, optimized);
			let adjusted_naive = apply_post_estimation_adjustments(target, naive);
			assert_eq!(adjusted_optimized, adjusted_naive);
		}
	}

	#[test]
	fn block_subsidy_genesis() {
		assert_eq!(block_subsidy(0), Amount::from_sat(50 * 100_000_000));
	}

	#[test]
	fn block_subsidy_first_halving() {
		assert_eq!(block_subsidy(SUBSIDY_HALVING_INTERVAL), Amount::from_sat(25 * 100_000_000));
	}

	#[test]
	fn block_subsidy_second_halving() {
		assert_eq!(block_subsidy(SUBSIDY_HALVING_INTERVAL * 2), Amount::from_sat(1_250_000_000));
	}

	#[test]
	fn block_subsidy_exhausted_after_64_halvings() {
		assert_eq!(block_subsidy(SUBSIDY_HALVING_INTERVAL * 64), Amount::ZERO);
		assert_eq!(block_subsidy(SUBSIDY_HALVING_INTERVAL * 100), Amount::ZERO);
	}

	#[test]
	fn select_fee_rate_two_elements() {
		let rates = [1000, 5000];
		// 1-block: index 1 (highest) → 5000
		assert_eq!(select_fee_rate_for_target(&rates, 1), FeeRate::from_sat_per_kwu(5000));
		// 6-block: (2*3)/4 = 1 → 5000
		assert_eq!(select_fee_rate_for_target(&rates, 6), FeeRate::from_sat_per_kwu(5000));
		// 12-block: 2/2 = 1 → 5000
		assert_eq!(select_fee_rate_for_target(&rates, 12), FeeRate::from_sat_per_kwu(5000));
		// 144-block: 2/4 = 0 → 1000
		assert_eq!(select_fee_rate_for_target(&rates, 144), FeeRate::from_sat_per_kwu(1000));
	}

	#[test]
	fn select_fee_rate_all_targets_use_valid_indices() {
		for size in 1..=6 {
			let rates: Vec<u64> = (1..=size).map(|i| i as u64 * 1000).collect();
			for target in get_all_conf_targets() {
				let num_blocks = get_num_block_defaults_for_target(target);
				let _ = select_fee_rate_for_target(&rates, num_blocks);
			}
		}
	}
}
