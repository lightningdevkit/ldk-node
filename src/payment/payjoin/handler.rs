use lightning::ln::channelmanager::PaymentId;

use crate::config::PAYJOIN_REQUEST_TIMEOUT;
use crate::error::Error;
use crate::event::PayjoinPaymentFailureReason;
use crate::io::utils::ohttp_headers;
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::PaymentStatus;
use crate::types::{ChainSource, EventQueue, PaymentStore, Wallet};
use crate::Event;

use bitcoin::address::NetworkChecked;
use bitcoin::block::Header;
use bitcoin::psbt::Psbt;
use bitcoin::{Address, Amount, BlockHash, Script, Transaction, Txid};
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::transaction::TransactionData;
use lightning::chain::{Filter, WatchedOutput};

use std::sync::{Arc, RwLock};

#[derive(Clone, Debug)]
enum PayjoinTransaction {
	PendingFirstConfirmation {
		original_psbt: Psbt,
		tx: Transaction,
		receiver: Address,
		amount: Amount,
	},
	PendingThresholdConfirmations {
		original_psbt: Psbt,
		tx: Transaction,
		receiver: Address,
		amount: Amount,
		first_confirmation_height: u32,
		first_confirmation_hash: BlockHash,
	},
}

impl PayjoinTransaction {
	fn txid(&self) -> Option<Txid> {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { tx, .. } => Some(tx.txid()),
			PayjoinTransaction::PendingThresholdConfirmations { tx, .. } => Some(tx.txid()),
		}
	}
	fn original_psbt(&self) -> &Psbt {
		match self {
			PayjoinTransaction::PendingFirstConfirmation { original_psbt, .. } => original_psbt,
			PayjoinTransaction::PendingThresholdConfirmations { original_psbt, .. } => {
				original_psbt
			},
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
}

pub(crate) struct PayjoinHandler {
	payjoin_relay: payjoin::Url,
	chain_source: Arc<ChainSource>,
	transactions: RwLock<Vec<PayjoinTransaction>>,
	event_queue: Arc<EventQueue>,
	wallet: Arc<Wallet>,
	payment_store: Arc<PaymentStore>,
}

impl PayjoinHandler {
	pub(crate) fn new(
		payjoin_relay: payjoin::Url, chain_source: Arc<ChainSource>, event_queue: Arc<EventQueue>,
		wallet: Arc<Wallet>, payment_store: Arc<PaymentStore>,
	) -> Self {
		Self {
			payjoin_relay,
			transactions: RwLock::new(Vec::new()),
			chain_source,
			event_queue,
			wallet,
			payment_store,
		}
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
		.map_err(|_e| Error::PayjoinRequestCreationFailed)?;
		let response = reqwest::Client::new()
			.post(request.url.clone())
			.body(request.body.clone())
			.timeout(PAYJOIN_REQUEST_TIMEOUT)
			.headers(ohttp_headers())
			.send()
			.await?;
		let response = response.error_for_status()?;
		let response = response.bytes().await?;
		let response = response.to_vec();
		context
			.process_response(&mut response.as_slice())
			.map_err(|_e| Error::PayjoinResponseProcessingFailed)
	}

	pub(crate) fn handle_request_failure(
		&self, payjoin_uri: payjoin::Uri<NetworkChecked>, original_psbt: &Psbt,
	) -> Result<(), Error> {
		self.event_queue.add_event(Event::PayjoinPaymentFailed {
			txid: Some(original_psbt.unsigned_tx.txid()),
			receipient: payjoin_uri.address.clone().into(),
			amount: payjoin_uri.amount.unwrap().to_sat(),
			reason: PayjoinPaymentFailureReason::RequestFailed,
		})
	}

	pub(crate) fn handle_request_timeout(
		&self, payjoin_uri: payjoin::Uri<NetworkChecked>, original_psbt: &Psbt,
	) -> Result<(), Error> {
		self.event_queue.add_event(Event::PayjoinPaymentFailed {
			txid: Some(original_psbt.unsigned_tx.txid()),
			receipient: payjoin_uri.address.clone().into(),
			amount: payjoin_uri.amount.unwrap().to_sat(),
			reason: PayjoinPaymentFailureReason::Timeout,
		})
	}

	pub(crate) fn process_response(
		&self, payjoin_proposal: &mut Psbt, original_psbt: &mut Psbt,
		payjoin_uri: payjoin::Uri<NetworkChecked>,
	) -> Result<Transaction, Error> {
		let wallet = self.wallet.clone();
		wallet.sign_payjoin_proposal(payjoin_proposal, original_psbt)?;
		let tx = payjoin_proposal.clone().extract_tx();
		let our_input =
			tx.output.iter().find(|output| wallet.is_mine(&output.script_pubkey).unwrap_or(false));
		if let Some(our_input) = our_input {
			self.transactions.write().unwrap().push(PayjoinTransaction::PendingFirstConfirmation {
				original_psbt: original_psbt.clone(),
				tx: tx.clone(),
				receiver: payjoin_uri.address.clone(),
				amount: payjoin_uri.amount.unwrap_or_default(),
			});
			let txid = tx.txid();
			self.register_tx(&txid, &our_input.script_pubkey);
			self.event_queue.add_event(Event::PayjoinPaymentPending {
				txid,
				amount: payjoin_uri.amount.unwrap_or_default().to_sat(),
				receipient: payjoin_uri.address.clone().into(),
			})?;
			Ok(tx)
		} else {
			self.event_queue.add_event(Event::PayjoinPaymentFailed {
				txid: None,
				amount: payjoin_uri.amount.unwrap_or_default().to_sat(),
				receipient: payjoin_uri.address.clone().into(),
				reason: PayjoinPaymentFailureReason::TransactionFinalisationFailed,
			})?;
			Err(Error::PayjoinReceiverRequestValidationFailed) // fixeror
		}
	}

	fn internal_transactions_confirmed(
		&self, header: &Header, txdata: &TransactionData, height: u32,
	) {
		let (_, tx) = txdata[0];
		let confirmed_tx_txid = tx.txid();
		let mut transactions = self.transactions.write().unwrap();
		let position = match transactions.iter().position(|o| o.txid() == Some(confirmed_tx_txid)) {
			Some(position) => position,
			None => {
				return;
			},
		};
		let pj_tx = transactions.remove(position);
		match pj_tx {
			PayjoinTransaction::PendingFirstConfirmation {
				ref tx,
				receiver,
				amount,
				original_psbt,
			} => {
				transactions.push(PayjoinTransaction::PendingThresholdConfirmations {
					original_psbt,
					tx: tx.clone(),
					receiver,
					amount,
					first_confirmation_height: height,
					first_confirmation_hash: header.block_hash(),
				});
			},
			_ => {
				unreachable!()
			},
		};
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

	fn internal_best_block_updated(&self, height: u32) {
		let mut transactions = self.transactions.write().unwrap();
		transactions.retain(|tx| {
			if let (Some(first_conf), Some(txid)) = (tx.first_confirmation_height(), tx.txid()) {
				if height - first_conf >= ANTI_REORG_DELAY {
					let payment_id: [u8; 32] =
						tx.original_psbt().unsigned_tx.txid()[..].try_into().unwrap();
					let mut update_details = PaymentDetailsUpdate::new(PaymentId(payment_id));
					update_details.status = Some(PaymentStatus::Succeeded);
					let _ = self.payment_store.update(&update_details);
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
}

impl Filter for PayjoinHandler {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.chain_source.register_tx(txid, script_pubkey);
	}

	fn register_output(&self, output: WatchedOutput) {
		self.chain_source.register_output(output);
	}
}

impl lightning::chain::Confirm for PayjoinHandler {
	fn transactions_confirmed(&self, header: &Header, txdata: &TransactionData, height: u32) {
		self.internal_transactions_confirmed(header, txdata, height);
	}

	fn transaction_unconfirmed(&self, _txid: &Txid) {}

	fn best_block_updated(&self, _header: &Header, height: u32) {
		self.internal_best_block_updated(height);
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		self.internal_get_relevant_txids()
	}
}
