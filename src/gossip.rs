// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::chain::ChainSource;
use crate::config::RGS_SYNC_TIMEOUT_SECS;
use crate::logger::{log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{GossipSync, Graph, P2PGossipSync, PeerManager, RapidGossipSync, UtxoLookup};
use crate::Error;

use lightning_block_sync::gossip::{FutureSpawner, GossipVerifier};

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub(crate) enum GossipSource {
	P2PNetwork {
		gossip_sync: Arc<P2PGossipSync>,
	},
	RapidGossipSync {
		gossip_sync: Arc<RapidGossipSync>,
		server_url: String,
		latest_sync_timestamp: AtomicU32,
		logger: Arc<Logger>,
	},
}

impl GossipSource {
	pub fn new_p2p(network_graph: Arc<Graph>, logger: Arc<Logger>) -> Self {
		let gossip_sync = Arc::new(P2PGossipSync::new(
			network_graph,
			None::<Arc<UtxoLookup>>,
			Arc::clone(&logger),
		));
		Self::P2PNetwork { gossip_sync }
	}

	pub fn new_rgs(
		server_url: String, latest_sync_timestamp: u32, network_graph: Arc<Graph>,
		logger: Arc<Logger>,
	) -> Self {
		let gossip_sync = Arc::new(RapidGossipSync::new(network_graph, Arc::clone(&logger)));
		let latest_sync_timestamp = AtomicU32::new(latest_sync_timestamp);
		Self::RapidGossipSync { gossip_sync, server_url, latest_sync_timestamp, logger }
	}

	pub fn is_rgs(&self) -> bool {
		matches!(self, Self::RapidGossipSync { .. })
	}

	pub fn as_gossip_sync(&self) -> GossipSync {
		match self {
			Self::RapidGossipSync { gossip_sync, .. } => GossipSync::Rapid(Arc::clone(gossip_sync)),
			Self::P2PNetwork { gossip_sync, .. } => GossipSync::P2P(Arc::clone(gossip_sync)),
		}
	}

	pub(crate) fn set_gossip_verifier(
		&self, chain_source: Arc<ChainSource>, peer_manager: Arc<PeerManager>,
		runtime: Arc<Runtime>,
	) {
		match self {
			Self::P2PNetwork { gossip_sync } => {
				if let Some(utxo_source) = chain_source.as_utxo_source() {
					let spawner = RuntimeSpawner::new(Arc::clone(&runtime));
					let gossip_verifier = Arc::new(GossipVerifier::new(
						utxo_source,
						spawner,
						Arc::clone(gossip_sync),
						peer_manager,
					));
					gossip_sync.add_utxo_lookup(Some(gossip_verifier));
				}
			},
			_ => (),
		}
	}

	pub async fn update_rgs_snapshot(&self) -> Result<u32, Error> {
		match self {
			Self::P2PNetwork { gossip_sync: _, .. } => Ok(0),
			Self::RapidGossipSync { gossip_sync, server_url, latest_sync_timestamp, logger } => {
				let query_timestamp = latest_sync_timestamp.load(Ordering::Acquire);
				let query_url = format!("{}/{}", server_url, query_timestamp);

				let response = tokio::time::timeout(
					Duration::from_secs(RGS_SYNC_TIMEOUT_SECS),
					reqwest::get(query_url),
				)
				.await
				.map_err(|e| {
					log_trace!(logger, "Retrieving RGS gossip update timed out: {}", e);
					Error::GossipUpdateTimeout
				})?
				.map_err(|e| {
					log_trace!(logger, "Failed to retrieve RGS gossip update: {}", e);
					Error::GossipUpdateFailed
				})?;

				match response.error_for_status() {
					Ok(res) => {
						let update_data = res.bytes().await.map_err(|e| {
							log_trace!(logger, "Failed to retrieve RGS gossip update: {}", e);
							Error::GossipUpdateFailed
						})?;

						let new_latest_sync_timestamp =
							gossip_sync.update_network_graph(&update_data).map_err(|e| {
								log_trace!(
									logger,
									"Failed to update network graph with RGS data: {:?}",
									e
								);
								Error::GossipUpdateFailed
							})?;
						latest_sync_timestamp.store(new_latest_sync_timestamp, Ordering::Release);
						Ok(new_latest_sync_timestamp)
					},
					Err(e) => {
						log_trace!(logger, "Failed to retrieve RGS gossip update: {}", e);
						Err(Error::GossipUpdateFailed)
					},
				}
			},
		}
	}
}

pub(crate) struct RuntimeSpawner {
	runtime: Arc<Runtime>,
}

impl RuntimeSpawner {
	pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
		Self { runtime }
	}
}

impl FutureSpawner for RuntimeSpawner {
	fn spawn<T: Future<Output = ()> + Send + 'static>(&self, future: T) {
		self.runtime.spawn_cancellable_background_task(future);
	}
}
