use crate::config::{PAYJOIN_REQUEST_TOTAL_DURATION, PAYJOIN_RETRY_INTERVAL};
use crate::event::PayjoinPaymentFailureReason;
use crate::logger::{FilesystemLogger, Logger};
use crate::types::Broadcaster;
use crate::{error::Error, Config};

use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::{log_error, log_info};

use std::sync::{Arc, RwLock};

pub(crate) mod handler;
use handler::PayjoinHandler;

/// Payjoin [`BIP77`] implementation. Compatible with previous Payjoin version [`BIP78`].
///
/// Should be retrieved by calling [`Node::payjoin_payment`].
///
/// Payjoin transactions can be used to improve privacy by breaking the common-input-ownership
/// heuristic when Payjoin receivers contribute input(s) to the transaction. They can also be used
/// to save on fees, as the Payjoin receiver can direct the incoming funds to open a lightning
/// channel, forwards the funds to another address, or simply consolidate UTXOs.
///
/// In a Payjoin transaction, both the sender and receiver contribute inputs to the transaction in
/// a coordinated manner. The Payjoin mechanism is also called pay-to-endpoint(P2EP).  The Payjoin
/// receiver endpoint address is communicated through a [`BIP21`] URI, along with the payment
/// address and an optional amount parameter.  In the Payjoin process, parties edit, sign and pass
/// iterations of the transaction between each other, before a final version is broadcasted by the
/// Payjoin sender.  [`BIP77`] codifies a protocol with 2 iterations (or one round of interaction
/// beyond address sharing).
///
/// [`BIP77`] Defines the Payjoin process to happen asynchronously, with the Payjoin receiver
/// enrolling with a Payjoin Directory to receive Payjoin requests. The Payjoin sender can then
/// make requests through a proxy server, Payjoin Relay, to the Payjoin receiver even if the
/// receiver is offline. This mechanism requires the Payjoin sender to regularly check for response
/// from the Payjoin receiver as implemented in [`Node::payjoin_payment::send`].
///
/// A Payjoin Relay is a proxy server that forwards Payjoin requests from the Payjoin sender to the
/// Payjoin receiver subdirectory. A Payjoin Relay can be run by anyone. Public Payjoin Relay
/// servers are:
/// - <https://pj.bobspacebkk.com>
///
/// A Payjoin directory is a service that allows Payjoin receivers to receive Payjoin requests
/// offline. A Payjoin directory can be run by anyone. Public Payjoin Directory servers are:
/// - <https://payjo.in>
///
/// For further information on Payjoin, please visit the [Payjoin website](https://payjoin.org).
///
/// [`Node::payjoin_payment`]: crate::Node::payjoin_payment
/// [`Node::payjoin_payment::send`]: crate::payment::PayjoinPayment::send
/// [`BIP21`]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
/// [`BIP78`]: https://github.com/bitcoin/bips/blob/master/bip-0078.mediawiki
/// [`BIP77`]: https://github.com/bitcoin/bips/blob/3b863a402e0250658985f08a455a6cd103e269e5/bip-0077.mediawiki
pub struct PayjoinPayment {
	config: Arc<Config>,
	logger: Arc<FilesystemLogger>,
	payjoin_handler: Option<Arc<PayjoinHandler>>,
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	tx_broadcaster: Arc<Broadcaster>,
}

impl PayjoinPayment {
	pub(crate) fn new(
		config: Arc<Config>, logger: Arc<FilesystemLogger>,
		payjoin_handler: Option<Arc<PayjoinHandler>>,
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
		tx_broadcaster: Arc<Broadcaster>,
	) -> Self {
		Self { config, logger, payjoin_handler, runtime, tx_broadcaster }
	}

	/// Send a Payjoin transaction to the address specified in the `payjoin_uri`.
	///
	/// The `payjoin_uri` argument is expected to be a valid [`BIP21`] URI with Payjoin parameters.
	///
	/// Due to the asynchronous nature of the Payjoin process, this method will return immediately
	/// after constructing the Payjoin request. The sending part is performed in the background.
	/// The result of the operation will be communicated through the event queue. If the Payjoin
	/// request is successful, [`Event::PayjoinPaymentSuccessful`] event will be emitted.
	/// Otherwise, [`Event::PayjoinPaymentFailed`] is emitted.
	///
	/// [`BIP21`]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	/// [`Event::PayjoinPaymentSuccessful`]: crate::Event::PayjoinPaymentSuccessful
	/// [`Event::PayjoinPaymentFailed`]: crate::Event::PayjoinPaymentFailed
	pub fn send(&self, payjoin_uri: String) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let payjoin_handler = self.payjoin_handler.as_ref().ok_or(Error::PayjoinUnavailable)?;
		let payjoin_uri =
			payjoin::Uri::try_from(payjoin_uri).map_err(|_| Error::PayjoinUriInvalid).and_then(
				|uri| uri.require_network(self.config.network).map_err(|_| Error::InvalidNetwork),
			)?;
		let original_psbt = payjoin_handler.start_request(payjoin_uri.clone())?;
		let payjoin_handler = Arc::clone(payjoin_handler);
		let runtime = rt_lock.as_ref().unwrap();
		let tx_broadcaster = Arc::clone(&self.tx_broadcaster);
		let logger = Arc::clone(&self.logger);
		runtime.spawn(async move {
			let mut interval = tokio::time::interval(PAYJOIN_RETRY_INTERVAL);
			loop {
				tokio::select! {
					_ = tokio::time::sleep(PAYJOIN_REQUEST_TOTAL_DURATION) => {
						let _ = payjoin_handler.handle_request_failure(&original_psbt, PayjoinPaymentFailureReason::Timeout);
						break;
					}
					_ = interval.tick() => {
						let payjoin_uri = payjoin_uri.clone();
						match payjoin_handler.send_request(payjoin_uri.clone(), &mut original_psbt.clone()).await {
							Ok(Some(mut proposal)) => {
								match payjoin_handler.process_response(&mut proposal, &mut original_psbt.clone()) {
									Ok(tx) => {
										tx_broadcaster.broadcast_transactions(&[&tx]);
									},
									Err(e) => {
										log_error!(logger, "Failed to process Payjoin response: {}", e);
										let _ = payjoin_handler.handle_request_failure(&original_psbt, PayjoinPaymentFailureReason::ResponseProcessingFailed);
									},
								};
								break;
							},
							Ok(None) => {
								log_info!(logger, "Payjoin request sent, waiting for response...");
								continue;
							}
							Err(e) => {
								log_error!(logger, "Failed to send Payjoin request : {}", e);
								let _ = payjoin_handler.handle_request_failure(&original_psbt, PayjoinPaymentFailureReason::RequestSendingFailed);
								break;
							},
						}
					}
				}
			}
		});
		Ok(())
	}

	/// Send a Payjoin transaction to the address specified in the `payjoin_uri`.
	///
	/// The `payjoin_uri` argument is expected to be a valid [`BIP21`] URI with Payjoin parameters.
	///
	/// Due to the asynchronous nature of the Payjoin process, this method will return immediately
	/// after constructing the Payjoin request. The sending part is performed in the background.
	/// The result of the operation will be communicated through the event queue. If the Payjoin
	/// request is successful, [`Event::PayjoinPaymentSuccessful`] event will be emitted.
	/// Otherwise, [`Event::PayjoinPaymentFailed`] is emitted.
	///
	/// [`BIP21`]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	/// [`Event::PayjoinPaymentSuccessful`]: crate::Event::PayjoinPaymentSuccessful
	/// [`Event::PayjoinPaymentFailed`]: crate::Event::PayjoinPaymentFailed
	pub fn send_with_amount(&self, payjoin_uri: String, amount_sats: u64) -> Result<(), Error> {
		let mut payjoin_uri =
			payjoin::Uri::try_from(payjoin_uri).map_err(|_| Error::PayjoinUriInvalid).and_then(
				|uri| uri.require_network(self.config.network).map_err(|_| Error::InvalidNetwork),
			)?;
		payjoin_uri.amount = Some(bitcoin::Amount::from_sat(amount_sats));
		self.send(payjoin_uri.to_string())
	}
}
