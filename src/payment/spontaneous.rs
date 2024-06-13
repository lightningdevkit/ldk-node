//! Holds a payment handler allowing to send spontaneous ("keysend") payments.

use crate::config::{Config, LDK_PAYMENT_RETRY_TIMEOUT};
use crate::error::Error;
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::payment::store::{
	PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus, PaymentStore,
};
use crate::types::{ChannelManager, KeysManager, TlvEntry};

use lightning::ln::channelmanager::{PaymentId, RecipientOnionFields, Retry, RetryableSendFailure};
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::routing::router::{PaymentParameters, RouteParameters};
use lightning::sign::EntropySource;

use bitcoin::secp256k1::PublicKey;

use std::sync::{Arc, RwLock};

/// A payment handler allowing to send spontaneous ("keysend") payments.
///
/// Should be retrieved by calling [`Node::spontaneous_payment`].
///
/// [`Node::spontaneous_payment`]: crate::Node::spontaneous_payment
pub struct SpontaneousPayment {
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>,
	config: Arc<Config>,
	logger: Arc<FilesystemLogger>,
}

impl SpontaneousPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
		channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>, config: Arc<Config>,
		logger: Arc<FilesystemLogger>,
	) -> Self {
		Self { runtime, channel_manager, keys_manager, payment_store, config, logger }
	}

	/// Send a spontaneous, aka. "keysend", payment
	pub fn send(
		&self, amount_msat: u64, node_id: PublicKey, custom_tlvs: Vec<TlvEntry>,
	) -> Result<PaymentId, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let payment_preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
		let payment_hash = PaymentHash::from(payment_preimage);
		let payment_id = PaymentId(payment_hash.0);

		if let Some(payment) = self.payment_store.get(&payment_id) {
			if payment.status == PaymentStatus::Pending
				|| payment.status == PaymentStatus::Succeeded
			{
				log_error!(self.logger, "Payment error: must not send duplicate payments.");
				return Err(Error::DuplicatePayment);
			}
		}

		let route_params = RouteParameters::from_payment_params_and_value(
			PaymentParameters::from_node_id(node_id, self.config.default_cltv_expiry_delta),
			amount_msat,
		);
		let recipient_fields = RecipientOnionFields::spontaneous_empty()
			.with_custom_tlvs(custom_tlvs.into_iter().map(|tlv| (tlv.r#type, tlv.value)).collect())
			.map_err(|_| {
				log_error!(self.logger, "Payment error: invalid custom TLVs.");
				Error::InvalidCustomTlv
			})?;

		match self.channel_manager.send_spontaneous_payment_with_retry(
			Some(payment_preimage),
			recipient_fields,
			PaymentId(payment_hash.0),
			route_params,
			Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
		) {
			Ok(_hash) => {
				log_info!(self.logger, "Initiated sending {}msat to {}.", amount_msat, node_id);

				let payment = PaymentDetails {
					id: payment_id,
					kind: PaymentKind::Spontaneous {
						hash: payment_hash,
						preimage: Some(payment_preimage),
						// TODO: save custom TLVs here from recipient_fields.custom_tlvs()
					},
					status: PaymentStatus::Pending,
					direction: PaymentDirection::Outbound,
					amount_msat: Some(amount_msat),
					last_update: 0,
					fee_msat: None,
				};
				self.payment_store.insert(payment)?;

				Ok(payment_id)
			},
			Err(e) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);

				match e {
					RetryableSendFailure::DuplicatePayment => Err(Error::DuplicatePayment),
					_ => {
						let payment = PaymentDetails {
							id: payment_id,
							kind: PaymentKind::Spontaneous {
								hash: payment_hash,
								preimage: Some(payment_preimage),
							},

							status: PaymentStatus::Failed,
							direction: PaymentDirection::Outbound,
							amount_msat: Some(amount_msat),
							last_update: 0,
							fee_msat: None,
						};

						self.payment_store.insert(payment)?;
						Err(Error::PaymentSendingFailed)
					},
				}
			},
		}
	}

	/// Sends payment probes over all paths of a route that would be used to pay the given
	/// amount to the given `node_id`.
	///
	/// See [`Bolt11Payment::send_probes`] for more information.
	///
	/// [`Bolt11Payment::send_probes`]: crate::payment::Bolt11Payment
	pub fn send_probes(&self, amount_msat: u64, node_id: PublicKey) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let liquidity_limit_multiplier = Some(self.config.probing_liquidity_limit_multiplier);
		let cltv_expiry_delta = self.config.default_cltv_expiry_delta;

		self.channel_manager
			.send_spontaneous_preflight_probes(
				node_id,
				amount_msat,
				cltv_expiry_delta,
				liquidity_limit_multiplier,
			)
			.map_err(|e| {
				log_error!(self.logger, "Failed to send payment probes: {:?}", e);
				Error::ProbeSendingFailed
			})?;

		Ok(())
	}
}
