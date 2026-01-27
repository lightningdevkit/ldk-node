// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::Txid;
use lightning::{impl_writeable_tlv_based, ln::channelmanager::PaymentId};

use crate::{
	data_store::{StorableObject, StorableObjectUpdate},
	payment::{store::PaymentDetailsUpdate, PaymentDetails},
};

/// Represents a pending payment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPaymentDetails {
	/// The full payment details
	pub details: PaymentDetails,
	/// Transaction IDs that have replaced or conflict with this payment.
	pub conflicting_txids: Vec<Txid>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(details: PaymentDetails, conflicting_txids: Vec<Txid>) -> Self {
		Self { details, conflicting_txids }
	}

	/// Convert to finalized payment for the main payment store
	pub fn into_payment_details(self) -> PaymentDetails {
		self.details
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
}

impl StorableObject for PendingPaymentDetails {
	type Id = PaymentId;
	type Update = PendingPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.details.id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		let mut updated = false;

		// Update the underlying payment details if present
		if let Some(payment_update) = &update.payment_update {
			updated |= self.details.update(payment_update);
		}

		if let Some(new_conflicting_txids) = &update.conflicting_txids {
			if &self.conflicting_txids != new_conflicting_txids {
				self.conflicting_txids = new_conflicting_txids.clone();
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
		Self {
			id: value.id(),
			payment_update: Some(value.details.to_update()),
			conflicting_txids: Some(value.conflicting_txids.clone()),
		}
	}
}
