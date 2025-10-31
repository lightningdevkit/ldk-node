// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

// invoice_store.rs

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::offers::offer::OfferId;
use lightning::util::ser::{Readable, Writeable};
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based_enum, write_tlv_fields,
};
use lightning_types::payment::{PaymentHash, PaymentSecret};
use lightning_types::string::UntrustedString;
use rand::RngCore;

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::payment::store::LSPFeeLimits;

/// Represents an invoice or offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvoiceDetails {
	/// The unique identifier of this invoice/offer.
	pub id: InvoiceId,
	/// The kind of the invoice/offer.
	pub kind: InvoiceKind,
	/// The amount requested.
	///
	/// Will be `None` for variable-amount (zero-amount) invoices/offers.
	pub amount_msat: Option<u64>,
	/// The description of the invoice/offer.
	pub description: String,
	/// The timestamp, in seconds since start of the UNIX epoch, when this invoice/offer was created.
	pub created_at: u64,
	/// The expiry time in seconds from creation.
	pub expiry_secs: Option<u32>,
	/// The payment ID(s) associated with this invoice/offer.
	///
	/// For BOLT11 invoices and BOLT12 refunds: can have at most one payment (Option style).
	/// For BOLT12 offers: can have multiple payments (Vec style).
	///
	/// Will be empty/None at creation and populated when payments arrive.
	pub linked_payment_ids: Option<PaymentLinks>,
	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	pub latest_update_timestamp: u64,
}

impl InvoiceDetails {
	pub(crate) fn new(
		id: InvoiceId, kind: InvoiceKind, amount_msat: Option<u64>, description: String,
		expiry_secs: Option<u32>,
	) -> Self {
		let created_at = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();

		Self {
			id,
			kind,
			amount_msat,
			description,
			created_at,
			expiry_secs,
			linked_payment_ids: None,
			latest_update_timestamp: created_at,
		}
	}
}

impl Writeable for InvoiceDetails {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		write_tlv_fields!(writer, {
			(0, self.id, required),
			(2, self.kind, required),
			(4, self.amount_msat, required),
			(6, self.description, required),
			(8, self.created_at, required),
			(10, self.expiry_secs, required),
			(12, self.linked_payment_ids, option),
			(14, self.latest_update_timestamp, required)
		});
		Ok(())
	}
}

impl Readable for InvoiceDetails {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<InvoiceDetails, DecodeError> {
		let unix_time_secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();

		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, id, required),
			(2, kind, required),
			(4, amount_msat, required),
			(6, description, required),
			(8, created_at, required),
			(10, expiry_secs, required),
			(12, linked_payment_ids, option),  // Optional
			(14, latest_update_timestamp, (default_value, unix_time_secs))
		});

		let id: InvoiceId = id.0.ok_or(DecodeError::InvalidValue)?;
		let kind: InvoiceKind = kind.0.ok_or(DecodeError::InvalidValue)?;
		let amount_msat: Option<u64> = amount_msat.0.ok_or(DecodeError::InvalidValue)?;
		let description: String = description.0.ok_or(DecodeError::InvalidValue)?;
		let created_at: u64 = created_at.0.ok_or(DecodeError::InvalidValue)?;
		let expiry_secs: Option<u32> = expiry_secs.0.ok_or(DecodeError::InvalidValue)?;
		let linked_payment_ids: Option<PaymentLinks> = linked_payment_ids;
		let latest_update_timestamp: u64 =
			latest_update_timestamp.0.ok_or(DecodeError::InvalidValue)?;

		Ok(InvoiceDetails {
			id,
			kind,
			amount_msat,
			description,
			created_at,
			expiry_secs,
			linked_payment_ids,
			latest_update_timestamp,
		})
	}
}

impl StorableObjectId for InvoiceId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

impl StorableObject for InvoiceDetails {
	type Id = InvoiceId;
	type Update = InvoiceDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		debug_assert_eq!(
			self.id, update.id,
			"We should only ever override invoice data for the same invoice id"
		);

		let mut updated = false;

		if let Some(new_payment_id) = update.payment_id {
			// Initialize linked_payment_ids based on invoice kind if not already set
			if self.linked_payment_ids.is_none() {
				self.linked_payment_ids = Some(match &self.kind {
					InvoiceKind::Bolt12Offer { .. } => PaymentLinks::Multiple(vec![]),
					_ => PaymentLinks::Single(None),
				});
			}

			// Now add the payment ID
			match self.linked_payment_ids.as_mut().unwrap() {
				PaymentLinks::Single(ref mut opt) => {
					if opt.is_none() {
						*opt = Some(new_payment_id);
						updated = true;
					} else {
						debug_assert!(
							*opt == Some(new_payment_id),
							"Attempting to add different payment to single-payment invoice"
						);
					}
				},
				PaymentLinks::Multiple(ref mut vec) => {
					if !vec.contains(&new_payment_id) {
						vec.push(new_payment_id);
						updated = true;
					}
				},
			}
		}

		if updated {
			self.latest_update_timestamp = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or(Duration::from_secs(0))
				.as_secs();
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

/// Unique identifier for an invoice or offer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct InvoiceId(pub [u8; 32]);

impl InvoiceId {
	/// Generate a new random invoice ID.
	pub fn new() -> Self {
		let mut random_bytes = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut random_bytes);
		Self(random_bytes)
	}

	/// Create an invoice ID from a payment hash (for BOLT11).
	pub fn from_payment_hash(hash: PaymentHash) -> Self {
		Self(hash.0)
	}

	/// Create an invoice ID from an offer ID (for BOLT12).
	pub fn from_offer_id(offer_id: OfferId) -> Self {
		Self(offer_id.0)
	}
}

impl Writeable for InvoiceId {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		self.0.write(writer)
	}
}

impl Readable for InvoiceId {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		Ok(InvoiceId(Readable::read(reader)?))
	}
}

/// Represents payment ID links for invoices/offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentLinks {
	/// Single payment link (for BOLT11 invoices, BOLT12 refunds).
	Single(Option<PaymentId>),
	/// Multiple payment links (for BOLT12 offers).
	Multiple(Vec<PaymentId>),
}

impl Writeable for PaymentLinks {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		match self {
			PaymentLinks::Single(opt) => {
				0u8.write(writer)?;
				opt.write(writer)?;
			},
			PaymentLinks::Multiple(vec) => {
				1u8.write(writer)?;
				(vec.len() as u16).write(writer)?;
				for payment_id in vec {
					payment_id.write(writer)?;
				}
			},
		}
		Ok(())
	}
}

impl Readable for PaymentLinks {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let variant: u8 = Readable::read(reader)?;
		match variant {
			0 => {
				let opt: Option<PaymentId> = Readable::read(reader)?;
				Ok(PaymentLinks::Single(opt))
			},
			1 => {
				let len: u16 = Readable::read(reader)?;
				let mut vec = Vec::with_capacity(len as usize);
				for _ in 0..len {
					vec.push(Readable::read(reader)?);
				}
				Ok(PaymentLinks::Multiple(vec))
			},
			_ => Err(DecodeError::InvalidValue),
		}
	}
}

/// Represents the kind of an invoice or offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvoiceKind {
	/// A [BOLT 11] invoice.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	Bolt11 {
		/// The payment hash for this invoice.
		payment_hash: PaymentHash,
		/// The payment secret for this invoice.
		payment_secret: PaymentSecret,
	},
	/// A [BOLT 11] invoice intended to open a [bLIP-52 / LSPS 2] just-in-time channel.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
	Bolt11Jit {
		/// The payment hash for this invoice.
		payment_hash: PaymentHash,
		/// The payment secret for this invoice.
		payment_secret: PaymentSecret,
		/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
		lsp_fee_limits: LSPFeeLimits,
	},
	/// A [BOLT 12] offer.
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	Bolt12Offer {
		/// The offer ID.
		offer_id: OfferId,
		/// The quantity of items that can be requested.
		quantity: Option<u64>,
	},
	/// A [BOLT 12] refund request (inbound).
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	Bolt12RefundRequest {
		/// The payment hash for this refund request.
		payment_hash: PaymentHash,
		/// The payer note for the refund.
		payer_note: Option<UntrustedString>,
		/// The quantity of items the refund is for.
		quantity: Option<u64>,
	},
	/// A [BOLT 12] refund offer (outbound).
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	Bolt12RefundOffer {
		/// The payer note for the refund.
		payer_note: Option<UntrustedString>,
		/// The quantity of items the refund is for.
		quantity: Option<u64>,
	},
}

impl_writeable_tlv_based_enum!(InvoiceKind,
	(0, Bolt11) => {
		(0, payment_hash, required),
		(2, payment_secret, required),
	},
	(2, Bolt11Jit) => {
		(0, payment_hash, required),
		(2, payment_secret, required),
		(4, lsp_fee_limits, required),
	},
	(4, Bolt12Offer) => {
		(0, offer_id, required),
		(2, quantity, option),
	},
	(6, Bolt12RefundRequest) => {
		(0, payment_hash, required),
		(2, payer_note, option),
		(4, quantity, option),
	},
	(8, Bolt12RefundOffer) => {
		(0, payer_note, option),
		(2, quantity, option),
	}
);

/// An update to be applied to an [`InvoiceDetails`] object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvoiceDetailsUpdate {
	/// The invoice ID.
	pub id: InvoiceId,
	/// A payment ID to add to the linked payments.
	pub payment_id: Option<PaymentId>,
}

impl InvoiceDetailsUpdate {
	pub fn new(id: InvoiceId) -> Self {
		Self { id, payment_id: None }
	}
}

impl From<&InvoiceDetails> for InvoiceDetailsUpdate {
	fn from(value: &InvoiceDetails) -> Self {
		Self { id: value.id, payment_id: None }
	}
}

impl StorableObjectUpdate<InvoiceDetails> for InvoiceDetailsUpdate {
	fn id(&self) -> <InvoiceDetails as StorableObject>::Id {
		self.id
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use bitcoin::io::Cursor;

	#[test]
	fn invoice_details_created_without_payments() {
		let invoice_id = InvoiceId::new();
		let payment_hash = PaymentHash([42u8; 32]);
		let payment_secret = PaymentSecret([43u8; 32]);

		let kind = InvoiceKind::Bolt11 { payment_hash, payment_secret };

		let invoice = InvoiceDetails::new(
			invoice_id,
			kind,
			Some(50_000_000),
			"Test invoice".to_string(),
			Some(3600),
		);

		// Verify no payment IDs at creation
		assert!(invoice.linked_payment_ids.is_none());
		assert_eq!(invoice.created_at, invoice.latest_update_timestamp);
	}

	#[test]
	fn invoice_details_update_with_payment() {
		let invoice_id = InvoiceId::new();
		let payment_hash = PaymentHash([42u8; 32]);
		let payment_secret = PaymentSecret([43u8; 32]);

		let kind = InvoiceKind::Bolt11 { payment_hash, payment_secret };

		let mut invoice = InvoiceDetails::new(
			invoice_id,
			kind,
			Some(50_000_000),
			"Test invoice".to_string(),
			Some(3600),
		);

		let created_timestamp = invoice.created_at;

		// Add a payment ID
		let payment_id = PaymentId([45u8; 32]);
		let update = InvoiceDetailsUpdate { id: invoice_id, payment_id: Some(payment_id) };

		let was_updated = invoice.update(&update);
		assert!(was_updated);

		// Verify payment was linked
		assert!(invoice.linked_payment_ids.is_some());
		match invoice.linked_payment_ids.as_ref().unwrap() {
			PaymentLinks::Single(opt) => {
				assert_eq!(*opt, Some(payment_id));
			},
			_ => panic!("Expected Single payment link"),
		}

		// Verify timestamp was updated
		assert!(invoice.latest_update_timestamp >= created_timestamp);
	}

	#[test]
	fn invoice_details_bolt11_serialization() {
		let invoice_id = InvoiceId::new();
		let payment_hash = PaymentHash([42u8; 32]);
		let payment_secret = PaymentSecret([43u8; 32]);

		let kind = InvoiceKind::Bolt11 { payment_hash, payment_secret };

		let invoice = InvoiceDetails::new(
			invoice_id,
			kind,
			Some(50_000_000),
			"Test invoice".to_string(),
			Some(3600),
		);

		let encoded = invoice.encode();
		let decoded = InvoiceDetails::read(&mut Cursor::new(encoded.clone())).unwrap();

		assert_eq!(invoice, decoded);
		assert!(decoded.linked_payment_ids.is_none());
	}

	#[test]
	fn invoice_details_bolt12_offer_multiple_payments() {
		let invoice_id = InvoiceId::new();
		let offer_id = OfferId([44u8; 32]);

		let kind = InvoiceKind::Bolt12Offer { offer_id, quantity: Some(5) };

		let mut invoice = InvoiceDetails::new(
			invoice_id,
			kind,
			Some(100_000_000),
			"Test offer".to_string(),
			Some(7200),
		);

		let created_timestamp = invoice.created_at;

		// Add multiple payment IDs
		let payment_id1 = PaymentId([45u8; 32]);
		let payment_id2 = PaymentId([46u8; 32]);

		let update1 = InvoiceDetailsUpdate { id: invoice_id, payment_id: Some(payment_id1) };
		invoice.update(&update1);

		let update2 = InvoiceDetailsUpdate { id: invoice_id, payment_id: Some(payment_id2) };
		invoice.update(&update2);

		// Verify timestamp was updated
		assert!(invoice.latest_update_timestamp >= created_timestamp);

		let encoded = invoice.encode();
		let decoded = InvoiceDetails::read(&mut Cursor::new(encoded.clone())).unwrap();

		assert_eq!(invoice, decoded);

		// Verify multiple payments were linked
		assert!(decoded.linked_payment_ids.is_some());
		match decoded.linked_payment_ids.as_ref().unwrap() {
			PaymentLinks::Multiple(vec) => {
				assert_eq!(vec.len(), 2);
				assert!(vec.contains(&payment_id1));
				assert!(vec.contains(&payment_id2));
			},
			_ => panic!("Expected Multiple payment links"),
		}
	}

	#[test]
	fn payment_links_serialization() {
		// Test Single variant with None
		let single_none = PaymentLinks::Single(None);
		let encoded = single_none.encode();
		let decoded = PaymentLinks::read(&mut Cursor::new(encoded)).unwrap();
		assert_eq!(single_none, decoded);

		// Test Single variant with Some
		let single_some = PaymentLinks::Single(Some(PaymentId([47u8; 32])));
		let encoded = single_some.encode();
		let decoded = PaymentLinks::read(&mut Cursor::new(encoded)).unwrap();
		assert_eq!(single_some, decoded);

		// Test Multiple variant
		let multiple = PaymentLinks::Multiple(vec![
			PaymentId([48u8; 32]),
			PaymentId([49u8; 32]),
			PaymentId([50u8; 32]),
		]);
		let encoded = multiple.encode();
		let decoded = PaymentLinks::read(&mut Cursor::new(encoded)).unwrap();
		assert_eq!(multiple, decoded);
	}
}
