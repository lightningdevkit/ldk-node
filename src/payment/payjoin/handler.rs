use crate::config::PAYJOIN_REQUEST_TIMEOUT;
use crate::error::Error;
use crate::event::PayjoinPaymentFailureReason;
use crate::logger::FilesystemLogger;
use crate::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use crate::types::{Broadcaster, ChainSource, EventQueue, PaymentStore, Wallet};
use crate::Event;

use bitcoin::address::NetworkChecked;
use bitcoin::block::Header;
use bitcoin::psbt::Psbt;
use bitcoin::{Address, Amount, BlockHash, Script, Transaction, Txid};

use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::transaction::TransactionData;
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::ln::channelmanager::PaymentId;
use lightning::log_error;
use lightning::util::logger::Logger;
use payjoin::send::{ContextV2, RequestBuilder};
use rand::RngCore;

use std::sync::{Arc, RwLock};

pub(crate) struct PayjoinHandler {
	tx_broadcaster: Arc<Broadcaster>,
	logger: Arc<FilesystemLogger>,
	payjoin_relay: payjoin::Url,
	chain_source: Arc<ChainSource>,
	transactions: RwLock<Vec<PayjoinTransaction>>,
	event_queue: Arc<EventQueue>,
	wallet: Arc<Wallet>,
	payment_store: Arc<PaymentStore>,
}

impl PayjoinHandler {
	pub(crate) fn new(
		tx_broadcaster: Arc<Broadcaster>, logger: Arc<FilesystemLogger>,
		payjoin_relay: payjoin::Url, chain_source: Arc<ChainSource>, event_queue: Arc<EventQueue>,
		wallet: Arc<Wallet>, payment_store: Arc<PaymentStore>,
	) -> Self {
		Self {
			tx_broadcaster,
			logger,
			payjoin_relay,
			transactions: RwLock::new(Vec::new()),
			chain_source,
			event_queue,
			wallet,
			payment_store,
		}
	}

	pub(crate) async fn send_payjoin_transaction(
		&self, original_psbt: &mut Psbt, payjoin_uri: payjoin::Uri<'_, NetworkChecked>,
	) -> Result<Option<Txid>, Error> {
		let (request, context) =
			RequestBuilder::from_psbt_and_uri(original_psbt.clone(), payjoin_uri.clone())
				.and_then(|b| b.build_non_incentivizing())
				.and_then(|mut c| c.extract_v2(self.payjoin_relay.clone()))
				.map_err(|_e| Error::PayjoinRequestCreationFailed)
				.unwrap();
		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		self.payment_store.insert(PaymentDetails::new(
			PaymentId(random_bytes),
			PaymentKind::Payjoin,
			payjoin_uri.amount.map(|a| a.to_sat()),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		))?;
		let response = send_payjoin_ohttp_request(&request).await?;
		self.handle_payjoin_transaction_response(response, context, original_psbt, payjoin_uri)
			.await
	}

	pub(crate) async fn handle_payjoin_transaction_response(
		&self, response: Vec<u8>, context: ContextV2, original_psbt: &mut Psbt,
		payjoin_uri: payjoin::Uri<'_, NetworkChecked>,
	) -> Result<Option<Txid>, Error> {
		let amount = match payjoin_uri.amount {
			Some(amt) => amt.to_sat(),
			None => return Err(Error::PayjoinRequestMissingAmount),
		};
		match context.process_response(&mut response.as_slice()) {
			Ok(Some(pj_proposal)) => {
				let pj_proposal = &mut pj_proposal.clone();
				let tx =
					self.finalise_tx(pj_proposal, &mut original_psbt.clone(), payjoin_uri.clone())?;
				self.tx_broadcaster.broadcast_transactions(&[&tx]);
				let txid = tx.txid();
				let _ = self.event_queue.add_event(Event::PayjoinPaymentPending {
					txid,
					amount,
					receipient: payjoin_uri.address.clone().into(),
				});
				Ok(Some(txid))
			},
			Ok(None) => Ok(None),
			Err(_e) => {
				let _ = self.event_queue.add_event(Event::PayjoinPaymentFailed {
					txid: None,
					amount,
					receipient: payjoin_uri.address.clone().into(),
					reason: PayjoinPaymentFailureReason::InvalidReceiverResponse,
				});
				return Err(Error::PayjoinResponseProcessingFailed);
			},
		}
	}

	fn finalise_tx(
		&self, payjoin_proposal: &mut Psbt, original_psbt: &mut Psbt,
		payjoin_uri: payjoin::Uri<NetworkChecked>,
	) -> Result<Transaction, Error> {
		let wallet = self.wallet.clone();
		wallet.sign_payjoin_proposal(payjoin_proposal, original_psbt)?;
		let tx = payjoin_proposal.clone().extract_tx();
		if let Some(our_output) =
			tx.output.iter().find(|output| wallet.is_mine(&output.script_pubkey).unwrap_or(false))
		{
			let mut transactions = self.transactions.write().unwrap();
			let pj_tx = PayjoinTransaction::new(
				tx.clone(),
				payjoin_uri.address,
				payjoin_uri.amount.unwrap_or_default(),
			);
			transactions.push(pj_tx);
			self.register_tx(&tx.txid(), &our_output.script_pubkey);
			Ok(tx)
		} else {
			Err(Error::PaymentSendingFailed)
		}
	}

	pub(crate) fn timeout_payjoin_transaction(
		&self, payjoin_uri: payjoin::Uri<'_, NetworkChecked>,
	) -> Result<(), Error> {
		let amount = match payjoin_uri.amount {
			Some(amt) => amt.to_sat(),
			None => return Err(Error::PayjoinRequestMissingAmount),
		};
		let _ = self.event_queue.add_event(Event::PayjoinPaymentFailed {
			txid: None,
			receipient: payjoin_uri.address.clone().into(),
			amount,
			reason: PayjoinPaymentFailureReason::Timeout,
		});
		Ok(())
	}

	fn internal_transactions_confirmed(
		&self, header: &Header, txdata: &TransactionData, height: u32,
	) {
		let (_, tx) = txdata[0];
		let confirmed_tx_txid = tx.txid();
		let mut transactions = self.transactions.write().unwrap();
		if let Some(position) =
			transactions.iter().position(|o| o.txid() == Some(confirmed_tx_txid))
		{
			let tx = transactions.remove(position);
			tx.to_pending_threshold_confirmations(height, header.block_hash());
		} else {
			log_error!(
				self.logger,
				"Notified about UNTRACKED confirmed payjoin transaction {}",
				confirmed_tx_txid
			);
		};
	}

	fn internal_best_block_updated(&self, height: u32) {
		let mut transactions = self.transactions.write().unwrap();
		transactions.retain(|tx| {
			if let (Some(first_conf), Some(txid)) = (tx.first_confirmation_height(), tx.txid()) {
				if height - first_conf >= ANTI_REORG_DELAY {
					let _ = self.event_queue.add_event(Event::PayjoinPaymentSuccess {
						txid,
						amount: tx.amount().to_sat(),
						receipient: tx.receiver().into(),
					});
					false
				} else {
					true
				}
			} else {
				true
			}
		});
	}

	fn internal_get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		let state_lock = self.transactions.read().unwrap();
		state_lock
			.iter()
			.filter_map(|o| match o {
				PayjoinTransaction::PendingThresholdConfirmations {
					tx,
					first_confirmation_height,
					first_confirmation_hash,
					..
				} => Some((
					tx.clone().txid(),
					first_confirmation_height.clone(),
					Some(first_confirmation_hash.clone()),
				)),
				_ => None,
			})
			.collect::<Vec<_>>()
	}
}

#[derive(Clone, Debug)]
enum PayjoinTransaction {
	// PendingReceiverResponse,
	PendingFirstConfirmation {
		tx: Transaction,
		receiver: Address,
		amount: Amount,
	},
	PendingThresholdConfirmations {
		tx: Transaction,
		receiver: Address,
		amount: Amount,
		first_confirmation_height: u32,
		first_confirmation_hash: BlockHash,
	},
}

impl PayjoinTransaction {
	fn new(tx: Transaction, receiver: Address, amount: Amount) -> Self {
		PayjoinTransaction::PendingFirstConfirmation { tx, receiver, amount }
	}
	fn txid(&self) -> Option<Txid> {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { tx, .. } => Some(tx.txid()),
			PayjoinTransaction::PendingThresholdConfirmations { tx, .. } => Some(tx.txid()),
		}
	}
	fn first_confirmation_height(&self) -> Option<u32> {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { .. } => None,
			PayjoinTransaction::PendingThresholdConfirmations {
				first_confirmation_height, ..
			} => Some(*first_confirmation_height),
		}
	}
	fn amount(&self) -> Amount {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { amount, .. } => *amount,
			PayjoinTransaction::PendingThresholdConfirmations { amount, .. } => *amount,
		}
	}
	fn receiver(&self) -> Address {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { receiver, .. } => receiver.clone(),
			PayjoinTransaction::PendingThresholdConfirmations { receiver, .. } => receiver.clone(),
		}
	}

	fn to_pending_threshold_confirmations(&self, height: u32, hash: BlockHash) -> Self {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { tx, receiver, amount } => {
				PayjoinTransaction::PendingThresholdConfirmations {
					tx: tx.clone(),
					receiver: receiver.clone(),
					amount: *amount,
					first_confirmation_height: height,
					first_confirmation_hash: hash,
				}
			},
			_ => unreachable!(),
		}
	}
}

impl Filter for PayjoinHandler {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.chain_source.register_tx(txid, script_pubkey);
	}

	fn register_output(&self, output: WatchedOutput) {
		self.chain_source.register_output(output);
	}
}

impl Confirm for PayjoinHandler {
	fn transactions_confirmed(&self, header: &Header, txdata: &TransactionData, height: u32) {
		self.internal_transactions_confirmed(header, txdata, height);
	}

	fn best_block_updated(&self, _header: &Header, height: u32) {
		self.internal_best_block_updated(height);
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		self.internal_get_relevant_txids()
	}

	fn transaction_unconfirmed(&self, _txid: &Txid) {}
}

async fn send_payjoin_ohttp_request(request: &payjoin::Request) -> Result<Vec<u8>, Error> {
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
		.await?;
	let response = response.error_for_status()?.bytes().await?;
	Ok(response.to_vec())
}
