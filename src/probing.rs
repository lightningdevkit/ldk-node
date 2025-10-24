use std::sync::{Arc, RwLock};

use crate::{
	config::Config,
	graph::NetworkGraph,
	logger::{log_debug, log_error, LdkLogger, Logger},
	payment::SpontaneousPayment,
	types::{ChannelManager, Graph, KeysManager, PaymentStore},
};

/// Configuration for the probing service used to evaluate channel liquidity by sending pre-flight
/// probes to peers and routes.
pub struct ProbingService {
	pub probing_interval_secs: u64,
	max_probing_targets: usize,
	probing_amount_msat: u64,
	config: Arc<Config>,
	logger: Arc<Logger>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	is_running: Arc<RwLock<bool>>,
	payment_store: Arc<PaymentStore>,
	network_graph: Arc<Graph>,
}

impl ProbingService {
	/// Creates a new probing service with the given configuration and dependencies.
	pub fn new(
		probing_interval_secs: u64, max_probing_targets: usize, probing_amount_msat: u64,
		config: Arc<Config>, logger: Arc<Logger>, channel_manager: Arc<ChannelManager>,
		keys_manager: Arc<KeysManager>, is_running: Arc<RwLock<bool>>,
		payment_store: Arc<PaymentStore>, network_graph: Arc<Graph>,
	) -> Self {
		Self {
			probing_interval_secs,
			max_probing_targets,
			probing_amount_msat,
			config,
			logger,
			channel_manager,
			keys_manager,
			is_running,
			payment_store,
			network_graph,
		}
	}

	pub fn handle_probing(&self) {
		let channels = self.channel_manager.list_channels().len();
		if channels == 0 {
			log_debug!(self.logger, "Probing service found no channels, skipping probing.");
			return;
		}

		let network = self.network_graph();
		let spontaneous_payment = self.spontaneous_payment();

		let targets = network.select_probing_targets(self.max_probing_targets);
		for target in targets {
			let public_key = match target.as_pubkey() {
				Ok(pk) => pk,
				Err(_) => {
					log_error!(
						self.logger,
						"Probing service failed to get target pubkey: {}",
						target
					);
					continue;
				},
			};

			match spontaneous_payment.send_probes(self.probing_amount_msat, public_key) {
				Ok(_) => {
					log_debug!(self.logger, "Probing service sent probe to target: {}", public_key)
				},
				Err(e) => log_error!(
					self.logger,
					"Probing service failed to send probe to target {}: {}",
					public_key,
					e
				),
			}
		}
	}

	fn network_graph(&self) -> NetworkGraph {
		NetworkGraph::new(Arc::clone(&self.network_graph))
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
