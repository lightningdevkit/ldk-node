// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to create and pay [BOLT 12] offers and refunds.
//!
//! [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md

use crate::config::LDK_PAYMENT_RETRY_TIMEOUT;
use crate::error::Error;
use crate::ffi::{maybe_deref, maybe_wrap};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::payment::store::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use crate::types::{ChannelManager, PaymentStore};

use lightning::ln::channelmanager::{PaymentId, Retry};
use lightning::offers::offer::{Amount, Offer as LdkOffer, Quantity};
use lightning::offers::parse::Bolt12SemanticError;
use lightning::util::string::UntrustedString;

use rand::RngCore;

use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(not(feature = "uniffi"))]
type Bolt12Invoice = lightning::offers::invoice::Bolt12Invoice;
#[cfg(feature = "uniffi")]
type Bolt12Invoice = Arc<crate::ffi::Bolt12Invoice>;

#[cfg(not(feature = "uniffi"))]
type Offer = LdkOffer;
#[cfg(feature = "uniffi")]
type Offer = Arc<crate::ffi::Offer>;

#[cfg(not(feature = "uniffi"))]
type Refund = lightning::offers::refund::Refund;
#[cfg(feature = "uniffi")]
type Refund = Arc<crate::ffi::Refund>;

/// A payment handler allowing to create and pay [BOLT 12] offers and refunds.
///
/// Should be retrieved by calling [`Node::bolt12_payment`].
///
/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
/// [`Node::bolt12_payment`]: crate::Node::bolt12_payment
pub struct Bolt12Payment {
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	channel_manager: Arc<ChannelManager>,
	payment_store: Arc<PaymentStore>,
	logger: Arc<Logger>,
}

impl Bolt12Payment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
		channel_manager: Arc<ChannelManager>, payment_store: Arc<PaymentStore>,
		logger: Arc<Logger>,
	) -> Self {
		Self { runtime, channel_manager, payment_store, logger }
	}

	/// Send a payment given an offer.
	///
	/// If `payer_note` is `Some` it will be seen by the recipient and reflected back in the invoice
	/// response.
	///
	/// If `quantity` is `Some` it represents the number of items requested.
	pub fn send(
		&self, offer: &Offer, quantity: Option<u64>, payer_note: Option<String>,
	) -> Result<PaymentId, Error> {
		let offer = maybe_deref(offer);
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
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
			payer_note.clone(),
			payment_id,
			retry_strategy,
			max_total_routing_fee_msat,
		) {
			Ok(()) => {
				let payee_pubkey = offer.issuer_signing_pubkey();
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
					payer_note: payer_note.map(UntrustedString),
					quantity,
				};
				let payment = PaymentDetails::new(
					payment_id,
					kind,
					Some(offer_amount_msat),
					None,
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
							payer_note: payer_note.map(UntrustedString),
							quantity,
						};
						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(offer_amount_msat),
							None,
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
		&self, offer: &Offer, amount_msat: u64, quantity: Option<u64>, payer_note: Option<String>,
	) -> Result<PaymentId, Error> {
		let offer = maybe_deref(offer);
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

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
			payer_note.clone(),
			payment_id,
			retry_strategy,
			max_total_routing_fee_msat,
		) {
			Ok(()) => {
				let payee_pubkey = offer.issuer_signing_pubkey();
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
					payer_note: payer_note.map(UntrustedString),
					quantity,
				};
				let payment = PaymentDetails::new(
					payment_id,
					kind,
					Some(amount_msat),
					None,
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
							payer_note: payer_note.map(UntrustedString),
							quantity,
						};
						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(amount_msat),
							None,
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

	pub(crate) fn receive_inner(
		&self, amount_msat: u64, description: &str, expiry_secs: Option<u32>, quantity: Option<u64>,
	) -> Result<LdkOffer, Error> {
		let absolute_expiry = expiry_secs.map(|secs| {
			(SystemTime::now() + Duration::from_secs(secs as u64))
				.duration_since(UNIX_EPOCH)
				.unwrap()
		});

		let offer_builder =
			self.channel_manager.create_offer_builder(absolute_expiry).map_err(|e| {
				log_error!(self.logger, "Failed to create offer builder: {:?}", e);
				Error::OfferCreationFailed
			})?;

		let mut offer =
			offer_builder.amount_msats(amount_msat).description(description.to_string());

		if let Some(qty) = quantity {
			if qty == 0 {
				log_error!(self.logger, "Failed to create offer: quantity can't be zero.");
				return Err(Error::InvalidQuantity);
			} else {
				offer = offer.supported_quantity(Quantity::Bounded(NonZeroU64::new(qty).unwrap()))
			};
		};

		let finalized_offer = offer.build().map_err(|e| {
			log_error!(self.logger, "Failed to create offer: {:?}", e);
			Error::OfferCreationFailed
		})?;

		Ok(finalized_offer)
	}

	/// Returns a payable offer that can be used to request and receive a payment of the amount
	/// given.
	pub fn receive(
		&self, amount_msat: u64, description: &str, expiry_secs: Option<u32>, quantity: Option<u64>,
	) -> Result<Offer, Error> {
		let offer = self.receive_inner(amount_msat, description, expiry_secs, quantity)?;
		Ok(maybe_wrap(offer))
	}

	/// Returns a payable offer that can be used to request and receive a payment for which the
	/// amount is to be determined by the user, also known as a "zero-amount" offer.
	pub fn receive_variable_amount(
		&self, description: &str, expiry_secs: Option<u32>,
	) -> Result<Offer, Error> {
		let absolute_expiry = expiry_secs.map(|secs| {
			(SystemTime::now() + Duration::from_secs(secs as u64))
				.duration_since(UNIX_EPOCH)
				.unwrap()
		});

		let offer_builder =
			self.channel_manager.create_offer_builder(absolute_expiry).map_err(|e| {
				log_error!(self.logger, "Failed to create offer builder: {:?}", e);
				Error::OfferCreationFailed
			})?;
		let offer = offer_builder.description(description.to_string()).build().map_err(|e| {
			log_error!(self.logger, "Failed to create offer: {:?}", e);
			Error::OfferCreationFailed
		})?;

		Ok(maybe_wrap(offer))
	}

	/// Requests a refund payment for the given [`Refund`].
	///
	/// The returned [`Bolt12Invoice`] is for informational purposes only (i.e., isn't needed to
	/// retrieve the refund).
	///
	/// [`Refund`]: lightning::offers::refund::Refund
	/// [`Bolt12Invoice`]: lightning::offers::invoice::Bolt12Invoice
	pub fn request_refund_payment(&self, refund: &Refund) -> Result<Bolt12Invoice, Error> {
		let refund = maybe_deref(refund);
		let invoice = self.channel_manager.request_refund_payment(&refund).map_err(|e| {
			log_error!(self.logger, "Failed to request refund payment: {:?}", e);
			Error::InvoiceRequestCreationFailed
		})?;

		let payment_hash = invoice.payment_hash();
		let payment_id = PaymentId(payment_hash.0);

		let kind = PaymentKind::Bolt12Refund {
			hash: Some(payment_hash),
			preimage: None,
			secret: None,
			payer_note: refund.payer_note().map(|note| UntrustedString(note.0.to_string())),
			quantity: refund.quantity(),
		};

		let payment = PaymentDetails::new(
			payment_id,
			kind,
			Some(refund.amount_msats()),
			None,
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);

		self.payment_store.insert(payment)?;

		Ok(maybe_wrap(invoice))
	}

	/// Returns a [`Refund`] object that can be used to offer a refund payment of the amount given.
	///
	/// [`Refund`]: lightning::offers::refund::Refund
	pub fn initiate_refund(
		&self, amount_msat: u64, expiry_secs: u32, quantity: Option<u64>,
		payer_note: Option<String>,
	) -> Result<Refund, Error> {
		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		let payment_id = PaymentId(random_bytes);

		let absolute_expiry = (SystemTime::now() + Duration::from_secs(expiry_secs as u64))
			.duration_since(UNIX_EPOCH)
			.unwrap();
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let max_total_routing_fee_msat = None;

		let mut refund_builder = self
			.channel_manager
			.create_refund_builder(
				amount_msat,
				absolute_expiry,
				payment_id,
				retry_strategy,
				max_total_routing_fee_msat,
			)
			.map_err(|e| {
				log_error!(self.logger, "Failed to create refund builder: {:?}", e);
				Error::RefundCreationFailed
			})?;

		if let Some(qty) = quantity {
			refund_builder = refund_builder.quantity(qty);
		}

		if let Some(note) = payer_note.clone() {
			refund_builder = refund_builder.payer_note(note);
		}

		let refund = refund_builder.build().map_err(|e| {
			log_error!(self.logger, "Failed to create refund: {:?}", e);
			Error::RefundCreationFailed
		})?;

		log_info!(self.logger, "Offering refund of {}msat", amount_msat);

		let kind = PaymentKind::Bolt12Refund {
			hash: None,
			preimage: None,
			secret: None,
			payer_note: payer_note.map(|note| UntrustedString(note)),
			quantity,
		};
		let payment = PaymentDetails::new(
			payment_id,
			kind,
			Some(amount_msat),
			None,
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		);

		self.payment_store.insert(payment)?;

		Ok(maybe_wrap(refund))
	}
}
