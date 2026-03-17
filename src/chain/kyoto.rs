// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bip157::chain::{BlockHeaderChanges, IndexedHeader};
use bip157::{HeaderCheckpoint, Info};
use bitcoin::{BlockHash, FeeRate, Network, ScriptBuf, Transaction};
use electrum_client::{
	Batch, Client as ElectrumClient, ConfigBuilder as ElectrumConfigBuilder, ElectrumApi,
};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::chain::{BestBlock, Listen};
use tokio::sync::{mpsc, watch};

use super::FeeSourceConfig;
use crate::chain::electrum::ELECTRUM_CLIENT_NUM_RETRIES;
use crate::config::{
	Config, DEFAULT_FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, DEFAULT_PER_REQUEST_TIMEOUT_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

/// How long to wait for kyoto to establish the required peer connections before considering the startup failed.
const KYOTO_CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// The fee estimation back-end used by a BIP-157 chain source.
enum FeeSource {
	/// Derive fee rates from the coinbase reward of the most-recently seen block.
	///
	/// Provides a single average rate applied uniformly across all confirmation targets.
	/// Less accurate than a mempool-aware source but requires no extra connectivity.
	Kyoto,
	/// Delegate fee estimation to an Esplora HTTP server.
	Esplora { client: EsploraAsyncClient, timeout_secs: u64 },
	/// Delegate fee estimation to an Electrum server.
	///
	/// A fresh connection is opened for each estimation cycle; the server URL is stored
	/// rather than the client because `ElectrumClient` is not `Sync`.
	Electrum { server_url: String },
}

pub(super) struct KyotoChainSource {
	/// Peers at construction time.
	peers: Vec<std::net::SocketAddr>,
	/// The kyoto node, built in `new()` and rebuilt on each subsequent `start()` call.
	/// Consumed (taken) by `start()` when the node task is spawned.
	node: Mutex<Option<bip157::Node>>,
	/// Info channel, consumed by `start()` during the initial connection wait.
	info_rx: Mutex<Option<mpsc::Receiver<Info>>>,
	/// The remaining client channels, consumed by `continuously_sync_wallets()`.
	warn_rx: Mutex<Option<mpsc::UnboundedReceiver<bip157::Warning>>>,
	event_rx: Mutex<Option<mpsc::UnboundedReceiver<bip157::Event>>>,
	/// Cloneable handle used to send commands (e.g. shutdown) to the running node.
	/// Replaced on each `start()` with the handle for the newly-built node instance.
	requester: Mutex<Option<bip157::Requester>>,
	/// The most recently completed sync checkpoint, updated by `FiltersSynced` events.
	/// On restart this is passed to kyoto's `chain_state` so it resumes from where it left off.
	synced_checkpoint: Mutex<Option<bip157::HeaderCheckpoint>>,
	/// Sender half of the sync-tip watch channel, held until `continuously_sync_wallets()` takes
	/// it. When the background loop exits (node died or stop signal), dropping this sender causes
	/// all `wait_until_synced` callers to receive `RecvError` immediately.
	synced_checkpoint_tx: Mutex<Option<watch::Sender<Option<HeaderCheckpoint>>>>,
	/// Receiver half of the sync-tip watch channel. Cloned by `wait_until_synced` to observe
	/// `FiltersSynced` checkpoints emitted by the background loop.
	/// Replaced on each `start()` alongside the sender.
	synced_checkpoint_rx: Mutex<watch::Receiver<Option<HeaderCheckpoint>>>,
	/// Scripts registered by LDK (via `chain::Filter`) and by the BDK onchain wallet.
	/// On each `IndexedFilter` event the compact filter is checked against this set;
	/// a match triggers a full-block request via `requester.get_block()`.
	watched_scripts: Mutex<HashSet<ScriptBuf>>,
	/// Block headers accumulated from `ChainUpdate::Connected` events, keyed by block hash.
	///
	/// Kyoto downloads all headers first, then scans compact filters. When we process an
	/// `IndexedFilter` event we need the actual `Header` struct (for `filtered_block_connected`)
	/// but the filter event only carries the block hash. We look it up here and remove the
	/// entry once the filter has been handled to bound memory usage.
	header_cache: Mutex<HashMap<BlockHash, IndexedHeader>>,
	fee_source: FeeSource,
	fee_estimator: Arc<OnchainFeeEstimator>,
	kv_store: Arc<DynStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl KyotoChainSource {
	pub(super) fn new(
		peers: Vec<std::net::SocketAddr>, fee_source_config: Option<FeeSourceConfig>,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let data_dir = PathBuf::from(&config.storage_dir_path).join("kyoto");
		let (node, client) = bip157::Builder::new(config.network)
			.add_peers(peers.iter().copied().map(Into::into))
			.data_dir(data_dir)
			.fetch_witness_data()
			.build();
		let bip157::Client { requester, info_rx, warn_rx, event_rx } = client;

		let fee_source = match fee_source_config {
			Some(FeeSourceConfig::Esplora(url)) => {
				let esplora_client = esplora_client::Builder::new(&url)
					.timeout(DEFAULT_PER_REQUEST_TIMEOUT_SECS as u64)
					.build_async()
					.unwrap();
				FeeSource::Esplora {
					client: esplora_client,
					timeout_secs: DEFAULT_FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS,
				}
			},
			Some(FeeSourceConfig::Electrum(server_url)) => FeeSource::Electrum { server_url },
			None => FeeSource::Kyoto,
		};

		let (synced_checkpoint_tx, synced_checkpoint_rx) = watch::channel(None);

		Self {
			peers,
			node: Mutex::new(Some(node)),
			info_rx: Mutex::new(Some(info_rx)),
			warn_rx: Mutex::new(Some(warn_rx)),
			event_rx: Mutex::new(Some(event_rx)),
			requester: Mutex::new(Some(requester)),
			synced_checkpoint: Mutex::new(None),
			synced_checkpoint_tx: Mutex::new(Some(synced_checkpoint_tx)),
			synced_checkpoint_rx: Mutex::new(synced_checkpoint_rx),
			watched_scripts: Mutex::new(HashSet::new()),
			header_cache: Mutex::new(HashMap::new()),
			fee_source,
			fee_estimator,
			kv_store,
			config,
			logger,
			node_metrics,
		}
	}

	/// Spawn the kyoto node task, then block until it connects to the required number of peers.
	///
	/// Rebuilds the kyoto node on each call so that the node can be restarted after a stop.
	/// On the first start the node was already built in `new()` (with explicit peers); on
	/// subsequent starts a fresh node is rebuilt with the same peers so that nodes without a
	/// populated kyoto peer database (e.g. regtest) can reconnect after a stop/start cycle.
	///
	/// Returns `Err(Error::ConnectionFailed)` if no connection is established within
	/// [`KYOTO_CONNECTION_TIMEOUT`] or if the node exits before connecting.
	pub(super) fn start(&self, runtime: Arc<Runtime>) -> Result<(), Error> {
		// If this is a restart (node was consumed by the previous start), rebuild from disk.
		if self.node.lock().unwrap().is_none() {
			let data_dir = PathBuf::from(&self.config.storage_dir_path).join("kyoto");
			let checkpoint = *self.synced_checkpoint.lock().unwrap();
			let mut builder = bip157::Builder::new(self.config.network)
				.add_peers(self.peers.iter().copied().map(Into::into))
				.data_dir(data_dir)
				.fetch_witness_data();
			if let Some(cp) = checkpoint {
				builder = builder.chain_state(bip157::chain::ChainState::Checkpoint(cp));
			}
			let (node, client) = builder.build();
			let bip157::Client { requester, info_rx, warn_rx, event_rx } = client;
			let (synced_checkpoint_tx, synced_checkpoint_rx) = watch::channel(None);
			*self.node.lock().unwrap() = Some(node);
			*self.info_rx.lock().unwrap() = Some(info_rx);
			*self.warn_rx.lock().unwrap() = Some(warn_rx);
			*self.event_rx.lock().unwrap() = Some(event_rx);
			*self.requester.lock().unwrap() = Some(requester);
			*self.synced_checkpoint_tx.lock().unwrap() = Some(synced_checkpoint_tx);
			*self.synced_checkpoint_rx.lock().unwrap() = synced_checkpoint_rx;
			// header_cache entries are invalid for the new node instance.
			self.header_cache.lock().unwrap().clear();
			// synced_checkpoint is passed to the builder above; reset it so fee
			// estimation skips gracefully until the new instance hits FiltersSynced.
			*self.synced_checkpoint.lock().unwrap() = None;
		}

		// peers: Vec<std::net::SocketAddr>,
		// node: Mutex<Option<bip157::Node>>,
		// info_rx: Mutex<Option<mpsc::Receiver<Info>>>,
		// warn_rx: Mutex<Option<mpsc::UnboundedReceiver<bip157::Warning>>>,
		// event_rx: Mutex<Option<mpsc::UnboundedReceiver<bip157::Event>>>,
		// requester: Mutex<Option<bip157::Requester>>,
		// synced_checkpoint: Mutex<Option<bip157::HeaderCheckpoint>>,
		// synced_checkpoint_tx: Mutex<Option<watch::Sender<Option<HeaderCheckpoint>>>>,
		// synced_checkpoint_rx: Mutex<watch::Receiver<Option<HeaderCheckpoint>>>,
		// watched_scripts: Mutex<HashSet<ScriptBuf>>,
		// header_cache: Mutex<HashMap<BlockHash, IndexedHeader>>,
		// fee_source: FeeSource,
		// fee_estimator: Arc<OnchainFeeEstimator>,
		// kv_store: Arc<DynStore>,
		// config: Arc<Config>,
		// logger: Arc<Logger>,
		// node_metrics: Arc<RwLock<NodeMetrics>>,

		let node = self.node.lock().unwrap().take().unwrap();
		let mut info_rx = self.info_rx.lock().unwrap().take().unwrap();
		let logger = Arc::clone(&self.logger);

		runtime.spawn_background_task(async move {
			if let Err(e) = node.run().await {
				log_error!(logger, "Kyoto node exited with error: {}", e);
			}
		});

		// Block until kyoto connects to the required number of peers or we time out.
		runtime.block_on(async {
			let result = tokio::time::timeout(KYOTO_CONNECTION_TIMEOUT, async {
				while let Some(info) = info_rx.recv().await {
					match info {
						Info::ConnectionsMet => return Ok(()),
						_ => {
							log_debug!(self.logger, "Kyoto: {}", info);
						},
					}
				}
				// Channel closed — node exited before connecting.
				Err(Error::ConnectionFailed)
			})
			.await;

			match result {
				Ok(res) => res,
				Err(_timeout) => {
					log_error!(
						self.logger,
						"Kyoto failed to connect to peers within {}s.",
						KYOTO_CONNECTION_TIMEOUT.as_secs()
					);
					Err(Error::ConnectionFailed)
				},
			}
		})
	}

	/// Signal the kyoto node to shut down. Called during `Node::stop()`.
	pub(super) fn stop(&self) {
		if let Some(r) = self.requester.lock().unwrap().as_ref() {
			let _ = r.shutdown();
		}
	}

	/// Register a script pubkey to watch in compact block filters.
	///
	/// Called from [`super::ChainSource`]'s [`lightning::chain::Filter`] impl when LDK
	/// registers a channel output or transaction script, and when seeding the BDK wallet's
	/// known scripts at startup.
	pub(super) fn register_output(&self, script: ScriptBuf) {
		log_debug!(self.logger, "BIP157: register_output script={}", script);
		self.watched_scripts.lock().unwrap().insert(script);
	}

	/// Main sync loop driven by kyoto events. Runs for the lifetime of the node.
	pub(super) async fn continuously_sync_wallets(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) {
		let mut warn_rx =
			self.warn_rx.lock().unwrap().take().expect("kyoto warn_rx already consumed");
		let mut event_rx =
			self.event_rx.lock().unwrap().take().expect("kyoto event_rx already consumed");
		let synced_checkpoint_tx = self
			.synced_checkpoint_tx
			.lock()
			.unwrap()
			.take()
			.expect("kyoto synced_checkpoint_tx already consumed");
		// Clone the requester handle once so async calls inside the loop don't need to lock.
		let requester =
			self.requester.lock().unwrap().as_ref().expect("kyoto requester not set").clone();

		loop {
			tokio::select! {
				_ = stop_sync_receiver.changed() => {
					let _ = requester.shutdown();
					return;
				}
				event = event_rx.recv() => {
					let Some(event) = event else {
						log_error!(self.logger, "Kyoto node stopped unexpectedly.");
						return;
					};
					match event {
						bip157::Event::ChainUpdate(changes) => {
							match changes {
								BlockHeaderChanges::Connected(indexed_header) => {
									// Cache the header so we can look it up when the corresponding `IndexedFilter` event arrives.
									self.header_cache.lock().unwrap().insert(indexed_header.block_hash(), indexed_header);
									// Advance the BDK wallet's LocalChain so that block_connected
									// calls on filter-matching blocks can connect. BDK requires the
									// parent block to already be in the chain.
									onchain_wallet.apply_header(&indexed_header.header, indexed_header.height);
								},
								BlockHeaderChanges::Reorganized { accepted, reorganized } => {
									// Notify LDK of the reorg using the fork-point block,
									// i.e. the last block common to both forks.
									if let Some(first_disconnected) = reorganized.first() {
										let fork_hash = first_disconnected.header.prev_blockhash;
										let fork_height = first_disconnected.height.saturating_sub(1);
										let fork_point = BestBlock::new(fork_hash, fork_height);
										channel_manager.blocks_disconnected(fork_point);
										chain_monitor.blocks_disconnected(fork_point);
										output_sweeper.blocks_disconnected(fork_point);
										// BDK wallet treats disconnections as a no-op.
										onchain_wallet.blocks_disconnected(fork_point);
									}
									// Update the header cache: drop stale entries and cache
									// the newly-accepted headers whose filters will follow.
									let mut cache = self.header_cache.lock().unwrap();
									for h in &reorganized {
										cache.remove(&h.block_hash());
									}
									for h in &accepted {
										cache.insert(h.block_hash(), *h);
										onchain_wallet.apply_header(&h.header, h.height);
									}
								},
								BlockHeaderChanges::ForkAdded(_) => {
									// A competing fork that hasn't overtaken the main chain;
									// nothing to do.
								},
							}
						},
						bip157::Event::IndexedFilter(filter) => {
							let block_hash = filter.block_hash();
							let height = filter.height();

							// Retrieve the cached header — it must have arrived in an
							// earlier `ChainUpdate::Connected` event.
							let indexed_header = self.header_cache.lock().unwrap().get(&block_hash).copied();
							let Some(indexed_header) = indexed_header else {
								log_error!(self.logger, "BIP157: missing cached header for block {} at height {}; skipping block.", block_hash, height);
								continue;
							};

							// Skip blocks already in LDK's chain. On restart kyoto re-emits
							// blocks from the last checkpoint, but LDK's in-memory state is
							// preserved across stop/start so feeding it old blocks would panic.
							let ldk_tip_height = channel_manager.current_best_block().height;
							if height <= ldk_tip_height {
								self.header_cache.lock().unwrap().remove(&block_hash);
								continue;
							}

							let has_match = {
								let scripts = self.watched_scripts.lock().unwrap();
								filter.contains_any(scripts.iter())
							};

							if has_match {
								log_debug!(self.logger, "BIP157: compact filter matched at height {}; fetching full block.", height);
								match requester.get_block(block_hash).await {
									Ok(indexed_block) => {
										log_info!(self.logger, "BIP157: block_connected height={} txs={}", height, indexed_block.block.txdata.len());
										onchain_wallet.block_connected(&indexed_block.block, height);
										channel_manager.block_connected(&indexed_block.block, height);
										log_info!(self.logger, "BIP157: channel_manager block_connected done height={}", height);
										chain_monitor.block_connected(&indexed_block.block, height);
										output_sweeper.block_connected(&indexed_block.block, height);
									},
									Err(e) => {
										log_error!(self.logger, "BIP157: failed to fetch block {} at height {}: {}", block_hash, height, e);
									},
								}
							} else {
								log_info!(self.logger, "BIP157: filtered_block_connected height={} (no match)", height);
								// No script match: tell LDK the block passed but carry no
								// transactions.  BDK does not need non-matching blocks.
								let txdata = [];
								channel_manager.filtered_block_connected(&indexed_header.header, &txdata, height);
								log_info!(self.logger, "BIP157: channel_manager filtered_block_connected done height={}", height);
								chain_monitor.filtered_block_connected(&indexed_header.header, &txdata, height);
								output_sweeper.filtered_block_connected(&indexed_header.header, &txdata, height);
							}

							// Header no longer needed after the filter is processed.
							self.header_cache.lock().unwrap().remove(&block_hash);
						},
						bip157::Event::Block(_indexed_block) => {
							// Blocks requested via `requester.get_block()` are delivered
							// directly via the oneshot channel in the `IndexedFilter`
							// handler above.  This event path is unused in the current
							// version of kyoto.
							log_debug!(self.logger, "BIP157: received unexpected Event::Block; ignoring.");
						},
						bip157::Event::FiltersSynced(update) => {
							let tip = update.tip();
							log_info!(self.logger, "BIP157 compact filters synced to height {}.", tip.height);
							*self.synced_checkpoint.lock().unwrap() = Some(tip);
							let _ = synced_checkpoint_tx.send(Some(tip));
						},
					}
				}
				Some(warn) = warn_rx.recv() => {
					log_debug!(self.logger, "Kyoto: {}", warn);
				}
			}
		}
	}

	/// Block until the background sync loop has processed all blocks up to the current chain tip.
	///
	/// Gets the current tip from the kyoto node, then waits for `FiltersSynced` to fire at or
	/// beyond that height. If the loop has already synced past the tip this returns immediately.
	pub(super) async fn wait_until_synced(&self) -> Result<(), Error> {
		let requester =
			self.requester.lock().unwrap().as_ref().ok_or(Error::WalletOperationFailed)?.clone();
		let tip = requester.chain_tip().await.map_err(|_| Error::WalletOperationFailed)?;
		let mut rx = self.synced_checkpoint_rx.lock().unwrap().clone();
		rx.wait_for(|v: &Option<HeaderCheckpoint>| v.map_or(false, |cp| cp.height >= tip.height))
			.await
			.map(|_| ())
			.map_err(|_| Error::WalletOperationFailed)
	}

	pub(super) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		// Returns None when the update should be skipped without error (e.g. no tip yet).
		let new_fee_rate_cache = match &self.fee_source {
			FeeSource::Kyoto => self.fee_rate_cache_from_kyoto().await?,
			FeeSource::Esplora { client, timeout_secs } => {
				Some(self.fee_rate_cache_from_esplora(client, *timeout_secs).await?)
			},
			FeeSource::Electrum { server_url } => {
				Some(self.fee_rate_cache_from_electrum(server_url).await?)
			},
		};

		let Some(new_fee_rate_cache) = new_fee_rate_cache else {
			return Ok(());
		};

		self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache);

		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		{
			let mut locked_node_metrics = self.node_metrics.write().unwrap();
			locked_node_metrics.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt;
			write_node_metrics(&*locked_node_metrics, &*self.kv_store, &*self.logger)?;
		}

		Ok(())
	}

	/// Derive a uniform fee rate from the latest block's coinbase reward minus the block subsidy.
	///
	/// Returns `Ok(None)` when no chain tip is available yet (first startup before sync).
	async fn fee_rate_cache_from_kyoto(
		&self,
	) -> Result<Option<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>>, Error> {
		let checkpoint = *self.synced_checkpoint.lock().unwrap();
		let Some(checkpoint) = checkpoint else {
			log_debug!(self.logger, "BIP157 fee estimation skipped: no chain tip available yet.");
			return Ok(None);
		};

		if checkpoint.height == 0 {
			// Genesis is never stored in kyoto's peer-downloaded headers map, nothing to estimate.
			log_debug!(self.logger, "BIP157 fee estimation skipped: chain tip is at genesis.");
			return Ok(None);
		}

		let requester = self
			.requester
			.lock()
			.unwrap()
			.as_ref()
			.ok_or(Error::FeerateEstimationUpdateFailed)?
			.clone();
		let fee_rate = requester
			.average_fee_rate(checkpoint.hash)
			.await
			.map_err(|_| Error::FeerateEstimationUpdateFailed)?;

		let mut cache = HashMap::with_capacity(10);
		for target in get_all_conf_targets() {
			let adjusted = apply_post_estimation_adjustments(target, fee_rate);
			log_debug!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted.to_sat_per_kwu()
			);
			cache.insert(target, adjusted);
		}
		Ok(Some(cache))
	}

	/// Fetch per-target fee rates from an Esplora server.
	async fn fee_rate_cache_from_esplora(
		&self, client: &EsploraAsyncClient, timeout_secs: u64,
	) -> Result<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>, Error> {
		let estimates =
			tokio::time::timeout(Duration::from_secs(timeout_secs), client.get_fee_estimates())
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
				"Failed to retrieve fee rate estimates: empty estimates are disallowed on Mainnet."
			);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let mut cache = HashMap::with_capacity(10);
		for target in get_all_conf_targets() {
			let num_blocks = get_num_block_defaults_for_target(target);
			let converted = esplora_client::convert_fee_rate(num_blocks, estimates.clone())
				.map_or(1.0_f32, |r| r.max(1.0));
			let fee_rate = FeeRate::from_sat_per_kwu((converted * 250.0_f32) as u64);
			let adjusted = apply_post_estimation_adjustments(target, fee_rate);
			log_debug!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted.to_sat_per_kwu()
			);
			cache.insert(target, adjusted);
		}
		Ok(cache)
	}

	/// Fetch per-target fee rates from an Electrum server.
	///
	/// Opens a fresh connection for each call; `ElectrumClient` is not `Sync` so it cannot be
	/// stored as a struct field and shared across async tasks.
	async fn fee_rate_cache_from_electrum(
		&self, server_url: &str,
	) -> Result<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>, Error> {
		let server_url = server_url.to_owned();
		let confirmation_targets = get_all_conf_targets();

		let raw_estimates = tokio::time::timeout(
			Duration::from_secs(DEFAULT_FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
			tokio::task::spawn_blocking(move || {
				let electrum_config = ElectrumConfigBuilder::new()
					.retry(ELECTRUM_CLIENT_NUM_RETRIES)
					.timeout(Some(DEFAULT_PER_REQUEST_TIMEOUT_SECS))
					.build();
				let client = ElectrumClient::from_config(&server_url, electrum_config)
					.map_err(|_| Error::FeerateEstimationUpdateFailed)?;
				let mut batch = Batch::default();
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

		if raw_estimates.len() != confirmation_targets.len()
			&& self.config.network == Network::Bitcoin
		{
			log_error!(
				self.logger,
				"Failed to retrieve fee rate estimates: Electrum server didn't return all expected results."
			);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let mut cache = HashMap::with_capacity(10);
		for (target, raw_rate) in confirmation_targets.into_iter().zip(raw_estimates.into_iter()) {
			// Electrum returns BTC/KvB; fall back to 1 sat/vb (= 0.00001 BTC/KvB) on failure.
			let fee_rate_btc_per_kvb = raw_rate.as_f64().map_or(0.00001_f64, |v| v.max(0.00001));
			// Convert BTC/KvB → sat/kwu: multiply by 25_000_000 (= 10^8 / 4).
			let fee_rate =
				FeeRate::from_sat_per_kwu((fee_rate_btc_per_kvb * 25_000_000.0).round() as u64);
			let adjusted = apply_post_estimation_adjustments(target, fee_rate);
			log_debug!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted.to_sat_per_kwu()
			);
			cache.insert(target, adjusted);
		}
		Ok(cache)
	}

	pub(super) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
		let requester = match self.requester.lock().unwrap().as_ref() {
			Some(r) => r.clone(),
			None => return,
		};
		for tx in package {
			match requester.broadcast_tx(tx).await {
				Ok(wtxid) => {
					log_debug!(self.logger, "BIP157: broadcast transaction {}", wtxid);
				},
				Err(e) => {
					log_error!(self.logger, "BIP157: failed to broadcast transaction: {}", e);
				},
			}
		}
	}
}
