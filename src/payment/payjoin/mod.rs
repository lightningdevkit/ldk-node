//! Holds a payment handler allowing to send Payjoin payments.

use crate::config::{PAYJOIN_REQUEST_TOTAL_DURATION, PAYJOIN_RETRY_INTERVAL};
use crate::logger::{log_info, log_error, FilesystemLogger, Logger};
use crate::types::Wallet;
use payjoin::PjUri;

use crate::payjoin_receiver::PayjoinReceiver;
use crate::{error::Error, Config};

use std::sync::{Arc, RwLock};

pub(crate) mod handler;

use handler::PayjoinHandler;

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
	handler: Option<Arc<PayjoinHandler>>,
	receiver: Option<Arc<PayjoinReceiver>>,
	config: Arc<Config>,
	logger: Arc<FilesystemLogger>,
	wallet: Arc<Wallet>,
}

impl PayjoinPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
		handler: Option<Arc<PayjoinHandler>>, receiver: Option<Arc<PayjoinReceiver>>,
		config: Arc<Config>, logger: Arc<FilesystemLogger>, wallet: Arc<Wallet>,
	) -> Self {
		Self { runtime, handler, receiver, config, logger, wallet }
	}

	/// Send a Payjoin transaction to the address specified in the `payjoin_uri`.
	///
	/// The `payjoin_uri` argument is expected to be a valid [`BIP21`] URI with Payjoin parameters
	/// set.
	///
	/// Due to the asynchronous nature of the Payjoin process, this method will return immediately
	/// after constucting the Payjoin request and sending it in the background. The result of the
	/// operation will be communicated through the event queue. If the Payjoin request is
	/// successful, [`Event::PayjoinPaymentSuccess`] event will be added to the event queue.
	/// Otherwise, [`Event::PayjoinPaymentFailed`] is added.
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
	/// [`Event::PayjoinPaymentSuccess`]: crate::Event::PayjoinPaymentSuccess
	/// [`Event::PayjoinPaymentFailed`]: crate::Event::PayjoinPaymentFailed
	pub fn send(&self, payjoin_uri: String) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let payjoin_uri =
			payjoin::Uri::try_from(payjoin_uri).map_err(|_| Error::PayjoinUriInvalid).and_then(
				|uri| uri.require_network(self.config.network).map_err(|_| Error::InvalidNetwork),
			)?;
		let mut original_psbt = self.wallet.build_payjoin_transaction(payjoin_uri.clone())?;
		let payjoin_handler = self.handler.as_ref().ok_or(Error::PayjoinUnavailable)?;
		let payjoin_handler = Arc::clone(payjoin_handler);
		let logger = Arc::clone(&self.logger);
		log_info!(logger, "Sending Payjoin request to: {}", payjoin_uri.address);
		rt_lock.as_ref().unwrap().spawn(async move {
			let mut interval = tokio::time::interval(PAYJOIN_RETRY_INTERVAL);
			loop {
				tokio::select! {
					_ = tokio::time::sleep(PAYJOIN_REQUEST_TOTAL_DURATION) => {
						log_error!(logger, "Payjoin request timed out.");
						let _ = payjoin_handler.timeout_payjoin_transaction(payjoin_uri.clone());
						break;
					}
					_ = interval.tick() => {
						match payjoin_handler.send_payjoin_transaction(&mut original_psbt, payjoin_uri.clone()).await {
							Ok(Some(_)) => {
								log_info!(logger, "Payjoin transaction sent successfully.");
								break
							},
							Ok(None) => {
								log_info!(logger, "No Payjoin response yet.");
								continue
							},
							Err(e) => {
								log_error!(logger, "Failed to process Payjoin receiver response: {}.", e);
								break;
							}
						}
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
	/// successful, [`Event::PayjoinPaymentSuccess`] event will be added to the event queue.
	/// Otherwise, [`Event::PayjoinPaymentFailed`] is added.
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
	/// [`Event::PayjoinPaymentSuccess`]: crate::Event::PayjoinPaymentSuccess
	/// [`Event::PayjoinPaymentFailed`]: crate::Event::PayjoinPaymentFailed
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

	/// Receive onchain Payjoin transaction.
	///
	/// This method will enroll with the configured Payjoin directory if not already,
	/// and returns a [BIP21] URI pointing to our enrolled subdirectory that you can share with
	/// Payjoin sender.
	///
	/// [BIP21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	pub async fn receive(&self, amount: bitcoin::Amount) -> Result<PjUri, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		if let Some(receiver) = &self.receiver {
			receiver.receive(amount).await
		} else {
			Err(Error::PayjoinReceiverUnavailable)
		}
	}
}
