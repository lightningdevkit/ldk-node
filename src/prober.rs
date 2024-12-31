// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a prober object that can be used to send probes to a destination node
//! outside of regular payment flows.

use std::{
	sync::{Arc, Mutex},
	time::{SystemTime, UNIX_EPOCH},
};

use crate::{
	error::Error,
	logger::LdkNodeLogger,
	types::{ChannelManager, Router, Scorer},
};
use bitcoin::secp256k1::PublicKey;
use lightning::{
	ln::{channel_state::ChannelDetails, channelmanager::PaymentId, PaymentHash},
	log_error,
	routing::{
		router::{Path, PaymentParameters, Route, RouteParameters, Router as _},
		scoring::ScoreUpdate as _,
	},
	util::logger::Logger as _,
};

/// The Prober can be used to send probes to a destination node outside of regular payment flows.
pub struct Prober {
	channel_manager: Arc<ChannelManager>,
	router: Arc<Router>,
	scorer: Arc<Mutex<Scorer>>,
	logger: Arc<LdkNodeLogger>,
	node_id: PublicKey,
}

impl Prober {
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, router: Arc<Router>, scorer: Arc<Mutex<Scorer>>,
		logger: Arc<LdkNodeLogger>, node_id: PublicKey,
	) -> Self {
		Self { channel_manager, router, scorer, logger, node_id }
	}

	/// Find a route from the node to a given destination on the network.
	pub fn find_route(
		&self, destination: PublicKey, amount_msat: u64, attempts: u8, cltv_expiry_delta: u32,
	) -> Result<Route, Error> {
		let payment_params = PaymentParameters::from_node_id(destination, cltv_expiry_delta)
			.with_max_path_count(attempts);
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

	/// Updates the scorer based on the probe result.
	pub fn update_scorer(&self, path: &Path, failed_scid: Option<u64>) {
		let duration_since_epoch =
			SystemTime::now().duration_since(UNIX_EPOCH).expect("Time is before UNIX epoch");
		let mut scorer = self.scorer.lock().expect("Lock poisoned");
		if let Some(scid) = failed_scid {
			scorer.probe_failed(path, scid, duration_since_epoch);
		} else {
			scorer.probe_successful(path, duration_since_epoch);
		}
	}
}
