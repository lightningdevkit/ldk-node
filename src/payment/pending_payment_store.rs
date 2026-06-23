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
use crate::payment::{PaymentDetails, PaymentKind};

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

		if let PaymentKind::Onchain { txid, .. } = &self.details.kind {
			let conflicts_len = self.conflicting_txids.len();
			self.conflicting_txids.retain(|conflicting_txid| conflicting_txid != txid);
			updated |= self.conflicting_txids.len() != conflicts_len;
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
		Self { id: value.id(), payment_update: Some(value.details.to_update()), conflicting_txids }
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;

	use super::*;
	use crate::payment::{ConfirmationStatus, PaymentDirection, PaymentKind, PaymentStatus};

	fn test_txid(byte: u8) -> Txid {
		Txid::from_byte_array([byte; 32])
	}

	fn pending_onchain_payment(payment_id: PaymentId, txid: Txid) -> PaymentDetails {
		PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain { txid, status: ConfirmationStatus::Unconfirmed, tx_type: None },
			Some(1_000),
			Some(100),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	#[test]
	fn pending_onchain_conflicts_exclude_current_txid_after_txid_rotation() {
		let original_txid = test_txid(1);
		let replacement_txid = test_txid(2);
		let payment_id = PaymentId(original_txid.to_byte_array());

		let mut pending_payment = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, replacement_txid),
			vec![original_txid],
		);
		let update = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, original_txid),
			Vec::new(),
		)
		.to_update();

		assert!(pending_payment.update(update));
		assert_eq!(
			pending_payment.conflicting_txids,
			Vec::<Txid>::new(),
			"current txid must not remain in its own conflict list"
		);
	}
}
