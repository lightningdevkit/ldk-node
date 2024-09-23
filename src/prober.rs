use std::{
	sync::{Arc, Mutex},
	time::{SystemTime, UNIX_EPOCH},
};

use crate::{
	error::Error,
	event::EventQueue,
	logger::FilesystemLogger,
	types::{ChannelManager, Router, Scorer},
	Event,
};
use bitcoin::secp256k1::PublicKey;
use lightning::{
	ln::{channelmanager, msgs::LightningError},
	log_error, log_info,
	routing::{
		router::{Path, PaymentParameters, Route, RouteParameters, Router as _},
		scoring::ScoreUpdate,
	},
	util::logger::Logger as _,
};

pub struct ProbeResult {
	pub success: bool,
	pub paths_attempted: Vec<Path>,
	pub start_time: u64,
	pub end_time: u64,
	pub errors: Vec<String>,
}

pub struct Prober {
	channel_manager: Arc<ChannelManager>,
	event_queue: Arc<EventQueue<Arc<FilesystemLogger>>>,
	router: Arc<Router>,
	scorer: Arc<Mutex<Scorer>>,
	logger: Arc<FilesystemLogger>,
	node_id: PublicKey,
}

impl Prober {
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, event_queue: Arc<EventQueue<Arc<FilesystemLogger>>>,
		router: Arc<Router>, scorer: Arc<Mutex<Scorer>>, logger: Arc<FilesystemLogger>,
		node_id: PublicKey,
	) -> Self {
		Self { channel_manager, event_queue, router, scorer, logger, node_id }
	}

	pub async fn send_probe(
		&self, destination: PublicKey, amount_msat: u64, attempts: u8, cltv_expiry_delta: u32,
	) -> Result<ProbeResult, Error> {
		let route = self
			.find_route(destination, amount_msat, attempts, cltv_expiry_delta)
			.map_err(|e| {
				log_error!(self.logger, "Failed to find route: {e:?}");
				Error::ProbeSendingFailed
			})?;

		let mut probe_result = ProbeResult {
			success: false,
			paths_attempted: Vec::with_capacity(route.paths.len()),
			start_time: SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.expect("Time went backwards")
				.as_secs(),
			end_time: 0,
			errors: vec![],
		};

		log_info!(self.logger, "Sending Probes");
		for path in route.paths {
			log_info!(self.logger, "Probing path: {:?}", &path);
			probe_result.paths_attempted.push(path.clone());
			match self.channel_manager.send_probe(path.clone()) {
				Err(e) => {
					log_error!(self.logger, "Failed to send probe: {e:?}");
					probe_result.errors.push(format!("{e:?}"));
				},
				Ok((probe_hash, probe_id)) => {
					log_info!(self.logger, "Probe sent");
					loop {
						match self.event_queue.next_event_async().await {
							Event::ProbeFailed { payment_id, payment_hash, short_channel_id } => {
								if payment_id == probe_id && payment_hash == probe_hash {
									log_info!(self.logger, "Probe failed");
									if let Some(scid) = short_channel_id {
										{
											let duration_since_epoch = SystemTime::now()
												.duration_since(UNIX_EPOCH)
												.expect("Time went backwards");
											let mut scorer = self.scorer.lock().unwrap();
											scorer.probe_failed(&path, scid, duration_since_epoch);
											probe_result
												.errors
												.push(format!("Probe failed on channel: {scid}"));
										}
									} else {
										probe_result.errors.push(
											"Probe failed without short_channel_id".to_string(),
										);
									}
									self.event_queue.event_handled()?;
									break;
								}
							},
							Event::ProbeSuccessful { payment_id, payment_hash } => {
								if payment_id == probe_id && payment_hash == probe_hash {
									log_info!(self.logger, "Probe successful");
									probe_result.success = true;
									{
										let mut scorer = self.scorer.lock().unwrap();
										let duration_since_epoch = SystemTime::now()
											.duration_since(UNIX_EPOCH)
											.expect("Time went backwards");
										scorer.probe_successful(&path, duration_since_epoch);
									}
									self.event_queue.event_handled()?;
									break;
								}
							},
							_ => {},
						}
					}
					break;
				},
			}
		}
		probe_result.end_time =
			SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();
		Ok(probe_result)
	}

	fn find_route(
		&self, destination: PublicKey, amount_msat: u64, attempts: u8, cltv_expiry_delta: u32,
	) -> Result<Route, LightningError> {
		let payment_params = PaymentParameters::from_node_id(destination, cltv_expiry_delta)
			.with_max_path_count(attempts);
		let route_params =
			RouteParameters::from_payment_params_and_value(payment_params, amount_msat);
		let usable_channels = self.channel_manager.list_usable_channels();
		let first_hops: Vec<&channelmanager::ChannelDetails> = usable_channels.iter().collect();
		let inflight_htlcs = self.channel_manager.compute_inflight_htlcs();
		self.router.find_route(&self.node_id, &route_params, Some(&first_hops[..]), inflight_htlcs)
	}
}
