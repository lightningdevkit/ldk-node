// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::consensus::encode::serialize_hex;
use bitcoin::{Amount, FeeRate, TxIn, Weight};
use lightning::ln::channelmanager::PaymentId;
use payjoin::persist::{AsyncSessionPersister, OptionalTransitionOutcome};
use payjoin::receive::InputPair;
use payjoin::ImplementationError;

use crate::chain::ChainSource;
use crate::config::Config;
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::payment::payjoin_payment::payjoin_session::{PayjoinDirection, PayjoinStatus};
use crate::types::{Broadcaster, DynStore};
use crate::Error;
use crate::{
	payment::payjoin_payment::persist::KVStorePayjoinReceiverPersister, types::PayjoinSessionStore,
	wallet::Wallet,
};
use payjoin::bitcoin::psbt::Input;
use payjoin::io::fetch_ohttp_keys;
use payjoin::receive::v2::{
	replay_event_log_async as replay_receiver_event_log_async, HasReplyableError, Initialized,
	MaybeInputsOwned, MaybeInputsSeen, Monitor, OutputsUnknown, PayjoinProposal,
	ProvisionalProposal, ReceiveSession, Receiver, ReceiverBuilder, UncheckedOriginalPayload,
	WantsFeeRange, WantsInputs, WantsOutputs,
};
use rand::RngCore;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct PayjoinManager {
	payjoin_session_store: Arc<PayjoinSessionStore>,
	kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
	config: Arc<Config>,
	broadcaster: Arc<Broadcaster>,
	wallet: Arc<Wallet>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	chain_source: Arc<ChainSource>,
	stop_receiver: tokio::sync::watch::Receiver<()>,
}

// UPDATE ERROR TYPES!!!!!!!!!
// UPDATE ERROR TYPES!!!!!!!!!
// UPDATE ERROR TYPES!!!!!!!!!
// UPDATE ERROR TYPES!!!!!!!!!
// UPDATE ERROR TYPES!!!!!!!!!
// UPDATE ERROR TYPES!!!!!!!!!

impl PayjoinManager {
	pub(crate) fn new(
		payjoin_session_store: Arc<PayjoinSessionStore>, kv_store: Arc<DynStore>,
		logger: Arc<Logger>, config: Arc<Config>, broadcaster: Arc<Broadcaster>,
		wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		chain_source: Arc<ChainSource>, stop_receiver: tokio::sync::watch::Receiver<()>,
	) -> Self {
		Self {
			payjoin_session_store,
			kv_store,
			logger,
			config,
			broadcaster,
			wallet,
			fee_estimator,
			chain_source,
			stop_receiver,
		}
	}

	async fn receive_payjoin(
		&self, amount: Amount, fee_rate: Option<FeeRate>,
	) -> Result<(), Error> {
		let payjoin_config = self.config.payjoin_config.as_ref().ok_or(Error::InvalidAddress)?;

		// Generate a new session ID
		let mut random_bytes = [0u8; 32];
		rand::rng().fill_bytes(&mut random_bytes);
		let session_id = PaymentId(random_bytes);

		// Create a new persister for this session
		let persister = KVStorePayjoinReceiverPersister::new(
			session_id,
			self.kv_store.clone(),
			self.logger.clone(),
		)?;

		let address = self.wallet.get_new_address()?;
		let ohttp_keys = fetch_ohttp_keys(
			payjoin_config.ohttp_relay.clone().as_str(),
			payjoin_config.payjoin_directory.clone().as_str(),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Failed to fetch OHTTP keys: {}", e);
			Error::InvalidAddress // or create a new error variant like Error::OhttpKeyFetchFailed
		})?;
		log_debug!(self.logger, "Fetched OHTTP keys: {:?}", ohttp_keys);

		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let session = ReceiverBuilder::new(
			address,
			payjoin_config.payjoin_directory.clone().as_str(),
			ohttp_keys,
		)
		.map_err(|e| {
			log_error!(self.logger, "Failed to create receiver builder: {}", e);
			Error::InvalidAddress // or another appropriate variant
		})?
		.with_amount(amount)
		.with_max_fee_rate(fee_rate)
		.build()
		.save_async(&persister)
		.await?;

		log_info!(self.logger, "Receive session established");
		let pj_uri = session.pj_uri();
		log_info!(self.logger, "Request Payjoin by sharing this Payjoin Uri: {}", pj_uri);

		self.process_receiver_session(ReceiveSession::Initialized(session.clone()), &persister)
			.await?;
		Ok(())
	}

	async fn process_receiver_session(
		&self, session: ReceiveSession, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let res = {
			match session {
				ReceiveSession::Initialized(proposal) => {
					self.read_from_directory(proposal, persister).await
				},
				ReceiveSession::UncheckedOriginalPayload(proposal) => {
					self.check_proposal(proposal, persister).await
				},
				ReceiveSession::MaybeInputsOwned(proposal) => {
					self.check_inputs_not_owned(proposal, persister).await
				},
				ReceiveSession::MaybeInputsSeen(proposal) => {
					self.check_no_inputs_seen_before(proposal, persister).await
				},
				ReceiveSession::OutputsUnknown(proposal) => {
					self.identify_receiver_outputs(proposal, persister).await
				},
				ReceiveSession::WantsOutputs(proposal) => {
					self.commit_outputs(proposal, persister).await
				},
				ReceiveSession::WantsInputs(proposal) => {
					self.contribute_inputs(proposal, persister).await
				},
				ReceiveSession::WantsFeeRange(proposal) => {
					self.apply_fee_range(proposal, persister).await
				},
				ReceiveSession::ProvisionalProposal(proposal) => {
					self.finalize_proposal(proposal, persister).await
				},
				ReceiveSession::PayjoinProposal(proposal) => {
					self.send_payjoin_proposal(proposal, persister).await
				},
				ReceiveSession::HasReplyableError(error) => {
					self.handle_error(error, persister).await
				},
				ReceiveSession::Monitor(proposal) => {
					self.monitor_payjoin_proposal(proposal, persister).await
				},
				ReceiveSession::Closed(_) => return Err(Error::InvalidAddress),
			}
		};
		res
	}

	async fn read_from_directory(
		&self, session: Receiver<Initialized>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let mut interrupt = self.stop_receiver.clone();
		let receiver = tokio::select! {
			res = self.long_poll_fallback(session, &*persister) => res,
			_ = interrupt.changed() => {
				log_error!(self.logger, "Interrupted. Call the `resume` command to resume all sessions.");
				return Err(Error::InvalidAddress);
			}
		}?;
		self.check_proposal(receiver, &*persister).await
	}

	async fn long_poll_fallback(
		&self, session: Receiver<Initialized>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<Receiver<UncheckedOriginalPayload>, Error> {
		let payjoin_config = self.config.payjoin_config.as_ref().ok_or(Error::InvalidAddress)?;
		let ohttp_relay = payjoin_config.ohttp_relay.clone();

		let mut session = session;
		loop {
			let (req, context) =
				session.create_poll_request(ohttp_relay.as_str()).map_err(|e| {
					log_error!(self.logger, "Failed to create poll request: {}", e);
					Error::InvalidAddress
				})?;
			log_debug!(self.logger, "Polling receive request...");
			let ohttp_response = self.post_request(req).await?;
			let state_transition = session
				.process_response(ohttp_response.as_bytes().to_vec().as_slice(), context)
				.save_async(persister)
				.await;
			match state_transition {
				Ok(OptionalTransitionOutcome::Progress(next_state)) => {
					log_info!(
						self.logger,
						"Got a request from the sender. Responding with a Payjoin proposal."
					);
					return Ok(next_state);
				},
				Ok(OptionalTransitionOutcome::Stasis(current_state)) => {
					session = current_state;
					continue;
				},
				Err(_) => return Err(Error::PersistenceFailed),
			}
		}
	}

	async fn post_request(&self, req: payjoin::Request) -> Result<bitreq::Response, Error> {
		bitreq::post(req.url)
			.with_header("Content-Type", req.content_type)
			.with_body(req.body)
			.send_async()
			.await
			.map_err(|e| {
				log_error!(self.logger, "HTTP request failed: {}", e);
				Error::InvalidAddress
			})
	}

	async fn check_proposal(
		&self, proposal: Receiver<UncheckedOriginalPayload>,
		persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal
			.check_broadcast_suitability(None, |tx| {
				self.chain_source
					.can_broadcast_transaction(tx)
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;

		log_info!(self.logger, "Fallback transaction received. Consider broadcasting this to get paid if the Payjoin fails: {}", serialize_hex(&proposal.extract_tx_to_schedule_broadcast()));
		self.check_inputs_not_owned(proposal, persister).await
	}

	async fn check_inputs_not_owned(
		&self, proposal: Receiver<MaybeInputsOwned>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal
			.check_inputs_not_owned(&mut |input| {
				self.wallet
					.is_mine(input.to_owned())
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;

		self.check_no_inputs_seen_before(proposal, persister).await
	}

	async fn check_no_inputs_seen_before(
		&self, proposal: Receiver<MaybeInputsSeen>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal
			// TODO: DETERMINE IF SAVING THE INPUT AT THIS POINT IS NECESSARY FOR NOW WE JUST RETURN FALSE
			// BUT I THINK IT WOULD BE BETTER TO SAVE IT SO THAT IF THE SESSION IS RESUMED WE CAN CHECK AGAIN
			.check_no_inputs_seen_before(&mut |_| Ok(false))
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;
		self.identify_receiver_outputs(proposal, persister).await
	}

	async fn identify_receiver_outputs(
		&self, proposal: Receiver<OutputsUnknown>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal
			.identify_receiver_outputs(&mut |output_script| {
				self.wallet
					.is_mine(output_script.to_owned())
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;
		self.commit_outputs(proposal, persister).await
	}

	async fn commit_outputs(
		&self, proposal: Receiver<WantsOutputs>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal.commit_outputs().save_async(persister).await?;
		self.contribute_inputs(proposal, persister).await
	}

	async fn contribute_inputs(
		&self, proposal: Receiver<WantsInputs>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let candidate_inputs = self.list_input_pairs()?;

		if candidate_inputs.is_empty() {
			return Err({
				log_error!(
					self.logger,
					"No spendable UTXOs available in wallet. Cannot contribute inputs to payjoin."
				);
				Error::InvalidAddress
			});
		}

		let selected_input =
			proposal.try_preserving_privacy(candidate_inputs).map_err(|_| Error::InvalidAddress)?;
		let proposal = proposal
			.contribute_inputs(vec![selected_input])
			.map_err(|_| Error::InvalidAddress)?
			.commit_inputs()
			.save_async(persister)
			.await?;
		self.apply_fee_range(proposal, persister).await
	}

	fn list_input_pairs(&self) -> Result<Vec<InputPair>, Error> {
		let unspent = self.wallet.list_unspent_utxos()?;

		let mut input_pairs = Vec::with_capacity(unspent.len());

		for u in unspent {
			let txin = TxIn { previous_output: u.outpoint, ..Default::default() };
			let psbtin = Input { witness_utxo: Some(u.output.clone()), ..Default::default() };
			let satisfaction_weight = Weight::from_wu(u.satisfaction_weight);

			let input_pair =
				InputPair::new(txin, psbtin, Some(satisfaction_weight)).map_err(|e| {
					log_error!(self.logger, "Failed to create InputPair: {}", e);
					Error::InvalidAddress
				})?;

			input_pairs.push(input_pair);
		}

		Ok(input_pairs)
	}

	async fn apply_fee_range(
		&self, proposal: Receiver<WantsFeeRange>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal
			.apply_fee_range(None, None)
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;

		self.finalize_proposal(proposal, persister).await
	}

	async fn finalize_proposal(
		&self, proposal: Receiver<ProvisionalProposal>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let proposal = proposal
			.finalize_proposal(|psbt| {
				self.wallet
					.process_psbt(psbt.clone())
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;
		self.send_payjoin_proposal(proposal, persister).await
	}

	async fn send_payjoin_proposal(
		&self, proposal: Receiver<PayjoinProposal>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let payjoin_config = self.config.payjoin_config.as_ref().ok_or(Error::InvalidAddress)?;
		let ohttp_relay = payjoin_config.ohttp_relay.clone();
		let (req, ohttp_ctx) = proposal.create_post_request(ohttp_relay.as_str()).map_err(|e| {
			log_error!(self.logger, "v2 req extraction failed {}", e);
			Error::InvalidAddress
		})?;
		let res = self.post_request(req).await?;
		let payjoin_psbt = proposal.psbt().clone();
		let session = proposal
			.process_response(&res.as_bytes(), ohttp_ctx)
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;

		log_info!(
			self.logger,
			"Response successful. Watch mempool for successful Payjoin. TXID: {}",
			payjoin_psbt.extract_tx_unchecked_fee_rate().compute_txid()
		);

		return self.monitor_payjoin_proposal(session, persister).await;
	}

	async fn handle_error(
		&self, session: Receiver<HasReplyableError>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let payjoin_config = self.config.payjoin_config.as_ref().ok_or(Error::InvalidAddress)?;
		let ohttp_relay = payjoin_config.ohttp_relay.clone();

		let (err_req, err_ctx) = session
			.create_error_request(ohttp_relay.as_str())
			.map_err(|_| Error::InvalidAddress)?;

		let err_response = match self.post_request(err_req).await {
			Ok(response) => response,
			Err(_) => return Err(Error::InvalidAddress),
		};

		let err_bytes = err_response.as_bytes();

		if let Err(_) =
			session.process_error_response(&err_bytes, err_ctx).save_async(persister).await
		{
			return Err(Error::InvalidAddress);
		}

		Ok(())
	}

	async fn monitor_payjoin_proposal(
		&self, proposal: Receiver<Monitor>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		// On a session resumption, the receiver will resume again in this state.
		let poll_interval = tokio::time::Duration::from_millis(200);
		let timeout_duration = tokio::time::Duration::from_secs(5);

		let mut interval = tokio::time::interval(poll_interval);
		interval.tick().await;

		log_debug!(self.logger, "Polling for payment confirmation");

		let result = tokio::time::timeout(timeout_duration, async {
			loop {
				interval.tick().await;
				let check_result = proposal
					.check_payment(|txid| {
						self.chain_source
							.get_transaction(&txid)
							.map_err(|e| ImplementationError::from(e.to_string().as_str()))
					})
					.save_async(persister)
					.await;

				match check_result {
					Ok(_) => {
						log_info!(self.logger, "Payjoin transaction detected in the mempool!");
						return Ok(());
					},
					Err(_) => {
						// keep polling
						continue;
					},
				}
			}
		})
		.await;

		match result {
			Ok(ok) => ok,
			Err(_) => Err({
				log_error!(
					self.logger,
					"Timeout waiting for payment confirmation after {:?}",
					timeout_duration
				);
				Error::InvalidAddress
			}),
		}
	}

	async fn resume_payjoins(&self) -> Result<(), Error> {
		let recv_session_ids = self
			.payjoin_session_store
			.list_filter(|p| {
				p.direction == PayjoinDirection::Receive && p.status == PayjoinStatus::Active
			})
			.iter()
			.map(|s| s.session_id.clone())
			.collect::<Vec<PaymentId>>();

		if recv_session_ids.is_empty() {
			log_info!(self.logger, "No sessions to resume.");
			return Ok(());
		}

		let mut tasks = Vec::new();

		// Process receiver sessions
		for session_id in recv_session_ids {
			let self_clone = self.clone();
			// Create a persister for this session
			let recv_persister = match KVStorePayjoinReceiverPersister::from_session(
				session_id.clone(),
				self.kv_store.clone(),
				self.logger.clone(),
			) {
				Ok(p) => p,
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to create persister for session {:?}: {:?}",
						session_id,
						e
					);
					continue;
				},
			};

			match replay_receiver_event_log_async(&recv_persister).await {
				Ok((receiver_state, _)) => {
					tasks.push(tokio::spawn(async move {
						self_clone.process_receiver_session(receiver_state, &recv_persister).await
					}));
				},
				Err(e) => {
					log_error!(
						self.logger,
						"An error {:?} occurred while replaying receiver session",
						e
					);
					self.close_failed_session(&recv_persister, &session_id, "receiver").await;
				},
			}
		}

		let mut interrupt = self.stop_receiver.clone();
		tokio::select! {
			_ = async {
				for task in tasks {
					let _ = task.await;
				}
			} => {
				println!("All payjoin resumed sessions completed.");
			}
			_ = interrupt.changed() => {
				println!("Resumed payjoin sessions were interrupted.");
			}
		}
		Ok(())
	}

	async fn close_failed_session<P>(&self, persister: &P, session_id: &PaymentId, role: &str)
	where
		P: AsyncSessionPersister,
	{
		if let Err(close_err) = AsyncSessionPersister::close(persister).await {
			log_error!(
				self.logger,
				"Failed to close {} session {}: {:?}",
				role,
				session_id,
				close_err
			);
		} else {
			log_error!(self.logger, "Closed failed {} session: {}", role, session_id);
		}
	}
}
