// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::RGS_SYNC_TIMEOUT_SECS;
use crate::logger::{log_trace, LdkNodeLogger, Logger};
use crate::types::{GossipSync, Graph, P2PGossipSync, RapidGossipSync};
use crate::Error;

use lightning::routing::utxo::UtxoLookup;

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
		logger: Arc<LdkNodeLogger>,
	},
}

impl GossipSource {
	pub fn new_p2p(network_graph: Arc<Graph>, logger: Arc<LdkNodeLogger>) -> Self {
		let gossip_sync = Arc::new(P2PGossipSync::new(
			network_graph,
			None::<Arc<dyn UtxoLookup + Send + Sync>>,
			logger,
		));
		Self::P2PNetwork { gossip_sync }
	}

	pub fn new_rgs(
		server_url: String, latest_sync_timestamp: u32, network_graph: Arc<Graph>,
		logger: Arc<LdkNodeLogger>,
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

	pub async fn update_rgs_snapshot(&self, do_full_sync: bool) -> Result<u32, Error> {
		match self {
			Self::P2PNetwork { gossip_sync: _ } => Ok(0),
			Self::RapidGossipSync { gossip_sync, server_url, latest_sync_timestamp, logger } => {
				let query_timestamp =
					if do_full_sync { 0 } else { latest_sync_timestamp.load(Ordering::Acquire) };
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
