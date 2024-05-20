//! Holds a payment handler allowing to send Payjoin payments.

use lightning::chain::chaininterface::BroadcasterInterface;

use crate::config::{PAYJOIN_REQUEST_TOTAL_DURATION, PAYJOIN_RETRY_INTERVAL};
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::types::{Broadcaster, EventQueue, Wallet};
use crate::Event;
use crate::{error::Error, Config};

use std::sync::{Arc, RwLock};

use self::send::PayjoinSender;

pub(crate) mod send;

/// A payment handler allowing to send Payjoin payments.
///
/// Payjoin transactions can be used to improve privacy by breaking the common-input-ownership
/// heuristic when Payjoin receivers contribute input(s) to the transaction. They can also be used to
/// save on fees, as the Payjoin receiver can direct the incoming funds to open a lightning
/// channel, forwards the funds to another address, or simply consolidate UTXOs.
///
/// Payjoin [`BIP77`] implementation. Compatible also with previous Payjoin version [`BIP78`].
///
/// Should be retrieved by calling [`Node::payjoin_payment`].
///
/// In a Payjoin, both the sender and receiver contribute inputs to the transaction in a
/// coordinated manner. The Payjoin mechanism is also called pay-to-endpoint(P2EP).
///
/// The Payjoin receiver endpoint address is communicated through a [`BIP21`] URI, along with the
/// payment address and amount.  In the Payjoin process, parties edit, sign and pass iterations of
/// the transaction between each other, before a final version is broadcasted by the Payjoin
/// sender. [`BIP77`] codifies a protocol with 2 iterations (or one round of interaction beyond
/// address sharing).
///
/// [`BIP77`] Defines the Payjoin process to happen asynchronously, with the Payjoin receiver
/// enrolling with a Payjoin Directory to receive Payjoin requests. The Payjoin sender can then
/// make requests through a proxy server, Payjoin Relay, to the Payjoin receiver even if the
/// receiver is offline. This mechanism requires the Payjoin sender to regulary check for responses
/// from the Payjoin receiver as implemented in [`Node::payjoin_payment::send`].
///
/// A Payjoin Relay is a proxy server that forwards Payjoin requests from the Payjoin sender to the
///	Payjoin receiver subdirectory. A Payjoin Relay can be run by anyone. Public Payjoin Relay servers are:
///	- <https://pj.bobspacebkk.com>
///
/// A Payjoin directory is a service that allows Payjoin receivers to receive Payjoin requests
/// offline. A Payjoin directory can be run by anyone. Public Payjoin Directory servers are:
/// - <https://payjo.in>
///
/// For futher information on Payjoin, please refer to the BIPs included in this documentation. Or
/// visit the [Payjoin website](https://payjoin.org).
///
/// [`Node::payjoin_payment`]: crate::Node::payjoin_payment
/// [`Node::payjoin_payment::send`]: crate::payment::PayjoinPayment::send
/// [`BIP21`]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
/// [`BIP78`]: https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki
/// [`BIP77`]: https://github.com/bitcoin/bips/blob/3b863a402e0250658985f08a455a6cd103e269e5/bip-0077.mediawiki
pub struct PayjoinPayment {
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
	sender: Option<Arc<PayjoinSender>>,
	config: Arc<Config>,
	event_queue: Arc<EventQueue>,
	logger: Arc<FilesystemLogger>,
	wallet: Arc<Wallet>,
	tx_broadcaster: Arc<Broadcaster>,
}

impl PayjoinPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>, sender: Option<Arc<PayjoinSender>>,
		config: Arc<Config>, event_queue: Arc<EventQueue>, logger: Arc<FilesystemLogger>,
		wallet: Arc<Wallet>, tx_broadcaster: Arc<Broadcaster>,
	) -> Self {
		Self { runtime, sender, config, event_queue, logger, wallet, tx_broadcaster }
	}

	/// Send a Payjoin transaction to the address specified in the `payjoin_uri`.
	///
	/// The `payjoin_uri` argument is expected to be a valid [`BIP21`] URI with Payjoin parameters
	/// set.
	///
	/// Due to the asynchronous nature of the Payjoin process, this method will return immediately
	/// after constucting the Payjoin request and sending it in the background. The result of the
	/// operation will be communicated through the event queue. If the Payjoin request is
	/// successful, [`Event::PayjoinTxSendSuccess`] event will be added to the event queue.
	/// Otherwise, [`Event::PayjoinTxSendFailed`] is added.
	///
	/// The total duration of the Payjoin process is defined in `PAYJOIN_REQUEST_TOTAL_DURATION`.
	/// If the Payjoin receiver does not respond within this duration, the process is considered
	/// failed. Note, the Payjoin receiver can still broadcast the original PSBT shared with them as
	/// part of our request in a regular transaction if we timed out, or for any other reason. The
	/// Payjoin sender should monitor the blockchain for such transactions and handle them
	/// accordingly.
	///
	/// [`BIP21`]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	/// [`BIP77`]: https://github.com/bitcoin/bips/blob/d7ffad81e605e958dcf7c2ae1f4c797a8631f146/bip-0077.mediawiki
	/// [`Event::PayjoinTxSendSuccess`]: crate::Event::PayjoinTxSendSuccess
	/// [`Event::PayjoinTxSendFailed`]: crate::Event::PayjoinTxSendFailed
	pub fn send(&self, payjoin_uri: String) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let payjoin_sender = self.sender.as_ref().ok_or(Error::PayjoinUnavailable)?;
		let payjoin_uri =
			payjoin::Uri::try_from(payjoin_uri).map_err(|_| Error::PayjoinUriInvalid)?;
		let payjoin_uri =
			payjoin_uri.require_network(self.config.network).map_err(|_| Error::InvalidNetwork)?;
		let amount_to_send = match payjoin_uri.amount {
			Some(amount) => amount,
			None => return Err(Error::PayjoinRequestMissingAmount),
		};
		let original_psbt = self.wallet.build_payjoin_transaction(
			payjoin_uri.address.script_pubkey(),
			amount_to_send.to_sat(),
		)?;
		let payjoin_sender = Arc::clone(payjoin_sender);
		let runtime = rt_lock.as_ref().unwrap();
		let event_queue = Arc::clone(&self.event_queue);
		let tx_broadcaster = Arc::clone(&self.tx_broadcaster);
		let wallet = Arc::clone(&self.wallet);
		let logger = Arc::clone(&self.logger);
		let payjoin_relay = payjoin_sender.payjoin_relay().clone();
		runtime.spawn(async move {
			let mut interval = tokio::time::interval(PAYJOIN_RETRY_INTERVAL);
			loop {
				tokio::select! {
					_ = tokio::time::sleep(PAYJOIN_REQUEST_TOTAL_DURATION) => {
						let _ = event_queue.add_event(Event::PayjoinTxSendFailed {
							reason: "Payjoin request timed out.".to_string(),
						});
						break;
					}
					_ = interval.tick() => {
						let payjoin_uri = payjoin_uri.clone();

						let (request, context) =
							payjoin::send::RequestBuilder::from_psbt_and_uri(original_psbt.clone(), payjoin_uri)
							.and_then(|b| b.build_non_incentivizing())
							.and_then(|mut c| c.extract_v2(payjoin_relay.clone()))
							.map_err(|_e| Error::PayjoinRequestCreationFailed).unwrap();
						match payjoin_sender.send_request(&request).await {
							Some(response) => {
								match context.process_response(&mut response.as_slice()) {
											Ok(Some(payjoin_proposal_psbt)) => {
													let payjoin_proposal_psbt = &mut payjoin_proposal_psbt.clone();
													let is_signed = wallet.sign_payjoin_proposal(payjoin_proposal_psbt, &mut original_psbt.clone()).unwrap();
													if is_signed {
														let tx = payjoin_proposal_psbt.clone().extract_tx();
														tx_broadcaster.broadcast_transactions(&[&tx]);
														let txid = tx.txid();
														let _ = event_queue.add_event(Event::PayjoinPaymentPending { txid });
													} else {
														let _ = event_queue
															.add_event(Event::PayjoinTxSendFailed { reason: "Unable to sign proposal".to_string(), });
															break;
													}
											},
											Err(e) => {
													let _ = event_queue
														.add_event(Event::PayjoinTxSendFailed { reason: e.to_string() });
													log_error!(logger, "Error processing Payjoin response: {}", e);
													break;
											},
											Ok(None) => {
													log_info!(logger, "Payjoin response received, waiting for next response.");
													continue;
											}
								}
							},
						None => {
							continue;
						},
						};
					}
				}
			}
		});
		return Ok(());
	}

	/// Send a Payjoin transaction to the address specified in the `payjoin_uri`.
	///
	/// The `payjoin_uri` argument is expected to be a valid [`BIP21`] URI with Payjoin parameters
	/// set.
	///
	/// This method will ignore the amount specified in the `payjoin_uri` and use the `amount_sats`
	/// instead. The `amount_sats` argument is expected to be in satoshis.
	///
	/// Due to the asynchronous nature of the Payjoin process, this method will return immediately
	/// after constucting the Payjoin request and sending it in the background. The result of the
	/// operation will be communicated through the event queue. If the Payjoin request is
	/// successful, [`Event::PayjoinTxSendSuccess`] event will be added to the event queue.
	/// Otherwise, [`Event::PayjoinTxSendFailed`] is added.
	///
	/// The total duration of the Payjoin process is defined in `PAYJOIN_REQUEST_TOTAL_DURATION`.
	/// If the Payjoin receiver does not respond within this duration, the process is considered
	/// failed. Note, the Payjoin receiver can still broadcast the original PSBT shared with them as
	/// part of our request in a regular transaction if we timed out, or for any other reason. The
	/// Payjoin sender should monitor the blockchain for such transactions and handle them
	/// accordingly.
	///
	/// [`BIP21`]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	/// [`BIP77`]: https://github.com/bitcoin/bips/blob/d7ffad81e605e958dcf7c2ae1f4c797a8631f146/bip-0077.mediawiki
	/// [`Event::PayjoinTxSendSuccess`]: crate::Event::PayjoinTxSendSuccess
	/// [`Event::PayjoinTxSendFailed`]: crate::Event::PayjoinTxSendFailed
	pub fn send_with_amount(&self, payjoin_uri: String, amount_sats: u64) -> Result<(), Error> {
		let payjoin_uri = match payjoin::Uri::try_from(payjoin_uri) {
			Ok(uri) => uri,
			Err(_) => return Err(Error::PayjoinUriInvalid),
		};
		let mut payjoin_uri = match payjoin_uri.require_network(self.config.network) {
			Ok(uri) => uri,
			Err(_) => return Err(Error::InvalidNetwork),
		};
		payjoin_uri.amount = Some(bitcoin::Amount::from_sat(amount_sats));
		self.send(payjoin_uri.to_string())
	}
}
