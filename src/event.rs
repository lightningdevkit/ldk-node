use crate::{
	ChannelManager, EventSender, LdkLiteChainAccess, LdkLiteConfig, NetworkGraph,
	PaymentInfoStorage,
};

#[allow(unused_imports)]
use crate::logger::{Logger, FilesystemLogger, log_info, log_error, log_warn, log_trace, log_given_level, log_internal};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::keysinterface::KeysManager;
use lightning::util::events as ldk_events;

use bdk::database::MemoryDatabase;

use bitcoin::secp256k1::Secp256k1;
//use std::collections::{hash_map, VecDeque};
//use std::iter::Iterator;
use std::sync::Arc;

/// An LdkLiteEvent that should be handled by the user.
pub enum LdkLiteEvent {
	/// asdf
	Test,
}

pub(crate) struct LdkLiteEventHandler {
	chain_access: Arc<LdkLiteChainAccess<MemoryDatabase>>,
	channel_manager: Arc<ChannelManager>,
	_network_graph: Arc<NetworkGraph>,
	keys_manager: Arc<KeysManager>,
	_inbound_payments: Arc<PaymentInfoStorage>,
	_outbound_payments: Arc<PaymentInfoStorage>,
	_event_sender: EventSender,
	logger: Arc<FilesystemLogger>,
	_config: Arc<LdkLiteConfig>,
}

impl LdkLiteEventHandler {
	pub fn new(
		chain_access: Arc<LdkLiteChainAccess<MemoryDatabase>>,
		channel_manager: Arc<ChannelManager>, _network_graph: Arc<NetworkGraph>,
		keys_manager: Arc<KeysManager>, _inbound_payments: Arc<PaymentInfoStorage>,
		_outbound_payments: Arc<PaymentInfoStorage>, _event_sender: EventSender,
		logger: Arc<FilesystemLogger>, _config: Arc<LdkLiteConfig>,
	) -> Self {
		Self {
			chain_access,
			channel_manager,
			_network_graph,
			keys_manager,
			_inbound_payments,
			_outbound_payments,
			_event_sender,
			logger,
			_config,
		}
	}
}

impl ldk_events::EventHandler for LdkLiteEventHandler {
	// TODO: implement error handling for events (i.e., get rid of any unwraps())
	fn handle_event(&self, event: &ldk_events::Event) {
		match event {
			ldk_events::Event::FundingGenerationReady {
				temporary_channel_id,
				counterparty_node_id,
				channel_value_satoshis,
				output_script,
				..
			} => {
				// Construct the raw transaction with one output, that is paid the amount of the
				// channel.

				// TODO: what is a good default target here?
				let confirmation_target = ConfirmationTarget::Normal;

				// Sign the final funding transaction and broadcast it.
				match self.chain_access.create_funding_transaction(
					&output_script,
					*channel_value_satoshis,
					confirmation_target,
				) {
					Ok(final_tx) => {
						// Give the funding transaction back to LDK for opening the channel.
						if self
							.channel_manager
							.funding_transaction_generated(
								&temporary_channel_id,
								counterparty_node_id,
								final_tx,
							)
							.is_err()
						{
							log_error!(self.logger, "Channel went away before we could fund it. The peer disconnected or refused the channel");
						}
					}
					Err(err) => {
						log_error!(self.logger, "Failed to create funding transaction: {}", err);
					}
				}
			}
			ldk_events::Event::PaymentReceived { .. } => {}
			ldk_events::Event::PaymentClaimed { .. } => {}
			ldk_events::Event::PaymentSent { .. } => {}
			ldk_events::Event::PaymentFailed { .. } => {}
			ldk_events::Event::PaymentPathSuccessful { .. } => {}
			ldk_events::Event::PaymentPathFailed { .. } => {}
			ldk_events::Event::ProbeSuccessful { .. } => {}
			ldk_events::Event::ProbeFailed { .. } => {}
			ldk_events::Event::HTLCHandlingFailed { .. } => {}
			ldk_events::Event::PendingHTLCsForwardable { .. } => {}
			ldk_events::Event::SpendableOutputs { outputs } => {
				let destination_address = self.chain_access.get_new_address().unwrap();
				let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
				let tx_feerate =
					self.chain_access.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
				let spending_tx = self
					.keys_manager
					.spend_spendable_outputs(
						output_descriptors,
						Vec::new(),
						destination_address.script_pubkey(),
						tx_feerate,
						&Secp256k1::new(),
					)
					.unwrap();
				self.chain_access.broadcast_transaction(&spending_tx);
			}
			ldk_events::Event::OpenChannelRequest { .. } => {}
			ldk_events::Event::PaymentForwarded { .. } => {}
			ldk_events::Event::ChannelClosed { .. } => {}
			ldk_events::Event::DiscardFunding { .. } => {}
		}
	}
}
