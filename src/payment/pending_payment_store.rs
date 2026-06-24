// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::Txid;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::PaymentId;

use crate::data_store::{StorableObject, StorableObjectUpdate};
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::PaymentDetails;

/// Represents a pending payment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPaymentDetails {
	/// The full payment details
	pub details: PaymentDetails,
	/// Transaction IDs that have replaced or conflict with this payment.
	pub conflicting_txids: Vec<Txid>,
	/// The timestamp after which this pending payment can be pruned.
	pub expires_at: Option<u64>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(details: PaymentDetails, conflicting_txids: Vec<Txid>) -> Self {
		Self::new_with_expiry(details, conflicting_txids, None)
	}

	pub(crate) fn new_with_expiry(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, expires_at: Option<u64>,
	) -> Self {
		Self { details, conflicting_txids, expires_at }
	}

	/// Convert to finalized payment for the main payment store
	pub fn into_payment_details(self) -> PaymentDetails {
		self.details
	}

	pub(crate) fn has_expired(&self, now: u64) -> bool {
		self.expires_at.map_or(false, |expires_at| expires_at <= now)
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
	(4, expires_at, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub expires_at: Option<Option<u64>>,
}

impl StorableObject for PendingPaymentDetails {
	type Id = PaymentId;
	type Update = PendingPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.details.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		let mut updated = false;

		// Update the underlying payment details if present
		if let Some(payment_update) = update.payment_update {
			updated |= self.details.update(payment_update);
		}

		if let Some(new_conflicting_txids) = update.conflicting_txids {
			if self.conflicting_txids != new_conflicting_txids {
				self.conflicting_txids = new_conflicting_txids;
				updated = true;
			}
		}

		if let Some(new_expires_at) = update.expires_at {
			if self.expires_at != new_expires_at {
				self.expires_at = new_expires_at;
				updated = true;
			}
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

impl StorableObjectUpdate<PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn id(&self) -> <PendingPaymentDetails as StorableObject>::Id {
		self.id
	}
}

impl From<&PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn from(value: &PendingPaymentDetails) -> Self {
		let conflicting_txids = if value.conflicting_txids.is_empty() {
			None
		} else {
			Some(value.conflicting_txids.clone())
		};
		Self {
			id: value.id(),
			payment_update: Some(value.details.to_update()),
			conflicting_txids,
			expires_at: Some(value.expires_at),
		}
	}
}
