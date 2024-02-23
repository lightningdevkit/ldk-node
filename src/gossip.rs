use crate::logger::{log_trace, FilesystemLogger, Logger};
use crate::types::{GossipSync, NetworkGraph, P2PGossipSync, RapidGossipSync};
use crate::Error;

use lightning::routing::utxo::UtxoLookup;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub(crate) enum GossipSource {
	P2PNetwork {
		gossip_sync: Arc<P2PGossipSync>,
	},
	RapidGossipSync {
		gossip_sync: Arc<RapidGossipSync>,
		server_url: String,
		latest_sync_timestamp: AtomicU32,
		logger: Arc<FilesystemLogger>,
	},
}

impl GossipSource {
	pub fn new_p2p(network_graph: Arc<NetworkGraph>, logger: Arc<FilesystemLogger>) -> Self {
		let gossip_sync = Arc::new(P2PGossipSync::new(
			network_graph,
			None::<Arc<dyn UtxoLookup + Send + Sync>>,
			logger,
		));
		Self::P2PNetwork { gossip_sync }
	}

	pub fn new_rgs(
		server_url: String, latest_sync_timestamp: u32, network_graph: Arc<NetworkGraph>,
		logger: Arc<FilesystemLogger>,
	) -> Self {
		let gossip_sync = Arc::new(RapidGossipSync::new(network_graph, Arc::clone(&logger)));
		let latest_sync_timestamp = AtomicU32::new(latest_sync_timestamp);
		Self::RapidGossipSync { gossip_sync, server_url, latest_sync_timestamp, logger }
	}

	pub fn is_rgs(&self) -> bool {
		if let Self::RapidGossipSync { .. } = self {
			true
		} else {
			false
		}
	}

	pub fn as_gossip_sync(&self) -> GossipSync {
		match self {
			Self::RapidGossipSync { gossip_sync, .. } => {
				GossipSync::Rapid(Arc::clone(&gossip_sync))
			},
			Self::P2PNetwork { gossip_sync, .. } => GossipSync::P2P(Arc::clone(&gossip_sync)),
		}
	}

	pub async fn update_rgs_snapshot(&self) -> Result<u32, Error> {
		match self {
			Self::P2PNetwork { gossip_sync: _ } => Ok(0),
			Self::RapidGossipSync { gossip_sync, server_url, latest_sync_timestamp, logger } => {
				let query_timestamp = latest_sync_timestamp.load(Ordering::Acquire);
				let query_url = format!("{}/{}", server_url, query_timestamp);
				let response = reqwest::get(query_url).await.map_err(|e| {
					log_trace!(logger, "Failed to retrieve RGS gossip update: {}", e);
					Error::GossipUpdateFailed
				})?;

				match response.error_for_status() {
					Ok(res) => {
						let update_data = res.bytes().await.map_err(|e| {
							log_trace!(logger, "Failed to retrieve RGS gossip update: {}", e);
							Error::GossipUpdateFailed
						})?;

						let new_latest_sync_timestamp = gossip_sync
							.update_network_graph(&update_data)
							.map_err(|_| Error::GossipUpdateFailed)?;
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
