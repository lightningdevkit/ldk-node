use bitcoin::secp256k1::PublicKey;
use std::sync::{Arc, RwLock};

use crate::{
	config::Config,
	logger::{log_debug, log_error, LdkLogger, Logger},
	payment::SpontaneousPayment,
	types::{ChannelManager, KeysManager, PaymentStore},
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
