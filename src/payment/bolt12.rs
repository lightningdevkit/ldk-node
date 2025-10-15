// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to create and pay [BOLT 12] offers and refunds.
//!
//! [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md

use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lightning::blinded_path::message::BlindedMessagePath;
use lightning::ln::channelmanager::{OptionalOfferPaymentParams, PaymentId, Retry};
use lightning::offers::offer::{Amount, Offer as LdkOffer, Quantity};
use lightning::offers::parse::Bolt12SemanticError;
use lightning::routing::router::RouteParametersConfig;
#[cfg(feature = "uniffi")]
use lightning::util::ser::{Readable, Writeable};
use lightning_types::string::UntrustedString;
use rand::RngCore;

use crate::config::{AsyncPaymentsRole, LDK_PAYMENT_RETRY_TIMEOUT};
use crate::error::Error;
use crate::ffi::{maybe_deref, maybe_wrap};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::payment::store::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use crate::types::{ChannelManager, PaymentStore};

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
	channel_manager: Arc<ChannelManager>,
	payment_store: Arc<PaymentStore>,
	is_running: Arc<RwLock<bool>>,
	logger: Arc<Logger>,
	async_payments_role: Option<AsyncPaymentsRole>,
}

impl Bolt12Payment {
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, payment_store: Arc<PaymentStore>,
		is_running: Arc<RwLock<bool>>, logger: Arc<Logger>,
		async_payments_role: Option<AsyncPaymentsRole>,
	) -> Self {
		Self { channel_manager, payment_store, is_running, logger, async_payments_role }
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
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let offer = maybe_deref(offer);

		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		let payment_id = PaymentId(random_bytes);
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let route_params_config = RouteParametersConfig::default();

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

		let params = OptionalOfferPaymentParams {
			payer_note: payer_note.clone(),
			retry_strategy,
			route_params_config,
		};
		let res = if let Some(quantity) = quantity {
			self.channel_manager
				.pay_for_offer_with_quantity(&offer, None, payment_id, params, quantity)
		} else {
			self.channel_manager.pay_for_offer(&offer, None, payment_id, params)
		};

		match res {
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
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let offer = maybe_deref(offer);

		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		let payment_id = PaymentId(random_bytes);
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let route_params_config = RouteParametersConfig::default();

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

		let params = OptionalOfferPaymentParams {
			payer_note: payer_note.clone(),
			retry_strategy,
			route_params_config,
		};
		let res = if let Some(quantity) = quantity {
			self.channel_manager.pay_for_offer_with_quantity(
				&offer,
				Some(amount_msat),
				payment_id,
				params,
				quantity,
			)
		} else {
			self.channel_manager.pay_for_offer(&offer, Some(amount_msat), payment_id, params)
		};

		match res {
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
		let mut offer_builder = self.channel_manager.create_offer_builder().map_err(|e| {
			log_error!(self.logger, "Failed to create offer builder: {:?}", e);
			Error::OfferCreationFailed
		})?;

		if let Some(expiry_secs) = expiry_secs {
			let absolute_expiry = (SystemTime::now() + Duration::from_secs(expiry_secs as u64))
				.duration_since(UNIX_EPOCH)
				.unwrap();
			offer_builder = offer_builder.absolute_expiry(absolute_expiry);
		}

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
		let mut offer_builder = self.channel_manager.create_offer_builder().map_err(|e| {
			log_error!(self.logger, "Failed to create offer builder: {:?}", e);
			Error::OfferCreationFailed
		})?;

		if let Some(expiry_secs) = expiry_secs {
			let absolute_expiry = (SystemTime::now() + Duration::from_secs(expiry_secs as u64))
				.duration_since(UNIX_EPOCH)
				.unwrap();
			offer_builder = offer_builder.absolute_expiry(absolute_expiry);
		}

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
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

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
		let route_params_config = RouteParametersConfig::default();

		let mut refund_builder = self
			.channel_manager
			.create_refund_builder(
				amount_msat,
				absolute_expiry,
				payment_id,
				retry_strategy,
				route_params_config,
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

	/// Retrieve an [`Offer`] for receiving async payments as an often-offline recipient.
	///
	/// Will only return an offer if [`Bolt12Payment::set_paths_to_static_invoice_server`] was called and we succeeded
	/// in interactively building a [`StaticInvoice`] with the static invoice server.
	///
	/// Useful for posting offers to receive payments later, such as posting an offer on a website.
	///
	/// **Caution**: Async payments support is considered experimental.
	///
	/// [`StaticInvoice`]: lightning::offers::static_invoice::StaticInvoice
	/// [`Offer`]: lightning::offers::offer::Offer
	pub fn receive_async(&self) -> Result<Offer, Error> {
		self.channel_manager
			.get_async_receive_offer()
			.map(maybe_wrap)
			.or(Err(Error::OfferCreationFailed))
	}

	/// Sets the [`BlindedMessagePath`]s that we will use as an async recipient to interactively build [`Offer`]s with a
	/// static invoice server, so the server can serve [`StaticInvoice`]s to payers on our behalf when we're offline.
	///
	/// **Caution**: Async payments support is considered experimental.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`StaticInvoice`]: lightning::offers::static_invoice::StaticInvoice
	#[cfg(not(feature = "uniffi"))]
	pub fn set_paths_to_static_invoice_server(
		&self, paths: Vec<BlindedMessagePath>,
	) -> Result<(), Error> {
		self.channel_manager
			.set_paths_to_static_invoice_server(paths)
			.or(Err(Error::InvalidBlindedPaths))
	}

	/// Sets the [`BlindedMessagePath`]s that we will use as an async recipient to interactively build [`Offer`]s with a
	/// static invoice server, so the server can serve [`StaticInvoice`]s to payers on our behalf when we're offline.
	///
	/// **Caution**: Async payments support is considered experimental.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`StaticInvoice`]: lightning::offers::static_invoice::StaticInvoice
	#[cfg(feature = "uniffi")]
	pub fn set_paths_to_static_invoice_server(&self, paths: Vec<u8>) -> Result<(), Error> {
		let decoded_paths = <Vec<BlindedMessagePath> as Readable>::read(&mut &paths[..])
			.or(Err(Error::InvalidBlindedPaths))?;

		self.channel_manager
			.set_paths_to_static_invoice_server(decoded_paths)
			.or(Err(Error::InvalidBlindedPaths))
	}

	/// [`BlindedMessagePath`]s for an async recipient to communicate with this node and interactively
	/// build [`Offer`]s and [`StaticInvoice`]s for receiving async payments.
	///
	/// **Caution**: Async payments support is considered experimental.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`StaticInvoice`]: lightning::offers::static_invoice::StaticInvoice
	#[cfg(not(feature = "uniffi"))]
	pub fn blinded_paths_for_async_recipient(
		&self, recipient_id: Vec<u8>,
	) -> Result<Vec<BlindedMessagePath>, Error> {
		self.blinded_paths_for_async_recipient_internal(recipient_id)
	}

	/// [`BlindedMessagePath`]s for an async recipient to communicate with this node and interactively
	/// build [`Offer`]s and [`StaticInvoice`]s for receiving async payments.
	///
	/// **Caution**: Async payments support is considered experimental.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`StaticInvoice`]: lightning::offers::static_invoice::StaticInvoice
	#[cfg(feature = "uniffi")]
	pub fn blinded_paths_for_async_recipient(
		&self, recipient_id: Vec<u8>,
	) -> Result<Vec<u8>, Error> {
		let paths = self.blinded_paths_for_async_recipient_internal(recipient_id)?;

		let mut bytes = Vec::new();
		paths.write(&mut bytes).or(Err(Error::InvalidBlindedPaths))?;
		Ok(bytes)
	}

	fn blinded_paths_for_async_recipient_internal(
		&self, recipient_id: Vec<u8>,
	) -> Result<Vec<BlindedMessagePath>, Error> {
		match self.async_payments_role {
			Some(AsyncPaymentsRole::Server) => {},
			_ => {
				return Err(Error::AsyncPaymentServicesDisabled);
			},
		}

		self.channel_manager
			.blinded_paths_for_async_recipient(recipient_id, None)
			.or(Err(Error::InvalidBlindedPaths))
	}
}
