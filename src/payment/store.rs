// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::{BlockHash, Txid};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::offers::offer::OfferId;
use lightning::util::ser::{Readable, Writeable};
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based,
	impl_writeable_tlv_based_enum, write_tlv_fields,
};
use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning_types::string::UntrustedString;

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;

/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentDetails {
	/// The identifier of this payment.
	pub id: PaymentId,
	/// The kind of the payment.
	pub kind: PaymentKind,
	/// The amount transferred.
	///
	/// Will be `None` for variable-amount payments until we receive them.
	pub amount_msat: Option<u64>,
	/// The fees that were paid for this payment.
	///
	/// For Lightning payments, this will only be updated for outbound payments once they
	/// succeeded.
	///
	/// Will be `None` for Lightning payments made with LDK Node v0.4.x and earlier.
	pub fee_paid_msat: Option<u64>,
	/// The direction of the payment.
	pub direction: PaymentDirection,
	/// The status of the payment.
	pub status: PaymentStatus,
	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	pub latest_update_timestamp: u64,
}

impl PaymentDetails {
	pub(crate) fn new(
		id: PaymentId, kind: PaymentKind, amount_msat: Option<u64>, fee_paid_msat: Option<u64>,
		direction: PaymentDirection, status: PaymentStatus,
	) -> Self {
		let latest_update_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		Self { id, kind, amount_msat, fee_paid_msat, direction, status, latest_update_timestamp }
	}
}

impl Writeable for PaymentDetails {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		write_tlv_fields!(writer, {
			(0, self.id, required), // Used to be `hash` for v0.2.1 and prior
			// 1 briefly used to be lsp_fee_limits, could probably be reused at some point in the future.
			// 2 used to be `preimage` before it was moved to `kind` in v0.3.0
			(2, None::<Option<PaymentPreimage>>, required),
			(3, self.kind, required),
			// 4 used to be `secret` before it was moved to `kind` in v0.3.0
			(4, None::<Option<PaymentSecret>>, required),
			(5, self.latest_update_timestamp, required),
			(6, self.amount_msat, required),
			(7, self.fee_paid_msat, option),
			(8, self.direction, required),
			(10, self.status, required)
		});
		Ok(())
	}
}

impl Readable for PaymentDetails {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<PaymentDetails, DecodeError> {
		let unix_time_secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, id, required), // Used to be `hash`
			(1, lsp_fee_limits, option),
			(2, preimage, required),
			(3, kind_opt, option),
			(4, secret, required),
			(5, latest_update_timestamp, (default_value, unix_time_secs)),
			(6, amount_msat, required),
			(7, fee_paid_msat, option),
			(8, direction, required),
			(10, status, required)
		});

		let id: PaymentId = id.0.ok_or(DecodeError::InvalidValue)?;
		let preimage: Option<PaymentPreimage> = preimage.0.ok_or(DecodeError::InvalidValue)?;
		let secret: Option<PaymentSecret> = secret.0.ok_or(DecodeError::InvalidValue)?;
		let latest_update_timestamp: u64 =
			latest_update_timestamp.0.ok_or(DecodeError::InvalidValue)?;
		let amount_msat: Option<u64> = amount_msat.0.ok_or(DecodeError::InvalidValue)?;
		let direction: PaymentDirection = direction.0.ok_or(DecodeError::InvalidValue)?;
		let status: PaymentStatus = status.0.ok_or(DecodeError::InvalidValue)?;

		let kind = if let Some(kind) = kind_opt {
			// If we serialized the payment kind, use it.
			// This will always be the case for any version after v0.2.1.
			kind
		} else {
			// Otherwise we persisted with v0.2.1 or before, and puzzle together the kind from the
			// provided fields.

			// We used to track everything by hash, but switched to track everything by id
			// post-v0.2.1. As both are serialized identically, we just switched the `0`-type field above
			// from `PaymentHash` to `PaymentId` and serialize a separate `PaymentHash` in
			// `PaymentKind` when needed. Here, for backwards compat, we can just re-create the
			// `PaymentHash` from the id, as 'back then' `payment_hash == payment_id` was always
			// true.
			let hash = PaymentHash(id.0);

			if secret.is_some() {
				if let Some(lsp_fee_limits) = lsp_fee_limits {
					let counterparty_skimmed_fee_msat = None;
					PaymentKind::Bolt11Jit {
						hash,
						preimage,
						secret,
						counterparty_skimmed_fee_msat,
						lsp_fee_limits,
					}
				} else {
					PaymentKind::Bolt11 { hash, preimage, secret }
				}
			} else {
				PaymentKind::Spontaneous { hash, preimage }
			}
		};

		Ok(PaymentDetails {
			id,
			kind,
			amount_msat,
			fee_paid_msat,
			direction,
			status,
			latest_update_timestamp,
		})
	}
}

impl StorableObjectId for PaymentId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}
impl StorableObject for PaymentDetails {
	type Id = PaymentId;
	type Update = PaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		debug_assert_eq!(
			self.id, update.id,
			"We should only ever override payment data for the same payment id"
		);

		let mut updated = false;

		macro_rules! update_if_necessary {
			($val:expr, $update:expr) => {
				if $val != $update {
					$val = $update;
					updated = true;
				}
			};
		}

		if let Some(hash_opt) = update.hash {
			match self.kind {
				PaymentKind::Bolt12Offer { ref mut hash, .. } => {
					debug_assert_eq!(
						self.direction,
						PaymentDirection::Outbound,
						"We should only ever override payment hash for outbound BOLT 12 payments"
					);
					debug_assert!(
						hash.is_none() || *hash == hash_opt,
						"We should never change a payment hash after being initially set"
					);
					update_if_necessary!(*hash, hash_opt);
				},
				PaymentKind::Bolt12Refund { ref mut hash, .. } => {
					debug_assert_eq!(
						self.direction,
						PaymentDirection::Outbound,
						"We should only ever override payment hash for outbound BOLT 12 payments"
					);
					debug_assert!(
						hash.is_none() || *hash == hash_opt,
						"We should never change a payment hash after being initially set"
					);
					update_if_necessary!(*hash, hash_opt);
				},
				_ => {
					// We can omit updating the hash for BOLT11 payments as the payment hash
					// will always be known from the beginning.
				},
			}
		}
		if let Some(preimage_opt) = update.preimage {
			match self.kind {
				PaymentKind::Bolt11 { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Bolt11Jit { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Bolt12Offer { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Bolt12Refund { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Spontaneous { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				_ => {},
			}
		}

		if let Some(secret_opt) = update.secret {
			match self.kind {
				PaymentKind::Bolt11 { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				PaymentKind::Bolt11Jit { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				PaymentKind::Bolt12Offer { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				PaymentKind::Bolt12Refund { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				_ => {},
			}
		}

		if let Some(amount_opt) = update.amount_msat {
			update_if_necessary!(self.amount_msat, amount_opt);
		}

		if let Some(fee_paid_msat_opt) = update.fee_paid_msat {
			update_if_necessary!(self.fee_paid_msat, fee_paid_msat_opt);
		}

		if let Some(skimmed_fee_msat) = update.counterparty_skimmed_fee_msat {
			match self.kind {
				PaymentKind::Bolt11Jit { ref mut counterparty_skimmed_fee_msat, .. } => {
					update_if_necessary!(*counterparty_skimmed_fee_msat, skimmed_fee_msat);
				},
				_ => debug_assert!(
					false,
					"We should only ever override counterparty_skimmed_fee_msat for JIT payments"
				),
			}
		}

		if let Some(status) = update.status {
			update_if_necessary!(self.status, status);
		}

		if let Some(confirmation_status) = update.confirmation_status {
			match self.kind {
				PaymentKind::Onchain { ref mut status, .. } => {
					update_if_necessary!(*status, confirmation_status);
				},
				_ => {},
			}
		}

		if let Some(conflicting_txids_opt) = &update.conflicting_txids {
			match self.kind {
				PaymentKind::Onchain { ref mut conflicting_txids, .. } => {
					update_if_necessary!(*conflicting_txids, conflicting_txids_opt.to_vec());
				},
				_ => {},
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

/// Represents the direction of a payment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PaymentDirection {
	/// The payment is inbound.
	Inbound,
	/// The payment is outbound.
	Outbound,
}

impl_writeable_tlv_based_enum!(PaymentDirection,
	(0, Inbound) => {},
	(1, Outbound) => {}
);

/// Represents the current status of a payment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
	/// The payment is still pending.
	Pending,
	/// The payment succeeded.
	Succeeded,
	/// The payment failed.
	Failed,
}

impl_writeable_tlv_based_enum!(PaymentStatus,
	(0, Pending) => {},
	(2, Succeeded) => {},
	(4, Failed) => {}
);

/// Represents the kind of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentKind {
	/// An on-chain payment.
	///
	/// Payments of this kind will be considered pending until the respective transaction has
	/// reached [`ANTI_REORG_DELAY`] confirmations on-chain.
	///
	/// [`ANTI_REORG_DELAY`]: lightning::chain::channelmonitor::ANTI_REORG_DELAY
	Onchain {
		/// The transaction identifier of this payment.
		txid: Txid,
		/// The confirmation status of this payment.
		status: ConfirmationStatus,
		/// Transaction IDs that have replaced or conflict with this payment.
		conflicting_txids: Vec<Txid>,
	},
	/// A [BOLT 11] payment.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	Bolt11 {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
	},
	/// A [BOLT 11] payment intended to open an [bLIP-52 / LSPS 2] just-in-time channel.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
	Bolt11Jit {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The value, in thousands of a satoshi, that was deducted from this payment as an extra
		/// fee taken by our channel counterparty.
		///
		/// Will only be `Some` once we received the payment. Will always be `None` for LDK Node
		/// v0.4 and prior.
		counterparty_skimmed_fee_msat: Option<u64>,
		/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
		///
		/// Allowing them to deduct this fee from the first inbound payment will pay for the LSP's
		/// channel opening fees.
		///
		/// See [`LdkChannelConfig::accept_underpaying_htlcs`] for more information.
		///
		/// [`LdkChannelConfig::accept_underpaying_htlcs`]: lightning::util::config::ChannelConfig::accept_underpaying_htlcs
		lsp_fee_limits: LSPFeeLimits,
	},
	/// A [BOLT 12] 'offer' payment, i.e., a payment for an [`Offer`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Offer`]: crate::lightning::offers::offer::Offer
	Bolt12Offer {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: Option<PaymentHash>,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The ID of the offer this payment is for.
		offer_id: OfferId,
		/// The payer note for the payment.
		///
		/// Truncated to [`PAYER_NOTE_LIMIT`] characters.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		///
		/// [`PAYER_NOTE_LIMIT`]: lightning::offers::invoice_request::PAYER_NOTE_LIMIT
		payer_note: Option<UntrustedString>,
		/// The quantity of an item requested in the offer.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		quantity: Option<u64>,
	},
	/// A [BOLT 12] 'refund' payment, i.e., a payment for a [`Refund`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Refund`]: lightning::offers::refund::Refund
	Bolt12Refund {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: Option<PaymentHash>,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The payer note for the refund payment.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		payer_note: Option<UntrustedString>,
		/// The quantity of an item that the refund is for.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		quantity: Option<u64>,
	},
	/// A spontaneous ("keysend") payment.
	Spontaneous {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
	},
}

impl_writeable_tlv_based_enum!(PaymentKind,
	(0, Onchain) => {
		(0, txid, required),
		(1, conflicting_txids, optional_vec),
		(2, status, required),
	},
	(2, Bolt11) => {
		(0, hash, required),
		(2, preimage, option),
		(4, secret, option),
	},
	(4, Bolt11Jit) => {
		(0, hash, required),
		(1, counterparty_skimmed_fee_msat, option),
		(2, preimage, option),
		(4, secret, option),
		(6, lsp_fee_limits, required),
	},
	(6, Bolt12Offer) => {
		(0, hash, option),
		(1, payer_note, option),
		(2, preimage, option),
		(3, quantity, option),
		(4, secret, option),
		(6, offer_id, required),
	},
	(8, Spontaneous) => {
		(0, hash, required),
		(2, preimage, option),
	},
	(10, Bolt12Refund) => {
		(0, hash, option),
		(1, payer_note, option),
		(2, preimage, option),
		(3, quantity, option),
		(4, secret, option),
	}
);

/// Represents the confirmation status of a transaction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConfirmationStatus {
	/// The transaction is confirmed in the best chain.
	Confirmed {
		/// The hash of the block in which the transaction was confirmed.
		block_hash: BlockHash,
		/// The height under which the block was confirmed.
		height: u32,
		/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
		timestamp: u64,
	},
	/// The transaction is unconfirmed.
	Unconfirmed,
}

impl_writeable_tlv_based_enum!(ConfirmationStatus,
	(0, Confirmed) => {
		(0, block_hash, required),
		(2, height, required),
		(4, timestamp, required),
	},
	(2, Unconfirmed) => {},
);

/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
///
/// See [`LdkChannelConfig::accept_underpaying_htlcs`] for more information.
///
/// [`LdkChannelConfig::accept_underpaying_htlcs`]: lightning::util::config::ChannelConfig::accept_underpaying_htlcs
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LSPFeeLimits {
	/// The maximal total amount we allow any configured LSP withhold from us when forwarding the
	/// payment.
	pub max_total_opening_fee_msat: Option<u64>,
	/// The maximal proportional fee, in parts-per-million millisatoshi, we allow any configured
	/// LSP withhold from us when forwarding the payment.
	pub max_proportional_opening_fee_ppm_msat: Option<u64>,
}

impl_writeable_tlv_based!(LSPFeeLimits, {
	(0, max_total_opening_fee_msat, option),
	(2, max_proportional_opening_fee_ppm_msat, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentDetailsUpdate {
	pub id: PaymentId,
	pub hash: Option<Option<PaymentHash>>,
	pub preimage: Option<Option<PaymentPreimage>>,
	pub secret: Option<Option<PaymentSecret>>,
	pub amount_msat: Option<Option<u64>>,
	pub fee_paid_msat: Option<Option<u64>>,
	pub counterparty_skimmed_fee_msat: Option<Option<u64>>,
	pub direction: Option<PaymentDirection>,
	pub status: Option<PaymentStatus>,
	pub confirmation_status: Option<ConfirmationStatus>,
	pub conflicting_txids: Option<Vec<Txid>>,
}

impl PaymentDetailsUpdate {
	pub fn new(id: PaymentId) -> Self {
		Self {
			id,
			hash: None,
			preimage: None,
			secret: None,
			amount_msat: None,
			fee_paid_msat: None,
			counterparty_skimmed_fee_msat: None,
			direction: None,
			status: None,
			confirmation_status: None,
			conflicting_txids: None,
		}
	}
}

impl From<&PaymentDetails> for PaymentDetailsUpdate {
	fn from(value: &PaymentDetails) -> Self {
		let (hash, preimage, secret) = match value.kind {
			PaymentKind::Bolt11 { hash, preimage, secret, .. } => (Some(hash), preimage, secret),
			PaymentKind::Bolt11Jit { hash, preimage, secret, .. } => (Some(hash), preimage, secret),
			PaymentKind::Bolt12Offer { hash, preimage, secret, .. } => (hash, preimage, secret),
			PaymentKind::Bolt12Refund { hash, preimage, secret, .. } => (hash, preimage, secret),
			PaymentKind::Spontaneous { hash, preimage, .. } => (Some(hash), preimage, None),
			_ => (None, None, None),
		};

		let (confirmation_status, conflicting_txids) = match &value.kind {
			PaymentKind::Onchain { status, conflicting_txids, .. } => {
				(Some(*status), conflicting_txids.clone())
			},
			_ => (None, Vec::new()),
		};

		let counterparty_skimmed_fee_msat = match value.kind {
			PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
				Some(counterparty_skimmed_fee_msat)
			},
			_ => None,
		};

		Self {
			id: value.id,
			hash: Some(hash),
			preimage: Some(preimage),
			secret: Some(secret),
			amount_msat: Some(value.amount_msat),
			fee_paid_msat: Some(value.fee_paid_msat),
			counterparty_skimmed_fee_msat,
			direction: Some(value.direction),
			status: Some(value.status),
			confirmation_status,
			conflicting_txids: Some(conflicting_txids),
		}
	}
}

impl StorableObjectUpdate<PaymentDetails> for PaymentDetailsUpdate {
	fn id(&self) -> <PaymentDetails as StorableObject>::Id {
		self.id
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::io::Cursor;
	use lightning::util::ser::Readable;

	use super::*;

	/// We refactored `PaymentDetails` to hold a payment id and moved some required fields into
	/// `PaymentKind`. Here, we keep the old layout available in order test de/ser compatibility.
	#[derive(Clone, Debug, PartialEq, Eq)]
	struct OldPaymentDetails {
		pub hash: PaymentHash,
		pub preimage: Option<PaymentPreimage>,
		pub secret: Option<PaymentSecret>,
		pub amount_msat: Option<u64>,
		pub direction: PaymentDirection,
		pub status: PaymentStatus,
		pub lsp_fee_limits: Option<LSPFeeLimits>,
	}

	impl_writeable_tlv_based!(OldPaymentDetails, {
		(0, hash, required),
		(1, lsp_fee_limits, option),
		(2, preimage, required),
		(4, secret, required),
		(6, amount_msat, required),
		(8, direction, required),
		(10, status, required)
	});

	#[test]
	fn old_payment_details_deser_compat() {
		// We refactored `PaymentDetails` to hold a payment id and moved some required fields into
		// `PaymentKind`. Here, we test compatibility with the old layout.
		let hash = PaymentHash([42u8; 32]);
		let preimage = Some(PaymentPreimage([43u8; 32]));
		let secret = Some(PaymentSecret([44u8; 32]));
		let amount_msat = Some(45_000_000);

		// Test `Bolt11` de/ser
		{
			let old_bolt11_payment = OldPaymentDetails {
				hash,
				preimage,
				secret,
				amount_msat,
				direction: PaymentDirection::Inbound,
				status: PaymentStatus::Pending,
				lsp_fee_limits: None,
			};

			let old_bolt11_encoded = old_bolt11_payment.encode();
			assert_eq!(
				old_bolt11_payment,
				OldPaymentDetails::read(&mut Cursor::new(old_bolt11_encoded.clone())).unwrap()
			);

			let bolt11_decoded =
				PaymentDetails::read(&mut Cursor::new(old_bolt11_encoded)).unwrap();
			let bolt11_reencoded = bolt11_decoded.encode();
			assert_eq!(
				bolt11_decoded,
				PaymentDetails::read(&mut Cursor::new(bolt11_reencoded)).unwrap()
			);

			match bolt11_decoded.kind {
				PaymentKind::Bolt11 { hash: h, preimage: p, secret: s } => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
					assert_eq!(secret, s);
				},
				_ => {
					panic!("Unexpected kind!");
				},
			}
		}

		// Test `Bolt11Jit` de/ser
		{
			let lsp_fee_limits = Some(LSPFeeLimits {
				max_total_opening_fee_msat: Some(46_000),
				max_proportional_opening_fee_ppm_msat: Some(47_000),
			});

			let old_bolt11_jit_payment = OldPaymentDetails {
				hash,
				preimage,
				secret,
				amount_msat,
				direction: PaymentDirection::Inbound,
				status: PaymentStatus::Pending,
				lsp_fee_limits,
			};

			let old_bolt11_jit_encoded = old_bolt11_jit_payment.encode();
			assert_eq!(
				old_bolt11_jit_payment,
				OldPaymentDetails::read(&mut Cursor::new(old_bolt11_jit_encoded.clone())).unwrap()
			);

			let bolt11_jit_decoded =
				PaymentDetails::read(&mut Cursor::new(old_bolt11_jit_encoded)).unwrap();
			let bolt11_jit_reencoded = bolt11_jit_decoded.encode();
			assert_eq!(
				bolt11_jit_decoded,
				PaymentDetails::read(&mut Cursor::new(bolt11_jit_reencoded)).unwrap()
			);

			match bolt11_jit_decoded.kind {
				PaymentKind::Bolt11Jit {
					hash: h,
					preimage: p,
					secret: s,
					counterparty_skimmed_fee_msat: c,
					lsp_fee_limits: l,
				} => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
					assert_eq!(secret, s);
					assert_eq!(None, c);
					assert_eq!(lsp_fee_limits, Some(l));
				},
				_ => {
					panic!("Unexpected kind!");
				},
			}
		}

		// Test `Spontaneous` de/ser
		{
			let old_spontaneous_payment = OldPaymentDetails {
				hash,
				preimage,
				secret: None,
				amount_msat,
				direction: PaymentDirection::Inbound,
				status: PaymentStatus::Pending,
				lsp_fee_limits: None,
			};

			let old_spontaneous_encoded = old_spontaneous_payment.encode();
			assert_eq!(
				old_spontaneous_payment,
				OldPaymentDetails::read(&mut Cursor::new(old_spontaneous_encoded.clone())).unwrap()
			);

			let spontaneous_decoded =
				PaymentDetails::read(&mut Cursor::new(old_spontaneous_encoded)).unwrap();
			let spontaneous_reencoded = spontaneous_decoded.encode();
			assert_eq!(
				spontaneous_decoded,
				PaymentDetails::read(&mut Cursor::new(spontaneous_reencoded)).unwrap()
			);

			match spontaneous_decoded.kind {
				PaymentKind::Spontaneous { hash: h, preimage: p } => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
				},
				_ => {
					panic!("Unexpected kind!");
				},
			}
		}
	}
}
