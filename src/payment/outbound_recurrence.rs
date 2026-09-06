// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::ln::channelmanager::PaymentId;
use lightning::ln::outbound_payment::Retry;
pub use lightning::offers::invoice_request::RecurrenceId;
use lightning::routing::router::RouteParametersConfig;
use lightning::{impl_ser_tlv_based, impl_ser_tlv_based_enum};
use lightning_types::string::UntrustedString;

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;

impl StorableObjectId for RecurrenceId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		hex_utils::to_vec(s)?.try_into().ok().map(Self)
	}
}

/// The lifecycle status of an outbound recurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundRecurrenceStatus {
	/// Outbound recurrence can accept its next payment attempt.
	Active,
	/// Current payment window closed before a payment succeeded.
	Missed,
	/// Outbound recurrence will not accept further payments.
	Cancelled,
	/// Outbound recurrence reached its configured payment limit.
	Completed,
}

impl_ser_tlv_based_enum!(OutboundRecurrenceStatus,
	(0, Active) => {},
	(2, Missed) => {},
	(4, Cancelled) => {},
	(6, Completed) => {},
);

/// The payment attempt state of an outbound recurrence.
///
/// Tracks an in-progress attempt or retains the ID of a failed attempt. This is
/// separate from [`OutboundRecurrenceStatus`]: a failed attempt does not by itself
/// end the outbound recurrence or advance its paid period count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundRecurrencePaymentState {
	/// No attempt is in progress and no failed attempt is retained.
	///
	/// Used before the first attempt and after a successful payment, whose ID is
	/// recorded in [`OutboundRecurrenceDetails::last_successful_payment_id`].
	NoActivePayment,
	/// An attempt with this payment ID is in progress.
	///
	/// Another attempt must not be started while this attempt remains active.
	Active(PaymentId),
	/// The attempt with this payment ID failed and is no longer in progress.
	///
	/// Retains the failed ID until another attempt replaces it. A new attempt may
	/// be started if the outbound recurrence status and payment window permit it.
	Failed(PaymentId),
}

impl_ser_tlv_based_enum!(OutboundRecurrencePaymentState,
	(0, NoActivePayment) => {},
	{2, Active} => (),
	{4, Failed} => (),
);

/// Persisted state for an outbound recurrence, where this node is the payer.
///
/// Stored using [`RecurrenceId`] as the key. Lookup and list APIs return owned copies;
/// changing a returned value does not update the stored recurrence.
#[derive(Clone, Debug)]
pub struct OutboundRecurrenceDetails {
	/// Recurrence ID.
	pub id: RecurrenceId,

	/// Current lifecycle status of the outbound recurrence.
	pub status: OutboundRecurrenceStatus,

	/// State of the outbound recurrence's payment attempt.
	///
	/// Retains the payment ID while an attempt is active or after it fails.
	/// Successful attempts are recorded in [`Self::last_successful_payment_id`].
	pub payment_state: OutboundRecurrencePaymentState,

	/// Serialized original offer defining the outbound recurrence terms.
	///
	/// Retained to ensure that subsequent payments use the same offer.
	pub original_offer: Vec<u8>,

	/// Amount in millisatoshis to use for each payment by default.
	///
	/// May be increased by the user before the next period begins. If the original
	/// offer specifies an amount, this must be at least that amount multiplied by
	/// [`Self::quantity`], when a quantity is set.
	pub amount_msat: u64,

	/// Quantity to include in each invoice request, if any.
	///
	/// May be changed by the user before the next period begins.
	pub quantity: Option<u64>,

	/// Note to include in each invoice request.
	pub payer_note: Option<UntrustedString>,

	/// UNIX timestamp anchoring period zero of the outbound recurrence.
	///
	/// If the offer does not specify a basetime, period zero is anchored to
	/// the creation time of the first successfully paid invoice. In that case,
	/// this is `None` until the first payment succeeds and `Some` thereafter.
	pub basetime: Option<u64>,

	/// First outbound recurrence period to pay, if specified.
	pub initial_start: Option<u32>,

	/// Number of outbound recurrence periods successfully paid.
	pub paid_count: u32,

	/// Opaque state provided by the payee for the next invoice request.
	pub opaque_state: Option<Vec<u8>>,

	/// Retry strategy to use when attempting each payment.
	pub retry_policy: Retry,

	/// Routing configuration for this outbound recurrence overriding the node-wide default.
	pub routing_override: Option<RouteParametersConfig>,

	/// Payment ID of the most recently completed payment.
	pub last_successful_payment_id: Option<PaymentId>,

	/// Whether future outbound recurrence periods should be paid automatically.
	pub pay_next_automatically: bool,
}

impl Default for OutboundRecurrenceDetails {
	fn default() -> Self {
		Self {
			id: RecurrenceId([0; 32]),
			status: OutboundRecurrenceStatus::Active,
			payment_state: OutboundRecurrencePaymentState::NoActivePayment,
			original_offer: Vec::new(),
			amount_msat: 0,
			quantity: None,
			payer_note: None,
			basetime: None,
			initial_start: None,
			paid_count: 0,
			opaque_state: None,
			retry_policy: Retry::Attempts(0),
			routing_override: None,
			last_successful_payment_id: None,
			pay_next_automatically: false,
		}
	}
}

impl_ser_tlv_based!(OutboundRecurrenceDetails, {
	(0, id, required),
	(2, status, required),
	(4, payment_state, required),
	(6, original_offer, required),
	(8, amount_msat, required),
	(10, quantity, option),
	(12, payer_note, option),
	(14, basetime, option),
	(16, initial_start, option),
	(18, paid_count, required),
	(20, opaque_state, option),
	(22, retry_policy, required),
	(24, routing_override, option),
	(26, last_successful_payment_id, option),
	(28, pay_next_automatically, required),
});

impl StorableObject for OutboundRecurrenceDetails {
	type Id = RecurrenceId;
	type Update = OutboundRecurrenceDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		if self.id() != update.details.id() {
			return false;
		}
		*self = update.details;
		true
	}

	fn to_update(&self) -> Self::Update {
		OutboundRecurrenceDetailsUpdate { details: self.clone() }
	}
}

/// Update instructions for [`OutboundRecurrenceDetails`].
#[derive(Clone, Debug)]
pub(crate) struct OutboundRecurrenceDetailsUpdate {
	pub details: OutboundRecurrenceDetails,
}

impl StorableObjectUpdate<OutboundRecurrenceDetails> for OutboundRecurrenceDetailsUpdate {
	fn id(&self) -> RecurrenceId {
		self.details.id
	}
}
