//! Holds a payment handler allowing to create and pay [BOLT 12] offers and refunds.
//!
//! [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md

use crate::config::LDK_PAYMENT_RETRY_TIMEOUT;
use crate::error::Error;
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::payment::store::{
	PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus, PaymentStore,
};
use crate::types::ChannelManager;

use lightning::ln::channelmanager::{PaymentId, Retry};
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::{Amount, Offer};
use lightning::offers::parse::Bolt12SemanticError;
use lightning::offers::refund::Refund;

use rand::RngCore;

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A payment handler allowing to create and pay [BOLT 12] offers and refunds.
///
/// Should be retrieved by calling [`Node::bolt12_payment`].
///
/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
/// [`Node::bolt12_payment`]: crate::Node::bolt12_payment
pub struct Bolt12Payment {
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	channel_manager: Arc<ChannelManager>,
	payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>,
	logger: Arc<FilesystemLogger>,
}

impl Bolt12Payment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
		channel_manager: Arc<ChannelManager>,
		payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>, logger: Arc<FilesystemLogger>,
	) -> Self {
		Self { runtime, channel_manager, payment_store, logger }
	}

	/// Send a payment given an offer.
	///
	/// If `payer_note` is `Some` it will be seen by the recipient and reflected back in the invoice
	/// response.
	pub fn send(&self, offer: &Offer, payer_note: Option<String>) -> Result<PaymentId, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let quantity = None;
		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		let payment_id = PaymentId(random_bytes);
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let max_total_routing_fee_msat = None;

		let offer_amount_msat = match offer.amount() {
			Some(Amount::Bitcoin { amount_msats }) => amount_msats,
			Some(_) => {
				log_error!(self.logger, "Failed to send payment as the provided offer was denominated in an unsupported currency.");
				return Err(Error::UnsupportedCurrency);
			},
			None => {
				log_error!(self.logger, "Failed to send payment due to the given offer being \"zero-amount\". Please use send_using_amount instead.");
				return Err(Error::InvalidOffer);
			},
		};

		match self.channel_manager.pay_for_offer(
			&offer,
			quantity,
			None,
			payer_note,
			payment_id,
			retry_strategy,
			max_total_routing_fee_msat,
		) {
			Ok(()) => {
				let payee_pubkey = offer.signing_pubkey();
				log_info!(
					self.logger,
					"Initiated sending {}msat to {:?}",
					offer_amount_msat,
					payee_pubkey
				);

				let kind = PaymentKind::Bolt12Offer {
					hash: None,
					preimage: None,
					secret: None,
					offer_id: offer.id(),
				};
				let payment = PaymentDetails::new(
					payment_id,
					kind,
					Some(*offer_amount_msat),
					PaymentDirection::Outbound,
					PaymentStatus::Pending,
				);
				self.payment_store.insert(payment)?;

				Ok(payment_id)
			},
			Err(e) => {
				log_error!(self.logger, "Failed to send invoice request: {:?}", e);
				match e {
					Bolt12SemanticError::DuplicatePaymentId => Err(Error::DuplicatePayment),
					_ => {
						let kind = PaymentKind::Bolt12Offer {
							hash: None,
							preimage: None,
							secret: None,
							offer_id: offer.id(),
						};
						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(*offer_amount_msat),
							PaymentDirection::Outbound,
							PaymentStatus::Failed,
						);
						self.payment_store.insert(payment)?;
						Err(Error::InvoiceRequestCreationFailed)
					},
				}
			},
		}
	}

	/// Send a payment given an offer and an amount in millisatoshi.
	///
	/// This will fail if the amount given is less than the value required by the given offer.
	///
	/// This can be used to pay a so-called "zero-amount" offers, i.e., an offer that leaves the
	/// amount paid to be determined by the user.
	///
	/// If `payer_note` is `Some` it will be seen by the recipient and reflected back in the invoice
	/// response.
	pub fn send_using_amount(
		&self, offer: &Offer, payer_note: Option<String>, amount_msat: u64,
	) -> Result<PaymentId, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let quantity = None;
		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		let payment_id = PaymentId(random_bytes);
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let max_total_routing_fee_msat = None;

		let offer_amount_msat = match offer.amount() {
			Some(Amount::Bitcoin { amount_msats }) => *amount_msats,
			Some(_) => {
				log_error!(self.logger, "Failed to send payment as the provided offer was denominated in an unsupported currency.");
				return Err(Error::UnsupportedCurrency);
			},
			None => amount_msat,
		};

		if amount_msat < offer_amount_msat {
			log_error!(
				self.logger,
				"Failed to pay as the given amount needs to be at least the offer amount: required {}msat, gave {}msat.", offer_amount_msat, amount_msat);
			return Err(Error::InvalidAmount);
		}

		match self.channel_manager.pay_for_offer(
			&offer,
			quantity,
			Some(amount_msat),
			payer_note,
			payment_id,
			retry_strategy,
			max_total_routing_fee_msat,
		) {
			Ok(()) => {
				let payee_pubkey = offer.signing_pubkey();
				log_info!(
					self.logger,
					"Initiated sending {}msat to {:?}",
					amount_msat,
					payee_pubkey
				);

				let kind = PaymentKind::Bolt12Offer {
					hash: None,
					preimage: None,
					secret: None,
					offer_id: offer.id(),
				};
				let payment = PaymentDetails::new(
					payment_id,
					kind,
					Some(amount_msat),
					PaymentDirection::Outbound,
					PaymentStatus::Pending,
				);
				self.payment_store.insert(payment)?;

				Ok(payment_id)
			},
			Err(e) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);
				match e {
					Bolt12SemanticError::DuplicatePaymentId => Err(Error::DuplicatePayment),
					_ => {
						let kind = PaymentKind::Bolt12Offer {
							hash: None,
							preimage: None,
							secret: None,
							offer_id: offer.id(),
						};
						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(amount_msat),
							PaymentDirection::Outbound,
							PaymentStatus::Failed,
						);
						self.payment_store.insert(payment)?;
						Err(Error::PaymentSendingFailed)
					},
				}
			},
		}
	}

	/// Returns a payable offer that can be used to request and receive a payment of the amount
	/// given.
	pub fn receive(&self, amount_msat: u64, description: &str) -> Result<Offer, Error> {
		let offer_builder = self.channel_manager.create_offer_builder().map_err(|e| {
			log_error!(self.logger, "Failed to create offer builder: {:?}", e);
			Error::OfferCreationFailed
		})?;
		let offer = offer_builder
			.amount_msats(amount_msat)
			.description(description.to_string())
			.build()
			.map_err(|e| {
				log_error!(self.logger, "Failed to create offer: {:?}", e);
				Error::OfferCreationFailed
			})?;

		Ok(offer)
	}

	/// Returns a payable offer that can be used to request and receive a payment for which the
	/// amount is to be determined by the user, also known as a "zero-amount" offer.
	pub fn receive_variable_amount(&self, description: &str) -> Result<Offer, Error> {
		let offer_builder = self.channel_manager.create_offer_builder().map_err(|e| {
			log_error!(self.logger, "Failed to create offer builder: {:?}", e);
			Error::OfferCreationFailed
		})?;
		let offer = offer_builder.description(description.to_string()).build().map_err(|e| {
			log_error!(self.logger, "Failed to create offer: {:?}", e);
			Error::OfferCreationFailed
		})?;

		Ok(offer)
	}

	/// Requests a refund payment for the given [`Refund`].
	///
	/// The returned [`Bolt12Invoice`] is for informational purposes only (i.e., isn't needed to
	/// retrieve the refund).
	pub fn request_refund_payment(&self, refund: &Refund) -> Result<Bolt12Invoice, Error> {
		let invoice = self.channel_manager.request_refund_payment(refund).map_err(|e| {
			log_error!(self.logger, "Failed to request refund payment: {:?}", e);
			Error::InvoiceRequestCreationFailed
		})?;

		let payment_hash = invoice.payment_hash();
		let payment_id = PaymentId(payment_hash.0);

		let kind =
			PaymentKind::Bolt12Refund { hash: Some(payment_hash), preimage: None, secret: None };

		let payment = PaymentDetails::new(
			payment_id,
			kind,
			Some(refund.amount_msats()),
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);

		self.payment_store.insert(payment)?;

		Ok(invoice)
	}

	/// Returns a [`Refund`] object that can be used to offer a refund payment of the amount given.
	pub fn initiate_refund(&self, amount_msat: u64, expiry_secs: u32) -> Result<Refund, Error> {
		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		let payment_id = PaymentId(random_bytes);

		let expiration = (SystemTime::now() + Duration::from_secs(expiry_secs as u64))
			.duration_since(UNIX_EPOCH)
			.unwrap();
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let max_total_routing_fee_msat = None;

		let refund = self
			.channel_manager
			.create_refund_builder(
				amount_msat,
				expiration,
				payment_id,
				retry_strategy,
				max_total_routing_fee_msat,
			)
			.map_err(|e| {
				log_error!(self.logger, "Failed to create refund builder: {:?}", e);
				Error::RefundCreationFailed
			})?
			.build()
			.map_err(|e| {
				log_error!(self.logger, "Failed to create refund: {:?}", e);
				Error::RefundCreationFailed
			})?;

		log_info!(self.logger, "Offering refund of {}msat", amount_msat);

		let kind = PaymentKind::Bolt12Refund { hash: None, preimage: None, secret: None };

		let payment = PaymentDetails::new(
			payment_id,
			kind,
			Some(amount_msat),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		);

		self.payment_store.insert(payment)?;

		Ok(refund)
	}
}
