use crate::channel_scheduler::ChannelScheduler;
use crate::error::Error;
use crate::types::{ChannelManager, Wallet};
use crate::ScheduledChannel;
use bdk::SignOptions;
use bitcoin::address::NetworkChecked;
use bitcoin::psbt::{Input, PartiallySignedTransaction, Psbt};
use bitcoin::secp256k1::PublicKey;
use bitcoin::{ScriptBuf, Txid};
use lightning::ln::ChannelId;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};
use payjoin::receive::v2::{Enrolled, Enroller, ProvisionalProposal, UncheckedProposal};
use payjoin::send::RequestContext;
use payjoin::{OhttpKeys, PjUriBuilder};
use payjoin::{PjUri, Url};
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::sleep;

// Payjoin receiver is a node that can receive payjoin requests.
//
// In order to setup a payjoin receiver, you need to enroll in the payjoin directory and receive
// ohttp keys.  You can enroll using `PayjoinReceiverSetup::enroll` function.
//
// The payjoin receiver can then process payjoin requests and respond to them.
pub(crate) struct PayjoinReceiver<L: Deref>
where
	L::Target: Logger,
{
	logger: L,
	scheduler: Arc<Mutex<ChannelScheduler>>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	enrolled: Enrolled,
	ohttp_keys: OhttpKeys,
}

impl<L: Deref> PayjoinReceiver<L>
where
	L::Target: Logger,
{
	pub(crate) fn enroll(
		ohttp_keys: &Option<OhttpKeys>, payjoin_directory: &Url, payjoin_relay: &Url,
		scheduler: Arc<Mutex<ChannelScheduler>>, wallet: Arc<Wallet>,
		channel_manager: Arc<ChannelManager>, logger: L,
	) -> Result<PayjoinReceiver<L>, Error> {
		let ohttp_keys = match ohttp_keys {
			Some(ohttp_keys) => ohttp_keys.clone(),
			None => {
				let payjoin_directory =
					payjoin_directory.join("/ohttp-keys").map_err(|_| Error::PayjoinEnrollment)?;
				let res = BlockingHttpClient::new_proxy(payjoin_relay)
					.and_then(|c| c.get(&payjoin_directory))?;
				OhttpKeys::decode(res.as_slice()).map_err(|_| Error::PayjoinEnrollment)?
			},
		};
		let enrolled = {
			let mut enroller = Enroller::from_directory_config(
				payjoin_directory.clone(),
				ohttp_keys.clone(),
				payjoin_relay.clone(),
			);
			let (req, ctx) = enroller.extract_req()?;
			let headers = payjoin_receiver_request_headers();
			let response = BlockingHttpClient::new()
				.and_then(|c| c.post(&req.url.to_string(), req.body, headers, None))?;
			let enrolled = enroller.process_res(response.as_slice(), ctx)?;
			enrolled
		};
		Ok(PayjoinReceiver { logger, scheduler, wallet, channel_manager, enrolled, ohttp_keys })
	}

	async fn post_request(url: &payjoin::Url, body: Vec<u8>) -> Result<Vec<u8>, Error> {
		let headers = payjoin_receiver_request_headers();
		let client = HttpClient::new()?;
		let response = client.post(url, body, headers.clone()).await?;
		Ok(response)
	}

	async fn fetch_payjoin_request(
		&self,
	) -> Result<Option<(ProvisionalProposal, bitcoin::Amount)>, Error> {
		let min_fee_rate = bitcoin::FeeRate::from_sat_per_vb(1);
		let (req, context) = self.enrolled.clone().extract_req().unwrap();
		let payjoin_request = Self::post_request(&req.url, req.body).await?;
		let unchecked_proposal = self.enrolled.process_res(payjoin_request.as_slice(), context)?;
		match unchecked_proposal {
			None => return Ok(None),
			Some(unchecked_proposal) => {
				let (provisional_proposal, amount_to_us) =
					match self.validate_payjoin_request(unchecked_proposal, min_fee_rate).await {
						Ok(proposal) => proposal,
						Err(_e) => {
							return Ok(None);
						},
					};
				Ok(Some((provisional_proposal, amount_to_us)))
			},
		}
	}

	pub(crate) async fn process_payjoin_request(&self) {
		let mut scheduler = self.scheduler.lock().await;
		if !scheduler.in_progress() {
			let (provisional_proposal, amount_to_us) = match self.fetch_payjoin_request().await {
				Ok(Some(proposal)) => proposal,
				_ => {
					return;
				},
			};
			let scheduled_channel = scheduler.get_next_channel(amount_to_us);
			if let Some(channel) = scheduled_channel {
				let (channel_id, address, temporary_channel_id, _, counterparty_node_id) = channel;
				let mut channel_provisional_proposal = provisional_proposal.clone();
				channel_provisional_proposal.substitute_output_address(address);
				let payjoin_proposal = channel_provisional_proposal
					.finalize_proposal(|psbt| Ok(psbt.clone()), None)
					.and_then(|mut proposal| {
						let (receiver_request, _) = proposal.extract_v2_req().unwrap();
						let tx = proposal.psbt().clone().extract_tx();
						Ok((receiver_request, tx))
					});
				if let Ok(payjoin_proposal) = payjoin_proposal {
					if let (true, Ok(())) = (
						scheduler.set_funding_tx_created(
							channel_id,
							&payjoin_proposal.0.url,
							payjoin_proposal.0.body,
						),
						self.channel_manager.unsafe_funding_transaction_generated(
							&ChannelId::from_bytes(temporary_channel_id),
							&counterparty_node_id,
							payjoin_proposal.1,
						),
					) {
						return;
					}
				}
			}
			self.accept_normal_payjoin_request(provisional_proposal).await;
		}
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
		match Self::post_request(&receiver_request.url, receiver_request.body).await {
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

	pub(crate) fn schedule_channel(
		&self, amount: bitcoin::Amount, counterparty_node_id: PublicKey, channel_id: u128,
	) {
		let channel = ScheduledChannel::new(amount, counterparty_node_id, channel_id);
		self.scheduler.blocking_lock().schedule(
			channel.channel_value_satoshi(),
			channel.counterparty_node_id(),
			channel.channel_id(),
		);
	}

	pub(crate) fn list_scheduled_channels(&self) -> Vec<ScheduledChannel> {
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
pub fn payjoin_receiver_request_headers() -> HeaderMap<HeaderValue> {
	let mut headers = HeaderMap::new();
	let header_value = HeaderValue::from_static("message/ohttp-req");
	headers.insert(reqwest::header::CONTENT_TYPE, header_value);
	headers
}

pub(crate) struct PayjoinSender<L: Deref>
where
	L::Target: Logger,
{
	logger: L,
	wallet: Arc<Wallet>,
	payjoin_relay: Url,
	payjoin_directory: Url,
	pending_requests: Mutex<Vec<(RequestContext, Psbt)>>,
}

impl<L: Deref> PayjoinSender<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(
		logger: L, wallet: Arc<Wallet>, payjoin_relay: &Url, payjoin_directory: &Url,
	) -> Self {
		Self {
			logger,
			wallet,
			payjoin_relay: payjoin_relay.clone(),
			payjoin_directory: payjoin_directory.clone(),
			pending_requests: Mutex::new(Vec::new()),
		}
	}

	// Create a payjoin request based on the payjoin URI parameters.  This function builds a PSBT
	// based on the amount and receiver address extracted from the payjoin URI, that can be used to
	// send a payjoin request to the receiver using `PayjoinSender::send_payjoin_request`.
	pub(crate) fn create_payjoin_request(
		&self, payjoin_uri: payjoin::Uri<'static, NetworkChecked>,
	) -> Result<Psbt, Error> {
		// Extract amount and receiver address from URI
		let amount_to_send = match payjoin_uri.amount {
			Some(amount) => amount,
			None => {
				log_error!(self.logger, "Payjoin Sender: send: No amount found in URI");
				return Err(Error::PayjoinSender);
			},
		};
		let receiver_address = payjoin_uri.address.clone().script_pubkey();
		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;
		let original_psbt = self
			.wallet
			.build_transaction(receiver_address, amount_to_send.to_sat(), sign_options)
			.unwrap();
		Ok(original_psbt)
	}

	// Send payjoin transaction based on the payjoin URI parameters.
	//
	// This function sends the payjoin request to the receiver and saves the context and request in
	// the pending_requests field to process the response async.
	pub(crate) fn send_payjoin_request(
		&self, payjoin_uri: payjoin::Uri<'static, NetworkChecked>, original_psbt: Psbt,
	) -> Result<Option<Txid>, Error> {
		let mut request_context =
			payjoin::send::RequestBuilder::from_psbt_and_uri(original_psbt.clone(), payjoin_uri)
				.and_then(|b| b.build_non_incentivizing())
				.map_err(|e| {
					log_error!(self.logger, "Payjoin Sender: send: Error building request: {}", e);
					Error::PayjoinSender
				})?;
		let (sender_request, sender_ctx) =
			request_context.extract_v2(self.payjoin_directory.clone()).map_err(|e| {
				log_error!(self.logger, "Payjoin Sender: send: Error building request: {}", e);
				Error::PayjoinSender
			})?;
		let (body, url) = (sender_request.body.clone(), sender_request.url.to_string());
		log_info!(self.logger, "Payjoin Sender: send: sending payjoin request to: {}", url);
		let mut headers = HeaderMap::new();
		headers.insert(reqwest::header::CONTENT_TYPE, "text/plain".parse().unwrap());
		let response = BlockingHttpClient::new_proxy(&self.payjoin_relay)
			.and_then(|c| c.post(&url, body, headers, Some(std::time::Duration::from_secs(5))));
		if let Ok(response) = response {
			let psbt = match sender_ctx.process_response(&mut response.as_slice()) {
				Ok(Some(psbt)) => psbt,
				_ => {
					log_info!(
						self.logger,
						"Payjoin Sender: No payjoin response found yet. Setting request as pending."
					);
					self.queue_request(request_context, original_psbt);
					return Ok(None);
				},
			};
			return self.finalise_payjoin_tx(psbt, original_psbt.clone());
		} else {
			self.queue_request(request_context, original_psbt);
			return Ok(None);
		}
	}

	// Process the payjoin response from the receiver.
	//
	// After sending the payjoin request to the receiver, we will process the response from the
	// receiver and finalise the payjoin transaction.
	//
	// Because the response from the receiver is asynchronous, this function first checks if there
	// is a pending request in the pending_requests field. If there is a pending request, it will
	// check if a response was received in the directory and process it accordingly. A successful
	// responsonse from the directory but with no payjoin proposal will return Ok(None). If a
	// payjoin proposal is found, we will attempt to finalise the payjoin transaction and broadcast
	// it.
	pub(crate) async fn process_payjoin_response(&self) {
		let mut pending_requests = self.pending_requests.lock().await;
		let (mut request_context, original_psbt) = match pending_requests.pop() {
			Some(request_context) => request_context,
			None => {
				log_info!(self.logger, "Payjoin Sender: No pending request found. ");
				return;
			},
		};
		let now = std::time::Instant::now();
		let (psbt, original_psbt) = match self.poll(&mut request_context, original_psbt, now).await
		{
			Some((psbt, original_psbt)) => (psbt, original_psbt),
			None => {
				return;
			},
		};
		match self.finalise_payjoin_tx(psbt.clone(), original_psbt.clone()) {
			Ok(Some(txid)) => {
				log_info!(self.logger, "Payjoin Sender: Payjoin transaction broadcasted: {}", txid);
			},
			Ok(None) => {
				log_info!(
					self.logger,
					"Payjoin Sender: Was not able to finalise payjoin transaction {}.",
					psbt.extract_tx().txid()
				);
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Payjoin Sender: Error finalising payjoin transaction: {}",
					e
				);
			},
		}
	}

	async fn poll(
		&self, request_context: &mut RequestContext, original_psbt: Psbt, time: Instant,
	) -> Option<(Psbt, Psbt)> {
		let duration = std::time::Duration::from_secs(180);
		loop {
			if time.elapsed() > duration {
				log_info!(self.logger, "Payjoin Sender: Polling timed out");
				return None;
			}
			let (req, ctx) = match request_context.extract_v2(self.payjoin_directory.clone()) {
				Ok(req) => req,
				Err(e) => {
					log_error!(self.logger, "Payjoin Sender: Error extracting v2 request: {}", e);
					sleep(std::time::Duration::from_secs(5)).await;
					return None;
				},
			};
			let mut headers = HeaderMap::new();
			headers.insert(reqwest::header::CONTENT_TYPE, "text/plain".parse().unwrap());
			let client = HttpClient::new().ok()?;
			let response = client.post(&req.url, req.body, headers.clone()).await.ok()?;
			let psbt = match ctx.process_response(&mut response.as_slice()) {
				Ok(Some(psbt)) => psbt,
				Ok(None) => {
					log_info!(self.logger, "Payjoin Sender: No pending payjoin response");
					sleep(std::time::Duration::from_secs(5)).await;
					continue;
				},
				Err(e) => {
					log_error!(self.logger, "Payjoin Sender: malformed payjoin response: {}", e);
					sleep(std::time::Duration::from_secs(10)).await;
					continue;
				},
			};
			return Some((psbt, original_psbt.clone()));
		}
	}

	// finalise the payjoin transaction and broadcast it
	fn finalise_payjoin_tx(
		&self, mut psbt: Psbt, mut ocean_psbt: Psbt,
	) -> Result<Option<Txid>, Error> {
		// for BDK, we need to reintroduce utxo from original psbt.
		// Otherwise we wont be able to sign the transaction.
		fn input_pairs(
			psbt: &mut PartiallySignedTransaction,
		) -> Box<dyn Iterator<Item = (&bdk::bitcoin::TxIn, &mut Input)> + '_> {
			Box::new(psbt.unsigned_tx.input.iter().zip(&mut psbt.inputs))
		}

		// get original inputs from original psbt clone (ocean_psbt)
		let mut original_inputs = input_pairs(&mut ocean_psbt).peekable();
		for (proposed_txin, proposed_psbtin) in input_pairs(&mut psbt) {
			if let Some((original_txin, original_psbtin)) = original_inputs.peek() {
				if proposed_txin.previous_output == original_txin.previous_output {
					proposed_psbtin.witness_utxo = original_psbtin.witness_utxo.clone();
					proposed_psbtin.non_witness_utxo = original_psbtin.non_witness_utxo.clone();
					original_inputs.next();
				}
			}
		}

		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;
		sign_options.try_finalize = true;
		let (_is_signed, psbt) = self.wallet.sign_transaction(&psbt, sign_options)?;
		let tx = psbt.extract_tx();
		self.wallet.broadcast_transaction(&tx);
		let txid = tx.txid();
		Ok(Some(txid))
	}

	fn queue_request(&self, request_context: RequestContext, original_psbt: Psbt) {
		log_info!(&self.logger, "Payjoin Sender: saving pending request for txid");
		self.pending_requests.blocking_lock().push((request_context, original_psbt));
	}
}

pub struct HttpClient {
	client: reqwest::Client,
}

impl HttpClient {
	fn new() -> Result<Self, Error> {
		let client = reqwest::Client::builder().build().map_err(|_| Error::PayjoinReqwest)?;
		Ok(Self { client })
	}

	async fn post(
		&self, url: &Url, body: Vec<u8>, headers: HeaderMap<HeaderValue>,
	) -> Result<Vec<u8>, Error> {
		Ok(self
			.client
			.post(url.to_string())
			.headers(headers)
			.body(body)
			.send()
			.await
			.and_then(|response| response.error_for_status())
			.map_err(|_| Error::PayjoinReqwest)?
			.bytes()
			.await
			.map_err(|_| Error::PayjoinReqwest)?
			.to_vec())
	}
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
			.map_err(|_| Error::PayjoinReqwest)?
			.bytes()
			.map_err(|_| Error::PayjoinReqwest)?
			.to_vec())
	}

	fn post(
		&self, url: &str, body: Vec<u8>, headers: HeaderMap<HeaderValue>,
		timeout: Option<std::time::Duration>,
	) -> Result<Vec<u8>, Error> {
		Ok(self
			.client
			.post(url)
			.headers(headers)
			.body(body)
			.timeout(timeout.unwrap_or(std::time::Duration::from_secs(15)))
			.send()
			.and_then(|response| response.error_for_status())
			.map_err(|_| Error::PayjoinReqwest)?
			.bytes()
			.map_err(|_| Error::PayjoinReqwest)?
			.to_vec())
	}
}

// https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki#user-content-Receivers_original_PSBT_checklist
