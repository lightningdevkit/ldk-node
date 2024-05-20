use bdk::SignOptions;
use bitcoin::address::NetworkChecked;
use bitcoin::psbt::{Input, PartiallySignedTransaction, Psbt};
use bitcoin::Txid;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};
use payjoin::send::RequestContext;
use payjoin::Url;
use reqwest::header::HeaderMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::error::Error;
use crate::types::Wallet;

/// Payjoin Sender
pub(crate) struct PayjoinSender<L: Deref>
where
	L::Target: Logger,
{
	logger: L,
	wallet: Arc<Wallet>,
	payjoin_relay: Url,
	pending_requests: Mutex<Vec<(RequestContext, Psbt)>>,
}

impl<L: Deref> PayjoinSender<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(logger: L, wallet: Arc<Wallet>, payjoin_relay: &Url) -> Self {
		Self {
			logger,
			wallet,
			payjoin_relay: payjoin_relay.clone(),
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
		dbg!("payjoin_uri: {}", payjoin_uri.to_string());
		let mut request_context =
			payjoin::send::RequestBuilder::from_psbt_and_uri(original_psbt.clone(), payjoin_uri)
				.and_then(|b| b.build_non_incentivizing())
				.map_err(|e| {
					dbg!("Error building requestaaa");
					log_error!(self.logger, "Payjoin Sender: send: Error building request: {}", e);
					Error::PayjoinSender
				})?;
		let (sender_request, sender_ctx) =
			request_context.extract_v2(self.payjoin_relay.clone()).map_err(|e| {
				dbg!("Error extracting v2 request");
				log_error!(self.logger, "Payjoin Sender: send: Error building request: {}", e);
				Error::PayjoinSender
			})?;
		let (body, url) = (sender_request.body.clone(), sender_request.url.to_string());
		dbg!("sending payjoin request to: {}", &url);
		log_info!(self.logger, "Payjoin Sender: send: sending payjoin request to: {}", url);
		let mut headers = HeaderMap::new();
		headers.insert(reqwest::header::CONTENT_TYPE, "text/plain".parse().unwrap());
		dbg!("making request {}", url.clone());
		let client = reqwest::blocking::Client::new();
		let response = client.post(&url).body(body).headers(headers).send();
		if let Ok(response) = response {
			if response.status().is_success() {
				let response = match response.bytes() {
					Ok(response) => response.to_vec(),
					Err(e) => {
						dbg!("hereeea {} ", e);
						return Ok(Some(original_psbt.extract_tx().txid()));
					},
				};
				let psbt = match sender_ctx.process_response(&mut response.as_slice()) {
					Ok(Some(psbt)) => psbt,
					_ => {
						dbg!("No payjoin response found yet22");
						log_info!(
							self.logger,
							"Payjoin Sender: No payjoin response found yet. Setting request as pending."
						);
						self.queue_request(request_context, original_psbt.clone());
						return Ok(Some(original_psbt.extract_tx().txid()));
					},
				};
				return self.finalise_payjoin_tx(psbt, original_psbt.clone());
			}
		};
		self.queue_request(request_context, original_psbt.clone());
		return Ok(Some(original_psbt.extract_tx().txid()));
	}

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
		let duration = std::time::Duration::from_secs(360);
		loop {
			if time.elapsed() > duration {
				log_info!(self.logger, "Payjoin Sender: Polling timed out");
				return None;
			}

			let payjoin_directory = payjoin::Url::parse("https://payjo.in").unwrap();
			let (req, ctx) = match request_context.extract_v2(payjoin_directory.clone()) {
				Ok(req) => req,
				Err(e) => {
					log_error!(self.logger, "Payjoin Sender: Error extracting v2 request: {}", e);
					sleep(std::time::Duration::from_secs(3)).await;
					continue;
				},
			};
			let mut headers = HeaderMap::new();
			headers.insert(reqwest::header::CONTENT_TYPE, "text/plain".parse().unwrap());

			let client = reqwest::Client::new();
			let response =
				match client.post(req.url).body(req.body).headers(headers.clone()).send().await {
					Ok(response) => response,
					Err(e) => {
						log_error!(self.logger, "Payjoin Sender: Error sending request: {}", e);
						sleep(std::time::Duration::from_secs(3)).await;
						continue;
					},
				};
			let response = match response.bytes().await {
				Ok(response) => response.to_vec(),
				Err(e) => {
					log_error!(self.logger, "Payjoin Sender: Error reading response: {}", e);
					sleep(std::time::Duration::from_secs(3)).await;
					continue;
				},
			};
			if response.is_empty() {
				dbg!("No payjoin response found yet1");
				log_info!(self.logger, "Payjoin Sender: No payjoin response found yet");
				sleep(std::time::Duration::from_secs(3)).await;
				continue;
			}
			dbg!("response.len()", response.len());
			if response.len() == 54 {
				dbg!("No payjoin response found yet2");
				log_error!(self.logger, "Payjoin Sender: malformed payjoin response");
				sleep(std::time::Duration::from_secs(3)).await;
				continue;
			}
			let psbt = match ctx.process_response(&mut response.as_slice()) {
				Ok(Some(psbt)) => psbt,
				Ok(None) => {
					dbg!("No payjoin response found yet3");
					log_info!(self.logger, "Payjoin Sender: No pending payjoin response");
					sleep(std::time::Duration::from_secs(3)).await;
					continue;
				},
				Err(e) => {
					dbg!("No payjoin response found yet4");
					log_error!(self.logger, "Payjoin Sender: malformed payjoin response: {}", e);
					sleep(std::time::Duration::from_secs(3)).await;
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
