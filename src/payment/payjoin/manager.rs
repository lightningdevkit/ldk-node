// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::{Amount, FeeRate, TxIn, Weight};
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::ln::channelmanager::PaymentId;
use payjoin::persist::{AsyncSessionPersister, OptionalTransitionOutcome};
use payjoin::receive::InputPair;
use payjoin::ImplementationError;

use crate::chain::ChainSource;
use crate::config::{Config, PayjoinConfig, PAYJOIN_SESSION_CLEANUP_AGE_SECS};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::payment::payjoin::payjoin_session::{PayjoinDirection, PayjoinStatus};
use crate::payment::{
	ConfirmationStatus, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
	PendingPaymentDetails,
};
use crate::total_anchor_channels_reserve_sats;
use crate::types::{Broadcaster, ChannelManager};
use crate::types::{PaymentStore, PendingPaymentStore};
use crate::Error;
use crate::{
	payment::payjoin::persist::KVStorePayjoinReceiverPersister, types::PayjoinSessionStore,
	wallet::Wallet,
};
use payjoin::bitcoin::psbt::Input;
use payjoin::io::fetch_ohttp_keys;
use payjoin::receive::v2::{
	replay_event_log_async as replay_receiver_event_log_async, HasReplyableError, Initialized,
	MaybeInputsOwned, MaybeInputsSeen, Monitor, OutputsUnknown, PayjoinProposal,
	ProvisionalProposal, ReceiveSession, Receiver, ReceiverBuilder, SessionOutcome,
	UncheckedOriginalPayload, WantsFeeRange, WantsInputs, WantsOutputs,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub(crate) struct PayjoinManager {
	payjoin_session_store: Arc<PayjoinSessionStore>,
	logger: Arc<Logger>,
	config: Arc<Config>,
	wallet: Arc<Wallet>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	chain_source: Arc<ChainSource>,
	channel_manager: Arc<ChannelManager>,
	stop_receiver: tokio::sync::watch::Receiver<()>,
	payment_store: Arc<PaymentStore>,
	pending_payment_store: Arc<PendingPaymentStore>,
	broadcaster: Arc<Broadcaster>,
}

impl PayjoinManager {
	pub(crate) fn new(
		payjoin_session_store: Arc<PayjoinSessionStore>, logger: Arc<Logger>, config: Arc<Config>,
		wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		chain_source: Arc<ChainSource>, channel_manager: Arc<ChannelManager>,
		stop_receiver: tokio::sync::watch::Receiver<()>, payment_store: Arc<PaymentStore>,
		pending_payment_store: Arc<PendingPaymentStore>, broadcaster: Arc<Broadcaster>,
	) -> Self {
		Self {
			payjoin_session_store,
			logger,
			config,
			wallet,
			fee_estimator,
			chain_source,
			channel_manager,
			stop_receiver,
			payment_store,
			pending_payment_store,
			broadcaster,
		}
	}

	pub(crate) async fn receive_payjoin(
		&self, amount_sats: u64, fee_rate: Option<FeeRate>,
	) -> Result<String, Error> {
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		if payjoin_config.ohttp_relays.is_empty() {
			log_error!(self.logger, "No OHTTP relays configured.");
			return Err(Error::PayjoinNotConfigured);
		}

		// Generate a new session ID
		let mut random_bytes = [0u8; 32];
		getrandom::fill(&mut random_bytes).map_err(|e| {
			log_error!(self.logger, "Failed to generate random session ID: {}", e);
			Error::PayjoinSessionCreationFailed
		})?;
		let session_id = PaymentId(random_bytes);

		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		// Create a new persister for this session
		let persister = KVStorePayjoinReceiverPersister::new(
			session_id,
			Some(amount_sats * 1000),
			self.payjoin_session_store.clone(),
			fee_rate.to_sat_per_kwu(),
			None,
			None,
			None,
		)?;

		let address = self.wallet.get_new_address()?;
		let ohttp_keys = {
			let mut result = Err(Error::ConnectionFailed);
			for relay in Self::relay_order(payjoin_config) {
				match fetch_ohttp_keys(relay, payjoin_config.payjoin_directory.as_str()).await {
					Ok(keys) => {
						result = Ok(keys);
						break;
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to fetch OHTTP keys via {}: {}. Trying next relay.",
							relay,
							e
						);
					},
				}
			}
			result
		}?;
		log_debug!(self.logger, "Fetched OHTTP keys: {:?}", ohttp_keys);

		let amount = Amount::from_sat(amount_sats);

		let session = ReceiverBuilder::new(
			address,
			payjoin_config.payjoin_directory.clone().as_str(),
			ohttp_keys,
		)
		.map_err(|e| {
			log_error!(self.logger, "Failed to create receiver builder: {}", e);
			Error::PayjoinSessionCreationFailed
		})?
		.with_amount(amount)
		.with_max_fee_rate(fee_rate)
		.build()
		.save_async(&persister)
		.await
		.map_err(|_| Error::PersistenceFailed)?;

		log_info!(self.logger, "Receive session established");
		let pj_uri = session.pj_uri();
		log_info!(self.logger, "Request Payjoin by sharing this Payjoin Uri: {}", pj_uri);

		Ok(pj_uri.to_string())
	}

	fn relay_order<'a>(payjoin_config: &'a PayjoinConfig) -> Vec<&'a str> {
		let count = payjoin_config.ohttp_relays.len();
		let start = if count > 0 {
			let mut bytes = [0u8; 8];
			getrandom::fill(&mut bytes).unwrap_or(());
			usize::from_ne_bytes(bytes) % count
		} else {
			0
		};
		(0..count).map(|i| payjoin_config.ohttp_relays[(start + i) % count].as_str()).collect()
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
				ReceiveSession::Closed(outcome) => {
					self.handle_closed_session(outcome, persister).await
				},
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
				log_info!(self.logger, "Session interrupted by node shutdown. Will resume on restart.");
				return Err(Error::NotRunning);
			}
		}?;
		self.check_proposal(receiver, &*persister).await
	}

	async fn long_poll_fallback(
		&self, session: Receiver<Initialized>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<Receiver<UncheckedOriginalPayload>, Error> {
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		let mut session = session;
		loop {
			let (ohttp_response, context) = {
				let mut result = Err(Error::ConnectionFailed);
				for relay in Self::relay_order(payjoin_config) {
					let (req, ctx) = match session.create_poll_request(relay) {
						Ok(r) => r,
						Err(e) => {
							// create_poll_request is fatal. No fallback tx exists yet at this stage
							// (sender hasn't sent their PSBT), so we just fail the session.
							log_error!(self.logger, "Failed to create poll request: {}", e);
							result = Err(Error::PayjoinSessionFailed);
							self.handle_closed_session(SessionOutcome::Failure, persister).await?;
							break;
						},
					};
					match self.post_request(req).await {
						Ok(resp) => {
							result = Ok((resp, ctx));
							break;
						},
						Err(e) => {
							log_error!(self.logger, "Polling failed via relay, trying next: {}", e);
						},
					}
				}
				result
			}?;
			log_debug!(self.logger, "Polling receive request...");
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
				Error::ConnectionFailed
			})
	}

	async fn check_proposal(
		&self, proposal: Receiver<UncheckedOriginalPayload>,
		persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let session_id = persister.session_id();
		let mut session =
			self.payjoin_session_store.get(&session_id).ok_or(Error::InvalidPaymentId)?;

		// Note: broadcast suitability can only be verified when using the BitcoindRpc backend.
		// For Esplora and Electrum backends this check will return an error as they do not
		// support a testmempoolaccept equivalent.
		let proposal = proposal
			.check_broadcast_suitability(None, |tx| {
				self.chain_source
					.can_broadcast_transaction(tx)
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;

		// If the payjoin fails or times out, broadcast this fallback tx to ensure the receiver still gets paid.
		let fallback_tx = proposal.extract_tx_to_schedule_broadcast();
		session.fallback_tx = Some(fallback_tx);
		self.payjoin_session_store.insert_or_update(session)?;

		log_info!(
			self.logger,
			"Fallback transaction received. This will be broadcast if the Payjoin fails"
		);
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
			.check_no_inputs_seen_before(&mut |input| {
				persister
					.insert_input_seen(*input)
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
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
		let proposal = proposal
			.commit_outputs()
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;
		self.contribute_inputs(proposal, persister).await
	}

	async fn contribute_inputs(
		&self, proposal: Receiver<WantsInputs>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		// Check wallet has spendable funds after accounting for anchor reserve
		let cur_anchor_reserve_sats =
			total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let spendable_amount_sats =
			self.wallet.get_spendable_amount_sats(cur_anchor_reserve_sats).unwrap_or(0);

		if spendable_amount_sats == 0 {
			log_error!(
				self.logger,
				"No spendable funds available after anchor reserve. Cannot contribute inputs to payjoin."
			);
			return Err(Error::InsufficientFunds);
		}

		let candidate_inputs = self.list_input_pairs()?;

		if candidate_inputs.is_empty() {
			return Err({
				log_error!(
					self.logger,
					"No spendable UTXOs available in wallet. Cannot contribute inputs to payjoin."
				);
				Error::InsufficientFunds
			});
		}

		let selected_input = proposal.try_preserving_privacy(candidate_inputs).map_err(|e| {
			log_error!(self.logger, "Failed to select input for payjoin contribution: {}", e);
			Error::PayjoinSessionFailed
		})?;
		let proposal = proposal
			.contribute_inputs(vec![selected_input])
			.map_err(|e| {
				log_error!(self.logger, "Failed to contribute inputs to payjoin: {}", e);
				Error::PayjoinSessionFailed
			})?
			.commit_inputs()
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;
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
					Error::PayjoinSessionFailed
				})?;

			input_pairs.push(input_pair);
		}

		Ok(input_pairs)
	}

	async fn apply_fee_range(
		&self, proposal: Receiver<WantsFeeRange>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let session = persister.get_session().ok_or(Error::InvalidPaymentId)?;
		let fee_rate = FeeRate::from_sat_per_kwu(session.fee_rate_kwu);

		let proposal = proposal
			.apply_fee_range(None, Some(fee_rate))
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
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		let session_id = persister.session_id();
		let mut payjoin_session =
			self.payjoin_session_store.get(&session_id).ok_or(Error::InvalidPaymentId)?;

		let (res, ohttp_ctx) = {
			let mut result = Err(Error::ConnectionFailed);
			for relay in Self::relay_order(payjoin_config) {
				let (req, ctx) = match proposal.create_post_request(relay) {
					Ok(r) => r,
					Err(e) => {
						// create_post_request is fatal, so we trigger a session failure
						// immediately and broadcast the fallback transaction.
						log_error!(self.logger, "v2 req extraction failed {}", e);
						result = Err(Error::PayjoinSessionFailed);
						self.handle_closed_session(SessionOutcome::Failure, persister).await?;
						break;
					},
				};
				match self.post_request(req).await {
					Ok(resp) => {
						result = Ok((resp, ctx));
						break;
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Proposal send failed via relay, trying next: {}",
							e
						);
					},
				}
			}
			result
		}?;
		let payjoin_psbt = proposal.psbt().clone();
		let session = proposal
			.process_response(&res.as_bytes(), ohttp_ctx)
			.save_async(persister)
			.await
			.map_err(|_| Error::PersistenceFailed)?;

		// at this point we will persist the fee and txid to the session store
		let payjoin_psbt_clone = payjoin_psbt.clone();
		let fee = payjoin_psbt_clone.fee().unwrap_or(Amount::ZERO);
		let fee_sat = fee.to_sat();
		let txid = payjoin_psbt_clone.extract_tx_unchecked_fee_rate().compute_txid();

		payjoin_session.fee_paid_msat = Some(fee_sat * 1000);
		payjoin_session.txid = Some(txid);
		self.payjoin_session_store.insert_or_update(payjoin_session).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to update payjoin session for {:?}: {:?}",
				session_id,
				e
			);
			Error::PersistenceFailed
		})?;

		log_info!(
			self.logger,
			"Response successful. Watch mempool for successful Payjoin. TXID: {}",
			txid
		);

		self.monitor_payjoin_proposal(session, persister).await
	}

	async fn handle_error(
		&self, session: Receiver<HasReplyableError>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		let (err_response, err_ctx) = {
			let mut result = Err(Error::ConnectionFailed);
			for relay in Self::relay_order(payjoin_config) {
				let (req, ctx) = match session.create_error_request(relay) {
					Ok(r) => r,
					Err(_) => {
						result = Err(Error::PayjoinSessionFailed);
						break;
					},
				};
				match self.post_request(req).await {
					Ok(resp) => {
						result = Ok((resp, ctx));
						break;
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Error response send failed via relay, trying next: {}",
							e
						);
					},
				}
			}
			result
		}?;

		let err_bytes = err_response.as_bytes();

		if let Err(_) =
			session.process_error_response(&err_bytes, err_ctx).save_async(persister).await
		{
			return Err(Error::PersistenceFailed);
		}

		Ok(())
	}

	async fn handle_closed_session(
		&self, outcome: SessionOutcome, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let session_id = persister.session_id();
		let mut session =
			self.payjoin_session_store.get(&session_id).ok_or(Error::InvalidPaymentId)?;

		match outcome {
			SessionOutcome::Success(_) => {
				log_info!(
					self.logger,
					"Payjoin session detected in the mempool and completed successfully."
				);
				let kind = PaymentKind::Payjoin {
					txid: session.txid.ok_or_else(|| {
						log_error!(self.logger, "Payjoin session {} missing txid.", session_id);
						Error::PayjoinSessionFailed
					})?,
					status: ConfirmationStatus::Unconfirmed,
				};

				let payment = PaymentDetails::new(
					session_id,
					kind,
					session.amount_msat,
					session.fee_paid_msat,
					PaymentDirection::Inbound,
					PaymentStatus::Pending,
				);

				self.payment_store.insert_or_update(payment.clone())?;

				let pending_payment = PendingPaymentDetails::new(payment, Vec::new());
				self.pending_payment_store.insert_or_update(pending_payment)?;

				session.status = PayjoinStatus::Completed;
				session.completed_at = Some(
					SystemTime::now()
						.duration_since(UNIX_EPOCH)
						.unwrap_or(Duration::from_secs(0))
						.as_secs(),
				);
				self.payjoin_session_store.insert_or_update(session)?;
			},
			SessionOutcome::PayjoinProposalSent => {
				log_info!(
					self.logger,
					"Payjoin proposal sent. Cannot track broadcast due to non-SegWit sender inputs."
				);
				session.status = PayjoinStatus::Completed;
				session.completed_at = Some(
					SystemTime::now()
						.duration_since(UNIX_EPOCH)
						.unwrap_or(Duration::from_secs(0))
						.as_secs(),
				);
				self.payjoin_session_store.insert_or_update(session)?;
			},
			SessionOutcome::FallbackBroadcasted => {
				log_info!(self.logger, "Payjoin failed. Fallback transaction was broadcasted.");
				session.status = PayjoinStatus::Failed;
				self.payjoin_session_store.insert_or_update(session)?;
			},
			SessionOutcome::Cancel => {
				log_info!(self.logger, "Payjoin session was cancelled.");

				session.status = PayjoinStatus::Failed;
				self.payjoin_session_store.insert_or_update(session.clone())?;

				let fallback_tx = session.fallback_tx.as_ref().ok_or_else(|| {
					log_error!(
						self.logger,
						"Payjoin session {} missing fallback transaction.",
						session_id
					);
					Error::PayjoinSessionFailed
				})?;
				self.broadcaster.broadcast_transactions(&[(
					fallback_tx,
					lightning::chain::chaininterface::TransactionType::Sweep { channels: vec![] },
				)]);
			},
			SessionOutcome::Failure => {
				log_error!(self.logger, "Payjoin session failed due to a protocol error.");

				session.status = PayjoinStatus::Failed;
				self.payjoin_session_store.insert_or_update(session.clone())?;

				let fallback_tx = session.fallback_tx.as_ref().ok_or_else(|| {
					log_error!(
						self.logger,
						"Payjoin session {} missing fallback transaction.",
						session_id
					);
					Error::PayjoinSessionFailed
				})?;
				self.broadcaster.broadcast_transactions(&[(
					fallback_tx,
					lightning::chain::chaininterface::TransactionType::Sweep { channels: vec![] },
				)]);
			},
		}
		Ok(())
	}

	async fn monitor_payjoin_proposal(
		&self, proposal: Receiver<Monitor>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		// On a session resumption, the receiver will resume again in this state.
		let poll_interval = tokio::time::Duration::from_secs(2);

		let timeout_duration = tokio::time::Duration::from_secs(10);

		let mut interval = tokio::time::interval(poll_interval);
		interval.tick().await;

		log_debug!(self.logger, "Polling for payjoin transaction in the mempool...");

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
						continue;
					},
				}
			}
		})
		.await;

		match result {
			Ok(ok) => ok,
			Err(_) => {
				log_debug!(
					self.logger,
					"Payjoin transaction not yet seen after {:?}. Will retry on next background tick.",
					timeout_duration
				);
				Ok(())
			},
		}
	}

	pub(crate) async fn resume_payjoin_sessions(&self) -> Result<(), Error> {
		let recv_session_ids = self
			.payjoin_session_store
			.list_filter(|p| {
				p.direction == PayjoinDirection::Receive && p.status == PayjoinStatus::Active
			})
			.iter()
			.map(|s| s.session_id.clone())
			.collect::<Vec<PaymentId>>();

		if recv_session_ids.is_empty() {
			log_debug!(self.logger, "No sessions to resume.");
			return Ok(());
		}

		let mut join_set: tokio::task::JoinSet<Result<(), Error>> = tokio::task::JoinSet::new();

		// Process receiver sessions
		for session_id in recv_session_ids {
			let self_clone = self.clone();
			// Create a persister for this session
			let recv_persister = match KVStorePayjoinReceiverPersister::from_session(
				session_id.clone(),
				self.payjoin_session_store.clone(),
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
					join_set.spawn(async move {
						self_clone.process_receiver_session(receiver_state, &recv_persister).await
					});
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
				while let Some(result) = join_set.join_next().await {
					match result {
						Ok(Ok(())) => log_info!(self.logger, "A payjoin session completed successfully."),
						Ok(Err(e)) => log_error!(self.logger, "A payjoin session failed: {:?}", e),
						Err(e) => log_error!(self.logger, "A payjoin session task panicked: {:?}", e),
					}
				}
			} => {
				log_info!(self.logger, "All payjoin resumed sessions completed.");
			}
			_ = interrupt.changed() => {
				join_set.abort_all();
				log_info!(self.logger, "Resumed payjoin sessions were interrupted.");
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
			log_info!(self.logger, "Closed failed {} session: {}", role, session_id);
		}
	}

	/// Cleans up old payjoin sessions that are completed or failed.
	/// Sessions older than `PAYJOIN_SESSION_CLEANUP_AGE_SECS` will be removed.
	pub(crate) fn cleanup_old_sessions(&self) -> Result<(), Error> {
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or(std::time::Duration::from_secs(0))
			.as_secs();

		let sessions_to_remove: Vec<PaymentId> = self
			.payjoin_session_store
			.list_filter(|s| {
				let is_terminal =
					s.status == PayjoinStatus::Completed || s.status == PayjoinStatus::Failed;
				let age = now.saturating_sub(s.latest_update_timestamp);
				is_terminal && age > PAYJOIN_SESSION_CLEANUP_AGE_SECS
			})
			.iter()
			.map(|s| s.session_id)
			.collect();

		if sessions_to_remove.is_empty() {
			return Ok(());
		}

		log_info!(self.logger, "Cleaning up {} old payjoin sessions", sessions_to_remove.len());

		for session_id in sessions_to_remove {
			if let Err(e) = self.payjoin_session_store.remove(&session_id) {
				log_error!(
					self.logger,
					"Failed to remove old payjoin session {:?}: {:?}",
					session_id,
					e
				);
			}
		}

		Ok(())
	}
}
