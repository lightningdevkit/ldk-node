use crate::error::Error;
use crate::payjoin_scheduler::PayjoinScheduler;
use crate::types::{ChannelManager, Wallet};
use crate::PayjoinChannel;
use bitcoin::secp256k1::PublicKey;
use bitcoin::ScriptBuf;
use lightning::ln::ChannelId;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};
use payjoin::receive::v2::{Enrolled, Enroller, ProvisionalProposal, UncheckedProposal};
use payjoin::{OhttpKeys, PjUriBuilder};
use payjoin::{PjUri, Url};
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Payjoin Receiver
async fn post_request(url: &payjoin::Url, body: Vec<u8>) -> Result<Vec<u8>, Error> {
	let headers = payjoin_receiver_request_headers();
	let client = reqwest::Client::new();
	let response =
		client.post(url.to_string()).body(body).headers(headers).send().await.map_err(|e| {
			dbg!(&e);
			Error::PayjoinReceiver
		})?;
	let response = response.bytes().await.map_err(|e| {
		dbg!(&e);
		Error::PayjoinReceiver
	})?;
	Ok(response.to_vec())
}

pub fn enroll_payjoin_receivers(
	ohttp_keys: &Option<OhttpKeys>, payjoin_directory: &Url, payjoin_relay: &Url,
) -> Result<(Enrolled, Enrolled, OhttpKeys), Error> {
	let ohttp_keys = match ohttp_keys {
		Some(ohttp_keys) => ohttp_keys.clone(),
		None => {
			let payjoin_directory =
				payjoin_directory.join("/ohttp-keys").map_err(|_| Error::PayjoinEnrollment)?;
			let res = BlockingHttpClient::new_proxy(payjoin_relay)
				.and_then(|c| c.get(&payjoin_directory))?;
			OhttpKeys::decode(res.as_slice()).map_err(|e| {
				dbg!(&e);
				Error::PayjoinEnrollment
			})?
		},
	};
	let enrolled = {
		let mut enroller = Enroller::from_directory_config(
			payjoin_directory.clone(),
			ohttp_keys.clone(),
			payjoin_relay.clone(),
		);
		let (req, ctx) = enroller.extract_req()?;
		dbg!("normal subdirectory", enroller.subdirectory());
		let headers = payjoin_receiver_request_headers();
		let response = BlockingHttpClient::new()
			.and_then(|c| c.post(&req.url.to_string(), req.body, headers, None))?;
		let response = match response.bytes() {
			Ok(response) => response,
			Err(e) => {
				dbg!(&e);
				return Err(Error::PayjoinEnrollment);
			},
		};
		let enrolled = enroller.process_res(response.to_vec().as_slice(), ctx)?;
		enrolled
	};

	let lightning_enrolled = {
		let mut enroller = Enroller::from_directory_config(
			payjoin_directory.clone(),
			ohttp_keys.clone(),
			payjoin_relay.clone(),
		);
		dbg!("lightning subdirectory", enroller.subdirectory());
		let (req, ctx) = enroller.extract_req()?;
		let headers = payjoin_receiver_request_headers();
		let response = BlockingHttpClient::new()
			.and_then(|c| c.post(&req.url.to_string(), req.body, headers, None))?;
		let response = match response.bytes() {
			Ok(response) => response,
			Err(e) => {
				dbg!(&e);
				return Err(Error::PayjoinEnrollment);
			},
		};
		let enrolled = enroller.process_res(response.to_vec().as_slice(), ctx)?;
		enrolled
	};

	Ok((enrolled, lightning_enrolled, ohttp_keys))
}

pub(crate) struct PayjoinLightningReceiver<L: Deref>
where
	L::Target: Logger,
{
	logger: L,
	scheduler: Arc<Mutex<PayjoinScheduler>>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	ohttp_keys: OhttpKeys,
	enrolled: Enrolled,
}

impl<L: Deref> PayjoinLightningReceiver<L>
where
	L::Target: Logger,
{
	pub fn new(
		logger: L, wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		scheduler: Arc<Mutex<PayjoinScheduler>>, enrolled: Enrolled, ohttp_keys: OhttpKeys,
	) -> Self {
		Self { logger, wallet, channel_manager, scheduler, enrolled, ohttp_keys }
	}

	async fn fetch_payjoin_request(&self) -> Result<Option<UncheckedProposal>, Error> {
		let mut enrolled = self.enrolled.clone();
		let (req, context) = match enrolled.extract_req() {
			Ok(req) => req,
			Err(e) => {
				dbg!("Error: {}", &e);
				log_error!(self.logger, "Payjoin Receiver: {}", e);
				return Err(Error::PayjoinReceiver);
			},
		};
		let payjoin_request = post_request(&req.url, req.body).await?;
		Ok(enrolled.process_res(payjoin_request.as_slice(), context)?)
	}

	pub(crate) async fn process_payjoin_request(&self) {
		let min_fee_rate = bitcoin::FeeRate::from_sat_per_vb(1);
		let mut scheduler = self.scheduler.lock().await;
		if scheduler.list_channels().is_empty() {}
		if !scheduler.in_progress() {
			let unchecked_proposal = match self.fetch_payjoin_request().await {
				Ok(Some(proposal)) => proposal,
				_ => {
					return;
				},
			};
			let tx = unchecked_proposal.extract_tx_to_schedule_broadcast();
			match scheduler.add_seen_tx(&tx) {
				true => {},
				false => {
					dbg!("Input seen before");
					log_error!(self.logger, "Payjoin Receiver: Seen tx before");
					return;
				},
			};
			let (provisional_proposal, amount_to_us) =
				match self.validate_payjoin_request(unchecked_proposal, min_fee_rate).await {
					Ok(proposal) => proposal,
					Err(_e) => {
						return;
					},
				};
			let scheduled_channel = scheduler.get_next_channel(amount_to_us);
			if let Some(channel) = scheduled_channel {
				let (channel_id, address, temporary_channel_id, _, counterparty_node_id) = channel;
				let mut channel_provisional_proposal = provisional_proposal.clone();
				channel_provisional_proposal.substitute_output_address(address);
				let payjoin_proposal = match channel_provisional_proposal
					.finalize_proposal(|psbt| Ok(psbt.clone()), None)
				{
					Ok(proposal) => proposal,
					Err(e) => {
						log_error!(self.logger, "Payjoin Receiver: {}", e);
						return;
					},
				};
				let (receiver_request, _) = match payjoin_proposal.clone().extract_v2_req() {
					Ok((req, ctx)) => (req, ctx),
					Err(e) => {
						log_error!(self.logger, "Payjoin Receiver: {}", e);
						return;
					},
				};
				let tx = payjoin_proposal.psbt().clone().extract_tx();
				if let (true, Ok(())) = (
					scheduler.set_funding_tx_created(
						channel_id,
						&receiver_request.url,
						receiver_request.body,
					),
					self.channel_manager.unsafe_funding_transaction_generated(
						&ChannelId::from_bytes(temporary_channel_id),
						&counterparty_node_id,
						tx.clone(),
					),
				) {
					return;
				}
			}
		}
	}

	pub(crate) fn payjoin_uri(&self, amount: bitcoin::Amount) -> Result<PjUri, Error> {
		let address = self.wallet.get_new_address()?;
		let pj_part = Url::parse(&self.enrolled.fallback_target()).map_err(|e| {
			dbg!(&e);
			Error::PayjoinUri
		})?;
		let payjoin_uri = PjUriBuilder::new(address, pj_part, Some(self.ohttp_keys.clone()))
			.amount(amount)
			.build();
		Ok(payjoin_uri)
	}

	pub(crate) fn schedule_channel(
		&self, amount: bitcoin::Amount, counterparty_node_id: PublicKey, channel_id: u128,
	) {
		let channel = PayjoinChannel::new(amount, counterparty_node_id, channel_id);
		self.scheduler.blocking_lock().schedule(
			channel.channel_value_satoshi(),
			channel.counterparty_node_id(),
			channel.channel_id(),
		);
	}

	pub(crate) fn list_scheduled_channels(&self) -> Vec<PayjoinChannel> {
		self.scheduler.blocking_lock().list_channels().clone()
	}

	pub(crate) async fn set_channel_accepted(
		&self, channel_id: u128, output_script: &ScriptBuf, temporary_channel_id: [u8; 32],
	) -> bool {
		let mut scheduler = self.scheduler.lock().await;
		scheduler.set_channel_accepted(channel_id, output_script, temporary_channel_id)
	}

	async fn validate_payjoin_request(
		&self, proposal: UncheckedProposal, min_fee_rate: Option<bitcoin::FeeRate>,
	) -> Result<(ProvisionalProposal, bitcoin::Amount), Error> {
		let tx = proposal.extract_tx_to_schedule_broadcast();
		let verified = self.wallet.verify_tx(&tx).await;
		let amount_to_us = self.wallet.funds_directed_to_us(&tx).unwrap_or_default();
		let proposal =
			proposal.check_broadcast_suitability(min_fee_rate, |_t| Ok(verified.is_ok()))?;
		let proposal = proposal.check_inputs_not_owned(|script| {
			Ok(self.wallet.is_mine(&script.to_owned()).unwrap_or(false))
		})?;
		let proposal = match proposal.check_no_mixed_input_scripts() {
			Ok(proposal) => proposal,
			Err(_) => {
				log_error!(self.logger, "Payjoin Receiver: Mixed input scripts");
				return Err(Error::PayjoinReceiver);
			},
		};
		let proposal = proposal.check_no_inputs_seen_before(|_outpoint| Ok(false))?;
		let original_proposal = proposal.clone().identify_receiver_outputs(|script| {
			Ok(self.wallet.is_mine(&script.to_owned()).unwrap_or(false))
		})?;
		Ok((original_proposal, amount_to_us))
	}
}

pub(crate) struct PayjoinReceiver<L: Deref>
where
	L::Target: Logger,
{
	logger: L,
	wallet: Arc<Wallet>,
	enrolled: Enrolled,
	ohttp_keys: OhttpKeys,
}

impl<L: Deref> PayjoinReceiver<L>
where
	L::Target: Logger,
{
	pub fn new(logger: L, wallet: Arc<Wallet>, enrolled: Enrolled, ohttp_keys: OhttpKeys) -> Self {
		Self { logger, wallet, enrolled, ohttp_keys }
	}

	async fn fetch_payjoin_request(&self) -> Result<Option<UncheckedProposal>, Error> {
		dbg!("fetching pajoing request");
		let mut enrolled = self.enrolled.clone();
		dbg!("1");
		let (req, context) = match enrolled.extract_req() {
			Ok(req) => req,
			Err(e) => {
				dbg!("Error: {}", &e);
				log_error!(self.logger, "Payjoin Receiver: {}", e);
				return Err(Error::PayjoinReceiver);
			},
		};
		dbg!("2");
		let payjoin_request = post_request(&req.url, req.body).await?;
		dbg!("3");
		let response = match enrolled.process_res(payjoin_request.as_slice(), context) {
			Ok(response) => response,
			Err(e) => {
				dbg!("Error process payjoin request: {}", &e);
				log_error!(self.logger, "Payjoin Receiver: {}", e);
				return Err(Error::PayjoinReceiver);
			},
		};
		dbg!("4");
		Ok(response)
	}

	pub(crate) async fn process_payjoin_request(&self) {
		dbg!("processing normal payjoin request");
		let min_fee_rate = bitcoin::FeeRate::from_sat_per_vb(1);
		let unchecked_proposal = match self.fetch_payjoin_request().await {
			Ok(Some(proposal)) => proposal,
			_ => {
				dbg!("NONE");
				return;
			},
		};
		dbg!("got unchecked proposal");
		let (provisional_proposal, _) =
			match self.validate_payjoin_request(unchecked_proposal, min_fee_rate).await {
				Ok(proposal) => proposal,
				Err(_e) => {
					dbg!("Error validating payjoin request");
					return;
				},
			};
		dbg!("finalizing payjoin request");
		self.accept_normal_payjoin_request(provisional_proposal).await
	}

	async fn accept_normal_payjoin_request(&self, provisional_proposal: ProvisionalProposal) {
		let mut finalized_proposal =
			match provisional_proposal.finalize_proposal(|psbt| Ok(psbt.clone()), None) {
				Ok(proposal) => proposal,
				Err(e) => {
					log_error!(self.logger, "Payjoin Receiver: {}", e);
					return;
				},
			};
		let (receiver_request, _) = match finalized_proposal.extract_v2_req() {
			Ok(req) => req,
			Err(e) => {
				log_error!(self.logger, "Payjoin Receiver: {}", e);
				return;
			},
		};
		match post_request(&receiver_request.url, receiver_request.body).await {
			Ok(_response) => {
				log_info!(self.logger, "Payjoin Receiver: Payjoin request sent to sender");
			},
			Err(e) => {
				log_error!(self.logger, "Payjoin Receiver: {}", e);
			},
		}
	}

	pub(crate) fn payjoin_uri(&self, amount: bitcoin::Amount) -> Result<PjUri, Error> {
		let address = self.wallet.get_new_address()?;
		let pj_part =
			Url::parse(&self.enrolled.fallback_target()).map_err(|_| Error::PayjoinUri)?;
		let payjoin_uri = PjUriBuilder::new(address, pj_part, Some(self.ohttp_keys.clone()))
			.amount(amount)
			.build();
		Ok(payjoin_uri)
	}

	async fn validate_payjoin_request(
		&self, proposal: UncheckedProposal, min_fee_rate: Option<bitcoin::FeeRate>,
	) -> Result<(ProvisionalProposal, bitcoin::Amount), Error> {
		let tx = proposal.extract_tx_to_schedule_broadcast();
		let verified = self.wallet.verify_tx(&tx).await;
		let amount_to_us = self.wallet.funds_directed_to_us(&tx).unwrap_or_default();
		let proposal =
			proposal.check_broadcast_suitability(min_fee_rate, |_t| Ok(verified.is_ok()))?;
		let proposal = proposal.check_inputs_not_owned(|script| {
			Ok(self.wallet.is_mine(&script.to_owned()).unwrap_or(false))
		})?;
		let proposal = match proposal.check_no_mixed_input_scripts() {
			Ok(proposal) => proposal,
			Err(_) => {
				log_error!(self.logger, "Payjoin Receiver: Mixed input scripts");
				return Err(Error::PayjoinReceiver);
			},
		};
		let proposal = proposal.check_no_inputs_seen_before(|_outpoint| Ok(false))?;
		let original_proposal = proposal.clone().identify_receiver_outputs(|script| {
			Ok(self.wallet.is_mine(&script.to_owned()).unwrap_or(false))
		})?;
		Ok((original_proposal, amount_to_us))
	}
}

pub(crate) fn payjoin_receiver_request_headers() -> HeaderMap<HeaderValue> {
	let mut headers = HeaderMap::new();
	let header_value = HeaderValue::from_static("message/ohttp-req");
	headers.insert(reqwest::header::CONTENT_TYPE, header_value);
	headers
}

struct BlockingHttpClient {
	client: reqwest::blocking::Client,
}

impl BlockingHttpClient {
	fn new() -> Result<Self, Error> {
		let client =
			reqwest::blocking::Client::builder().build().map_err(|_| Error::PayjoinReqwest)?;
		Ok(Self { client })
	}

	fn new_proxy(payjoin_relay: &Url) -> Result<Self, Error> {
		let proxy =
			reqwest::Proxy::all(payjoin_relay.to_string()).map_err(|_| Error::PayjoinReqwest)?;
		let client = reqwest::blocking::Client::builder()
			.proxy(proxy)
			.build()
			.map_err(|_| Error::PayjoinReqwest)?;
		Ok(Self { client })
	}

	fn get(&self, url: &Url) -> Result<Vec<u8>, Error> {
		Ok(self
			.client
			.get(url.to_string())
			.send()
			.and_then(|response| response.error_for_status())
			.map_err(|e| {
				dbg!(&e);
				Error::PayjoinReqwest
			})?
			.bytes()
			.map_err(|e| {
				dbg!(&e);
				Error::PayjoinReqwest
			})?
			.to_vec())
	}

	fn post(
		&self, url: &str, body: Vec<u8>, headers: HeaderMap<HeaderValue>,
		timeout: Option<std::time::Duration>,
	) -> Result<reqwest::blocking::Response, Error> {
		Ok(self
			.client
			.post(url)
			.timeout(timeout.unwrap_or(std::time::Duration::from_secs(35)))
			.headers(headers)
			.body(body)
			.send()
			.map_err(|e| {
				dbg!(&e);
				Error::PayjoinReqwest
			})?)
	}
}
