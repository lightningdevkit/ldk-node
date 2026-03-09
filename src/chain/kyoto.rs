// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::{BlockHash, FeeRate, Network, Transaction};
use bip157::Info;
use electrum_client::{Batch, Client as ElectrumClient, ConfigBuilder as ElectrumConfigBuilder, ElectrumApi};
use esplora_client::AsyncClient as EsploraAsyncClient;
use tokio::sync::mpsc;

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

/// How long to wait for kyoto to establish the required peer connections before
/// considering the startup failed.
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
	/// The kyoto node, held until `start()` spawns it.
	node: Mutex<Option<bip157::Node>>,
	/// Info channel, held until `start()` uses it to wait for initial connections.
	info_rx: Mutex<Option<mpsc::Receiver<Info>>>,
	/// The remaining client channels, held until `continuously_sync_wallets()` takes them.
	warn_rx: Mutex<Option<mpsc::UnboundedReceiver<bip157::Warning>>>,
	event_rx: Mutex<Option<mpsc::UnboundedReceiver<bip157::Event>>>,
	/// Cloneable handle used to send commands (e.g. shutdown) to the running node.
	requester: bip157::Requester,
	/// The most recently observed chain tip block hash, updated by `FiltersSynced` events.
	/// Only used when `fee_source` is [`FeeSource::Kyoto`].
	tip_hash: Mutex<Option<BlockHash>>,
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
			.add_peers(peers.into_iter().map(Into::into))
			.data_dir(data_dir)
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

		Self {
			node: Mutex::new(Some(node)),
			info_rx: Mutex::new(Some(info_rx)),
			warn_rx: Mutex::new(Some(warn_rx)),
			event_rx: Mutex::new(Some(event_rx)),
			requester,
			tip_hash: Mutex::new(None),
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
	/// Returns `Err(Error::ConnectionFailed)` if no connection is established within
	/// [`KYOTO_CONNECTION_TIMEOUT`] or if the node exits before connecting.
	pub(super) fn start(&self, runtime: Arc<Runtime>) -> Result<(), Error> {
		let node = self.node.lock().unwrap().take().expect("kyoto node already started");
		let mut info_rx =
			self.info_rx.lock().unwrap().take().expect("kyoto info_rx already taken");
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
		let _ = self.requester.shutdown();
	}

	/// Main sync loop driven by kyoto events. Runs for the lifetime of the node.
	pub(super) async fn continuously_sync_wallets(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		_onchain_wallet: Arc<Wallet>, _channel_manager: Arc<ChannelManager>,
		_chain_monitor: Arc<ChainMonitor>, _output_sweeper: Arc<Sweeper>,
	) {
		let mut warn_rx =
			self.warn_rx.lock().unwrap().take().expect("kyoto warn_rx already consumed");
		let mut event_rx =
			self.event_rx.lock().unwrap().take().expect("kyoto event_rx already consumed");

		loop {
			tokio::select! {
				_ = stop_sync_receiver.changed() => {
					let _ = self.requester.shutdown();
					return;
				}
				event = event_rx.recv() => {
					let Some(event) = event else {
						log_error!(self.logger, "Kyoto node stopped unexpectedly.");
						return;
					};
					match event {
						bip157::Event::ChainUpdate(_changes) => {
							// TODO: notify LDK of connected/disconnected blocks via the
							// `Listen` trait (`block_connected` / `block_disconnected`).
							todo!("Handle chain update")
						},
						bip157::Event::IndexedFilter(_filter) => {
							// TODO: check the filter against wallet script pubkeys;
							// if a match, request the full block via `requester.get_block()`.
							todo!("Handle indexed filter")
						},
						bip157::Event::Block(_indexed_block) => {
							// TODO: apply the full block to the onchain wallet and LDK.
							todo!("Handle block")
						},
						bip157::Event::FiltersSynced(update) => {
							let tip = update.tip();
							log_info!(
								self.logger,
								"BIP157 compact filters synced to height {}.",
								tip.height,
							);
							*self.tip_hash.lock().unwrap() = Some(tip.hash);
						},
					}
				}
				Some(warn) = warn_rx.recv() => {
					log_debug!(self.logger, "Kyoto: {}", warn);
				}
			}
		}
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
		let tip_hash = *self.tip_hash.lock().unwrap();
		let Some(hash) = tip_hash else {
			log_debug!(
				self.logger,
				"BIP157 fee estimation skipped: no chain tip available yet."
			);
			return Ok(None);
		};

		let fee_rate = self
			.requester
			.average_fee_rate(hash)
			.await
			.map_err(|_| Error::FeerateEstimationUpdateFailed)?;

		let mut cache = HashMap::with_capacity(10);
		for target in get_all_conf_targets() {
			let adjusted = apply_post_estimation_adjustments(target, fee_rate);
			log_debug!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted.to_sat_per_kwu(),
			);
			cache.insert(target, adjusted);
		}
		Ok(Some(cache))
	}

	/// Fetch per-target fee rates from an Esplora server.
	async fn fee_rate_cache_from_esplora(
		&self, client: &EsploraAsyncClient, timeout_secs: u64,
	) -> Result<HashMap<crate::fee_estimator::ConfirmationTarget, FeeRate>, Error> {
		let estimates = tokio::time::timeout(
			Duration::from_secs(timeout_secs),
			client.get_fee_estimates(),
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
				adjusted.to_sat_per_kwu(),
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
			let fee_rate = FeeRate::from_sat_per_kwu((fee_rate_btc_per_kvb * 25_000_000.0).round() as u64);
			let adjusted = apply_post_estimation_adjustments(target, fee_rate);
			log_debug!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted.to_sat_per_kwu(),
			);
			cache.insert(target, adjusted);
		}
		Ok(cache)
	}

	pub(super) async fn process_broadcast_package(&self, _package: Vec<Transaction>) {
		// TODO: broadcast transactions via the kyoto P2P network using
		// `requester.broadcast_tx()`.
		todo!("Transaction broadcasting is not yet supported for the BIP157 chain source")
	}
}
