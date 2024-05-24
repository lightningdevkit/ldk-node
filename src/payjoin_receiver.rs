use crate::error::Error;
use crate::io::utils::ohttp_headers;
use crate::logger::FilesystemLogger;
use crate::payjoin_channel_scheduler::{PayjoinChannel, PayjoinChannelScheduler};
use crate::types::{ChannelManager, Wallet};
use crate::Config;
use bitcoin::{ScriptBuf, Transaction};
use lightning::ln::ChannelId;
use lightning::log_info;
use lightning::util::logger::Logger;
use payjoin::receive::v2::{Enrolled, Enroller, ProvisionalProposal, UncheckedProposal};
use payjoin::{OhttpKeys, PjUriBuilder};
use payjoin::{PjUri, Url};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Implements Payjoin protocol as specified in [BIP77]
///
/// [BIP77]: https://github.com/bitcoin/bips/blob/3b863a402e0250658985f08a455a6cd103e269e5/bip-0077.mediawiki
pub(crate) struct PayjoinReceiver {
	logger: Arc<FilesystemLogger>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	channel_scheduler: RwLock<PayjoinChannelScheduler>,
	/// Directory receiver wish to enroll with
	payjoin_directory: Url,
	/// Proxy server receiver wish to make requests through
	payjoin_relay: Url,
	/// Enrollement object indicates a successful enrollement if is defined.
	enrolled: RwLock<Option<Enrolled>>,
	/// Directory keys.
	///
	/// Optional as they can be fetched on behalf of the user if not provided.
	/// They are required in order to enroll.
	ohttp_keys: RwLock<Option<OhttpKeys>>,
	config: Arc<Config>,
}

impl PayjoinReceiver {
	pub(crate) fn new(
		logger: Arc<FilesystemLogger>, wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		config: Arc<Config>, payjoin_directory: payjoin::Url, payjoin_relay: payjoin::Url,
		ohttp_keys: Option<OhttpKeys>,
	) -> Self {
		Self {
			logger,
			wallet,
			channel_manager,
			channel_scheduler: RwLock::new(PayjoinChannelScheduler::new()),
			config,
			payjoin_directory,
			payjoin_relay,
			enrolled: RwLock::new(None),
			ohttp_keys: RwLock::new(ohttp_keys),
		}
	}

	/// Before receiving Payjoin transactions we `enroll` with a Payjoin directory
	/// and we acquire a subdirectory we can receive Payjoin transactions to while offline.
	///
	/// This function returns [BIP21] URI with Payjoin parameters.
	///
	/// [BIP21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	pub(crate) async fn receive(&self, amount: bitcoin::Amount) -> Result<PjUri, Error> {
		if !self.is_enrolled().await {
			self.enroll().await?;
		}
		let enrolled = self.enrolled.read().await;
		let enrolled = match enrolled.as_ref() {
			Some(enrolled) => enrolled,
			None => {
				log_info!(self.logger, "Payjoin Receiver: Not enrolled");
				return Err(Error::PayjoinReceiverUnavailable);
			},
		};
		let fallback_target = enrolled.fallback_target();
		let ohttp_keys = self.ohttp_keys.read().await;
		let ohttp_keys = match ohttp_keys.as_ref() {
			Some(okeys) => okeys,
			None => {
				log_info!(self.logger, "Payjoin Receiver: No ohttp keys");
				return Err(Error::PayjoinReceiverUnavailable);
			},
		};
		let address = self.wallet.get_new_address()?;
		let pj_part = match payjoin::Url::parse(&fallback_target) {
			Ok(pj_part) => pj_part,
			Err(_) => {
				log_info!(self.logger, "Payjoin Receiver: Invalid fallback target");
				return Err(Error::PayjoinReceiverUnavailable);
			},
		};
		let payjoin_uri =
			PjUriBuilder::new(address, pj_part, Some(ohttp_keys.clone())).amount(amount).build();
		Ok(payjoin_uri)
	}

	pub(crate) async fn set_channel_accepted(
		&self, channel_id: u128, output_script: &ScriptBuf, temporary_channel_id: [u8; 32],
	) -> bool {
		let mut scheduler = self.channel_scheduler.write().await;
		scheduler.set_channel_accepted(channel_id, output_script, temporary_channel_id)
	}

	/// After enrolling, we should periodacly check if we have received any Payjoin transactions.
	///
	/// This function will try to fetch pending Payjoin requests from the subdirectory, and if a
	/// successful response received, we validate the request as specified in [BIP78]. After
	/// validation we check if we have a pending matching channel, and if so, we try fund the channel
	/// with the incoming funds from the payjoin request. Otherwise, we accept the Payjoin request
	/// normally by trying to preserve privacy, finalise the Payjoin proposal and send it back the
	/// the Payjoin sender.
	///
	/// [BIP78]: https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki#user-content-Receivers_original_PSBT_checklist
	pub(crate) async fn process_payjoin_request(&self) {
		let mut enrolled = self.enrolled.write().await;
		if let Some(mut enrolled) = enrolled.take() {
			let (req, context) = match enrolled.extract_req() {
				Ok(req) => req,
				Err(e) => {
					log_info!(
						self.logger,
						"Payjoin Receiver: Unable to extract enrollement request and context{}",
						e
					);
					return;
				},
			};

			let client = reqwest::Client::new();
			let response = match client
				.post(req.url.to_string())
				.body(req.body)
				.headers(ohttp_headers())
				.send()
				.await
			{
				Ok(response) => response,
				Err(e) => {
					log_info!(
						self.logger,
						"Payjoin Receiver: Unable to fetch payjoin request {}",
						e
					);
					return;
				},
			};
			if response.status() != reqwest::StatusCode::OK {
				log_info!(
					self.logger,
					"Payjoin Receiver: Got non-200 response from directory server {}",
					response.status()
				);
				return;
			};
			let response = match response.bytes().await {
				Ok(response) => response,
				Err(e) => {
					log_info!(self.logger, "Payjoin Receiver: Error reading response {}", e);
					return;
				},
			};
			if response.is_empty() {
				log_info!(self.logger, "Payjoin Receiver: Empty response from directory server");
				return;
			};
			let response = match enrolled.process_res(response.to_vec().as_slice(), context) {
				Ok(response) => response,
				Err(e) => {
					log_info!(
						self.logger,
						"Payjoin Receiver: Unable to process payjoin request {}",
						e
					);
					return;
				},
			};
			let unchecked_proposal = match response {
				Some(proposal) => proposal,
				None => {
					return;
				},
			};
			let original_tx = unchecked_proposal.extract_tx_to_schedule_broadcast();
			let provisional_proposal = match self.validate_payjoin_request(unchecked_proposal).await
			{
				Ok(proposal) => proposal,
				Err(e) => {
					log_info!(self.logger, "Payjoin Validation: {}", e);
					return;
				},
			};
			let amount = match self.wallet.funds_directed_to_us(&original_tx) {
				Ok(a) => a,
				Err(e) => {
					// This should not happen in practice as the validation checks would fail if
					// the sender didnt include us in the outputs
					log_info!(self.logger, "Not able to find any ouput directed to us: {}", e);
					return;
				},
			};
			let mut scheduler = self.channel_scheduler.write().await;
			let network = self.config.network;
			if let Some(channel) = scheduler.get_next_channel(amount, network) {
				log_info!(self.logger, "Found a channel match for incoming Payjoin request");
				let (channel_id, funding_tx_address, temporary_channel_id, _, counterparty_node_id) =
					channel;
				let mut channel_provisional_proposal = provisional_proposal.clone();
				channel_provisional_proposal.substitute_output_address(funding_tx_address);
				let payjoin_proposal = match channel_provisional_proposal
					.finalize_proposal(|psbt| Ok(psbt.clone()), None)
				{
					Ok(proposal) => proposal,
					Err(e) => {
						dbg!(&e);
						return;
					},
				};
				let (receiver_request, _) = match payjoin_proposal.clone().extract_v2_req() {
					Ok((req, ctx)) => (req, ctx),
					Err(e) => {
						dbg!(&e);
						return;
					},
				};
				let tx = payjoin_proposal.psbt().clone().extract_tx();
				scheduler.set_funding_tx_created(
					channel_id,
					&receiver_request.url,
					receiver_request.body,
				);
				match self.channel_manager.unsafe_manual_funding_transaction_generated(
					&ChannelId::from_bytes(temporary_channel_id),
					&counterparty_node_id,
					tx.clone(),
				) {
					Ok(_) => {
						// Created Funding Transaction and waiting for `FundingTxBroadcastSafe` event before returning a response
						log_info!(self.logger, "Created channel funding transaction from Payjoin request and waiting for `FundingTxBroadcastSafe`");
					},
					Err(_) => {
						log_info!(
							self.logger,
							"Unable to channel create funding tx from Payjoin request"
						);
					},
				}
			} else {
				log_info!(
					self.logger,
					"Couldnt match a channel to Payjoin request, accepting normally"
				);
				self.accept_payjoin_transaction(provisional_proposal).await;
			}
		} else {
			log_info!(self.logger, "Payjoin Receiver: Unable to get enrolled object");
		}
	}

	async fn accept_payjoin_transaction(&self, mut provisional_proposal: ProvisionalProposal) {
		// Preserve privacy
		let (candidate_inputs, utxo_set) = match self.wallet.payjoin_receiver_candidate_input() {
			Ok(a) => a,
			Err(e) => {
				log_info!(self.logger, "Didnt find candidate inputs: {}", e);
				return;
			},
		};
		match provisional_proposal.try_preserving_privacy(candidate_inputs) {
			Ok(selected_outpoint) => {
				if let Some(selected_utxo) = utxo_set.iter().find(|i| {
					i.outpoint.txid == selected_outpoint.txid
						&& i.outpoint.vout == selected_outpoint.vout
				}) {
					let txo_to_contribute = bitcoin::TxOut {
						value: selected_utxo.txout.value,
						script_pubkey: selected_utxo.txout.script_pubkey.clone(),
					};
					let outpoint_to_contribute = bitcoin::OutPoint {
						txid: selected_utxo.outpoint.txid,
						vout: selected_utxo.outpoint.vout,
					};
					provisional_proposal
						.contribute_witness_input(txo_to_contribute, outpoint_to_contribute);
				}
			},
			Err(_) => {
				log_info!(self.logger, "Failed to select utxos to improve payjoin request privacy. Payjoin proceeds regardless");
			},
		};
		// Finalise Payjoin Proposal
		let mut payjoin_proposal = match provisional_proposal.finalize_proposal(
			|psbt| {
				self.wallet.prepare_payjoin_proposal(psbt.clone()).map_err(|e| {
					log_info!(self.logger, "Payjoin Receiver: Unable to sign proposal {}", e);
					payjoin::Error::Server(e.into())
				})
			},
			None,
		) {
			Ok(proposal) => proposal,
			Err(e) => {
				log_info!(self.logger, "Payjoin Receiver: Unable to finalize proposal {}", e);
				return;
			},
		};

		let (receiver_request, _) = match payjoin_proposal.extract_v2_req() {
			Ok(req) => req,
			Err(e) => {
				log_info!(self.logger, "Payjoin Receiver: Unable to extract V2 request {}", e);
				return;
			},
		};
		// Send Payjoin Proposal response back to Payjoin sender
		match reqwest::Client::new()
			.post(&receiver_request.url.to_string())
			.body(receiver_request.body)
			.headers(ohttp_headers())
			.send()
			.await
		{
			Ok(response) => {
				if response.status() == reqwest::StatusCode::OK {
					log_info!(self.logger, "Payjoin Receiver: Payjoin response sent to sender");
				} else {
					log_info!(
						self.logger,
						"Payjoin Receiver: Got non-200 response from directory {}",
						response.status()
					);
				}
			},
			Err(e) => {
				log_info!(
					self.logger,
					"Payjoin Receiver: Unable to make request to directory {}",
					e
				);
			},
		};
	}

	/// Enrolls a Payjoin receiver with the specified Payjoin directory.
	///
	/// If directory `ohttp_keys` are not provided, they will be fetched from the directory using
	/// the Payjoin relay as proxy to improve privacy and not expose users IP address.
	async fn enroll(&self) -> Result<(), Error> {
		let ohttp_keys = match self.ohttp_keys.read().await.deref() {
			Some(okeys) => okeys.clone(),
			None => {
				let payjoin_directory = &self.payjoin_directory;
				let payjoin_directory = match payjoin_directory.join("/ohttp-keys") {
					Ok(payjoin_directory) => payjoin_directory,
					Err(e) => {
						log_info!(
							self.logger,
							"Payjoin Receiver: Unable to construct ohttp keys url {}",
							e
						);
						return Err(Error::PayjoinReceiverEnrollementFailed);
					},
				};
				let proxy = match reqwest::Proxy::all(self.payjoin_relay.to_string()) {
					Ok(proxy) => proxy,
					Err(e) => {
						log_info!(
							self.logger,
							"Payjoin Receiver: Unable to construct reqwest proxy {}",
							e
						);
						return Err(Error::PayjoinReceiverEnrollementFailed);
					},
				};
				let client = match reqwest::Client::builder().proxy(proxy).build() {
					Ok(client) => client,
					Err(e) => {
						log_info!(
							self.logger,
							"Payjoin Receiver: Unable to construct reqwest client {}",
							e
						);
						return Err(Error::PayjoinReceiverEnrollementFailed);
					},
				};
				let response = match client.get(payjoin_directory).send().await {
					Ok(response) => response,
					Err(e) => {
						log_info!(
							self.logger,
							"Payjoin Receiver: Unable to make request to fetch ohttp keys {}",
							e
						);
						return Err(Error::PayjoinReceiverEnrollementFailed);
					},
				};
				if response.status() != reqwest::StatusCode::OK {
					log_info!(
						self.logger,
						"Payjoin Receiver: Got non 200 response when fetching ohttp keys {}",
						response.status()
					);
					return Err(Error::PayjoinReceiverEnrollementFailed);
				}
				let response = match response.bytes().await {
					Ok(response) => response,
					Err(e) => {
						log_info!(
							self.logger,
							"Payjoin Receiver: Error reading ohttp keys response {}",
							e
						);
						return Err(Error::PayjoinReceiverEnrollementFailed);
					},
				};
				OhttpKeys::decode(response.to_vec().as_slice()).map_err(|e| {
					log_info!(self.logger, "Payjoin Receiver: Unable to decode ohttp keys {}", e);
					Error::PayjoinReceiverEnrollementFailed
				})?
			},
		};
		let mut enroller = Enroller::from_directory_config(
			self.payjoin_directory.clone(),
			ohttp_keys.clone(),
			self.payjoin_relay.clone(),
		);
		let (req, ctx) = match enroller.extract_req() {
			Ok(req) => req,
			Err(e) => {
				log_info!(
					self.logger,
					"Payjoin Receiver: unable to extract enrollement request {}",
					e
				);
				return Err(Error::PayjoinReceiverEnrollementFailed);
			},
		};
		let response = match reqwest::Client::new()
			.post(&req.url.to_string())
			.body(req.body)
			.headers(ohttp_headers())
			.send()
			.await
		{
			Ok(response) => response,
			Err(_) => {
				log_info!(self.logger, "Payjoin Receiver: unable to make enrollement request");
				return Err(Error::PayjoinReceiverEnrollementFailed);
			},
		};
		let response = match response.bytes().await {
			Ok(response) => response,
			Err(_) => {
				panic!("Error reading response");
			},
		};
		let enrolled = match enroller.process_res(response.to_vec().as_slice(), ctx) {
			Ok(enrolled) => enrolled,
			Err(e) => {
				log_info!(
					self.logger,
					"Payjoin Receiver: unable to process enrollement response {}",
					e
				);
				return Err(Error::PayjoinReceiverEnrollementFailed);
			},
		};

		*self.ohttp_keys.write().await = Some(ohttp_keys);
		*self.enrolled.write().await = Some(enrolled);
		Ok(())
	}

	async fn is_enrolled(&self) -> bool {
		self.enrolled.read().await.deref().is_some()
			&& self.ohttp_keys.read().await.deref().is_some()
	}

	/// Schedule a channel to opened upon receiving a Payjoin tranasction value with the same
	/// channel funding amount.
	pub(crate) async fn schedule_channel(
		&self, amount: bitcoin::Amount, counterparty_node_id: bitcoin::secp256k1::PublicKey,
		channel_id: u128,
	) {
		let channel = PayjoinChannel::new(amount, counterparty_node_id, channel_id);
		self.channel_scheduler.write().await.schedule(
			channel.channel_value_satoshi(),
			channel.counterparty_node_id(),
			channel.channel_id(),
		);
	}

	/// This should only be called upon receiving [`Event::FundingTxBroadcastSafe`]
	///
	/// [`Event::FundingTxBroadcastSafe`]: lightning::events::Event::FundingTxBroadcastSafe
	pub(crate) async fn set_funding_tx_signed(
		&self, funding_tx: Transaction,
	) -> Option<(payjoin::Url, Vec<u8>)> {
		self.channel_scheduler.write().await.set_funding_tx_signed(funding_tx)
	}

	/// Validate an incoming Payjoin request as specified in [BIP78].
	///
	/// [BIP78]: https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki#user-content-Receivers_original_PSBT_checklist
	async fn validate_payjoin_request(
		&self, proposal: UncheckedProposal,
	) -> Result<ProvisionalProposal, Error> {
		let wallet = &self.wallet;
		let proposal = proposal.assume_interactive_receiver();
		let proposal = proposal
			.check_inputs_not_owned(|script| {
				Ok(wallet.is_mine(&script.to_owned()).unwrap_or(false))
			})
			.map_err(|e| {
				log_info!(self.logger, "Inputs owned by us check failed {}", e);
				Error::PayjoinReceiverRequestValidationFailed
			})?;
		let proposal = proposal.check_no_mixed_input_scripts().map_err(|e| {
			log_info!(self.logger, "Mixed input scripts check failed {}", e);
			Error::PayjoinReceiverRequestValidationFailed
		})?;
		// Fixme: discuss how to handle this, instead of the Ok(false) we should have a way to
		// store seen outpoints and check against them
		let proposal =
			proposal.check_no_inputs_seen_before(|_outpoint| Ok(false)).map_err(|e| {
				log_info!(self.logger, "Inputs seen before check failed {}", e);
				Error::PayjoinReceiverRequestValidationFailed
			})?;
		let provisional_proposal = proposal
			.identify_receiver_outputs(|script| {
				Ok(wallet.is_mine(&script.to_owned()).unwrap_or(false))
			})
			.map_err(|e| {
				log_info!(self.logger, "Identify receiver outputs failed {}", e);
				Error::PayjoinReceiverRequestValidationFailed
			})?;
		Ok(provisional_proposal)
	}
}
