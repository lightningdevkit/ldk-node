use bitcoin::secp256k1::PublicKey;
use std::sync::{Arc, Mutex, RwLock};

use crate::{
	config::Config,
	graph::NetworkGraph,
	logger::{log_debug, log_error, LdkLogger, Logger},
	payment::SpontaneousPayment,
	types::{ChannelManager, Graph, KeysManager, PaymentStore},
};

/// Trait for probing strategies to select targets for liquidity assessment.
pub trait ProbingStrategy {
	/// Loads the targets to be used in the current probing cycle.
	///
	/// Called at the start of each probing cycle before sending probes.
	fn load_targets(&self);

	/// Returns the next target public key for probing, or None if no more targets are available.
	fn next_target(&self) -> Option<PublicKey>;
}

/// Simple strategy that selects targets based on highest channel capacity.
pub struct HighCapacityStrategy {
	network_graph: Arc<Graph>,
	max_targets_per_cycle: usize,
	target_cache_reuse_limit: usize,
	targets: Mutex<Vec<PublicKey>>,
	target_index: Mutex<usize>,
	uses_since_load: Mutex<usize>,
}

impl HighCapacityStrategy {
	pub fn new(network_graph: Arc<Graph>, max_targets: usize, max_reloads: usize) -> Self {
		Self {
			network_graph,
			max_targets_per_cycle: max_targets,
			target_cache_reuse_limit: max_reloads,
			targets: Mutex::new(Vec::new()),
			target_index: Mutex::new(0),
			uses_since_load: Mutex::new(0),
		}
	}

	fn network_graph(&self) -> NetworkGraph {
		NetworkGraph::new(Arc::clone(&self.network_graph))
	}
}

impl ProbingStrategy for HighCapacityStrategy {
	fn load_targets(&self) {
		let mut targets = self.targets.lock().unwrap();
		let mut uses_since_load = self.uses_since_load.lock().unwrap();

		if !targets.is_empty() && *uses_since_load < self.target_cache_reuse_limit {
			*uses_since_load += 1;
			return;
		}

		let network = self.network_graph();
		let highest_capacity_nodes =
			network.select_highest_capacity_nodes(self.max_targets_per_cycle);
		*targets =
			highest_capacity_nodes.iter().filter_map(|node_id| node_id.as_pubkey().ok()).collect();

		let mut target_index = self.target_index.lock().unwrap();
		*target_index = 0;
		*uses_since_load = 0;
	}

	fn next_target(&self) -> Option<PublicKey> {
		let mut target_index = self.target_index.lock().unwrap();
		let targets = self.targets.lock().unwrap();

		if *target_index < targets.len() {
			let pk = targets[*target_index];
			*target_index += 1;
			Some(pk)
		} else {
			// reset index for next cycle
			*target_index = 0;
			None
		}
	}
}

/// Configuration for the probing service used to evaluate channel liquidity by sending pre-flight
/// probes to peers and routes.
pub struct ProbingService {
	pub probing_interval_secs: u64,
	probing_amount_msat: u64,
	strategy: Arc<dyn ProbingStrategy + Send + Sync>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	is_running: Arc<RwLock<bool>>,
	payment_store: Arc<PaymentStore>,
}

impl ProbingService {
	/// Creates a new probing service with the given configuration and dependencies.
	pub fn new(
		probing_interval_secs: u64, probing_amount_msat: u64,
		strategy: Arc<dyn ProbingStrategy + Send + Sync>, config: Arc<Config>, logger: Arc<Logger>,
		channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		is_running: Arc<RwLock<bool>>, payment_store: Arc<PaymentStore>,
	) -> Self {
		Self {
			probing_interval_secs,
			probing_amount_msat,
			strategy,
			config,
			logger,
			channel_manager,
			keys_manager,
			is_running,
			payment_store,
		}
	}

	pub fn handle_probing(&self) {
		self.strategy.load_targets();
		loop {
			if let Some(target) = self.strategy.next_target() {
				let spontaneous_payment = self.spontaneous_payment();
				match spontaneous_payment.send_probes(self.probing_amount_msat, target) {
					Ok(_) => {
						log_debug!(self.logger, "Probing service sent probe to target: {}", target)
					},
					Err(e) => log_error!(
						self.logger,
						"Probing service failed to send probe to target {}: {}",
						target,
						e
					),
				}
			} else {
				break;
			}
		}
	}

	fn spontaneous_payment(&self) -> SpontaneousPayment {
		SpontaneousPayment::new(
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
		)
	}
}
