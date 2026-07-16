use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bip157::chain::ChainState;
use bip157::{
	chain::BlockHeaderChanges, Builder as KyotoBuilder, Client, Event, HashCheckpoint, Header,
	IndexedBlock, Info, Node as KyotoNode, Package, Requester, TrustedPeer, Warning,
};
use bitcoin::{BlockHash, FeeRate, Network, Script, ScriptBuf, Transaction, Txid};
use electrum_client::{Client as ElectrumClient, ConfigBuilder as ElectrumConfigBuilder};
use lightning::chain::{BlockLocator, Listen, WatchedOutput};

use tokio::sync::{mpsc, watch};

use crate::chain::bitcoind::ChainListener;
use crate::chain::electrum::get_electrum_fee_rate_cache_update;
use crate::chain::CbfFeeSourceConfig;
use crate::config::{Config, DEFAULT_FEE_RATE_CACHE_UPDATE_INTERVAL_SECS};
use crate::error::Error;
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_fallback_rate_for_target,
	get_num_block_defaults_for_target, ConfirmationTarget, OnchainFeeEstimator,
};
use crate::io::utils::update_and_persist_node_metrics;
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::DynStore;
use crate::util::{cbf_percentile_for_target, coinbase_fee_rate, percentile_of_sorted};
use crate::wallet::Wallet;
use crate::PersistedNodeMetrics;

/// Walk back this many blocks from the wallet's persisted tip when deriving
/// the kyoto resume checkpoint, so a recent reorg cannot strand the node
/// above the new best chain.
const REORG_SAFETY_BLOCKS: u32 = 7;
const BLOCK_FEE_CACHE_CAPACITY: usize = REORG_SAFETY_BLOCKS as usize * 2;

/// Peer response timeout passed to kyoto's `Builder::response_timeout`.
const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 30;

/// Number of peers that must agree on filter headers before they're accepted.
const DEFAULT_REQUIRED_PEERS: u8 = 1;

/// Maximum consecutive `node.run()` failures before the restart loop gives up.
const MAX_RESTART_RETRIES: u32 = 5;

/// Initial backoff delay between restart attempts; doubles each failure.
const INITIAL_BACKOFF_MS: u64 = 500;

/// Retry matched block downloads before surfacing a CBF sync failure.
const CBF_BLOCK_FETCH_RETRIES: u8 = 3;

const ESPLORA_TIMEOUT: u64 = 2;

/// Retries and per-request timeout for the fresh Electrum connection opened each fee cycle.
const ELECTRUM_FEE_NUM_RETRIES: u8 = 3;
const ELECTRUM_FEE_TIMEOUT_SECS: u64 = 10;

/// Runtime status of the underlying kyoto node.
enum CbfRuntimeStatus {
	Started { requester: Requester },
	Stopped,
}

#[derive(Clone, Copy)]
enum CbfSyncState {
	Active {
		/// Highest tip whose preceding chain updates have been applied to all listeners.
		applied_tip: Option<u32>,
		/// Whether kyoto has reported catching up to the network tip (via `FiltersSynced`) and
		/// the resulting blocks have been applied. `wait_until_synced` blocks until this is set.
		///
		/// This must not be derived from a locally-sampled chain tip: kyoto does not persist, so a
		/// freshly (re)started node's local header chain sits at genesis until it syncs from peers.
		/// Comparing against that would make `wait_until_synced` return before any sync happens.
		synced_to_tip: bool,
	},
	Failed(Error),
}

/// Marks that we are applying a block past the last `FiltersSynced` tip, so a `sync_wallets` call
/// issued after new blocks are mined waits for the next `FiltersSynced` rather than returning on a
/// stale `synced_to_tip`. Only flips (and notifies waiters) when currently set.
///
/// Called both when a new block's filter is received (before it is fetched and applied) and after
/// it is applied, so `synced_to_tip` reflects "behind by an unapplied block" as soon as we learn
/// that block exists, not only once we've finished catching up to it.
fn mark_syncing(sync_state_tx: &watch::Sender<CbfSyncState>) {
	// Copy the current state out and drop the `watch` read guard before calling `send_replace`:
	// `borrow()` holds a read lock for the lifetime of its temporary, and `send_replace` takes
	// the write lock, so holding the borrow across it deadlocks. `CbfSyncState` is `Copy`, so the
	// deref copies and the guard is released at the end of this statement.
	let current = *sync_state_tx.borrow();
	if let CbfSyncState::Active { applied_tip, synced_to_tip: true } = current {
		sync_state_tx.send_replace(CbfSyncState::Active { applied_tip, synced_to_tip: false });
	}
}

/// Struct for holding cbf chain source
pub struct CbfChainSource {
	/// Trusted peer addresses for kyoto's `Builder::add_peers`.
	trusted_peers: Vec<TrustedPeer>,
	/// Scripts tracked by LDK, onchain wallet's scripts are pulled from the onchain wallet
	registered_scripts: Arc<Mutex<HashSet<ScriptBuf>>>,
	fee_source: FeeSource,
	/// Tracks whether the kyoto node is running and holds the live requester.
	cbf_runtime_status: Arc<Mutex<CbfRuntimeStatus>>,
	/// Highest CBF sync tip whose preceding chain updates have been applied to all listeners.
	sync_state_tx: watch::Sender<CbfSyncState>,
	/// Handle used to spawn the background tasks and offload blocking work.
	runtime: Arc<Runtime>,
	/// Node configuration (network, storage path).
	config: Arc<Config>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	kv_store: Arc<DynStore>,
	node_metrics: Arc<PersistedNodeMetrics>,
	logger: Arc<Logger>,
}

#[derive(Debug)]
enum ChainOp {
	ConnectFull { block: IndexedBlock },
	ConnectFiltered { header: Header, height: u32 },
	Disconnect { fork_point: BlockLocator },
	Synced { tip_height: u32 },
	Failed { error: Error },
}

struct BlockApplicator {
	chain_listener: ChainListener,
	ops_rx: mpsc::UnboundedReceiver<ChainOp>,
	next_height: u32,
	sync_state_tx: watch::Sender<CbfSyncState>,
	/// Present only for the native CBF fee source: lets us cache the fee rate of blocks we download
	/// here, so the fee estimator doesn't have to re-download them.
	block_fee_cache: Option<BlockFeeCache>,
	kv_store: Arc<DynStore>,
	node_metrics: Arc<PersistedNodeMetrics>,
	logger: Arc<Logger>,
}

impl BlockApplicator {
	async fn run(mut self) {
		while let Some(op) = self.ops_rx.recv().await {
			match op {
				ChainOp::ConnectFull { block: ib } => {
					if ib.height != self.next_height {
						log_debug!(
							self.logger,
							"CBF skipping out-of-sequence block at height {} (expected {})",
							ib.height,
							self.next_height
						);
						continue;
					}
					self.chain_listener.block_connected(&ib.block, ib.height);
					self.next_height += 1;
					mark_syncing(&self.sync_state_tx);
					if let Some(cache) = &self.block_fee_cache {
						let fee_rate = coinbase_fee_rate(&ib.block, ib.height);
						cache
							.lock()
							.expect("lock")
							.insert(ib.height, (ib.block.block_hash(), fee_rate));
					}
				},
				ChainOp::ConnectFiltered { header, height } => {
					if height != self.next_height {
						log_debug!(
							self.logger,
							"CBF skipping out-of-sequence block at height {} (expected {})",
							height,
							self.next_height
						);
						continue;
					}
					self.chain_listener.filtered_block_connected(&header, &[], height);
					self.next_height += 1;
					mark_syncing(&self.sync_state_tx);
				},
				ChainOp::Disconnect { fork_point } => {
					self.chain_listener.blocks_disconnected(fork_point);
					self.next_height = fork_point.height + 1;
					self.sync_state_tx.send_replace(CbfSyncState::Active {
						applied_tip: Some(fork_point.height),
						synced_to_tip: false,
					});
				},
				ChainOp::Synced { tip_height } => {
					log_info!(self.logger, "CBF caught up to tip {}", tip_height);
					if self.next_height > tip_height {
						self.publish_synced_tip(tip_height).await;
					} else {
						log_debug!(
							self.logger,
							"CBF waiting to apply blocks through tip {} (next height {})",
							tip_height,
							self.next_height
						);
					}
					log_info!(self.logger, "we set new tip and published at {}", tip_height);
				},
				ChainOp::Failed { error } => {
					log_info!(self.logger, "we received error chain op {}", error);
					self.sync_state_tx.send_replace(CbfSyncState::Failed(error));
				},
			}
		}
	}

	async fn publish_synced_tip(&self, tip_height: u32) {
		let already_published = {
			let sync_state = *self.sync_state_tx.borrow();
			match sync_state {
				CbfSyncState::Active { applied_tip, .. } => applied_tip,
				CbfSyncState::Failed(_) => None,
			}
		};
		if already_published.map_or(false, |published_height| published_height >= tip_height) {
			// Even if the applied tip is unchanged, we have now confirmed we are caught up to the
			// network tip, so ensure the synced flag is set for any `wait_until_synced` waiter.
			self.sync_state_tx.send_replace(CbfSyncState::Active {
				applied_tip: already_published,
				synced_to_tip: true,
			});
			return;
		}
		self.sync_state_tx.send_replace(CbfSyncState::Active {
			applied_tip: Some(tip_height),
			synced_to_tip: true,
		});
		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		if let Err(e) = update_and_persist_node_metrics(
			&self.node_metrics,
			&*self.kv_store,
			&*self.logger,
			|m| {
				m.latest_lightning_wallet_sync_timestamp = unix_time_secs_opt;
				m.latest_onchain_wallet_sync_timestamp = unix_time_secs_opt;
			},
		)
		.await
		{
			log_error!(self.logger, "Failed to persist CBF sync metrics: {:?}", e);
		}
	}
}

/// Number of most recent blocks whose coinbase-derived fee rates feed the native CBF estimator.
const FEE_WINDOW_BLOCKS: u32 = BLOCK_FEE_CACHE_CAPACITY as u32;

/// Lower bound for native CBF fee estimates (1 sat/vB), matching the floor used by the Esplora and
/// Electrum fee sources. Coinbase-derived rates are frequently zero on regtest/signet.
const CBF_MIN_FEERATE_SAT_PER_KWU: u64 = 250;

/// Per-attempt timeout when downloading a block from a peer — used both for matched blocks we apply
/// to the listeners and for the coinbase-fee-rate samples. Kyoto queues the request and awaits a
/// peer response with no timeout of its own, so a slow or unresponsive peer would otherwise park the
/// fetch forever. Kept short so a single request is bounded and can be retried (or, for fees, only
/// delays one sample) rather than stalling.
const CBF_BLOCK_FETCH_TIMEOUT_SECS: u64 = 10;

/// Recent per-block coinbase-derived fee rates, keyed by height so we can window on the tip, evict
/// stale entries, and detect reorged-out blocks (a height whose cached hash no longer matches the
/// canonical chain). Shared via `Arc` between the fee estimator and the [`BlockApplicator`].
type BlockFeeCache = Arc<Mutex<BTreeMap<u32, (BlockHash, FeeRate)>>>;

enum FeeSource {
	/// Derive fee rates from the coinbase reward of recent blocks.  Downloads full blocks in order to calculate fee estimation.
	///
	/// The [`BlockApplicator`] also opportunistically inserts the fee rate of any block it already
	/// downloads on a filter match, saving a re-download in the reconciliation loop.
	Cbf { block_fee_cache: BlockFeeCache },
	/// Delegate fee estimation to an Esplora HTTP server.
	Esplora { client: esplora_client::AsyncClient },
	/// Delegate fee estimation to an Electrum server.
	///
	/// A fresh connection is opened for each estimation cycle.
	Electrum { server_url: String },
}

impl CbfChainSource {
	pub(crate) fn new(
		peers: Vec<String>, fee_source_config: Option<CbfFeeSourceConfig>, runtime: Arc<Runtime>,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<PersistedNodeMetrics>,
	) -> Result<Self, Error> {
		let trusted_peers: Vec<TrustedPeer> = peers
			.iter()
			.filter_map(|peer_str| {
				peer_str.parse::<SocketAddr>().ok().map(TrustedPeer::from_socket_addr)
			})
			.collect();

		let fee_source = match fee_source_config {
			Some(CbfFeeSourceConfig::Esplora(server_url)) => {
				let mut esplora_builder = esplora_client::Builder::new(&server_url);
				esplora_builder = esplora_builder.timeout(ESPLORA_TIMEOUT);
				let client = esplora_builder.build_async().map_err(|e| {
					log_error!(logger, "Failed to build esplora client: {}", e);
					Error::ConnectionFailed
				})?;
				FeeSource::Esplora { client }
			},
			Some(CbfFeeSourceConfig::Electrum(server_url)) => FeeSource::Electrum { server_url },
			None => FeeSource::Cbf { block_fee_cache: Arc::new(Mutex::new(BTreeMap::new())) },
		};
		let registered_scripts = Arc::new(Mutex::new(HashSet::new()));
		let cbf_runtime_status = Arc::new(Mutex::new(CbfRuntimeStatus::Stopped));
		let (sync_state_tx, _) =
			watch::channel(CbfSyncState::Active { applied_tip: None, synced_to_tip: false });
		Ok(Self {
			trusted_peers,
			fee_source,
			registered_scripts,
			cbf_runtime_status,
			sync_state_tx,
			runtime,
			config,
			fee_estimator,
			kv_store,
			node_metrics,
			logger,
		})
	}

	fn build_kyoto(
		trusted_peers: &[TrustedPeer], config: &Config, logger: &Logger,
		chain_listener: &ChainListener,
	) -> (KyotoNode, Client) {
		let mut kyoto_builder = KyotoBuilder::new(config.network);

		let data_dir = std::path::PathBuf::from(&config.storage_dir_path).join("bip157_data");
		kyoto_builder = kyoto_builder.data_dir(data_dir);

		if !trusted_peers.is_empty() {
			kyoto_builder = kyoto_builder.add_peers(trusted_peers.to_vec());
		}

		kyoto_builder = kyoto_builder.required_peers(DEFAULT_REQUIRED_PEERS);
		kyoto_builder = kyoto_builder.fetch_witness_data();
		kyoto_builder =
			kyoto_builder.response_timeout(Duration::from_secs(DEFAULT_RESPONSE_TIMEOUT_SECS));

		if let Some(header_cp) = resume_checkpoint(logger, chain_listener) {
			log_debug!(
				logger,
				"CBF builder: resuming from checkpoint height={}, hash={}",
				header_cp.height,
				header_cp.hash,
			);
			kyoto_builder = kyoto_builder.chain_state(ChainState::Checkpoint(header_cp));
		}

		kyoto_builder.build()
	}

	pub(crate) fn start(&self, chain_listener: ChainListener) {
		let (node, client) =
			Self::build_kyoto(&self.trusted_peers, &self.config, &self.logger, &chain_listener);
		let Client { requester, info_rx, warn_rx, event_rx } = client;

		{
			let mut status = self.cbf_runtime_status.lock().expect("lock");
			if matches!(*status, CbfRuntimeStatus::Started { .. }) {
				debug_assert!(false, "start() called while CBF chain source is already running");
				return;
			}
			*status = CbfRuntimeStatus::Started { requester };
		}

		let (ops_tx, ops_rx) = mpsc::unbounded_channel();
		let block_fee_cache = match &self.fee_source {
			FeeSource::Cbf { block_fee_cache } => Some(Arc::clone(block_fee_cache)),
			_ => None,
		};
		let best_block_height = chain_listener.get_best_block().height;
		self.sync_state_tx.send_replace(CbfSyncState::Active {
			applied_tip: Some(best_block_height),
			synced_to_tip: false,
		});
		let block_applicator = BlockApplicator {
			next_height: best_block_height + 1,
			sync_state_tx: self.sync_state_tx.clone(),
			chain_listener: chain_listener.clone(),
			ops_rx,
			block_fee_cache,
			kv_store: Arc::clone(&self.kv_store),
			node_metrics: Arc::clone(&self.node_metrics),
			logger: Arc::clone(&self.logger),
		};
		self.runtime.spawn_background_task(block_applicator.run());

		log_info!(self.logger, "CBF chain source started.");

		let restart_status = Arc::clone(&self.cbf_runtime_status);
		let restart_logger = Arc::clone(&self.logger);
		let restart_peers = self.trusted_peers.clone();
		let restart_config = Arc::clone(&self.config);
		let restart_listener = chain_listener;
		let restart_registered_scripts = Arc::clone(&self.registered_scripts);
		let restart_cbf_runtime_status = Arc::clone(&self.cbf_runtime_status);
		let restart_sync_state_tx = self.sync_state_tx.clone();

		self.runtime.spawn_background_task(async move {
			let mut current_node = node;
			let mut current_info_rx = info_rx;
			let mut current_warn_rx = warn_rx;
			let mut current_event_rx = event_rx;
			let mut retries = 0u32;
			let mut backoff_ms = INITIAL_BACKOFF_MS;

			loop {
				let info_handle = tokio::spawn(Self::process_info_messages(
					current_info_rx,
					Arc::clone(&restart_logger),
				));
				let warn_handle = tokio::spawn(Self::process_warn_messages(
					current_warn_rx,
					Arc::clone(&restart_logger),
				));

				let event_handle = tokio::spawn(Self::process_kyoto_events(
					Arc::clone(&restart_logger),
					current_event_rx,
					Arc::clone(&restart_registered_scripts),
					Arc::clone(&restart_cbf_runtime_status),
					ops_tx.clone(),
					Arc::clone(&restart_listener.onchain_wallet),
					restart_sync_state_tx.clone(),
				));

				match current_node.run().await {
					Ok(()) => {
						log_info!(restart_logger, "CBF node shut down cleanly.");
						*restart_status.lock().expect("lock") = CbfRuntimeStatus::Stopped;
						restart_sync_state_tx.send_replace(CbfSyncState::Failed(Error::NotRunning));
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
							*restart_status.lock().expect("lock") = CbfRuntimeStatus::Stopped;
							restart_sync_state_tx
								.send_replace(CbfSyncState::Failed(Error::TxSyncFailed));
							break;
						}
						log_error!(
							restart_logger,
							"CBF node exited with error (attempt {}/{}): {:?}. Restarting in {}ms.",
							retries,
							MAX_RESTART_RETRIES,
							e,
							backoff_ms,
						);

						// Abort the old log consumers before rebuilding.
						info_handle.abort();
						warn_handle.abort();
						event_handle.abort();

						tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
						backoff_ms = backoff_ms.saturating_mul(2);
						let (new_node, new_client) = Self::build_kyoto(
							&restart_peers,
							&restart_config,
							&restart_logger,
							&restart_listener,
						);
						let Client {
							requester: new_requester,
							info_rx: new_info_rx,
							warn_rx: new_warn_rx,
							event_rx: new_event_rx,
						} = new_client;

						{
							let mut status = restart_status.lock().expect("lock");
							if matches!(*status, CbfRuntimeStatus::Stopped) {
								let _ = new_requester.shutdown();
								restart_sync_state_tx
									.send_replace(CbfSyncState::Failed(Error::NotRunning));
								log_info!(
									restart_logger,
									"CBF restart aborted: stop() called during backoff."
								);
								break;
							}
							*status = CbfRuntimeStatus::Started { requester: new_requester };
							restart_sync_state_tx.send_replace(CbfSyncState::Active {
								applied_tip: Some(restart_listener.get_best_block().height),
								synced_to_tip: false,
							});
						}

						current_node = new_node;
						current_info_rx = new_info_rx;
						current_warn_rx = new_warn_rx;
						current_event_rx = new_event_rx;
					},
				}
			}
		});
	}

	pub(crate) fn stop(&self) {
		let requester = {
			let mut status = self.cbf_runtime_status.lock().expect("lock");
			match &*status {
				CbfRuntimeStatus::Started { requester } => {
					let requester = requester.clone();
					*status = CbfRuntimeStatus::Stopped;
					Some(requester)
				},
				CbfRuntimeStatus::Stopped => None,
			}
		};

		if let Some(requester) = requester {
			if let Err(e) = requester.shutdown() {
				log_error!(self.logger, "Failed to shut down CBF node: {:?}", e);
			}
		}
		self.sync_state_tx.send_replace(CbfSyncState::Failed(Error::NotRunning));
	}

	pub(crate) async fn wait_until_synced(&self) -> Result<(), Error> {
		if matches!(&*self.cbf_runtime_status.lock().expect("lock"), CbfRuntimeStatus::Stopped) {
			return Err(Error::NotRunning);
		}
		let mut sync_state_rx = self.sync_state_tx.subscribe();

		// Wait for kyoto to report catching up to the network tip (a `FiltersSynced`-driven
		// `synced_to_tip`) and for the resulting blocks to be applied. We must not target a
		// locally-sampled `chain_tip()`: kyoto does not persist, so a freshly (re)started node's
		// local header chain sits at genesis until it syncs from peers, which would let this return
		// before any sync happens.
		loop {
			match *sync_state_rx.borrow() {
				CbfSyncState::Active { synced_to_tip, .. } => {
					if synced_to_tip {
						return Ok(());
					}
				},
				CbfSyncState::Failed(error) => return Err(error),
			}

			if let Err(e) = sync_state_rx.changed().await {
				debug_assert!(false, "Failed to receive CBF sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive CBF sync result: {:?}", e);
				return Err(Error::TxSyncFailed);
			}
		}
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

	async fn process_kyoto_events(
		logger: Arc<Logger>, mut event_rx: mpsc::UnboundedReceiver<Event>,
		registered_scripts: Arc<Mutex<HashSet<ScriptBuf>>>,
		cbf_runtime_status: Arc<Mutex<CbfRuntimeStatus>>, ops_tx: mpsc::UnboundedSender<ChainOp>,
		onchain_wallet: Arc<Wallet>, sync_state_tx: watch::Sender<CbfSyncState>,
	) {
		while let Some(event) = event_rx.recv().await {
			match event {
				Event::IndexedFilter(indexed_filter) => {
					// A new block's filter arrived, so we're behind by at least this block until it
					// is fetched (if matched) and applied. Flip this before the fetch, not after,
					// so a `sync_wallets` call issued in between doesn't return on a stale
					// `synced_to_tip` that predates this block.
					mark_syncing(&sync_state_tx);

					let requester = match &*cbf_runtime_status.lock().expect("lock") {
						CbfRuntimeStatus::Started { requester } => requester.clone(),
						CbfRuntimeStatus::Stopped => {
							let _ = ops_tx.send(ChainOp::Failed { error: Error::NotRunning });
							return;
						},
					};
					//registered_scripts contains only LDK scripts, not onchain wallet's scripts,
					//as don't want to track them twice: once in bdk, once in CbfChainSource, thus
					//each time we receive an IndexedFilter event, we ask bdk to give us all
					//revealed scripts. We create all_scripts starting from onchain wallet's
					//scripts and extend them with LDK's ones
					let mut all_scripts = onchain_wallet.list_watched_scripts();
					all_scripts.extend(registered_scripts.lock().expect("lock").iter().cloned());

					let block_hash = indexed_filter.block_hash();
					let matched = indexed_filter.contains_any(all_scripts.iter());

					let chop: ChainOp = if matched {
						let mut attempt = 0;
						let block = loop {
							attempt += 1;
							let handle = match requester.request_block(block_hash) {
								Ok(handle) => handle,
								Err(_) => {
									log_error!(
										logger,
										"Failed to obtain receiver for matched CBF block {}; node is stopped",
										block_hash
									);
									let _ =
										ops_tx.send(ChainOp::Failed { error: Error::NotRunning });
									return;
								},
							};

							// Bound the download so an unresponsive peer can't park the fetch forever,
							// then flatten the three error layers (timeout / receiver dropped / fetch
							// error) into a single reason so the retry-or-fail decision is written once.
							let fetched = tokio::time::timeout(
								Duration::from_secs(CBF_BLOCK_FETCH_TIMEOUT_SECS),
								handle,
							)
							.await
							.map_err(|_| {
								format!("timed out after {}s", CBF_BLOCK_FETCH_TIMEOUT_SECS)
							})
							.and_then(|recv| recv.map_err(|_| "receiver was dropped".to_string()))
							.and_then(|fetch| fetch.map_err(|e| format!("failed: {:?}", e)));

							match fetched {
								Ok(block) => break block,
								Err(reason) if attempt < CBF_BLOCK_FETCH_RETRIES => {
									log_debug!(
										logger,
										"CBF block fetch for {} {} on attempt {}; retrying",
										block_hash,
										reason,
										attempt
									);
								},
								Err(reason) => {
									log_error!(
										logger,
										"CBF block fetch for {} {} after {} attempts; giving up",
										block_hash,
										reason,
										CBF_BLOCK_FETCH_RETRIES
									);
									let _ =
										ops_tx.send(ChainOp::Failed { error: Error::TxSyncFailed });
									return;
								},
							}
						};
						ChainOp::ConnectFull { block }
					} else {
						let height = indexed_filter.height();
						//TODO we need to recheck that a particular height has not been
						//reorganized, and we retrieve indeed the same block header that we
						//received `IndexedFilter` event of.
						match requester.get_header(height).await {
							Ok(Some(indexed_header)) => {
								if indexed_header.block_hash() != block_hash {
									log_debug!(
										logger,
										"Filter for {} reorged; skipping",
										block_hash
									);
									continue;
								}
								ChainOp::ConnectFiltered {
									header: indexed_header.header,
									height: indexed_header.height,
								}
							},
							Ok(None) => {
								log_error!(logger, "No header at height {}", height,);
								let _ = ops_tx.send(ChainOp::Failed { error: Error::TxSyncFailed });
								break;
							},
							Err(e) => {
								log_error!(
									logger,
									"Failed to fetch header at height {}: {:?}",
									height,
									e,
								);
								let _ = ops_tx.send(ChainOp::Failed { error: Error::TxSyncFailed });
								break;
							},
						}
					};
					if let Err(e) = ops_tx.send(chop) {
						log_debug!(logger, "ops_rx gone: {}", e);
					}
				},
				Event::FiltersSynced(sync_update) => {
					//Because application of blocks is async, the fact that kyoto synced up to the
					//tip does NOT mean that we caught everything up, that's why we send a ChainOp,
					//only processing of which means we processed all blocks up to the tip.
					log_info!(logger, "Kyoto synced up to the tip {}", sync_update.tip().height);
					let _ = ops_tx.send(ChainOp::Synced { tip_height: sync_update.tip().height });
				},
				Event::ChainUpdate(BlockHeaderChanges::Connected(indexed_header)) => {
					log_debug!(
						logger,
						"Kyoto connected header at height {}",
						indexed_header.height
					);
				},
				Event::ChainUpdate(BlockHeaderChanges::Reorganized {
					reorganized,
					accepted: _,
				}) => {
					// Rewind to the fork point; kyoto will re-deliver the new chain's filters.
					if let Some(lowest) = reorganized.first() {
						let fork_point = BlockLocator::new(
							lowest.prev_blockhash(),
							lowest.height.saturating_sub(1),
						);
						let _ = ops_tx.send(ChainOp::Disconnect { fork_point });
					}
				},
				Event::ChainUpdate(BlockHeaderChanges::ForkAdded(fork)) => {
					log_debug!(logger, "Kyoto added fork header at height {}", fork.height);
				},
			}
		}
	}

	pub(crate) fn register_tx(&self, _txid: &Txid, script_pubkey: &Script) {
		self.registered_scripts.lock().expect("lock").insert(script_pubkey.into());
	}

	pub(crate) fn register_output(&self, output: WatchedOutput) {
		self.registered_scripts.lock().expect("lock").insert(output.script_pubkey);
	}

	pub(crate) async fn continuously_update_fee_rate_estimates(
		&self, mut stop_sync_receiver: watch::Receiver<()>,
	) {
		let mut fee_rate_update_interval =
			tokio::time::interval(Duration::from_secs(DEFAULT_FEE_RATE_CACHE_UPDATE_INTERVAL_SECS));
		// We primed the cache once on startup, so skip the immediate first tick.
		fee_rate_update_interval.reset();
		fee_rate_update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		loop {
			tokio::select! {
				_ = stop_sync_receiver.changed() => {
					log_trace!(self.logger, "Stopping CBF fee-rate update loop.");
					return;
				}
				_ = fee_rate_update_interval.tick() => {
					if let Err(e) = self.update_fee_rate_estimates().await {
						log_error!(self.logger, "Failed to update fee rate estimates: {:?}", e);
					}
				}
			}
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		let new_fee_rate_cache = match &self.fee_source {
			FeeSource::Esplora { client } => {
				let estimates = client.get_fee_estimates().await.map_err(|e| {
					log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
					Error::FeerateEstimationUpdateFailed
				})?;

				if estimates.is_empty() && self.config.network == Network::Bitcoin {
					log_error!(
						self.logger,
						"Failed to retrieve fee rate: empty fee estimates are disallowed on Mainnet."
					);
					return Err(Error::FeerateEstimationUpdateFailed);
				}

				let mut new_fee_rate_cache = HashMap::with_capacity(10);
				for target in get_all_conf_targets() {
					let num_blocks = get_num_block_defaults_for_target(target);
					// Fall back to 1 sat/vb if we fail or it yields less than that, mostly to keep
					// going on signet/regtest where estimates may be missing or bogus.
					let converted_estimate_sat_vb =
						esplora_client::convert_fee_rate(num_blocks, estimates.clone())
							.map_or(1.0, |converted| converted.max(1.0));
					let fee_rate =
						FeeRate::from_sat_per_kwu((converted_estimate_sat_vb * 250.0) as u64);
					let adjusted_fee_rate = apply_post_estimation_adjustments(target, fee_rate);
					new_fee_rate_cache.insert(target, adjusted_fee_rate);
				}
				new_fee_rate_cache
			},
			FeeSource::Electrum { server_url } => {
				let electrum_config = ElectrumConfigBuilder::new()
					.retry(ELECTRUM_FEE_NUM_RETRIES)
					.timeout(Some(Duration::from_secs(ELECTRUM_FEE_TIMEOUT_SECS)))
					.build();

				let server_url = server_url.clone();
				let electrum_client = self
					.runtime
					.spawn_blocking(move || {
						ElectrumClient::from_config(&server_url, electrum_config)
					})
					.await
					.map_err(|e| {
						log_error!(self.logger, "Fee rate estimation task panicked: {}", e);
						Error::FeerateEstimationUpdateFailed
					})?
					.map_err(|e| {
						log_error!(self.logger, "Failed to connect to electrum server: {}", e);
						Error::ConnectionFailed
					})?;

				get_electrum_fee_rate_cache_update(
					Arc::clone(&self.runtime),
					Arc::new(electrum_client),
					self.config.network,
					ELECTRUM_FEE_TIMEOUT_SECS,
					Arc::clone(&self.logger),
				)
				.await?
			},
			FeeSource::Cbf { block_fee_cache } => {
				let requester = match &*self.cbf_runtime_status.lock().expect("lock") {
					CbfRuntimeStatus::Started { requester } => requester.clone(),
					CbfRuntimeStatus::Stopped => return Err(Error::FeerateEstimationUpdateFailed),
				};
				let mut samples_sat_per_kwu: Vec<u64> = self
					.refresh_block_fee_window(&requester, block_fee_cache)
					.await
					.iter()
					.map(|rate| rate.to_sat_per_kwu())
					.collect();
				samples_sat_per_kwu.sort_unstable();

				let mut new_fee_rate_cache = HashMap::with_capacity(10);
				for target in get_all_conf_targets() {
					let fee_rate = if samples_sat_per_kwu.is_empty() {
						FeeRate::from_sat_per_kwu(get_fallback_rate_for_target(target) as u64)
					} else {
						let percentile = cbf_percentile_for_target(target);
						let sat_per_kwu = percentile_of_sorted(&samples_sat_per_kwu, percentile)
							.max(CBF_MIN_FEERATE_SAT_PER_KWU);
						FeeRate::from_sat_per_kwu(sat_per_kwu)
					};
					let adjusted_fee_rate = apply_post_estimation_adjustments(target, fee_rate);
					new_fee_rate_cache.insert(target, adjusted_fee_rate);
				}
				new_fee_rate_cache
			},
		};

		self.commit_fee_rate_cache(new_fee_rate_cache).await
	}

	/// Writes a freshly computed per-target fee-rate map into the estimator cache and records the
	/// update timestamp in the node metrics.
	async fn commit_fee_rate_cache(
		&self, new_fee_rate_cache: HashMap<ConfirmationTarget, FeeRate>,
	) -> Result<(), Error> {
		self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache);
		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		update_and_persist_node_metrics(&self.node_metrics, &*self.kv_store, &*self.logger, |m| {
			m.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt
		})
		.await?;
		Ok(())
	}

	pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
		let requester = match &*self.cbf_runtime_status.lock().expect("lock") {
			CbfRuntimeStatus::Started { requester } => requester.clone(),
			CbfRuntimeStatus::Stopped => {
				debug_assert!(false, "We should have started the chain source before broadcasting");
				return;
			},
		};

		match Package::from_vec(package.clone()) {
			Ok(package) => {
				if let Err(e) = requester.submit_package(package).await {
					log_error!(self.logger, "Failed to broadcast transaction package: {:?}", e);
				}
			},
			Err(_) => {
				for tx in package {
					let txid = tx.compute_txid();
					if let Err(e) = requester.submit_package(tx).await {
						log_error!(
							self.logger,
							"Failed to broadcast transaction {}: {:?}",
							txid,
							e
						);
					}
				}
			},
		}
	}

	/// Reconciles the block-fee cache against the canonical chain and returns the per-block fee
	/// rates for the most recent [`FEE_WINDOW_BLOCKS`] blocks.
	///
	/// For each height in the window we fetch the canonical block hash; if the cached entry still
	/// matches we reuse its rate, otherwise (new block, or a block that was reorged out) we download
	/// it via [`Requester::average_fee_rate`]. Heights outside the window are evicted by replacing
	/// the cache with the freshly built window.
	///
	/// This is best-effort: a height we can't fetch a header or block for is simply skipped (so a
	/// slow or unresponsive peer can't stall or void the whole update), and an empty result just
	/// means we have no recent data yet. The window therefore fills incrementally over successive
	/// updates rather than requiring all [`FEE_WINDOW_BLOCKS`] downloads to succeed at once.
	async fn refresh_block_fee_window(
		&self, requester: &Requester, cache: &Mutex<BTreeMap<u32, (BlockHash, FeeRate)>>,
	) -> Vec<FeeRate> {
		let tip_height = match requester.chain_tip().await {
			Ok(tip) => tip.height,
			Err(e) => {
				log_error!(self.logger, "CBF fee update: failed to fetch chain tip: {:?}", e);
				return Vec::new();
			},
		};
		let lo = tip_height.saturating_sub(FEE_WINDOW_BLOCKS - 1);

		// Snapshot the cache so we never hold the std `Mutex` across an `.await`.
		let cached = cache.lock().expect("lock").clone();

		let mut window = BTreeMap::new();
		for height in lo..=tip_height {
			let canonical_hash = match requester.get_header(height).await {
				// Height not available (yet); skip it.
				Ok(None) => continue,
				Ok(Some(header)) => header.block_hash(),
				Err(e) => {
					log_debug!(
						self.logger,
						"CBF fee update: failed to fetch header at height {}, skipping: {:?}",
						height,
						e
					);
					continue;
				},
			};

			// Reuse the cached rate while the block is still canonical; otherwise download it.
			if let Some((hash, fee_rate)) = cached.get(&height) {
				if *hash == canonical_hash {
					window.insert(height, (canonical_hash, *fee_rate));
					continue;
				}
			}

			match tokio::time::timeout(
				Duration::from_secs(CBF_BLOCK_FETCH_TIMEOUT_SECS),
				requester.average_fee_rate(canonical_hash),
			)
			.await
			{
				Ok(Ok(fee_rate)) => {
					window.insert(height, (canonical_hash, fee_rate));
				},
				Ok(Err(e)) => {
					log_debug!(
						self.logger,
						"CBF fee update: failed to fetch fee rate for block {}, skipping: {:?}",
						canonical_hash,
						e
					);
				},
				Err(_) => {
					log_debug!(
						self.logger,
						"CBF fee update: timed out fetching block {} for fee estimation, skipping.",
						canonical_hash,
					);
				},
			}
		}

		let samples = window.values().map(|(_, fee_rate)| *fee_rate).collect();
		// Replacing the cache wholesale also evicts any entries that fell out of the window.
		*cache.lock().expect("lock") = window;
		samples
	}
}

fn resume_checkpoint(logger: &Logger, chain_listener: &ChainListener) -> Option<HashCheckpoint> {
	let min_best_block = chain_listener.get_best_block();
	let bdk_cp = chain_listener.onchain_wallet.latest_checkpoint();

	if let Some(bdk_at_height) = bdk_cp.get(min_best_block.height) {
		if bdk_at_height.hash() != min_best_block.block_hash {
			log_error!(
				logger,
				"CBF resume: listener best block at height {} has hash {} but BDK has {}; \
					 a component may be on a stale fork. Anchoring on BDK's chain.",
				min_best_block.height,
				min_best_block.block_hash,
				bdk_at_height.hash(),
			);
		}
	}

	// Walk BDK's checkpoint chain back to the reorg-safe anchor height.
	let target_height = min_best_block.height.saturating_sub(REORG_SAFETY_BLOCKS);
	let mut cursor = bdk_cp;
	while cursor.height() > target_height {
		match cursor.prev() {
			Some(prev) => cursor = prev,
			None => break,
		}
	}

	(cursor.height() > 0).then(|| HashCheckpoint::new(cursor.height(), cursor.hash()))
}
