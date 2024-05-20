/// An implementation of payjoin v2 sender as described in BIP-77.
use bdk::SignOptions;
use bitcoin::address::NetworkChecked;
use bitcoin::psbt::{Input, PartiallySignedTransaction, Psbt};
use bitcoin::Txid;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};
use payjoin::send::ContextV2;
use payjoin::Url;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::StatusCode;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;
use tokio::time::sleep;

use crate::error::Error;
use crate::types::Wallet;

pub(crate) struct PayjoinSender<L: Deref>
where
	L::Target: Logger,
{
	logger: L,
	wallet: Arc<Wallet>,
	payjoin_relay: Url,
}

impl<L: Deref> PayjoinSender<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(logger: L, wallet: Arc<Wallet>, payjoin_relay: &Url) -> Self {
		Self { logger, wallet, payjoin_relay: payjoin_relay.clone() }
	}

	// Create payjoin request based on the payjoin URI parameters.  This function builds a PSBT
	// based on the amount and receiver address extracted from the payjoin URI, that can be used to
	// send a payjoin request to the receiver using `PayjoinSender::send_payjoin_request`.
	pub(crate) fn create_payjoin_request(
		&self, payjoin_uri: payjoin::Uri<'static, NetworkChecked>, amount: Option<bitcoin::Amount>,
	) -> Result<Psbt, Error> {
		let amount_to_send = match (amount, payjoin_uri.amount) {
			(Some(amount), _) => amount,
			(None, Some(amount)) => amount,
			(None, None) => return Err(Error::PayjoinSender),
		};
		let receiver_address = payjoin_uri.address.clone().script_pubkey();
		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;
		let original_psbt = self.wallet.build_transaction(
			receiver_address,
			amount_to_send.to_sat(),
			sign_options,
		)?;
		Ok(original_psbt)
	}

	pub(crate) fn extract_request_data(
		&self, payjoin_uri: payjoin::Uri<'static, NetworkChecked>, original_psbt: Psbt,
	) -> Result<(payjoin::send::Request, ContextV2), Error> {
		let mut request_context =
			payjoin::send::RequestBuilder::from_psbt_and_uri(original_psbt.clone(), payjoin_uri)
				.and_then(|b| b.build_non_incentivizing())
				.map_err(|e| {
					dbg!(&e);
					log_error!(
						self.logger,
						"Payjoin Sender: send: Error building payjoin request {}",
						e
					);
					Error::PayjoinSender
				})?;
		let (sender_request, sender_ctx) =
			request_context.extract_v2(self.payjoin_relay.clone()).map_err(|e| {
				dbg!(&e);
				log_error!(
					self.logger,
					"Payjoin Sender: send: Error extracting payjoin request: {}",
					e
				);
				Error::PayjoinSender
			})?;
		Ok((sender_request, sender_ctx))
	}

	pub(crate) async fn poll(
		&self, request: &payjoin::send::Request, time: Instant,
	) -> Option<Vec<u8>> {
		let duration = std::time::Duration::from_secs(3600);
		let sleep = || sleep(std::time::Duration::from_secs(10));
		loop {
			if time.elapsed() > duration {
				log_info!(self.logger, "Payjoin Sender: Polling timed out");
				return None;
			}
			let client = reqwest::Client::new();

			let response = match client
				.post(request.url.clone())
				.body(request.body.clone())
				.headers(ohttp_req_header())
				.send()
				.await
			{
				Ok(response) => response,
				Err(e) => {
					log_info!(self.logger, "Payjoin Sender: Error polling request: {}", e);
					sleep().await;
					continue;
				},
			};
			let response = match response.error_for_status() {
				Ok(response) => response,
				Err(e) => {
					log_info!(self.logger, "Payjoin Sender: Status Error polling request: {}", e);
					sleep().await;
					continue;
				},
			};

			if response.status() == StatusCode::OK {
				let response = match response.bytes().await {
					Ok(response) => response.to_vec(),
					Err(e) => {
						log_info!(
							self.logger,
							"Payjoin Sender: Error reading polling response: {}",
							e
						);
						sleep().await;
						continue;
					},
				};
				if response.is_empty() {
					log_info!(self.logger, "Payjoin Sender: Got empty response while polling");
					sleep().await;
					continue;
				}
				return Some(response);
			} else {
				log_info!(
					self.logger,
					"Payjoin Sender: Error sending request, got status code
					{}",
					response.status()
				);
				sleep().await;
				continue;
			}
		}
	}

	// finalise the payjoin transaction and broadcast it
	pub(crate) fn finalise_payjoin_tx(
		&self, mut psbt: Psbt, ocean_psbt: Psbt,
	) -> Result<Txid, Error> {
		let mut ocean_psbt = ocean_psbt.clone();
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
		Ok(txid)
	}
}

fn ohttp_req_header() -> HeaderMap {
	let mut headers = HeaderMap::new();
	headers.insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static("message/ohttp-req"));
	headers
}
