// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::Txid;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::util::ser::{Readable, Writeable};
use lightning::{_init_and_read_len_prefixed_tlv_fields, write_tlv_fields};

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};

/// Details of an on-chain transaction that has replaced a previous transaction (e.g., via RBF).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedOnchainTransactionDetails {
	/// The new transaction ID.
	pub new_txid: Txid,
	/// The original transaction ID that was replaced.
	pub original_txid: Txid,
	/// The payment ID associated with the transaction.
	pub payment_id: PaymentId,
}

impl ReplacedOnchainTransactionDetails {
	pub(crate) fn new(new_txid: Txid, original_txid: Txid, payment_id: PaymentId) -> Self {
		Self { new_txid, original_txid, payment_id }
	}
}

impl Writeable for ReplacedOnchainTransactionDetails {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		write_tlv_fields!(writer, {
			(0, self.new_txid, required),
			(2, self.original_txid, required),
			(4, self.payment_id, required),
		});
		Ok(())
	}
}

impl Readable for ReplacedOnchainTransactionDetails {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<ReplacedOnchainTransactionDetails, DecodeError> {
		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, new_txid, required),
			(2, original_txid, required),
			(4, payment_id, required),
		});

		let new_txid: Txid = new_txid.0.ok_or(DecodeError::InvalidValue)?;
		let original_txid: Txid = original_txid.0.ok_or(DecodeError::InvalidValue)?;
		let payment_id: PaymentId = payment_id.0.ok_or(DecodeError::InvalidValue)?;

		Ok(ReplacedOnchainTransactionDetails { new_txid, original_txid, payment_id })
	}
}

impl StorableObjectId for Txid {
	fn encode_to_hex_str(&self) -> String {
		self.to_string()
	}
}
impl StorableObject for ReplacedOnchainTransactionDetails {
	type Id = Txid;
	type Update = ReplacedOnchainTransactionDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.new_txid
	}

	fn update(&mut self, _update: &Self::Update) -> bool {
		// We don't update, we delete on confirmation
		false
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplacedOnchainTransactionDetailsUpdate {
	pub id: Txid,
}

impl From<&ReplacedOnchainTransactionDetails> for ReplacedOnchainTransactionDetailsUpdate {
	fn from(value: &ReplacedOnchainTransactionDetails) -> Self {
		Self { id: value.new_txid }
	}
}

impl StorableObjectUpdate<ReplacedOnchainTransactionDetails>
	for ReplacedOnchainTransactionDetailsUpdate
{
	fn id(&self) -> <ReplacedOnchainTransactionDetails as StorableObject>::Id {
		self.id
	}
}
