// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lightning::impl_writeable_tlv_based;
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::offer::Offer as LdkOffer;
use lightning::offers::refund::Refund as LdkRefund;
use lightning_invoice::Bolt11Invoice as LdkBolt11Invoice;

use crate::data_store::{DataStore, StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::io::{
	PAYMENT_METADATA_PERSISTENCE_PRIMARY_NAMESPACE,
	PAYMENT_METADATA_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::payment::store::LSPFeeLimits;
use crate::types::DynStore;
use crate::Error;

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

/// A store for payment metadata, backed by a [`DataStore`].
///
/// Maintains a reverse index from [`PaymentId`] to [`MetadataId`] for efficient lookups.
pub(crate) struct PaymentMetadataStore {
	inner: DataStore<PaymentMetadataEntry, Arc<Logger>>,
	reverse_index: Mutex<HashMap<PaymentId, HashSet<MetadataId>>>,
}

impl PaymentMetadataStore {
	pub(crate) fn new(
		entries: Vec<PaymentMetadataEntry>, kv_store: Arc<DynStore>, logger: Arc<Logger>,
	) -> Self {
		let mut reverse_index: HashMap<PaymentId, HashSet<MetadataId>> = HashMap::new();
		for entry in &entries {
			for payment_id in &entry.payment_ids {
				reverse_index.entry(*payment_id).or_default().insert(entry.id);
			}
		}

		let inner = DataStore::new(
			entries,
			PAYMENT_METADATA_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
			PAYMENT_METADATA_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
			kv_store,
			logger,
		);

		Self { inner, reverse_index: Mutex::new(reverse_index) }
	}

	/// Insert a new metadata entry and update the reverse index.
	pub(crate) fn insert(&self, entry: PaymentMetadataEntry) -> Result<MetadataId, Error> {
		let id = entry.id;
		let payment_ids = entry.payment_ids.clone();

		self.inner.insert(entry)?;

		let mut locked_index = self.reverse_index.lock().unwrap();
		for payment_id in payment_ids {
			locked_index.entry(payment_id).or_default().insert(id);
		}

		Ok(id)
	}

	/// Associate an additional [`PaymentId`] with an existing metadata entry.
	pub(crate) fn add_payment_id(
		&self, metadata_id: MetadataId, payment_id: PaymentId,
	) -> Result<(), Error> {
		if let Some(mut entry) = self.inner.get(&metadata_id) {
			if !entry.payment_ids.contains(&payment_id) {
				entry.payment_ids.push(payment_id);
				let update = PaymentMetadataEntryUpdate {
					id: metadata_id,
					payment_ids: Some(entry.payment_ids),
				};
				self.inner.update(update)?;
				self.reverse_index
					.lock()
					.unwrap()
					.entry(payment_id)
					.or_default()
					.insert(metadata_id);
			}
			Ok(())
		} else {
			Err(Error::PersistenceFailed)
		}
	}

	/// Get a metadata entry by its ID.
	pub(crate) fn get(&self, metadata_id: &MetadataId) -> Option<PaymentMetadataEntry> {
		self.inner.get(metadata_id)
	}

	/// Get all metadata entries associated with a given payment ID.
	pub(crate) fn get_for_payment_id(&self, payment_id: &PaymentId) -> Vec<PaymentMetadataEntry> {
		let locked_index = self.reverse_index.lock().unwrap();
		if let Some(metadata_ids) = locked_index.get(payment_id) {
			metadata_ids.iter().filter_map(|mid| self.inner.get(mid)).collect()
		} else {
			Vec::new()
		}
	}

	/// Convenience method to get the [`LSPFeeLimits`] for a given payment ID, if any.
	pub(crate) fn get_lsp_fee_limits_for_payment_id(
		&self, payment_id: &PaymentId,
	) -> Option<LSPFeeLimits> {
		let entries = self.get_for_payment_id(payment_id);
		for entry in entries {
			if let PaymentMetadataKind::LSPFeeLimits { limits } = entry.kind {
				return Some(limits);
			}
		}
		None
	}

	/// Remove a metadata entry and clean up the reverse index.
	pub(crate) fn remove(&self, metadata_id: &MetadataId) -> Result<(), Error> {
		if let Some(entry) = self.inner.get(metadata_id) {
			let mut locked_index = self.reverse_index.lock().unwrap();
			for payment_id in &entry.payment_ids {
				if let Some(set) = locked_index.get_mut(payment_id) {
					set.remove(metadata_id);
					if set.is_empty() {
						locked_index.remove(payment_id);
					}
				}
			}
		}
		self.inner.remove(metadata_id)
	}

	/// Remove a [`PaymentId`] from all associated metadata entries.
	///
	/// This should be called when a payment store entry is removed to keep the reverse index
	/// consistent. If a metadata entry's `payment_ids` becomes empty after removal, it is
	/// **not** automatically deleted (the metadata may still be useful).
	pub(crate) fn remove_payment_id(&self, payment_id: &PaymentId) -> Result<(), Error> {
		let metadata_ids = {
			let mut locked_index = self.reverse_index.lock().unwrap();
			match locked_index.remove(payment_id) {
				Some(ids) => ids,
				None => return Ok(()),
			}
		};

		for metadata_id in metadata_ids {
			if let Some(mut entry) = self.inner.get(&metadata_id) {
				entry.payment_ids.retain(|id| id != payment_id);
				let update = PaymentMetadataEntryUpdate {
					id: metadata_id,
					payment_ids: Some(entry.payment_ids),
				};
				self.inner.update(update)?;
			}
		}

		Ok(())
	}
}
