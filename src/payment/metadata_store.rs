// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::impl_writeable_tlv_based;
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::offer::Offer as LdkOffer;
use lightning::offers::refund::Refund as LdkRefund;
use lightning_invoice::Bolt11Invoice as LdkBolt11Invoice;

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::payment::store::LSPFeeLimits;

/// An opaque identifier for a payment metadata entry.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct MetadataId {
	pub id: [u8; 32],
}

impl StorableObjectId for MetadataId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.id)
	}
}

impl_writeable_tlv_based!(MetadataId, { (0, id, required) });

/// The kind of metadata stored in a [`PaymentMetadataEntry`].
#[derive(Clone, Debug)]
pub(crate) enum PaymentMetadataKind {
	/// A BOLT 11 invoice.
	Bolt11Invoice { invoice: LdkBolt11Invoice },
	/// A BOLT 12 offer.
	Bolt12Offer { offer: LdkOffer },
	/// A BOLT 12 refund.
	Bolt12Refund { refund: LdkRefund },
	/// LSP fee limits for a JIT channel payment.
	LSPFeeLimits { limits: LSPFeeLimits },
}

impl_writeable_tlv_based_enum!(PaymentMetadataKind,
	(0, Bolt11Invoice) => {
		(0, invoice, required),
	},
	(2, Bolt12Offer) => {
		(0, offer, required),
	},
	(4, Bolt12Refund) => {
		(0, refund, required),
	},
	(6, LSPFeeLimits) => {
		(0, limits, required),
	}
);

/// A metadata entry associating a [`PaymentMetadataKind`] with one or more payments.
#[derive(Clone, Debug)]
pub(crate) struct PaymentMetadataEntry {
	/// The unique identifier for this metadata entry.
	pub id: MetadataId,
	/// The kind of metadata.
	pub kind: PaymentMetadataKind,
	/// The payment IDs associated with this metadata.
	pub payment_ids: Vec<PaymentId>,
}

impl_writeable_tlv_based!(PaymentMetadataEntry, {
	(0, id, required),
	(2, kind, required),
	(4, payment_ids, optional_vec),
});

/// An update to a [`PaymentMetadataEntry`].
#[derive(Clone, Debug)]
pub(crate) struct PaymentMetadataEntryUpdate {
	pub id: MetadataId,
	pub payment_ids: Option<Vec<PaymentId>>,
}

impl StorableObject for PaymentMetadataEntry {
	type Id = MetadataId;
	type Update = PaymentMetadataEntryUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		let mut updated = false;

		if let Some(new_payment_ids) = update.payment_ids {
			if self.payment_ids != new_payment_ids {
				self.payment_ids = new_payment_ids;
				updated = true;
			}
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		PaymentMetadataEntryUpdate { id: self.id, payment_ids: Some(self.payment_ids.clone()) }
	}
}

impl StorableObjectUpdate<PaymentMetadataEntry> for PaymentMetadataEntryUpdate {
	fn id(&self) -> <PaymentMetadataEntry as StorableObject>::Id {
		self.id
	}
}
