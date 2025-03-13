// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a prober object that can be used to send probes to a destination node
//! outside of regular payment flows.

use std::sync::{Arc, Mutex};

use crate::{
	error::Error,
	logger::LdkNodeLogger,
	types::{ChannelManager, Graph, Router, Scorer},
};
use bitcoin::secp256k1::PublicKey;
use lightning::{
	io::Cursor,
	ln::{channel_state::ChannelDetails, channelmanager::PaymentId, PaymentHash},
	log_error,
	routing::{
		router::{Path, PaymentParameters, Route, RouteParameters, Router as _},
		scoring::{
			ProbabilisticScorer, ProbabilisticScoringDecayParameters,
			ProbabilisticScoringFeeParameters,
		},
	},
	util::{
		logger::Logger as _,
		ser::{ReadableArgs as _, Writeable},
	},
};

/// The parameters used to configure the [`ProbabilisticScorer`] used by the node.
#[derive(Debug, Clone, Default)]
pub struct ProbabilisticScoringParameters {
	/// The fee parameters used by the router to compute path penalties.
	pub fee_params: ProbabilisticScoringFeeParameters,
	/// The decay parameters used by the scorer to reduce certainty of liquidity information.
	pub decay_params: ProbabilisticScoringDecayParameters,
}

/// The Prober can be used to send probes to a destination node outside of regular payment flows.
pub struct Prober {
	channel_manager: Arc<ChannelManager>,
	router: Arc<Router>,
	scorer: Arc<Mutex<Scorer>>,
	network_graph: Arc<Graph>,
	logger: Arc<LdkNodeLogger>,
	node_id: PublicKey,
}

impl Prober {
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, router: Arc<Router>, scorer: Arc<Mutex<Scorer>>,
		network_graph: Arc<Graph>, logger: Arc<LdkNodeLogger>, node_id: PublicKey,
	) -> Self {
		Self { channel_manager, router, scorer, network_graph, logger, node_id }
	}

	/// Find a route from the node to a given destination on the network.
	pub fn find_route(
		&self, destination: PublicKey, amount_msat: u64, cltv_expiry_delta: u32,
	) -> Result<Route, Error> {
		let payment_params = PaymentParameters::from_node_id(destination, cltv_expiry_delta);
		let route_params =
			RouteParameters::from_payment_params_and_value(payment_params, amount_msat);
		let usable_channels = self.channel_manager.list_usable_channels();
		let first_hops: Vec<&ChannelDetails> = usable_channels.iter().collect();
		let inflight_htlcs = self.channel_manager.compute_inflight_htlcs();
		self.router
			.find_route(&self.node_id, &route_params, Some(&first_hops[..]), inflight_htlcs)
			.map_err(|e| {
				log_error!(self.logger, "Failed to find route: {e:?}");
				Error::RouteNotFound
			})
	}

	/// Send a probe along the given path returning the payment hash and id of the fake payment.
	pub fn send_probe(&self, path: Path) -> Result<(PaymentHash, PaymentId), Error> {
		self.channel_manager.send_probe(path).map_err(|e| {
			log_error!(self.logger, "Failed to send probe: {e:?}");
			Error::ProbeSendingFailed
		})
	}

	/// Updates the scoring decay parameters while retaining channel liquidity information..
	pub fn update_scoring_decay_params(
		&self, decay_params: ProbabilisticScoringDecayParameters,
	) -> Result<(), std::io::Error> {
		let scorer_bytes = self.export_scorer()?;
		self.import_scorer(scorer_bytes, decay_params)
	}

	/// Export the scorer
	pub fn export_scorer(&self) -> Result<Vec<u8>, std::io::Error> {
		let scorer = self.scorer.lock().expect("Lock poisoned");
		let mut writer = Vec::new();
		scorer.write(&mut writer).map_err(|e| {
			log_error!(self.logger, "Failed to serialize scorer: {}", e);
			std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to serialize Scorer")
		})?;
		Ok(writer)
	}

	/// Import a new scorer
	pub fn import_scorer(
		&self, scorer_bytes: Vec<u8>, decay_params: ProbabilisticScoringDecayParameters,
	) -> Result<(), std::io::Error> {
		let mut reader = Cursor::new(scorer_bytes);
		let args = (decay_params, self.network_graph.clone(), self.logger.clone());
		let new_scorer = ProbabilisticScorer::read(&mut reader, args).map_err(|e| {
			log_error!(self.logger, "Failed to deserialize scorer: {}", e);
			std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize Scorer")
		})?;
		let mut scorer = self.scorer.lock().expect("Lock poisoned");
		*scorer = new_scorer;
		Ok(())
	}

	/// Reset scorer liquidity information
	pub fn reset_scorer(&self, decay_params: ProbabilisticScoringDecayParameters) {
		let new_scorer =
			ProbabilisticScorer::new(decay_params, self.network_graph.clone(), self.logger.clone());
		let mut scorer = self.scorer.lock().expect("Lock poisoned");
		*scorer = new_scorer;
	}
}
