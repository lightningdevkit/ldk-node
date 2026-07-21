// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to create and pay [BOLT 11] invoices.
//!
//! [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md

use std::sync::{Arc, RwLock};

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::{
	Bolt11InvoiceParameters, OptionalBolt11PaymentParams, PaymentId,
};
use lightning::ln::outbound_payment::{Bolt11PaymentError, Retry, RetryableSendFailure};
use lightning::routing::router::{PaymentParameters, RouteParameters, RouteParametersConfig};
use lightning_invoice::{
	Bolt11Invoice as LdkBolt11Invoice, Bolt11InvoiceDescription as LdkBolt11InvoiceDescription,
};
use lightning_types::payment::{PaymentHash, PaymentPreimage};

use crate::config::{Config, LDK_PAYMENT_RETRY_TIMEOUT};
use crate::connection::ConnectionManager;
use crate::data_store::DataStoreUpdateResult;
use crate::error::Error;
use crate::ffi::{maybe_deref, maybe_try_convert_enum, maybe_wrap};
use crate::liquidity::LiquiditySource;
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::payment::store::{
	LSPS2Parameters, PaymentDetails, PaymentDetailsUpdate, PaymentDirection, PaymentKind,
	PaymentStatus,
};
use crate::peer_store::{PeerInfo, PeerStore};
use crate::runtime::Runtime;
use crate::types::{ChannelManager, PaymentStore};

#[cfg(not(feature = "uniffi"))]
type Bolt11Invoice = LdkBolt11Invoice;
#[cfg(feature = "uniffi")]
type Bolt11Invoice = Arc<crate::ffi::Bolt11Invoice>;

#[cfg(not(feature = "uniffi"))]
type Bolt11InvoiceDescription = LdkBolt11InvoiceDescription;
#[cfg(feature = "uniffi")]
type Bolt11InvoiceDescription = crate::ffi::Bolt11InvoiceDescription;

/// Metadata carried in BOLT11 invoice `payment_metadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentMetadata {
	pub(crate) lsps2_parameters: Option<LSPS2Parameters>,
}

impl_writeable_tlv_based!(PaymentMetadata, {
	(0, lsps2_parameters, option),
});

/// A payment handler allowing to create and pay [BOLT 11] invoices.
///
/// Should be retrieved by calling [`Node::bolt11_payment`].
///
/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
/// [`Node::bolt11_payment`]: crate::Node::bolt11_payment
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct Bolt11Payment {
	runtime: Arc<Runtime>,
	channel_manager: Arc<ChannelManager>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Arc<LiquiditySource<Arc<Logger>>>,
	payment_store: Arc<PaymentStore>,
	peer_store: Arc<PeerStore<Arc<Logger>>>,
	config: Arc<Config>,
	is_running: Arc<RwLock<bool>>,
	logger: Arc<Logger>,
}

impl Bolt11Payment {
	pub(crate) fn new(
		runtime: Arc<Runtime>, channel_manager: Arc<ChannelManager>,
		connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Arc<LiquiditySource<Arc<Logger>>>, payment_store: Arc<PaymentStore>,
		peer_store: Arc<PeerStore<Arc<Logger>>>, config: Arc<Config>,
		is_running: Arc<RwLock<bool>>, logger: Arc<Logger>,
	) -> Self {
		Self {
			runtime,
			channel_manager,
			connection_manager,
			liquidity_source,
			payment_store,
			peer_store,
			config,
			is_running,
			logger,
		}
	}

	pub(crate) fn receive_inner(
		&self, amount_msat: Option<u64>, invoice_description: &LdkBolt11InvoiceDescription,
		expiry_secs: u32, manual_claim_payment_hash: Option<PaymentHash>,
	) -> Result<LdkBolt11Invoice, Error> {
		let invoice = {
			let invoice_params = Bolt11InvoiceParameters {
				amount_msats: amount_msat,
				description: invoice_description.clone(),
				invoice_expiry_delta_secs: Some(expiry_secs),
				payment_hash: manual_claim_payment_hash,
				..Default::default()
			};

			match self.channel_manager.create_bolt11_invoice(invoice_params) {
				Ok(inv) => {
					log_info!(self.logger, "Invoice created: {}", inv);
					inv
				},
				Err(e) => {
					log_error!(self.logger, "Failed to create invoice: {}", e);
					return Err(Error::InvoiceCreationFailed);
				},
			}
		};

		let payment_hash = invoice.payment_hash();
		let payment_secret = invoice.payment_secret();
		let id = PaymentId(payment_hash.0);
		let preimage = if manual_claim_payment_hash.is_none() {
			// If the user hasn't registered a custom payment hash, we're positive ChannelManager
			// will know the preimage at this point.
			let mut payment_metadata = invoice.payment_metadata().cloned();
			let res = self
				.channel_manager
				.get_payment_preimage_decrypt_metadata(
					payment_hash,
					payment_secret.clone(),
					payment_metadata.as_deref_mut(),
				)
				.ok();
			debug_assert!(res.is_some(), "We just let ChannelManager create an inbound payment, it can't have forgotten the preimage by now.");
			res
		} else {
			None
		};
		let kind = PaymentKind::Bolt11 {
			hash: payment_hash,
			preimage,
			secret: Some(payment_secret.clone()),
			counterparty_skimmed_fee_msat: None,
		};
		let payment = PaymentDetails::new(
			id,
			kind,
			amount_msat,
			None,
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);
		self.runtime.block_on(self.payment_store.insert(payment))?;

		Ok(invoice)
	}

	fn receive_via_jit_channel_inner(
		&self, amount_msat: Option<u64>, description: &LdkBolt11InvoiceDescription,
		expiry_secs: u32, payment_hash: Option<PaymentHash>,
	) -> Result<LdkBolt11Invoice, Error> {
		let connection_manager = Arc::clone(&self.connection_manager);
		let (invoice, chosen_lsp) = self.runtime.block_on(async move {
			if let Some(amount_msat) = amount_msat {
				self.liquidity_source
					.lsps2_client()
					.lsps2_receive_to_jit_channel(
						amount_msat,
						description,
						expiry_secs,
						payment_hash,
						connection_manager,
					)
					.await
			} else {
				self.liquidity_source
					.lsps2_client()
					.lsps2_receive_variable_amount_to_jit_channel(
						description,
						expiry_secs,
						payment_hash,
						connection_manager,
					)
					.await
			}
		})?;

		// Register payment in payment store.
		let payment_hash = invoice.payment_hash();
		let payment_secret = invoice.payment_secret();
		let id = PaymentId(payment_hash.0);
		let mut payment_metadata = invoice.payment_metadata().cloned();
		let preimage = self
			.channel_manager
			.get_payment_preimage_decrypt_metadata(
				payment_hash,
				payment_secret.clone(),
				payment_metadata.as_deref_mut(),
			)
			.ok();
		let kind = PaymentKind::Bolt11 {
			hash: payment_hash,
			preimage,
			secret: Some(payment_secret.clone()),
			counterparty_skimmed_fee_msat: None,
		};
		let payment = PaymentDetails::new(
			id,
			kind,
			amount_msat,
			None,
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);
		self.runtime.block_on(self.payment_store.insert(payment))?;

		// Persist the chosen LSP peer to make sure we reconnect on restart.
		let peer_info = PeerInfo { node_id: chosen_lsp.node_id, address: chosen_lsp.address };
		self.runtime.block_on(self.peer_store.add_peer(peer_info))?;

		Ok(invoice)
	}
}

#[cfg(test)]
mod tests {
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	#[test]
	fn empty_metadata_roundtrips() {
		let metadata = PaymentMetadata { lsps2_parameters: None };

		let encoded = metadata.encode();
		let decoded = PaymentMetadata::read(&mut &*encoded).unwrap();

		assert_eq!(metadata, decoded);
	}

	#[test]
	fn lsps2_parameters_roundtrip() {
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(42_000),
			max_proportional_opening_fee_ppm_msat: Some(17_000),
		};
		let metadata = PaymentMetadata { lsps2_parameters: Some(lsps2_parameters) };

		let encoded = metadata.encode();
		let decoded = PaymentMetadata::read(&mut &*encoded).unwrap();

		assert_eq!(metadata, decoded);
	}
}

impl Bolt11Payment {
	fn send_internal(
		&self, invoice: &LdkBolt11Invoice, amount_msat: Option<u64>,
		route_parameters: Option<RouteParametersConfig>,
		declared_total_mpp_value_msat_override: Option<u64>, invalid_amount_log: &'static str,
	) -> Result<PaymentId, Error> {
		if !*self.is_running.read().expect("lock") {
			return Err(Error::NotRunning);
		}

		let payment_hash = invoice.payment_hash();
		let payment_id = PaymentId(invoice.payment_hash().0);
		if let Some(payment) = self.payment_store.get(&payment_id) {
			if payment.status == PaymentStatus::Pending
				|| payment.status == PaymentStatus::Succeeded
			{
				log_error!(self.logger, "Payment error: an invoice must not be paid twice.");
				return Err(Error::DuplicatePayment);
			}
		}

		let route_params_config =
			route_parameters.or(self.config.route_parameters).unwrap_or_default();
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let payment_secret = Some(*invoice.payment_secret());
		let payment_amount_msat = match amount_msat.or_else(|| invoice.amount_milli_satoshis()) {
			Some(amount_msat) => amount_msat,
			None => {
				log_error!(self.logger, "{}", invalid_amount_log);
				return Err(Error::InvalidInvoice);
			},
		};

		let optional_params = OptionalBolt11PaymentParams {
			retry_strategy,
			route_params_config,
			declared_total_mpp_value_msat_override,
			..Default::default()
		};
		match self.channel_manager.pay_for_bolt11_invoice(
			invoice,
			payment_id,
			amount_msat,
			optional_params,
		) {
			Ok(()) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				log_info!(
					self.logger,
					"Initiated sending {} msat to {}",
					payment_amount_msat,
					payee_pubkey
				);

				let kind = PaymentKind::Bolt11 {
					hash: payment_hash,
					preimage: None,
					secret: payment_secret,
					counterparty_skimmed_fee_msat: None,
				};
				let payment = PaymentDetails::new(
					payment_id,
					kind,
					Some(payment_amount_msat),
					None,
					PaymentDirection::Outbound,
					PaymentStatus::Pending,
				);

				self.runtime.block_on(self.payment_store.insert(payment))?;

				Ok(payment_id)
			},
			Err(Bolt11PaymentError::InvalidAmount) => {
				log_error!(self.logger, "{}", invalid_amount_log);
				return Err(Error::InvalidInvoice);
			},
			Err(Bolt11PaymentError::SendingFailed(e)) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);
				match e {
					RetryableSendFailure::DuplicatePayment => Err(Error::DuplicatePayment),
					_ => {
						let kind = PaymentKind::Bolt11 {
							hash: payment_hash,
							preimage: None,
							secret: payment_secret,
							counterparty_skimmed_fee_msat: None,
						};
						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(payment_amount_msat),
							None,
							PaymentDirection::Outbound,
							PaymentStatus::Failed,
						);

						self.runtime.block_on(self.payment_store.insert(payment))?;
						Err(Error::PaymentSendingFailed)
					},
				}
			},
		}
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl Bolt11Payment {
	/// Send a payment given an invoice.
	///
	/// If `route_parameters` are provided they will override the default as well as the
	/// node-wide parameters configured via [`Config::route_parameters`] on a per-field basis.
	pub fn send(
		&self, invoice: &Bolt11Invoice, route_parameters: Option<RouteParametersConfig>,
	) -> Result<PaymentId, Error> {
		if !*self.is_running.read().expect("lock") {
			return Err(Error::NotRunning);
		}

		let invoice = maybe_deref(invoice);
		self.send_internal(
			invoice,
			None,
			route_parameters,
			None,
			"Failed to send payment due to the given invoice being \"zero-amount\". Please use send_using_amount instead.",
		)
	}

	/// Send a payment given an invoice and an amount in millisatoshis.
	///
	/// This will fail if the amount given is less than the value required by the given invoice.
	///
	/// This can be used to pay a so-called "zero-amount" invoice, i.e., an invoice that leaves the
	/// amount paid to be determined by the user.
	///
	/// If `route_parameters` are provided they will override the default as well as the
	/// node-wide parameters configured via [`Config::route_parameters`] on a per-field basis.
	pub fn send_using_amount(
		&self, invoice: &Bolt11Invoice, amount_msat: u64,
		route_parameters: Option<RouteParametersConfig>,
	) -> Result<PaymentId, Error> {
		if !*self.is_running.read().expect("lock") {
			return Err(Error::NotRunning);
		}

		let invoice = maybe_deref(invoice);
		if let Some(invoice_amount_msat) = invoice.amount_milli_satoshis() {
			if amount_msat < invoice_amount_msat {
				log_error!(
					self.logger,
					"Failed to pay as the given amount needs to be at least the invoice amount: required {}msat, gave {}msat.", invoice_amount_msat, amount_msat);
				return Err(Error::InvalidAmount);
			}
		}

		self.send_internal(
			invoice,
			Some(amount_msat),
			route_parameters,
			None,
			"Failed to send payment due to amount given being insufficient.",
		)
	}

	/// Send a payment given an invoice and an amount lower than the invoice amount.
	///
	/// This uses LDK's partial MPP support by declaring the invoice amount as the total MPP value
	/// while only sending `amount_msat` from this node. The receiving node must be willing to
	/// accept underpaying HTLCs for the payment to complete.
	///
	/// This will fail if the invoice is a zero-amount invoice, or if the amount given is greater
	/// than or equal to the value required by the invoice. Use [`Self::send_using_amount`] instead
	/// when paying a zero-amount invoice or paying at least the invoice amount.
	///
	/// If `route_parameters` are provided they will override the default as well as the
	/// node-wide parameters configured via [`Config::route_parameters`] on a per-field basis.
	pub fn send_using_amount_underpaying(
		&self, invoice: &Bolt11Invoice, amount_msat: u64,
		route_parameters: Option<RouteParametersConfig>,
	) -> Result<PaymentId, Error> {
		if !*self.is_running.read().expect("lock") {
			return Err(Error::NotRunning);
		}

		let invoice = maybe_deref(invoice);
		let invoice_amount_msat = invoice.amount_milli_satoshis().ok_or_else(|| {
			log_error!(self.logger, "Failed to underpay as the given invoice is \"zero-amount\".");
			Error::InvalidInvoice
		})?;

		if amount_msat >= invoice_amount_msat {
			log_error!(
				self.logger,
				"Failed to underpay as the given amount needs to be less than the invoice amount: required less than {}msat, gave {}msat.",
				invoice_amount_msat,
				amount_msat
			);
			return Err(Error::InvalidAmount);
		}

		self.send_internal(
			invoice,
			Some(amount_msat),
			route_parameters,
			Some(invoice_amount_msat),
			"Failed to send payment due to amount given being insufficient.",
		)
	}

	/// Allows to attempt manually claiming payments with the given preimage that have previously
	/// been registered via [`receive_for_hash`] or [`receive_variable_amount_for_hash`].
	///
	/// This should be called in reponse to a [`PaymentClaimable`] event as soon as the preimage is
	/// available.
	///
	/// Will check that the payment is known, and that the given preimage and claimable amount
	/// match our expectations before attempting to claim the payment, and will return an error
	/// otherwise.
	///
	/// When claiming the payment has succeeded, a [`PaymentReceived`] event will be emitted.
	///
	/// [`receive_for_hash`]: Self::receive_for_hash
	/// [`receive_variable_amount_for_hash`]: Self::receive_variable_amount_for_hash
	/// [`PaymentClaimable`]: crate::Event::PaymentClaimable
	/// [`PaymentReceived`]: crate::Event::PaymentReceived
	pub fn claim_for_hash(
		&self, payment_hash: PaymentHash, claimable_amount_msat: u64, preimage: PaymentPreimage,
	) -> Result<(), Error> {
		let payment_id = PaymentId(payment_hash.0);

		let expected_payment_hash = PaymentHash(Sha256::hash(&preimage.0).to_byte_array());

		if expected_payment_hash != payment_hash {
			log_error!(
				self.logger,
				"Failed to manually claim payment as the given preimage doesn't match the hash {}",
				payment_hash
			);
			return Err(Error::InvalidPaymentPreimage);
		}

		if let Some(details) = self.payment_store.get(&payment_id) {
			// For payments requested via `receive*_via_jit_channel_for_hash()`
			// `skimmed_fee_msat` held by LSP must be taken into account.
			let skimmed_fee_msat = match details.kind {
				PaymentKind::Bolt11 {
					counterparty_skimmed_fee_msat: Some(skimmed_fee_msat),
					..
				} => skimmed_fee_msat,
				_ => 0,
			};
			if let Some(invoice_amount_msat) = details.amount_msat {
				if claimable_amount_msat < invoice_amount_msat.saturating_sub(skimmed_fee_msat) {
					log_error!(
						self.logger,
						"Failed to manually claim payment {} as the claimable amount is less than expected",
						payment_id
					);
					return Err(Error::InvalidAmount);
				}
			}
		} else {
			log_error!(
				self.logger,
				"Failed to manually claim unknown payment with hash: {}",
				payment_hash
			);
			return Err(Error::InvalidPaymentHash);
		}

		self.channel_manager.claim_funds(preimage);
		Ok(())
	}

	/// Allows to manually fail payments with the given hash that have previously
	/// been registered via [`receive_for_hash`] or [`receive_variable_amount_for_hash`].
	///
	/// This should be called in reponse to a [`PaymentClaimable`] event if the payment needs to be
	/// failed back, e.g., if the correct preimage can't be retrieved in time before the claim
	/// deadline has been reached.
	///
	/// Will check that the payment is known before failing the payment, and will return an error
	/// otherwise.
	///
	/// [`receive_for_hash`]: Self::receive_for_hash
	/// [`receive_variable_amount_for_hash`]: Self::receive_variable_amount_for_hash
	/// [`PaymentClaimable`]: crate::Event::PaymentClaimable
	pub fn fail_for_hash(&self, payment_hash: PaymentHash) -> Result<(), Error> {
		let payment_id = PaymentId(payment_hash.0);

		let update = PaymentDetailsUpdate {
			status: Some(PaymentStatus::Failed),
			..PaymentDetailsUpdate::new(payment_id)
		};

		match self.runtime.block_on(self.payment_store.update(update)) {
			Ok(DataStoreUpdateResult::Updated) | Ok(DataStoreUpdateResult::Unchanged) => (),
			Ok(DataStoreUpdateResult::NotFound) => {
				log_error!(
					self.logger,
					"Failed to manually fail unknown payment with hash {}",
					payment_hash,
				);
				return Err(Error::InvalidPaymentHash);
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to manually fail payment with hash {}: {}",
					payment_hash,
					e
				);
				return Err(e);
			},
		}

		self.channel_manager.fail_htlc_backwards(&payment_hash);
		Ok(())
	}

	/// Returns a payable invoice that can be used to request and receive a payment of the amount
	/// given.
	///
	/// The inbound payment will be automatically claimed upon arrival.
	pub fn receive(
		&self, amount_msat: u64, description: &Bolt11InvoiceDescription, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice = self.receive_inner(Some(amount_msat), &description, expiry_secs, None)?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can be used to request a payment of the amount
	/// given for the given payment hash.
	///
	/// We will register the given payment hash and emit a [`PaymentClaimable`] event once
	/// the inbound payment arrives.
	///
	/// **Note:** users *MUST* handle this event and claim the payment manually via
	/// [`claim_for_hash`] as soon as they have obtained access to the preimage of the given
	/// payment hash. If they're unable to obtain the preimage, they *MUST* immediately fail the payment via
	/// [`fail_for_hash`].
	///
	/// [`PaymentClaimable`]: crate::Event::PaymentClaimable
	/// [`claim_for_hash`]: Self::claim_for_hash
	/// [`fail_for_hash`]: Self::fail_for_hash
	pub fn receive_for_hash(
		&self, amount_msat: u64, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		payment_hash: PaymentHash,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice =
			self.receive_inner(Some(amount_msat), &description, expiry_secs, Some(payment_hash))?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can be used to request and receive a payment for which the
	/// amount is to be determined by the user, also known as a "zero-amount" invoice.
	///
	/// The inbound payment will be automatically claimed upon arrival.
	pub fn receive_variable_amount(
		&self, description: &Bolt11InvoiceDescription, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice = self.receive_inner(None, &description, expiry_secs, None)?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can be used to request a payment for the given payment hash
	/// and the amount to be determined by the user, also known as a "zero-amount" invoice.
	///
	/// We will register the given payment hash and emit a [`PaymentClaimable`] event once
	/// the inbound payment arrives.
	///
	/// **Note:** users *MUST* handle this event and claim the payment manually via
	/// [`claim_for_hash`] as soon as they have obtained access to the preimage of the given
	/// payment hash. If they're unable to obtain the preimage, they *MUST* immediately fail the payment via
	/// [`fail_for_hash`].
	///
	/// [`PaymentClaimable`]: crate::Event::PaymentClaimable
	/// [`claim_for_hash`]: Self::claim_for_hash
	/// [`fail_for_hash`]: Self::fail_for_hash
	pub fn receive_variable_amount_for_hash(
		&self, description: &Bolt11InvoiceDescription, expiry_secs: u32, payment_hash: PaymentHash,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice = self.receive_inner(None, &description, expiry_secs, Some(payment_hash))?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can use a just-in-time (JIT) channel to receive a payment of
	/// the amount given when additional inbound liquidity is needed.
	///
	/// The configured [LSPS2]-compliant LSP may open a channel when the invoice is paid. The payment
	/// may instead arrive over pre-existing channels when they provide sufficient inbound liquidity,
	/// in which case no new channel is opened.
	///
	/// The configured LSPS2 fee limit is enforced before creating the invoice.
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	pub fn receive_via_jit_channel(
		&self, amount_msat: u64, description: &Bolt11InvoiceDescription, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice =
			self.receive_via_jit_channel_inner(Some(amount_msat), &description, expiry_secs, None)?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can use a just-in-time (JIT) channel to receive a payment of
	/// the amount given when additional inbound liquidity is needed.
	///
	/// The configured [LSPS2]-compliant LSP may open a channel when the invoice is paid. The payment
	/// may instead arrive over pre-existing channels when they provide sufficient inbound liquidity,
	/// in which case no new channel is opened.
	///
	/// The configured LSPS2 fee limit is enforced before creating the invoice.
	///
	/// We will register the given payment hash and emit a [`PaymentClaimable`] event once
	/// the inbound payment arrives. The check that [`counterparty_skimmed_fee_msat`] is within the limits
	/// is performed *before* emitting the event.
	///
	/// **Note:** users *MUST* handle this event and claim the payment manually via
	/// [`claim_for_hash`] as soon as they have obtained access to the preimage of the given
	/// payment hash. If they're unable to obtain the preimage, they *MUST* immediately fail the payment via
	/// [`fail_for_hash`].
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	/// [`PaymentClaimable`]: crate::Event::PaymentClaimable
	/// [`claim_for_hash`]: Self::claim_for_hash
	/// [`fail_for_hash`]: Self::fail_for_hash
	/// [`counterparty_skimmed_fee_msat`]: crate::payment::PaymentKind::Bolt11::counterparty_skimmed_fee_msat
	pub fn receive_via_jit_channel_for_hash(
		&self, amount_msat: u64, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		payment_hash: PaymentHash,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice = self.receive_via_jit_channel_inner(
			Some(amount_msat),
			&description,
			expiry_secs,
			Some(payment_hash),
		)?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can use a just-in-time (JIT) channel to receive a variable
	/// amount payment, also known as a "zero-amount" invoice, when additional inbound liquidity is
	/// needed.
	///
	/// The configured [LSPS2]-compliant LSP may open a channel when the invoice is paid. The payment
	/// may instead arrive over pre-existing channels when they provide sufficient inbound liquidity,
	/// in which case no new channel is opened.
	///
	/// The configured LSPS2 fee limit is enforced when the payment arrives.
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	pub fn receive_variable_amount_via_jit_channel(
		&self, description: &Bolt11InvoiceDescription, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice = self.receive_via_jit_channel_inner(None, &description, expiry_secs, None)?;
		Ok(maybe_wrap(invoice))
	}

	/// Returns a payable invoice that can use a just-in-time (JIT) channel to receive a variable
	/// amount payment, also known as a "zero-amount" invoice, when additional inbound liquidity is
	/// needed.
	///
	/// The configured [LSPS2]-compliant LSP may open a channel when the invoice is paid. The payment
	/// may instead arrive over pre-existing channels when they provide sufficient inbound liquidity,
	/// in which case no new channel is opened.
	///
	/// The configured LSPS2 fee limit is enforced when the payment arrives.
	///
	/// We will register the given payment hash and emit a [`PaymentClaimable`] event once
	/// the inbound payment arrives. The check that [`counterparty_skimmed_fee_msat`] is within the limits
	/// is performed *before* emitting the event.
	///
	/// **Note:** users *MUST* handle this event and claim the payment manually via
	/// [`claim_for_hash`] as soon as they have obtained access to the preimage of the given
	/// payment hash. If they're unable to obtain the preimage, they *MUST* immediately fail the payment via
	/// [`fail_for_hash`].
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	/// [`PaymentClaimable`]: crate::Event::PaymentClaimable
	/// [`claim_for_hash`]: Self::claim_for_hash
	/// [`fail_for_hash`]: Self::fail_for_hash
	/// [`counterparty_skimmed_fee_msat`]: crate::payment::PaymentKind::Bolt11::counterparty_skimmed_fee_msat
	pub fn receive_variable_amount_via_jit_channel_for_hash(
		&self, description: &Bolt11InvoiceDescription, expiry_secs: u32, payment_hash: PaymentHash,
	) -> Result<Bolt11Invoice, Error> {
		let description = maybe_try_convert_enum(description)?;
		let invoice = self.receive_via_jit_channel_inner(
			None,
			&description,
			expiry_secs,
			Some(payment_hash),
		)?;
		Ok(maybe_wrap(invoice))
	}

	/// Sends payment probes over all paths of a route that would be used to pay the given invoice.
	///
	/// This may be used to send "pre-flight" probes, i.e., to train our scorer before conducting
	/// the actual payment. Note this is only useful if there likely is sufficient time for the
	/// probe to settle before sending out the actual payment, e.g., when waiting for user
	/// confirmation in a wallet UI.
	///
	/// Otherwise, there is a chance the probe could take up some liquidity needed to complete the
	/// actual payment. Users should therefore be cautious and might avoid sending probes if
	/// liquidity is scarce and/or they don't expect the probe to return before they send the
	/// payment. To mitigate this issue, channels with available liquidity less than the required
	/// amount times [`Config::probing_liquidity_limit_multiplier`] won't be used to send
	/// pre-flight probes.
	///
	/// If `route_parameters` are provided they will override the default as well as the
	/// node-wide parameters configured via [`Config::route_parameters`] on a per-field basis.
	pub fn send_probes(
		&self, invoice: &Bolt11Invoice, route_parameters: Option<RouteParametersConfig>,
	) -> Result<(), Error> {
		if !*self.is_running.read().expect("lock") {
			return Err(Error::NotRunning);
		}

		let invoice = maybe_deref(invoice);
		let payment_params = PaymentParameters::from_bolt11_invoice(invoice);

		let amount_msat = invoice.amount_milli_satoshis().ok_or_else(|| {
			log_error!(self.logger, "Failed to send probes due to the given invoice being \"zero-amount\". Please use send_probes_using_amount instead.");
			Error::InvalidInvoice
		})?;

		let mut route_params =
			RouteParameters::from_payment_params_and_value(payment_params, amount_msat);

		if let Some(RouteParametersConfig {
			max_total_routing_fee_msat,
			max_total_cltv_expiry_delta,
			max_path_count,
			max_channel_saturation_power_of_half,
		}) = route_parameters.as_ref().or(self.config.route_parameters.as_ref())
		{
			route_params.max_total_routing_fee_msat = *max_total_routing_fee_msat;
			route_params.payment_params.max_total_cltv_expiry_delta = *max_total_cltv_expiry_delta;
			route_params.payment_params.max_path_count = *max_path_count;
			route_params.payment_params.max_channel_saturation_power_of_half =
				*max_channel_saturation_power_of_half;
		}

		let liquidity_limit_multiplier = Some(self.config.probing_liquidity_limit_multiplier);

		self.channel_manager
			.send_preflight_probes(route_params, liquidity_limit_multiplier)
			.map_err(|e| {
				log_error!(self.logger, "Failed to send payment probes: {:?}", e);
				Error::ProbeSendingFailed
			})?;

		Ok(())
	}

	/// Sends payment probes over all paths of a route that would be used to pay the given
	/// zero-value invoice using the given amount.
	///
	/// This can be used to send pre-flight probes for a so-called "zero-amount" invoice, i.e., an
	/// invoice that leaves the amount paid to be determined by the user.
	///
	/// If `route_parameters` are provided they will override the default as well as the
	/// node-wide parameters configured via [`Config::route_parameters`] on a per-field basis.
	///
	/// See [`Self::send_probes`] for more information.
	pub fn send_probes_using_amount(
		&self, invoice: &Bolt11Invoice, amount_msat: u64,
		route_parameters: Option<RouteParametersConfig>,
	) -> Result<(), Error> {
		if !*self.is_running.read().expect("lock") {
			return Err(Error::NotRunning);
		}

		let invoice = maybe_deref(invoice);
		let payment_params = PaymentParameters::from_bolt11_invoice(invoice);

		if let Some(invoice_amount_msat) = invoice.amount_milli_satoshis() {
			if amount_msat < invoice_amount_msat {
				log_error!(
					self.logger,
					"Failed to send probes as the given amount needs to be at least the invoice amount: required {}msat, gave {}msat.",
					invoice_amount_msat,
					amount_msat
				);
				return Err(Error::InvalidAmount);
			}
		}

		let mut route_params =
			RouteParameters::from_payment_params_and_value(payment_params, amount_msat);

		if let Some(RouteParametersConfig {
			max_total_routing_fee_msat,
			max_total_cltv_expiry_delta,
			max_path_count,
			max_channel_saturation_power_of_half,
		}) = route_parameters.as_ref().or(self.config.route_parameters.as_ref())
		{
			route_params.max_total_routing_fee_msat = *max_total_routing_fee_msat;
			route_params.payment_params.max_total_cltv_expiry_delta = *max_total_cltv_expiry_delta;
			route_params.payment_params.max_path_count = *max_path_count;
			route_params.payment_params.max_channel_saturation_power_of_half =
				*max_channel_saturation_power_of_half;
		}

		let liquidity_limit_multiplier = Some(self.config.probing_liquidity_limit_multiplier);

		self.channel_manager
			.send_preflight_probes(route_params, liquidity_limit_multiplier)
			.map_err(|e| {
				log_error!(self.logger, "Failed to send payment probes: {:?}", e);
				Error::ProbeSendingFailed
			})?;

		Ok(())
	}
}
