use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bip157::chain::ChainState;
use bip157::{
	chain::BlockHeaderChanges, error::FetchBlockError, Builder as KyotoBuilder, Client, Event,
	HashCheckpoint, Header, IndexedBlock, Info, Node as KyotoNode, Requester, TrustedPeer, Warning,
};
use bitcoin::{BlockHash, FeeRate, Script, ScriptBuf, Txid};
use lightning::chain::{Listen, WatchedOutput};

use tokio::sync::{mpsc, oneshot};

use crate::chain::bitcoind::ChainListener;
use crate::chain::CbfFeeSourceConfig;
use crate::config::Config;
use crate::error::Error;
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, Sweeper, Wallet};

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

const ESPLORA_TIMEOUT: u64 = 2;

/// Runtime status of the underlying kyoto node.
enum CbfRuntimeStatus {
	Started { requester: Requester },
	Stopped,
}

/// Struct for holding cbf chain source
pub struct CbfChainSource {
	/// Trusted peer addresses for kyoto's `Builder::add_peers`.
	trusted_peers: Vec<TrustedPeer>,
	registered_scripts: Arc<Mutex<HashSet<ScriptBuf>>>,
	fee_source: FeeSource,
	/// Tracks whether the kyoto node is running and holds the live requester.
	cbf_runtime_status: Arc<Mutex<CbfRuntimeStatus>>,
	/// Node configuration (network, storage path).
	config: Arc<Config>,
	logger: Arc<Logger>,
}

enum ChainOp {
	ConnectFull { block_rx: oneshot::Receiver<Result<IndexedBlock, FetchBlockError>> },
	ConnectFiltered { header: Header, height: u32 },
	//Reorg { /* accepted / reorganized from BlockHeaderChanges */ },
}

struct BlockApplicator {
	chain_listener: ChainListener,
	ops_rx: mpsc::UnboundedReceiver<ChainOp>,
	logger: Arc<Logger>,
}

impl BlockApplicator {
	async fn run(mut self) {
		while let Some(op) = self.ops_rx.recv().await {
			match op {
				ChainOp::ConnectFull { block_rx } => match block_rx.await {
					Ok(Ok(ib)) => self.chain_listener.block_connected(&ib.block, ib.height),
					Ok(Err(e)) => log_error!(self.logger, "block fetch failed: {:?}", e),
					Err(_) => log_error!(self.logger, "block oneshot dropped"),
				},
				ChainOp::ConnectFiltered { header, height } => {
					self.chain_listener.filtered_block_connected(&header, &[], height)
				},
				//ChainOp::Reorg { .. } => {},
			}
		}
	}
}

enum FeeSource {
	/// Derive fee rates from the coinbase reward of recent blocks.  Downloads full blocks in order to calculate fee estimation.
	Cbf { block_fee_cache: Mutex<VecDeque<(BlockHash, FeeRate)>> },
	/// Delegate fee estimation to an Esplora HTTP server.
	Esplora { client: esplora_client::AsyncClient },
	/// Delegate fee estimation to an Electrum server.
	///
	/// A fresh connection is opened for each estimation cycle.
	Electrum { server_url: String },
}

impl FeeSource {
	fn insert_cached_block(&self, block_hash: BlockHash, fee_rate: FeeRate) {
		match &self {
			Self::Cbf { block_fee_cache } => {
				block_fee_cache.lock().expect("lock").push_back((block_hash, fee_rate));
			},
			_ => {},
		}
	}
}

impl CbfChainSource {
	pub(crate) fn new(
		peers: Vec<String>, fee_source_config: Option<CbfFeeSourceConfig>, config: Arc<Config>,
		logger: Arc<Logger>,
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
			None => FeeSource::Cbf {
				block_fee_cache: Mutex::new(VecDeque::with_capacity(BLOCK_FEE_CACHE_CAPACITY)),
			},
		};
		let registered_scripts = Arc::new(Mutex::new(HashSet::new()));
		let cbf_runtime_status = Arc::new(Mutex::new(CbfRuntimeStatus::Stopped));
		Ok(Self {
			trusted_peers,
			fee_source,
			registered_scripts,
			cbf_runtime_status,
			config,
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

	pub(crate) fn start(&self, runtime: Arc<Runtime>, chain_listener: ChainListener) {
		//populate registered scripts with all the scripts from the onchain wallet
		for script in chain_listener.onchain_wallet.list_revealed_scripts() {
			self.register_script(script);
		}

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
		let block_applicator = BlockApplicator {
			chain_listener: chain_listener.clone(),
			ops_rx,
			logger: Arc::clone(&self.logger),
		};
		runtime.spawn_background_task(block_applicator.run());

		log_info!(self.logger, "CBF chain source started.");

		let restart_status = Arc::clone(&self.cbf_runtime_status);
		let restart_logger = Arc::clone(&self.logger);
		let restart_peers = self.trusted_peers.clone();
		let restart_config = Arc::clone(&self.config);
		let restart_listener = chain_listener;
		let restart_registered_scripts = Arc::clone(&self.registered_scripts);
		let restart_cbf_runtime_status = Arc::clone(&self.cbf_runtime_status);
		// let restart_block_applicator =

		runtime.spawn_background_task(async move {
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
					current_event_rx,
					Arc::clone(&restart_registered_scripts),
					Arc::clone(&restart_cbf_runtime_status),
					ops_tx.clone(),
				));

				match current_node.run().await {
					Ok(()) => {
						log_info!(restart_logger, "CBF node shut down cleanly.");
						*restart_status.lock().expect("lock") = CbfRuntimeStatus::Stopped;
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

						tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
						backoff_ms = backoff_ms.saturating_mul(2);

						// Abort the old log consumers before rebuilding.
						info_handle.abort();
						warn_handle.abort();
						event_handle.abort();

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

						*restart_status.lock().expect("lock") =
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
		mut event_rx: mpsc::UnboundedReceiver<Event>,
		registered_scripts: Arc<Mutex<HashSet<ScriptBuf>>>,
		cbf_runtime_status: Arc<Mutex<CbfRuntimeStatus>>, ops_tx: mpsc::UnboundedSender<ChainOp>,
	) {
		while let Some(event) = event_rx.recv().await {
			match event {
				// match download
				Event::IndexedFilter(indexed_filter) => {
					let matched = indexed_filter
						.contains_any(registered_scripts.lock().expect("lock").iter());
					if matched {
						let rtm = &*cbf_runtime_status.lock().expect("lock");
						let requestor = match rtm {
							CbfRuntimeStatus::Started { requester } => requester.clone(),
							CbfRuntimeStatus::Stopped => {
								//panic
								// todo!();
								continue;
							},
						};
						let block_rx = requestor
							.request_block(indexed_filter.block_hash())
							.expect("cannot request block");
						let chop = ChainOp::ConnectFull { block_rx };
						//here we feed evets to the driver
						ops_tx.send(chop);
					}
				},
				Event::FiltersSynced(sync_update) => {
					todo!();
				},
				Event::ChainUpdate(BlockHeaderChanges::Connected(connected_blocks)) => {
					todo!();
				},
				Event::ChainUpdate(BlockHeaderChanges::Reorganized { reorganized, accepted }) => {
					todo!();
				},
				Event::ChainUpdate(BlockHeaderChanges::ForkAdded(fork)) => {
					todo!();
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

	pub(crate) fn register_script(&self, script: ScriptBuf) {
		self.registered_scripts.lock().expect("lock").insert(script);
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
