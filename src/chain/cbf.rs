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
use bip157::chain::{BlockHeaderChanges, ChainState};
use bip157::error::FetchBlockError;
use bip157::{
	BlockHash, Builder, Client, Event, HeaderCheckpoint, Info, Node as CbfNode, Requester,
	SyncUpdate, TrustedPeer, Warning,
};
use bitcoin::constants::SUBSIDY_HALVING_INTERVAL;
use bitcoin::{Amount, FeeRate, Network, Script, ScriptBuf, Transaction, Txid};
use electrum_client::ElectrumApi;
use lightning::chain::{Confirm, WatchedOutput};
use lightning::util::ser::Writeable;
use tokio::sync::{mpsc, oneshot};

use super::{FeeSourceConfig, WalletSyncStatus};
use crate::config::{CbfSyncConfig, Config, BDK_CLIENT_STOP_GAP};
use crate::error::Error;
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::io::utils::update_and_persist_node_metrics;
use crate::logger::{log_bytes, log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::NodeMetrics;

/// Minimum fee rate: 1 sat/vB = 250 sat/kWU. Used as a floor for computed fee rates.
const MIN_FEERATE_SAT_PER_KWU: u64 = 250;

/// Number of recent blocks to look back for per-target fee rate estimation.
const FEE_RATE_LOOKBACK_BLOCKS: usize = 6;

/// Number of blocks to walk back from a component's persisted best block height
/// for reorg safety when computing the incremental scan skip height.
/// Matches bdk-kyoto's `IMPOSSIBLE_REORG_DEPTH`.
const REORG_SAFETY_BLOCKS: u32 = 7;

/// Maximum consecutive restart attempts before giving up.
const MAX_RESTART_RETRIES: u32 = 5;

/// Initial backoff delay for restart retries (doubles each attempt).
const INITIAL_BACKOFF_MS: u64 = 500;

/// The fee estimation back-end used by the CBF chain source.
enum FeeSource {
	/// Derive fee rates from the coinbase reward of recent blocks.
	///
	/// Provides a per-target rate using percentile selection across multiple blocks.
	/// Less accurate than a mempool-aware source but requires no extra connectivity.
	Cbf,
	/// Delegate fee estimation to an Esplora HTTP server.
	Esplora { client: esplora_client::AsyncClient },
	/// Delegate fee estimation to an Electrum server.
	///
	/// A fresh connection is opened for each estimation cycle because `ElectrumClient`
	/// is not `Sync`.
	Electrum { server_url: String },
}

pub(super) struct CbfChainSource {
	/// Peer addresses for sourcing compact block filters via P2P.
	peers: Vec<String>,
	/// User-provided sync configuration (timeouts, background sync intervals).
	pub(super) sync_config: CbfSyncConfig,
	/// Fee estimation back-end.
	fee_source: FeeSource,
	/// Tracks whether the bip157 node is running and holds the command handle.
	cbf_runtime_status: Arc<Mutex<CbfRuntimeStatus>>,
	/// Scripts to match against compact block filters during a scan.
	watched_scripts: Arc<RwLock<Vec<ScriptBuf>>>,
	/// Block (height, hash) pairs where filters matched watched scripts.
	matched_block_hashes: Arc<Mutex<Vec<(u32, BlockHash)>>>,
	/// One-shot channel sender to signal filter scan completion.
	sync_completion_tx: Arc<Mutex<Option<oneshot::Sender<SyncUpdate>>>>,
	/// Filters at or below this height are skipped during incremental scans.
	filter_skip_height: Arc<AtomicU32>,
	/// Serializes concurrent filter scans (on-chain and lightning).
	scan_lock: tokio::sync::Mutex<()>,
	/// Scripts registered by LDK's Filter trait for lightning channel monitoring.
	registered_scripts: Mutex<Vec<ScriptBuf>>,
	/// Deduplicates concurrent on-chain wallet sync requests.
	onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
	/// Deduplicates concurrent lightning wallet sync requests.
	lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
	/// Shared fee rate estimator, updated by this chain source.
	fee_estimator: Arc<OnchainFeeEstimator>,
	/// Persistent key-value store for node metrics.
	kv_store: Arc<DynStore>,
	/// Node configuration (network, storage path, etc.).
	config: Arc<Config>,
	/// On-chain wallet reference for deriving chain_state checkpoints on restart.
	onchain_wallet: Mutex<Option<Arc<Wallet>>>,
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
	watched_scripts: Arc<RwLock<Vec<ScriptBuf>>>,
	matched_block_hashes: Arc<Mutex<Vec<(u32, BlockHash)>>>,
	sync_completion_tx: Arc<Mutex<Option<oneshot::Sender<SyncUpdate>>>>,
	filter_skip_height: Arc<AtomicU32>,
}

impl CbfChainSource {
	pub(crate) fn new(
		peers: Vec<String>, sync_config: CbfSyncConfig, fee_source_config: Option<FeeSourceConfig>,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Result<Self, Error> {
		let fee_source = match fee_source_config {
			Some(FeeSourceConfig::Esplora(server_url)) => {
				let timeout = sync_config.timeouts_config.per_request_timeout_secs;
				let mut builder = esplora_client::Builder::new(&server_url);
				builder = builder.timeout(timeout as u64);
				let client = builder.build_async().map_err(|e| {
					log_error!(logger, "Failed to build esplora client: {}", e);
					Error::ConnectionFailed
				})?;
				FeeSource::Esplora { client }
			},
			Some(FeeSourceConfig::Electrum(server_url)) => FeeSource::Electrum { server_url },
			None => FeeSource::Cbf,
		};

		let cbf_runtime_status = Arc::new(Mutex::new(CbfRuntimeStatus::Stopped));
		let watched_scripts = Arc::new(RwLock::new(Vec::new()));
		let matched_block_hashes = Arc::new(Mutex::new(Vec::new()));
		let sync_completion_tx = Arc::new(Mutex::new(None));
		let filter_skip_height = Arc::new(AtomicU32::new(0));
		let registered_scripts = Mutex::new(Vec::new());
		let scan_lock = tokio::sync::Mutex::new(());
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let onchain_wallet = Mutex::new(None);
		Ok(Self {
			peers,
			sync_config,
			fee_source,
			cbf_runtime_status,
			watched_scripts,
			matched_block_hashes,
			sync_completion_tx,
			filter_skip_height,
			registered_scripts,
			scan_lock,
			onchain_wallet_sync_status,
			lightning_wallet_sync_status,
			fee_estimator,
			onchain_wallet,
			kv_store,
			config,
			logger,
			node_metrics,
		})
	}

	/// Build a new bip157 node and client from the current configuration.
	///
	/// Delegates to [`Self::build_cbf_node_static`], passing all needed fields.
	fn build_cbf_node(&self) -> (CbfNode, Client) {
		let wallet = self.onchain_wallet.lock().unwrap().clone();
		Self::build_cbf_node_static(
			&self.peers,
			&self.sync_config,
			&self.config,
			wallet.as_ref(),
			&self.logger,
		)
	}

	/// Static version of the builder: takes all required parameters explicitly
	/// so it can be called from an `async move` block without borrowing `self`.
	fn build_cbf_node_static(
		peers: &[String], sync_config: &CbfSyncConfig, config: &Config,
		wallet: Option<&Arc<Wallet>>, logger: &Logger,
	) -> (CbfNode, Client) {
		let network = config.network;

		let mut builder = Builder::new(network);

		// Configure data directory under the node's storage path.
		let data_dir = std::path::PathBuf::from(&config.storage_dir_path).join("bip157_data");
		builder = builder.data_dir(data_dir);

		// Add configured peers.
		let trusted_peers: Vec<TrustedPeer> = peers
			.iter()
			.filter_map(|peer_str| {
				peer_str.parse::<SocketAddr>().ok().map(TrustedPeer::from_socket_addr)
			})
			.collect();
		if !trusted_peers.is_empty() {
			builder = builder.add_peers(trusted_peers);
		}

		// Require multiple peers to agree on filter headers before accepting them,
		// as recommended by BIP 157 to mitigate malicious peer attacks.
		builder = builder.required_peers(sync_config.required_peers);

		// Request witness data so segwit transactions include full witnesses,
		// required for Lightning channel operations.
		builder = builder.fetch_witness_data();

		// Set peer response timeout from user configuration (default: 30s).
		builder = builder.response_timeout(Duration::from_secs(sync_config.response_timeout_secs));

		// If we have a wallet reference, derive a chain_state checkpoint so the
		// bip157 node can skip already-synced headers on restart.
		if let Some(wallet) = wallet {
			let cp = wallet.latest_checkpoint();
			let target_height = cp.height().saturating_sub(REORG_SAFETY_BLOCKS);
			// Walk the checkpoint chain back to the target height.
			let mut cursor = cp;
			while cursor.height() > target_height {
				match cursor.prev() {
					Some(prev) => cursor = prev,
					None => break,
				}
			}
			if cursor.height() > 0 {
				let header_cp = HeaderCheckpoint::new(cursor.height(), cursor.hash());
				builder = builder.chain_state(ChainState::Checkpoint(header_cp));
				log_debug!(
					logger,
					"CBF builder: resuming from checkpoint height={}, hash={}",
					cursor.height(),
					cursor.hash(),
				);
			}
		}

		builder.build()
	}

	/// Start the bip157 node and spawn background tasks for event processing.
	///
	/// The node runs inside a restart loop: if `node.run()` returns an error,
	/// the loop rebuilds the node, swaps the requester, and respawns channel
	/// processing tasks — up to [`MAX_RESTART_RETRIES`] consecutive failures
	/// with exponential backoff starting at [`INITIAL_BACKOFF_MS`].
	pub(crate) fn start(&self, runtime: Arc<Runtime>, onchain_wallet: Arc<Wallet>) {
		let mut status = self.cbf_runtime_status.lock().unwrap();
		if matches!(*status, CbfRuntimeStatus::Started { .. }) {
			debug_assert!(false, "We shouldn't call start if we're already started");
			return;
		}

		// Store the wallet reference for future restarts.
		*self.onchain_wallet.lock().unwrap() = Some(Arc::clone(&onchain_wallet));

		let (node, client) = self.build_cbf_node();

		let Client { requester, info_rx, warn_rx, event_rx } = client;

		*status = CbfRuntimeStatus::Started { requester };
		drop(status);

		log_info!(self.logger, "CBF chain source started.");

		// Clone all Arc references needed by the restart loop so the async
		// block is 'static (no borrows of `self`).
		let restart_status = Arc::clone(&self.cbf_runtime_status);
		let restart_logger = Arc::clone(&self.logger);
		let restart_watched_scripts = Arc::clone(&self.watched_scripts);
		let restart_matched_block_hashes = Arc::clone(&self.matched_block_hashes);
		let restart_sync_completion_tx = Arc::clone(&self.sync_completion_tx);
		let restart_filter_skip_height = Arc::clone(&self.filter_skip_height);
		let restart_peers = self.peers.clone();
		let restart_sync_config = self.sync_config.clone();
		let restart_config = Arc::clone(&self.config);
		let restart_wallet = Arc::clone(&onchain_wallet);

		runtime.spawn_background_task(async move {
			let mut current_node = node;
			let mut current_info_rx = info_rx;
			let mut current_warn_rx = warn_rx;
			let mut current_event_rx = event_rx;
			let mut retries = 0u32;
			let mut backoff_ms = INITIAL_BACKOFF_MS;

			loop {
				// Spawn channel processing tasks for this iteration.
				let info_handle = tokio::spawn(Self::process_info_messages(
					current_info_rx,
					Arc::clone(&restart_logger),
				));
				let warn_handle = tokio::spawn(Self::process_warn_messages(
					current_warn_rx,
					Arc::clone(&restart_logger),
				));
				let event_state = CbfEventState {
					watched_scripts: Arc::clone(&restart_watched_scripts),
					matched_block_hashes: Arc::clone(&restart_matched_block_hashes),
					sync_completion_tx: Arc::clone(&restart_sync_completion_tx),
					filter_skip_height: Arc::clone(&restart_filter_skip_height),
				};
				let event_handle = tokio::spawn(Self::process_events(
					current_event_rx,
					event_state,
					Arc::clone(&restart_logger),
				));

				// Run the node until it exits.
				match current_node.run().await {
					Ok(()) => {
						log_info!(restart_logger, "CBF node shut down cleanly.");
						break;
					},
					Err(e) => {
						retries += 1;
						if retries > MAX_RESTART_RETRIES {
							log_error!(
								restart_logger,
								"CBF node failed {} times, giving up: {:?}",
								retries,
								e,
							);
							*restart_status.lock().unwrap() = CbfRuntimeStatus::Stopped;
							break;
						}
						log_error!(
							restart_logger,
							"CBF node exited with error (attempt {}/{}): {:?}. \
							 Restarting in {}ms.",
							retries,
							MAX_RESTART_RETRIES,
							e,
							backoff_ms,
						);

						tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
						backoff_ms = backoff_ms.saturating_mul(2);

						// Abort old channel processing tasks.
						info_handle.abort();
						warn_handle.abort();
						event_handle.abort();

						// Rebuild the node from scratch.
						let (new_node, new_client) = Self::build_cbf_node_static(
							&restart_peers,
							&restart_sync_config,
							&restart_config,
							Some(&restart_wallet),
							&restart_logger,
						);
						let Client {
							requester: new_requester,
							info_rx: new_info_rx,
							warn_rx: new_warn_rx,
							event_rx: new_event_rx,
						} = new_client;

						// Swap the requester so callers pick up the new handle.
						*restart_status.lock().unwrap() =
							CbfRuntimeStatus::Started { requester: new_requester };

						current_node = new_node;
						current_info_rx = new_info_rx;
						current_warn_rx = new_warn_rx;
						current_event_rx = new_event_rx;
					},
				}
			}
		});
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
				Event::Block(_) => {},
				Event::ChainUpdate(header_changes) => match header_changes {
					BlockHeaderChanges::Reorganized { accepted, reorganized } => {
						log_debug!(
							logger,
							"CBF chain reorg detected: {} blocks removed, {} blocks accepted.",
							reorganized.len(),
							accepted.len(),
						);

						// No height reset needed: skip heights are derived from
						// BDK's checkpoint (on-chain) and LDK's best block
						// (lightning), both walked back by REORG_SAFETY_BLOCKS.
					},
					BlockHeaderChanges::Connected(header) => {
						log_trace!(logger, "CBF block connected at height {}", header.height,);
					},
					BlockHeaderChanges::ForkAdded(header) => {
						log_trace!(logger, "CBF fork block observed at height {}", header.height,);
					},
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
			CbfRuntimeStatus::Started { requester } if requester.is_running() => {
				Ok(requester.clone())
			},
			CbfRuntimeStatus::Started { .. } => {
				log_error!(
					self.logger,
					"CBF node is not running; sync will fail until restart completes."
				);
				Err(Error::ConnectionFailed)
			},
			CbfRuntimeStatus::Stopped => {
				debug_assert!(
					false,
					"We should have started the chain source before using the requester"
				);
				Err(Error::ConnectionFailed)
			},
		}
	}

	/// Reset filter scan state to a clean baseline.
	///
	/// Called on error paths in `run_filter_scan()` to ensure no stale state
	/// leaks between scans. The success path performs inline cleanup instead.
	fn cleanup_scan_state(&self) {
		self.filter_skip_height.store(0, Ordering::Release);
		self.watched_scripts.write().unwrap().clear();
		self.matched_block_hashes.lock().unwrap().clear();
		if let Some(tx) = self.sync_completion_tx.lock().unwrap().take() {
			drop(tx);
		}
	}

	/// Register a transaction script for Lightning channel monitoring.
	pub(crate) fn register_tx(&self, _txid: &Txid, script_pubkey: &Script) {
		self.registered_scripts.lock().unwrap().push(script_pubkey.to_owned());
	}

	/// Register a watched output script for Lightning channel monitoring.
	pub(crate) fn register_output(&self, output: WatchedOutput) {
		self.registered_scripts.lock().unwrap().push(output.script_pubkey.clone());
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

		let _scan_guard = self.scan_lock.lock().await;

		self.filter_skip_height.store(skip_before_height.unwrap_or(0), Ordering::Release);
		self.matched_block_hashes.lock().unwrap().clear();
		*self.watched_scripts.write().unwrap() = scripts;

		let (tx, rx) = oneshot::channel();
		*self.sync_completion_tx.lock().unwrap() = Some(tx);

		if let Err(e) = requester.rescan().map_err(|e| {
			log_error!(self.logger, "Failed to trigger CBF rescan: {:?}", e);
			Error::WalletOperationFailed
		}) {
			self.cleanup_scan_state();
			return Err(e);
		}

		match rx.await {
			Ok(sync_update) => {
				self.filter_skip_height.store(0, Ordering::Release);
				self.watched_scripts.write().unwrap().clear();
				let matched = std::mem::take(&mut *self.matched_block_hashes.lock().unwrap());
				Ok((sync_update, matched))
			},
			Err(e) => {
				log_error!(self.logger, "CBF sync completion channel dropped: {:?}", e);
				self.cleanup_scan_state();
				Err(Error::WalletOperationFailed)
			},
		}
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
				self.sync_onchain_wallet_op(requester, &onchain_wallet, scripts),
			);

			let (tx_update, sync_update) = match timeout_fut.await {
				Ok(res) => res?,
				Err(e) => {
					log_error!(self.logger, "Sync of on-chain wallet timed out: {}", e);
					return Err(Error::WalletOperationTimeout);
				},
			};

			// Build chain checkpoint extending from the wallet's current tip,
			// using `insert` (not `push`) so that reorgs are handled correctly.
			// `insert` detects conflicting hashes and purges stale blocks,
			// matching bdk-kyoto's approach in `UpdateBuilder::apply_chain_event`.
			let mut cp = onchain_wallet.latest_checkpoint();
			for (height, header) in sync_update.recent_history() {
				let block_id = BlockId { height: *height, hash: header.block_hash() };
				cp = cp.insert(block_id);
			}
			let tip = sync_update.tip();
			let tip_block_id = BlockId { height: tip.height, hash: tip.hash };
			cp = cp.insert(tip_block_id);

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
		&self, requester: Requester, onchain_wallet: &Wallet, scripts: Vec<ScriptBuf>,
	) -> Result<(TxUpdate<ConfirmationBlockTime>, SyncUpdate), Error> {
		// Derive skip height from BDK's persisted checkpoint, walked back by
		// REORG_SAFETY_BLOCKS for reorg safety (same approach as bdk-kyoto).
		// This survives restarts since BDK persists its checkpoint chain.
		//
		// We include LDK-registered scripts (e.g., channel funding output
		// scripts) alongside the wallet scripts. This ensures the on-chain
		// wallet scan also fetches blocks containing channel funding
		// transactions, whose outputs are needed by BDK's TxGraph to
		// calculate fees for subsequent spends such as splice transactions.
		// Without these, BDK's `calculate_fee` would fail with
		// `MissingTxOut` because the parent transaction's outputs are
		// unknown. This mirrors what the Bitcoind chain source does in
		// `Wallet::block_connected` by inserting registered tx outputs.
		let mut all_scripts = scripts;
		all_scripts.extend(self.registered_scripts.lock().unwrap().iter().cloned());
		let skip_height =
			onchain_wallet.latest_checkpoint().height().checked_sub(REORG_SAFETY_BLOCKS);
		let (sync_update, matched) = self.run_filter_scan(all_scripts, skip_height).await?;

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
		let per_request_timeout =
			Duration::from_secs(self.sync_config.timeouts_config.per_request_timeout_secs.into());
		for (height, block_hash) in &matched {
			let indexed_block =
				tokio::time::timeout(per_request_timeout, requester.get_block(*block_hash))
					.await
					.map_err(|_| {
						log_error!(self.logger, "Timed out fetching block {}", block_hash);
						Error::WalletOperationFailed
					})?
					.map_err(|e| {
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
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.lightning_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_debug!(self.logger, "Lightning wallet sync already in progress, waiting.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::TxSyncFailed
			})?;
		}

		let res = async {
			let requester = self.requester()?;
			let now = Instant::now();

			let scripts: Vec<ScriptBuf> = self.registered_scripts.lock().unwrap().clone();
			if scripts.is_empty() {
				log_debug!(self.logger, "No registered scripts for CBF lightning sync.");
				return Ok(());
			}

			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(
					self.sync_config.timeouts_config.lightning_wallet_sync_timeout_secs,
				),
				self.sync_lightning_wallet_op(
					requester,
					channel_manager,
					chain_monitor,
					output_sweeper,
					scripts,
				),
			);

			match timeout_fut.await {
				Ok(res) => res?,
				Err(e) => {
					log_error!(self.logger, "Sync of Lightning wallet timed out: {}", e);
					return Err(Error::TxSyncTimeout);
				},
			};

			log_debug!(
				self.logger,
				"Sync of Lightning wallet via CBF finished in {}ms.",
				now.elapsed().as_millis()
			);

			update_node_metrics_timestamp(
				&self.node_metrics,
				&*self.kv_store,
				&*self.logger,
				|m, t| {
					m.latest_lightning_wallet_sync_timestamp = t;
				},
			)?;

			Ok(())
		}
		.await;

		self.lightning_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

		res
	}

	async fn sync_lightning_wallet_op(
		&self, requester: Requester, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>, scripts: Vec<ScriptBuf>,
	) -> Result<(), Error> {
		let skip_height =
			channel_manager.current_best_block().height.checked_sub(REORG_SAFETY_BLOCKS);
		let (sync_update, matched) = self.run_filter_scan(scripts, skip_height).await?;

		log_debug!(
			self.logger,
			"CBF lightning filter scan complete: {} matching blocks found.",
			matched.len()
		);

		let confirmables: Vec<&(dyn Confirm + Sync + Send)> =
			vec![&*channel_manager, &*chain_monitor, &*output_sweeper];

		// Fetch matching blocks and confirm all their transactions.
		// The compact block filter already matched our scripts (covering both
		// created outputs and spent inputs), so we confirm every transaction
		// from matched blocks and let LDK determine relevance.
		let per_request_timeout =
			Duration::from_secs(self.sync_config.timeouts_config.per_request_timeout_secs.into());
		for (height, block_hash) in &matched {
			confirm_block_transactions(
				&requester,
				*block_hash,
				*height,
				&confirmables,
				per_request_timeout,
				&self.logger,
			)
			.await?;
		}

		// Update the best block tip.
		let tip = sync_update.tip();
		if let Some(tip_header) = sync_update.recent_history().get(&tip.height) {
			for confirmable in &confirmables {
				confirmable.best_block_updated(tip_header, tip.height);
			}
		}

		Ok(())
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		let new_fee_rate_cache = match &self.fee_source {
			FeeSource::Cbf => self.fee_rate_cache_from_cbf().await?,
			FeeSource::Esplora { client } => Some(self.fee_rate_cache_from_esplora(client).await?),
			FeeSource::Electrum { server_url } => {
				Some(self.fee_rate_cache_from_electrum(server_url).await?)
			},
		};

		let Some(new_fee_rate_cache) = new_fee_rate_cache else {
			return Ok(());
		};

		self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache);

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

	/// Derive per-target fee rates from recent blocks' coinbase outputs.
	///
	/// Returns `Ok(None)` when the chain is too short to sample `FEE_RATE_LOOKBACK_BLOCKS`
	/// blocks (e.g. kyoto has not yet synced past the genesis region).
	async fn fee_rate_cache_from_cbf(
		&self,
	) -> Result<Option<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>>, Error> {
		let requester = self.requester()?;

		let timeout = Duration::from_secs(
			self.sync_config.timeouts_config.fee_rate_cache_update_timeout_secs,
		);
		let fetch_start = Instant::now();

		// Ask kyoto for its current chain tip rather than maintaining a mirrored
		// cache: the returned hash is always fresh (post-reorg, post-restart),
		// so no defensive invalidation is needed below.
		let tip = match tokio::time::timeout(timeout, requester.chain_tip()).await {
			Ok(Ok(tip)) => tip,
			Ok(Err(e)) => {
				log_debug!(
					self.logger,
					"Failed to fetch CBF chain tip for fee estimation: {:?}",
					e,
				);
				return Ok(None);
			},
			Err(e) => {
				log_error!(self.logger, "Timed out fetching CBF chain tip: {}", e);
				return Err(Error::FeerateEstimationUpdateTimeout);
			},
		};

		if (tip.height as usize) < FEE_RATE_LOOKBACK_BLOCKS {
			log_debug!(
				self.logger,
				"CBF chain tip at height {} is below the {}-block lookback window, \
				 skipping fee estimation.",
				tip.height,
				FEE_RATE_LOOKBACK_BLOCKS,
			);
			return Ok(None);
		}

		let now = Instant::now();

		// Fetch fee rates from the last N blocks for per-target estimation.
		// We compute fee rates ourselves rather than using Requester::average_fee_rate,
		// so we can sample multiple blocks and select percentiles per confirmation target.
		let mut block_fee_rates: Vec<u64> = Vec::with_capacity(FEE_RATE_LOOKBACK_BLOCKS);
		let mut current_hash = tip.hash;

		for _ in 0..FEE_RATE_LOOKBACK_BLOCKS {
			// Check if we've exceeded the overall timeout for fee estimation.
			let remaining_timeout = timeout.saturating_sub(fetch_start.elapsed());
			if remaining_timeout.is_zero() {
				log_error!(self.logger, "Updating fee rate estimates timed out.");
				return Err(Error::FeerateEstimationUpdateTimeout);
			}

			let indexed_block =
				match tokio::time::timeout(remaining_timeout, requester.get_block(current_hash))
					.await
				{
					Ok(Ok(indexed_block)) => indexed_block,
					Ok(Err(FetchBlockError::UnknownHash)) => {
						// Kyoto doesn't know this block yet (e.g. startup before
						// filter sync, or hash is at/below the resume checkpoint).
						// Skip this cycle and try again later.
						log_debug!(
							self.logger,
							"CBF node does not yet have block {} for fee estimation; \
							 skipping until sync progresses.",
							current_hash,
						);
						return Ok(None);
					},
					Ok(Err(e)) => {
						log_error!(
							self.logger,
							"Failed to fetch block for fee estimation: {:?}",
							e
						);
						return Err(Error::FeerateEstimationUpdateFailed);
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

		log_debug!(
			self.logger,
			"CBF fee rate estimation finished in {}ms ({} blocks sampled).",
			now.elapsed().as_millis(),
			block_fee_rates.len(),
		);

		Ok(Some(new_fee_rate_cache))
	}

	/// Fetch per-target fee rates from an Esplora server.
	async fn fee_rate_cache_from_esplora(
		&self, client: &esplora_client::AsyncClient,
	) -> Result<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>, Error> {
		let timeout = Duration::from_secs(
			self.sync_config.timeouts_config.fee_rate_cache_update_timeout_secs,
		);
		let estimates = tokio::time::timeout(timeout, client.get_fee_estimates())
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
			log_error!(
				self.logger,
				"Failed to retrieve fee rate estimates: empty estimates are disallowed on Mainnet.",
			);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let confirmation_targets = get_all_conf_targets();
		let mut new_fee_rate_cache = HashMap::with_capacity(confirmation_targets.len());
		for target in confirmation_targets {
			let num_blocks = get_num_block_defaults_for_target(target);
			let converted_estimate_sat_vb =
				esplora_client::convert_fee_rate(num_blocks, estimates.clone())
					.map_or(1.0, |converted| converted.max(1.0));
			let fee_rate = FeeRate::from_sat_per_kwu((converted_estimate_sat_vb * 250.0) as u64);
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

	/// Fetch per-target fee rates from an Electrum server.
	///
	/// Opens a fresh connection for each call because `ElectrumClient` is not `Sync`.
	async fn fee_rate_cache_from_electrum(
		&self, server_url: &str,
	) -> Result<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>, Error> {
		let server_url = server_url.to_owned();
		let confirmation_targets = get_all_conf_targets();
		let per_request_timeout = self.sync_config.timeouts_config.per_request_timeout_secs;

		let raw_estimates: Vec<serde_json::Value> = tokio::time::timeout(
			Duration::from_secs(
				self.sync_config.timeouts_config.fee_rate_cache_update_timeout_secs,
			),
			tokio::task::spawn_blocking(move || {
				let electrum_config = electrum_client::ConfigBuilder::new()
					.retry(3)
					.timeout(Some(per_request_timeout))
					.build();
				let client = electrum_client::Client::from_config(&server_url, electrum_config)
					.map_err(|_| Error::FeerateEstimationUpdateFailed)?;
				let mut batch = electrum_client::Batch::default();
				for target in confirmation_targets {
					batch.estimate_fee(get_num_block_defaults_for_target(target));
				}
				client.batch_call(&batch).map_err(|_| Error::FeerateEstimationUpdateFailed)
			}),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
			Error::FeerateEstimationUpdateTimeout
		})?
		.map_err(|_| Error::FeerateEstimationUpdateFailed)? // JoinError
		?; // inner Result

		let confirmation_targets = get_all_conf_targets();

		if raw_estimates.len() != confirmation_targets.len()
			&& self.config.network == Network::Bitcoin
		{
			log_error!(
				self.logger,
				"Failed to retrieve fee rate estimates: Electrum server didn't return all expected results.",
			);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let mut new_fee_rate_cache = HashMap::with_capacity(confirmation_targets.len());
		for (target, raw_rate) in confirmation_targets.into_iter().zip(raw_estimates.into_iter()) {
			// Electrum returns BTC/KvB; fall back to 1 sat/vb (= 0.00001 BTC/KvB) on failure.
			let fee_rate_btc_per_kvb =
				raw_rate.as_f64().map_or(0.00001_f64, |v: f64| v.max(0.00001));
			// Convert BTC/KvB → sat/kwu: multiply by 25_000_000 (= 10^8 / 4).
			let fee_rate =
				FeeRate::from_sat_per_kwu((fee_rate_btc_per_kvb * 25_000_000.0).round() as u64);
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
}

/// Record the current timestamp in a `NodeMetrics` field and persist the metrics.
fn update_node_metrics_timestamp(
	node_metrics: &RwLock<NodeMetrics>, kv_store: &DynStore, logger: &Logger,
	setter: impl FnOnce(&mut NodeMetrics, Option<u64>),
) -> Result<(), Error> {
	let unix_time_secs_opt = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
	update_and_persist_node_metrics(node_metrics, kv_store, logger, |metrics| {
		setter(metrics, unix_time_secs_opt);
	})
}

/// Fetch a block by hash and call `transactions_confirmed` on each confirmable.
async fn confirm_block_transactions(
	requester: &Requester, block_hash: BlockHash, height: u32,
	confirmables: &[&(dyn Confirm + Sync + Send)], per_request_timeout: Duration, logger: &Logger,
) -> Result<(), Error> {
	let indexed_block = tokio::time::timeout(per_request_timeout, requester.get_block(block_hash))
		.await
		.map_err(|_| {
			log_error!(logger, "Timed out fetching block {}", block_hash);
			Error::TxSyncFailed
		})?
		.map_err(|e| {
			log_error!(logger, "Failed to fetch block {}: {:?}", block_hash, e);
			Error::TxSyncFailed
		})?;
	let block = &indexed_block.block;
	let header = &block.header;
	let txdata: Vec<(usize, &Transaction)> = block.txdata.iter().enumerate().collect();
	if !txdata.is_empty() {
		for confirmable in confirmables {
			confirmable.transactions_confirmed(header, &txdata, height);
		}
	}
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

	/// Test that checkpoint building from `recent_history` handles reorgs.
	///
	/// Scenario: wallet synced to height 103. A 3-block reorg replaces blocks
	/// 101-103 with new ones (same tip height). `recent_history` returns
	/// {94..=103} (last 10 blocks ending at tip) with new hashes at 101-103.
	///
	/// The checkpoint must reflect the reorged chain: new hashes at 101-103,
	/// pre-reorg blocks at ≤100 preserved.
	#[test]
	fn checkpoint_building_handles_reorg() {
		use bdk_chain::local_chain::LocalChain;
		use bdk_chain::{BlockId, CheckPoint};
		use bitcoin::BlockHash;
		use std::collections::BTreeMap;

		fn hash(seed: u32) -> BlockHash {
			use bitcoin::hashes::{sha256d, Hash, HashEngine};
			let mut engine = sha256d::Hash::engine();
			engine.input(&seed.to_le_bytes());
			BlockHash::from_raw_hash(sha256d::Hash::from_engine(engine))
		}

		let genesis = BlockId { height: 0, hash: hash(0) };

		// Wallet checkpoint: 0 → 100 → 101 → 102 → 103
		let wallet_cp = CheckPoint::from_block_ids([
			genesis,
			BlockId { height: 100, hash: hash(100) },
			BlockId { height: 101, hash: hash(101) },
			BlockId { height: 102, hash: hash(102) },
			BlockId { height: 103, hash: hash(103) },
		])
		.unwrap();

		// recent_history after reorg: 94-103, heights 101-103 have NEW hashes.
		let recent_history: BTreeMap<u32, BlockHash> = (94..=103)
			.map(|h| {
				let seed = if (101..=103).contains(&h) { h + 1000 } else { h };
				(h, hash(seed))
			})
			.collect();

		// Build checkpoint using the same logic as sync_onchain_wallet.
		let mut cp = wallet_cp;
		for (height, block_hash) in &recent_history {
			let block_id = BlockId { height: *height, hash: *block_hash };
			cp = cp.insert(block_id);
		}

		// Reorged blocks must have the NEW hashes.
		assert_eq!(cp.height(), 103);
		assert_eq!(
			cp.get(101).expect("height 101 must exist").hash(),
			hash(1101),
			"block 101 must have the reorged hash"
		);
		assert_eq!(cp.get(102).expect("height 102 must exist").hash(), hash(1102));
		assert_eq!(cp.get(103).expect("height 103 must exist").hash(), hash(1103));

		// Pre-reorg blocks are preserved.
		assert_eq!(cp.get(100).expect("height 100 must exist").hash(), hash(100));

		// The checkpoint must connect cleanly to a LocalChain.
		let (mut chain, _) = LocalChain::from_genesis_hash(genesis.hash);
		chain.apply_update(cp).expect("checkpoint must connect to chain");
	}
}
