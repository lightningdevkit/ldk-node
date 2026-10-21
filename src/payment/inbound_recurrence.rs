// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::hashes::{sha256, Hash as BitcoinHash, HashEngine};
use bitcoin::secp256k1::PublicKey;
use lightning::blinded_path::payment::Bolt12OfferContext;
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::invoice_request::InvoiceRequestRecurrence;
use lightning::offers::offer::{OfferId, Recurrence};
use lightning::{impl_ser_tlv_based, impl_ser_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;

/// A stable identifier derived from an inbound relationship's offer and payer signing public key.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct InboundRecurrenceId(pub [u8; 32]);

impl InboundRecurrenceId {
	/// Derives the relationship id by hashing both fixed-width identifiers in order.
	pub(crate) fn new(offer_id: OfferId, payer_signing_pubkey: PublicKey) -> Self {
		// The 32-byte hash encodes to 64 hex characters, within the KVStore key length limit.
		let mut engine = sha256::Hash::engine();
		engine.input(&offer_id.0);
		engine.input(&payer_signing_pubkey.serialize());
		Self(sha256::Hash::from_engine(engine).to_byte_array())
	}
}

impl StorableObjectId for InboundRecurrenceId {
	/// Encodes the relationship id as a KVStore key.
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}

	/// Recovers the relationship id from a hex key containing exactly 32 bytes.
	fn decode_from_hex_str(s: &str) -> Option<Self> {
		hex_utils::to_vec(s)?.try_into().ok().map(Self)
	}
}

/// The lifecycle status of a recurring offer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecurrenceStatus {
	/// Recurrence can accept its next payment attempt.
	Active,
	/// Current payment window closed before a payment succeeded.
	Missed,
	/// Recurrence will not accept further payments.
	Cancelled,
	/// Recurrence reached its configured payment limit.
	Completed,
}

impl_ser_tlv_based_enum!(RecurrenceStatus,
	(0, Active) => {},
	(2, Missed) => {},
	(4, Cancelled) => {},
	(6, Completed) => {},
);

/// The payment attempt status of an inbound recurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundRecurrenceClaimStatus {
	/// No claim is in progress. Successful payments are recorded separately.
	Idle,
	/// This payment owns the current expected counter while its claim is pending.
	Pending(PaymentId),
	/// A confirmed failed attempt for the current expected counter.
	///
	/// Retains the payment ID until another attempt replaces it.
	///
	/// TODO: Set this state when the whole payment is confirmed to have failed. Failure handling
	/// is deferred because one failed HTLC does not prove that the payment cannot still succeed.
	/// Until that outcome is known, keep Pending so another payment cannot take the reservation.
	Failed(PaymentId),
}

impl_ser_tlv_based_enum!(InboundRecurrenceClaimStatus,
	(0, Idle) => {},
	{2, Pending} => (),
	{4, Failed} => (),
);

/// Details of recurring payments received for an offer and payer signing public key.
///
/// Individual payment records are available through [`crate::Node::payment`].
///
/// Returned by [`crate::Node::inbound_recurrence`] and [`crate::Node::list_inbound_recurrences`]
/// as a copy of the saved state. Changing this copy does not change the node's stored recurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundRecurrenceDetails {
	/// ID of the offer defining this recurrence.
	pub offer_id: OfferId,

	/// Payer signing public key identifying the payer's relationship with this offer.
	pub payer_signing_pubkey: PublicKey,

	/// Recurrence terms defining the schedule, payment window, and any period limit.
	pub recurrence: Recurrence,

	/// Current lifecycle status of the recurrence.
	pub status: RecurrenceStatus,

	/// UNIX timestamp anchoring period zero of the recurrence, once established.
	///
	/// Together with the recurrence terms, determines when payment windows open and close.
	pub basetime: Option<u64>,

	/// First recurrence period selected by the payer, if specified.
	pub initial_start: Option<u32>,

	/// Status of the attempt for the next expected counter.
	///
	/// Retains the payment ID while a claim is pending or after a confirmed failure.
	/// Successful payments are recorded in [`Self::completed_payments`].
	pub claim_status: InboundRecurrenceClaimStatus,

	/// Successful payment IDs in counter order, starting at counter zero.
	///
	/// The index identifies the counter, and the length is the next counter to reserve. Append
	/// only after PaymentClaimed confirms success. Keep earlier entries to recognize replays
	/// without changing newer progress or reservations.
	pub completed_payments: Vec<PaymentId>,
}

impl_ser_tlv_based!(InboundRecurrenceDetails, {
	(0, offer_id, required),
	(2, payer_signing_pubkey, required),
	(4, recurrence, required),
	(6, status, required),
	(8, basetime, option),
	(10, initial_start, option),
	(14, claim_status, (default_value, InboundRecurrenceClaimStatus::Idle)),
	(16, completed_payments, required_vec),
});

impl InboundRecurrenceDetails {
	/// Reads the recurring payment's terms and schedule from the invoice's payment data.
	///
	/// Creates a starting record that expects payment number zero, and returns this payment's
	/// number separately. Nothing is saved and the expected payment number is not advanced here.
	/// Returns None if required data is missing, the request cancels recurring payments, or the
	/// requested period does not fit the offer's schedule or limit.
	pub(crate) fn from_payment_context(context: &Bolt12OfferContext) -> Option<(Self, u32)> {
		let recurrence = context.offer_recurrence?;
		let basetime = context.invoice_recurrence_basetime?;
		let (counter, initial_start, cancel) =
			match context.invoice_request.invoice_request_recurrence.as_ref()? {
				InvoiceRequestRecurrence::WithOfferBasetime(request) => {
					(request.counter(), Some(request.start()), request.cancel())
				},
				InvoiceRequestRecurrence::WithoutOfferBasetime(request) => {
					(request.counter(), None, request.cancel())
				},
			};
		let period = recurrence.period_index(counter, initial_start).ok()?;
		if cancel || recurrence.recurrence_limit.is_some_and(|limit| period > limit.0) {
			return None;
		}
		Some((
			Self {
				offer_id: context.offer_id,
				payer_signing_pubkey: context.invoice_request.payer_signing_pubkey,
				recurrence,
				status: RecurrenceStatus::Active,
				basetime: Some(basetime),
				initial_start,
				claim_status: InboundRecurrenceClaimStatus::Idle,
				completed_payments: Vec::new(),
			},
			counter,
		))
	}
}

impl StorableObject for InboundRecurrenceDetails {
	type Id = InboundRecurrenceId;
	// Updates are complete relationship snapshots. Event handling will own state transitions.
	type Update = Self;

	/// Derives the store key from this relationship's offer and payer signing public key.
	fn id(&self) -> Self::Id {
		InboundRecurrenceId::new(self.offer_id, self.payer_signing_pubkey)
	}

	/// Replaces a relationship snapshot only when its state changed.
	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(StorableObject::id(self), StorableObject::id(&update));
		if *self == update {
			return false;
		}
		*self = update;
		true
	}

	/// Copies the complete relationship state for the generic store's update path.
	fn to_update(&self) -> Self::Update {
		self.clone()
	}
}

impl StorableObjectUpdate<InboundRecurrenceDetails> for InboundRecurrenceDetails {
	/// Routes a snapshot update to its existing relationship.
	fn id(&self) -> InboundRecurrenceId {
		StorableObject::id(self)
	}
}
