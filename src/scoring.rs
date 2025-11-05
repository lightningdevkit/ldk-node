use std::io::Cursor;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use lightning::routing::scoring::ChannelLiquidities;
use lightning::util::ser::Readable;
use lightning::{log_error, log_info, log_trace};

use crate::config::{
	EXTERNAL_PATHFINDING_SCORES_SYNC_INTERVAL, EXTERNAL_PATHFINDING_SCORES_SYNC_TIMEOUT_SECS,
};
use crate::io::utils::write_external_pathfinding_scores_to_cache;
use crate::logger::LdkLogger;
use crate::runtime::Runtime;
use crate::{write_node_metrics, DynStore, Logger, NodeMetrics, Scorer};

/// Start a background task that periodically downloads scores via an external url and merges them into the local
/// pathfinding scores.
pub fn setup_background_pathfinding_scores_sync(
	url: String, scorer: Arc<Mutex<crate::types::Scorer>>, node_metrics: Arc<RwLock<NodeMetrics>>,
	kv_store: Arc<DynStore>, logger: Arc<Logger>, runtime: Arc<Runtime>,
	mut stop_receiver: tokio::sync::watch::Receiver<()>,
) {
	log_info!(logger, "External scores background syncing enabled from {}", url);

	let logger = Arc::clone(&logger);

	runtime.spawn_background_processor_task(async move {
		let mut interval = tokio::time::interval(EXTERNAL_PATHFINDING_SCORES_SYNC_INTERVAL);
		loop {
			tokio::select! {
				_ = stop_receiver.changed() => {
					log_trace!(
						logger,
						"Stopping background syncing external scores.",
					);
					return;
				}
				_ = interval.tick() => {
					log_trace!(
						logger,
						"Background sync of external scores started.",
					);

					sync_external_scores(logger.as_ref(), scorer.as_ref(), node_metrics.as_ref(), Arc::clone(&kv_store), &url).await;
				}
			}
		}
	});
}

async fn sync_external_scores(
	logger: &Logger, scorer: &Mutex<Scorer>, node_metrics: &RwLock<NodeMetrics>,
	kv_store: Arc<DynStore>, url: &String,
) -> () {
	let response = tokio::time::timeout(
		Duration::from_secs(EXTERNAL_PATHFINDING_SCORES_SYNC_TIMEOUT_SECS),
		reqwest::get(url),
	)
	.await;

	let response = match response {
		Ok(resp) => resp,
		Err(e) => {
			log_error!(logger, "Retrieving external scores timed out: {}", e);
			return;
		},
	};
	let response = match response {
		Ok(resp) => resp,
		Err(e) => {
			log_error!(logger, "Failed to retrieve external scores update: {}", e);
			return;
		},
	};
	let body = match response.bytes().await {
		Ok(bytes) => bytes,
		Err(e) => {
			log_error!(logger, "Failed to read external scores update: {}", e);
			return;
		},
	};
	let mut reader = Cursor::new(body);
	match ChannelLiquidities::read(&mut reader) {
		Ok(liquidities) => {
			if let Err(e) = write_external_pathfinding_scores_to_cache(
				Arc::clone(&kv_store),
				&liquidities,
				logger,
			)
			.await
			{
				log_error!(logger, "Failed to persist external scores to cache: {}", e);
			}

			let duration_since_epoch =
				SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
			scorer.lock().unwrap().merge(liquidities, duration_since_epoch);
			let mut locked_node_metrics = node_metrics.write().unwrap();
			locked_node_metrics.latest_pathfinding_scores_sync_timestamp =
				Some(duration_since_epoch.as_secs());
			write_node_metrics(&*locked_node_metrics, Arc::clone(&kv_store), logger)
				.unwrap_or_else(|e| {
					log_error!(logger, "Persisting node metrics failed: {}", e);
				});
			log_trace!(logger, "External scores merged successfully");
		},
		Err(e) => {
			log_error!(logger, "Failed to parse external scores update: {}", e);
		},
	}
}
