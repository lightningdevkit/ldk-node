use bitcoin::block::Header;
use bitcoin::psbt::Psbt;
use bitcoin::{BlockHash, Transaction, Txid};
use lightning::chain::transaction::TransactionData;

use crate::chain::ChainSource;
use crate::config::PAYJOIN_REQUEST_TIMEOUT;
use crate::error::Error;
use crate::event::{EventQueue, PayjoinPaymentFailureReason};
use crate::logger::FilesystemLogger;
use crate::payment::store::{PaymentDetailsUpdate, PaymentStore};
use crate::payment::PaymentKind;
use crate::payment::{PaymentDirection, PaymentStatus};
use crate::types::Wallet;
use crate::Event;
use crate::PaymentDetails;

// use lightning::chain::{BestBlock, Confirm, Filter};
use lightning::chain::Confirm;
use lightning::ln::channelmanager::PaymentId;
use lightning::log_error;
use lightning::util::logger::Logger;

use std::sync::Arc;

pub(crate) struct PayjoinHandler {
        #[allow(dead_code)]
	chain_source: Option<Arc<ChainSource>>,
	event_queue: Arc<EventQueue<Arc<FilesystemLogger>>>,
	logger: Arc<FilesystemLogger>,
	payjoin_relay: payjoin::Url,
	payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>,
	wallet: Arc<Wallet>,
}

impl PayjoinHandler {
	pub(crate) fn new(
		event_queue: Arc<EventQueue<Arc<FilesystemLogger>>>,
		logger: Arc<FilesystemLogger>, payjoin_relay: payjoin::Url,
                payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>,
                wallet: Arc<Wallet>,
	) -> Self {
		Self { chain_source: None,
                event_queue,
                logger, payjoin_relay,
                payment_store,
                wallet }
	}

	pub(crate) fn start_request(
		&self, payjoin_uri: payjoin::PjUri<'_>,
	) -> Result<Psbt, Error> {
		let amount = payjoin_uri.amount.ok_or(Error::PayjoinRequestMissingAmount)?;
		let receiver = payjoin_uri.address.clone();
		let original_psbt =
			self.wallet.build_payjoin_transaction(amount, receiver.clone().into())?;
		let tx = original_psbt.clone().unsigned_tx;
		let payment_id = self.payment_id(&tx.compute_txid());
		self.payment_store.insert(PaymentDetails::new(
			payment_id,
			PaymentKind::Payjoin,
			Some(amount.to_sat()),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		))?;
		// self.chain_source.register_tx(&tx.txid(), Script::empty());
		Ok(original_psbt)
	}

	pub(crate) async fn send_request(
		&self, payjoin_uri: payjoin::PjUri<'_>, original_psbt: &mut Psbt,
	) -> Result<Option<Psbt>, Error> {
                  let (request, context) =  payjoin::send::SenderBuilder::from_psbt_and_uri(
			original_psbt.clone(),
			payjoin_uri.clone(),
		)
		.and_then(|b| b.build_non_incentivizing(bitcoin::FeeRate::MIN))
		.and_then(|c| c.extract_v2(self.payjoin_relay.clone()))
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
		let _ret = context.process_response(&mut response.as_slice()).map_err(|e| {
			log_error!(self.logger, "Failed to process Payjoin response: {}", e);
			Error::PayjoinResponseProcessingFailed
		}).unwrap();
                Ok(None)
                // ret.process_response
	}

	pub(crate) fn process_response(
		&self, payjoin_proposal: &mut Psbt, original_psbt: &mut Psbt,
	) -> Result<Transaction, Error> {
		let wallet = self.wallet.clone();
		wallet.sign_payjoin_proposal(payjoin_proposal, original_psbt)?;
		let proposal_tx = payjoin_proposal.clone().extract_tx();
		let payment_store = self.payment_store.clone();
		let payment_id = self.payment_id(&original_psbt.unsigned_tx.compute_txid());
		let payment_details = payment_store.get(&payment_id);
		// let payment_details: Option<PaymentDetails> = None;
		if let Some(payment_details) = payment_details {
                        let proposal_tx = proposal_tx.unwrap();
                        let txid = proposal_tx.clone().compute_txid();
			// let mut payment_update = PaymentDetailsUpdate::new(payment_id);
			// payment_update.txid = Some(txid);
			// payment_store.update(&payment_update)?;
			// self.chain_source.register_tx(&txid, Script::empty());
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
		let payment_id = &self.payment_id(&original_psbt.unsigned_tx.compute_txid());
		let payment_details = payment_store.get(payment_id);
		if let Some(payment_details) = payment_details {
			let mut update_details = PaymentDetailsUpdate::new(payment_id.clone());
			update_details.status = Some(PaymentStatus::Failed);
			let _ = payment_store.update(&update_details);
			self.event_queue.add_event(Event::PayjoinPaymentFailed {
				txid: original_psbt.unsigned_tx.compute_txid(),
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

	fn internal_transactions_confirmed(
		&self, _header: &Header, _txdata: &TransactionData, _height: u32,
	) {
		// for (_, tx) in txdata {
		// 	let confirmed_tx_txid = tx.compute_txid();
			// let payment_store = self.payment_store.clone();
			// let payment_id = self.payment_id(&confirmed_tx_txid);
			// // let payjoin_tx_filter = |payment_details: &&PaymentDetails| {
			// 	payment_details.txid == Some(confirmed_tx_txid)
			// 		&& payment_details.amount_msat.is_some()
			// };
			// let payjoin_tx_details = payment_store.list_filter(payjoin_tx_filter);
			// if let Some(payjoin_tx_details) = payjoin_tx_details.get(0) {
			// 	let mut payment_update = PaymentDetailsUpdate::new(payjoin_tx_details.id);
			// 	payment_update.status = Some(PaymentStatus::Succeeded);
			// 	payment_update.best_block = Some(BestBlock::new(header.block_hash(), height));
			// 	let _ = payment_store.update(&payment_update);
				// let _ = self.event_queue.add_event(Event::PayjoinPaymentSuccessful {
				// 	txid: confirmed_tx_txid,
				// 	amount_sats: payjoin_tx_details
				// 		.amount_msat
				// 		.expect("Unreachable, asserted in `payjoin_tx_filter`"),
				// 	is_original_psbt_modified: if payment_id == payjoin_tx_details.id {
				// 		false
				// 	} else {
				// 		true
				// 	},
				// });
			// check if this is the original psbt transaction
			// } else if let Some(payment_details) = payment_store.get(&payment_id) {
			// 	let mut payment_update = PaymentDetailsUpdate::new(payment_id);
			// 	payment_update.status = Some(PaymentStatus::Succeeded);
			// 	let _ = payment_store.update(&payment_update);
			// 	payment_update.best_block = Some(BestBlock::new(header.block_hash(), height));
			// 	payment_update.txid = Some(confirmed_tx_txid);
				// let _ = self.event_queue.add_event(Event::PayjoinPaymentSuccessful {
				// 	txid: confirmed_tx_txid,
				// 	amount_sats: payment_details
				// 		.amount_msat
				// 		.expect("Unreachable, payjoin transactions must have amount"),
				// 	is_original_psbt_modified: false,
				// });
			// }
		// }
	}

	fn internal_get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
                let payjoin_tx_filter = |payment_details: &&PaymentDetails| {
                	// payment_details.txid.is_some()
                		// &&
                                    payment_details.status == PaymentStatus::Succeeded
                		&& payment_details.kind == PaymentKind::Payjoin
                };
                let _payjoin_tx_details = self.payment_store.list_filter(payjoin_tx_filter);
                let ret = Vec::new();
                // for payjoin_tx_details in payjoin_tx_details {
                // 	if let (Some(txid), Some(best_block)) =
                // 		(payjoin_tx_details.txid, payjoin_tx_details.best_block)
                // 	{
                // 		ret.push((txid, best_block.height, Some(best_block.block_hash)));
                // 	}
                // }
                ret
	}

	fn internal_best_block_updated(&self, _height: u32, _block_hash: BlockHash) {
		let payment_store = self.payment_store.clone();
		let payjoin_tx_filter = |payment_details: &&PaymentDetails| {
			payment_details.kind == PaymentKind::Payjoin
				&& payment_details.status == PaymentStatus::Succeeded
		};
		let _payjoin_tx_details = payment_store.list_filter(payjoin_tx_filter);
		// for payjoin_tx_details in payjoin_tx_details {
		// 	let mut payment_update = PaymentDetailsUpdate::new(payjoin_tx_details.id);
		// 	payment_update.best_block = Some(BestBlock::new(block_hash, height));
		// 	let _ = payment_store.update(&payment_update);
		// }
	}
}

impl Confirm for PayjoinHandler {
	fn transactions_confirmed(&self, header: &Header, txdata: &TransactionData, height: u32) {
		self.internal_transactions_confirmed(header, txdata, height);
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		self.internal_get_relevant_txids()
	}

	fn best_block_updated(&self, header: &Header, height: u32) {
		let block_hash = header.block_hash();
		self.internal_best_block_updated(height, block_hash);
	}

	fn transaction_unconfirmed(&self, _txid: &Txid) {}
}
