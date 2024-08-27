//! Holds a payment handler allowing to send spontaneous ("keysend") payments.

use crate::config::{Config, LDK_PAYMENT_RETRY_TIMEOUT};
use crate::error::Error;
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::payment::store::{
	PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus, PaymentStore,
};
use crate::payment::SendingParameters;
use crate::types::{ChannelManager, KeysManager};

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
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>,
	config: Arc<Config>,
	logger: Arc<FilesystemLogger>,
}

impl SpontaneousPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
		channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>, config: Arc<Config>,
		logger: Arc<FilesystemLogger>,
	) -> Self {
		Self { runtime, channel_manager, keys_manager, payment_store, config, logger }
	}

	/// Send a spontaneous aka. "keysend", payment.
	///
	/// If `sending_parameters` are provided they will override the default as well as the
	/// node-wide parameters configured via [`Config::sending_parameters`] on a per-field basis.
	pub fn send(
		&self, amount_msat: u64, node_id: PublicKey, sending_parameters: Option<SendingParameters>,
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

		let mut route_params = RouteParameters::from_payment_params_and_value(
			PaymentParameters::from_node_id(node_id, self.config.default_cltv_expiry_delta),
			amount_msat,
		);

		if let Some(user_set_params) = sending_parameters {
			if let Some(mut default_params) = self.config.sending_parameters.as_ref().cloned() {
				default_params.max_total_routing_fee_msat = user_set_params
					.max_total_routing_fee_msat
					.or(default_params.max_total_routing_fee_msat);
				default_params.max_total_cltv_expiry_delta = user_set_params
					.max_total_cltv_expiry_delta
					.or(default_params.max_total_cltv_expiry_delta);
				default_params.max_path_count =
					user_set_params.max_path_count.or(default_params.max_path_count);
				default_params.max_channel_saturation_power_of_half = user_set_params
					.max_channel_saturation_power_of_half
					.or(default_params.max_channel_saturation_power_of_half);

				route_params.max_total_routing_fee_msat = default_params.max_total_routing_fee_msat;
				route_params.payment_params.max_total_cltv_expiry_delta =
					default_params.max_total_cltv_expiry_delta.unwrap_or_default();
				route_params.payment_params.max_path_count =
					default_params.max_path_count.unwrap_or_default();
				route_params.payment_params.max_channel_saturation_power_of_half =
					default_params.max_channel_saturation_power_of_half.unwrap_or_default();
			}
		} else if let Some(default_params) = &self.config.sending_parameters {
			route_params.max_total_routing_fee_msat = default_params.max_total_routing_fee_msat;
			route_params.payment_params.max_total_cltv_expiry_delta =
				default_params.max_total_cltv_expiry_delta.unwrap_or_default();
			route_params.payment_params.max_path_count =
				default_params.max_path_count.unwrap_or_default();
			route_params.payment_params.max_channel_saturation_power_of_half =
				default_params.max_channel_saturation_power_of_half.unwrap_or_default();
		}

		let recipient_fields = RecipientOnionFields::spontaneous_empty();

		match self.channel_manager.send_spontaneous_payment_with_retry(
			Some(payment_preimage),
			recipient_fields,
			PaymentId(payment_hash.0),
			route_params,
			Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
		) {
			Ok(_hash) => {
				log_info!(self.logger, "Initiated sending {}msat to {}.", amount_msat, node_id);

				let kind = PaymentKind::Spontaneous {
					hash: payment_hash,
					preimage: Some(payment_preimage),
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
					RetryableSendFailure::DuplicatePayment => Err(Error::DuplicatePayment),
					_ => {
						let kind = PaymentKind::Spontaneous {
							hash: payment_hash,
							preimage: Some(payment_preimage),
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
