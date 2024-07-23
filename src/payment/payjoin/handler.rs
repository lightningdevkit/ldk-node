use bitcoin::address::NetworkChecked;
use bitcoin::psbt::Psbt;
use bitcoin::{Script, Transaction, Txid};

use crate::config::PAYJOIN_REQUEST_TIMEOUT;
use crate::error::Error;
use crate::event::PayjoinPaymentFailureReason;
use crate::logger::FilesystemLogger;
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::PaymentKind;
use crate::payment::{PaymentDirection, PaymentStatus};
use crate::types::{ChainSource, EventQueue, PaymentStore, Wallet};
use crate::Event;
use crate::PaymentDetails;

use lightning::chain::Filter;
use lightning::ln::channelmanager::PaymentId;
use lightning::log_error;
use lightning::util::logger::Logger;

use std::sync::Arc;

pub(crate) struct PayjoinHandler {
	chain_source: Arc<ChainSource>,
	event_queue: Arc<EventQueue>,
	logger: Arc<FilesystemLogger>,
	payjoin_relay: payjoin::Url,
	payment_store: Arc<PaymentStore>,
	wallet: Arc<Wallet>,
}

impl PayjoinHandler {
	pub(crate) fn new(
		chain_source: Arc<ChainSource>, event_queue: Arc<EventQueue>,
		logger: Arc<FilesystemLogger>, payjoin_relay: payjoin::Url,
		payment_store: Arc<PaymentStore>, wallet: Arc<Wallet>,
	) -> Self {
		Self { chain_source, event_queue, logger, payjoin_relay, payment_store, wallet }
	}

	pub(crate) fn start_request(
		&self, payjoin_uri: payjoin::Uri<'_, NetworkChecked>,
	) -> Result<Psbt, Error> {
		let amount = payjoin_uri.amount.ok_or(Error::PayjoinRequestMissingAmount)?;
		let receiver = payjoin_uri.address.clone();
		let original_psbt =
			self.wallet.build_payjoin_transaction(amount, receiver.clone().into())?;
		let tx = original_psbt.clone().unsigned_tx;
		let payment_id = self.payment_id(&original_psbt.unsigned_tx.txid());
		self.payment_store.insert(PaymentDetails::new(
			payment_id,
			PaymentKind::Payjoin,
			Some(amount.to_sat()),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		))?;
		let mut update_payment = PaymentDetailsUpdate::new(payment_id);
		update_payment.txid = Some(tx.txid());
		let _ = self.payment_store.update(&update_payment);
		self.chain_source.register_tx(&tx.txid(), Script::empty());
		Ok(original_psbt)
	}

	pub(crate) async fn send_request(
		&self, payjoin_uri: payjoin::Uri<'_, NetworkChecked>, original_psbt: &mut Psbt,
	) -> Result<Option<Psbt>, Error> {
		let (request, context) = payjoin::send::RequestBuilder::from_psbt_and_uri(
			original_psbt.clone(),
			payjoin_uri.clone(),
		)
		.and_then(|b| b.build_non_incentivizing())
		.and_then(|mut c| c.extract_v2(self.payjoin_relay.clone()))
		.map_err(|e| {
			log_error!(self.logger, "Failed to create Payjoin request: {}", e);
			Error::PayjoinRequestCreationFailed
		})?;
		let mut headers = reqwest::header::HeaderMap::new();
		headers.insert(
			reqwest::header::CONTENT_TYPE,
			reqwest::header::HeaderValue::from_static("message/ohttp-req"),
		);
		let response = reqwest::Client::new()
			.post(request.url.clone())
			.body(request.body.clone())
			.timeout(PAYJOIN_REQUEST_TIMEOUT)
			.headers(headers)
			.send()
			.await
			.and_then(|r| r.error_for_status())
			.map_err(|e| {
				log_error!(self.logger, "Failed to send Payjoin request: {}", e);
				Error::PayjoinRequestSendingFailed
			})?;
		let response = response.bytes().await.map_err(|e| {
			log_error!(
				self.logger,
				"Failed to send Payjoin request, receiver invalid response: {}",
				e
			);
			Error::PayjoinRequestSendingFailed
		})?;
		let response = response.to_vec();
		context.process_response(&mut response.as_slice()).map_err(|e| {
			log_error!(self.logger, "Failed to process Payjoin response: {}", e);
			Error::PayjoinResponseProcessingFailed
		})
	}

	pub(crate) fn process_response(
		&self, payjoin_proposal: &mut Psbt, original_psbt: &mut Psbt,
	) -> Result<Transaction, Error> {
		let wallet = self.wallet.clone();
		wallet.sign_payjoin_proposal(payjoin_proposal, original_psbt)?;
		let proposal_tx = payjoin_proposal.clone().extract_tx();
		let payment_store = self.payment_store.clone();
		let payment_id = self.payment_id(&original_psbt.unsigned_tx.txid());
		let payment_details = payment_store.get(&payment_id);
		if let Some(payment_details) = payment_details {
			let txid = proposal_tx.txid();
			let mut payment_update = PaymentDetailsUpdate::new(payment_id);
			payment_update.txid = Some(txid);
			payment_store.update(&payment_update)?;
			self.chain_source.register_tx(&txid, Script::empty());
			self.event_queue.add_event(Event::PayjoinPaymentAwaitingConfirmation {
				txid,
				amount_sats: payment_details
					.amount_msat
					.ok_or(Error::PayjoinRequestMissingAmount)?,
			})?;
			Ok(proposal_tx)
		} else {
			log_error!(self.logger, "Failed to process Payjoin response: transaction not found");
			Err(Error::PayjoinResponseProcessingFailed)
		}
	}

	fn payment_id(&self, original_psbt_txid: &Txid) -> PaymentId {
		let payment_id: [u8; 32] =
			original_psbt_txid[..].try_into().expect("Unreachable, Txid is 32 bytes");
		PaymentId(payment_id)
	}

	pub(crate) fn handle_request_failure(
		&self, original_psbt: &Psbt, reason: PayjoinPaymentFailureReason,
	) -> Result<(), Error> {
		let payment_store = self.payment_store.clone();
		let payment_id = &self.payment_id(&original_psbt.unsigned_tx.txid());
		let payment_details = payment_store.get(payment_id);
		if let Some(payment_details) = payment_details {
			let mut update_details = PaymentDetailsUpdate::new(payment_id.clone());
			update_details.status = Some(PaymentStatus::Failed);
			let _ = payment_store.update(&update_details);
			self.event_queue.add_event(Event::PayjoinPaymentFailed {
				txid: original_psbt.unsigned_tx.txid(),
				amount_sats: payment_details
					.amount_msat
					.ok_or(Error::PayjoinRequestMissingAmount)?,
				reason,
			})
		} else {
			log_error!(
				self.logger,
				"Failed to handle request failure for Payjoin payment: transaction not found"
			);
			Err(Error::PayjoinRequestSendingFailed)
		}
	}
}
