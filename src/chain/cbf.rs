// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bdk_chain::{BlockId, ConfirmationBlockTime, TxUpdate};
use bdk_wallet::Update;
use bip157::{
	BlockHash, Builder, Client, Event, Info, Requester, SyncUpdate, TrustedPeer, Warning,
};
use bitcoin::{Script, ScriptBuf, Transaction, Txid};
use lightning::chain::WatchedOutput;
use lightning::util::ser::Writeable;
use tokio::sync::{mpsc, oneshot};

use super::WalletSyncStatus;
use crate::config::{CbfSyncConfig, Config, BDK_CLIENT_STOP_GAP};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

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
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			peers,
			sync_config,
			cbf_runtime_status,
			latest_tip,
			watched_scripts,
			matched_block_hashes,
			sync_completion_tx,
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
	async fn run_filter_scan(
		&self, scripts: Vec<ScriptBuf>,
	) -> Result<(SyncUpdate, Vec<(u32, BlockHash)>), Error> {
		let requester = self.requester()?;

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

		let requester = self.requester()?;
		let now = Instant::now();

		let scripts = onchain_wallet.get_spks_for_cbf_sync(BDK_CLIENT_STOP_GAP);
		if scripts.is_empty() {
			log_debug!(self.logger, "No wallet scripts to sync via CBF.");
			return Ok(());
		}

		let timeout_fut = tokio::time::timeout(
			Duration::from_secs(self.sync_config.timeouts_config.onchain_wallet_sync_timeout_secs),
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

		let update = Update { last_active_indices: BTreeMap::new(), tx_update, chain: Some(cp) };

		// Apply update to wallet.
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

	async fn sync_onchain_wallet_op(
		&self, requester: Requester, scripts: Vec<ScriptBuf>,
	) -> Result<(TxUpdate<ConfirmationBlockTime>, SyncUpdate), Error> {
		let (sync_update, matched) = self.run_filter_scan(scripts).await?;

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
	// NOTE: This is a single-block fee estimation. A multi-block lookback with
	// per-target percentile selection is added later.
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

		let base_fee_rate = tokio::time::timeout(
			Duration::from_secs(
				self.sync_config.timeouts_config.fee_rate_cache_update_timeout_secs,
			),
			requester.average_fee_rate(tip_hash),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
			Error::FeerateEstimationUpdateTimeout
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to retrieve fee rate estimate: {:?}", e);
			Error::FeerateEstimationUpdateFailed
		})?;

		let confirmation_targets = get_all_conf_targets();
		let mut new_fee_rate_cache = HashMap::with_capacity(confirmation_targets.len());

		for target in confirmation_targets {
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
			"Fee rate cache update finished in {}ms.",
			now.elapsed().as_millis()
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
