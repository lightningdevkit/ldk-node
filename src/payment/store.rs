// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use bitcoin::{BlockHash, Txid};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::ln::types::ChannelId;
use lightning::offers::offer::OfferId;
use lightning::util::ser::{Readable, Writeable};
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based,
	impl_writeable_tlv_based_enum, write_tlv_fields,
};
use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning_types::string::UntrustedString;

use crate::data_store::{DataStore, StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::logger::{log_debug, log_error, LdkLogger, Logger};
use crate::types::UserChannelId;
use crate::Error;

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

		let confirmation_status = match value.kind {
			PaymentKind::Onchain { status, .. } => Some(status),
			_ => None,
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
				OldPaymentDetails::read(&mut &*old_bolt11_encoded.clone()).unwrap()
			);

			let bolt11_decoded = PaymentDetails::read(&mut &*old_bolt11_encoded).unwrap();
			let bolt11_reencoded = bolt11_decoded.encode();
			assert_eq!(bolt11_decoded, PaymentDetails::read(&mut &*bolt11_reencoded).unwrap());

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
				OldPaymentDetails::read(&mut &*old_bolt11_jit_encoded.clone()).unwrap()
			);

			let bolt11_jit_decoded = PaymentDetails::read(&mut &*old_bolt11_jit_encoded).unwrap();
			let bolt11_jit_reencoded = bolt11_jit_decoded.encode();
			assert_eq!(
				bolt11_jit_decoded,
				PaymentDetails::read(&mut &*bolt11_jit_reencoded).unwrap()
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
				OldPaymentDetails::read(&mut &*old_spontaneous_encoded.clone()).unwrap()
			);

			let spontaneous_decoded = PaymentDetails::read(&mut &*old_spontaneous_encoded).unwrap();
			let spontaneous_reencoded = spontaneous_decoded.encode();
			assert_eq!(
				spontaneous_decoded,
				PaymentDetails::read(&mut &*spontaneous_reencoded).unwrap()
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

	#[test]
	fn test_bucket_calculation() {
		// Test that bucket timestamps are calculated correctly
		let retention_minutes = 60;
		let bucket_size_secs = retention_minutes * 60; // 3600 seconds

		// Payment at exactly bucket boundary
		let timestamp1 = 1738800000;
		let bucket1 = (timestamp1 / bucket_size_secs) * bucket_size_secs;
		assert_eq!(bucket1, 1738800000); // Should be unchanged

		// Payment 1 second into bucket
		let timestamp2 = 1738800001;
		let bucket2 = (timestamp2 / bucket_size_secs) * bucket_size_secs;
		assert_eq!(bucket2, 1738800000); // Should round down

		// Payment at end of bucket (3599 seconds in)
		let timestamp3 = 1738803599;
		let bucket3 = (timestamp3 / bucket_size_secs) * bucket_size_secs;
		assert_eq!(bucket3, 1738800000); // Should still be in same bucket

		// Payment at start of next bucket
		let timestamp4 = 1738803600;
		let bucket4 = (timestamp4 / bucket_size_secs) * bucket_size_secs;
		assert_eq!(bucket4, 1738803600); // Should be in next bucket
	}

	#[test]
	fn test_channel_pair_stats_id_with_bucket() {
		use lightning::ln::types::ChannelId;

		let prev_channel = ChannelId([1u8; 32]);
		let next_channel = ChannelId([2u8; 32]);
		let bucket_timestamp = 1738800000u64;

		let id = ChannelPairStatsId::from_channel_pair_and_bucket(
			&prev_channel,
			&next_channel,
			bucket_timestamp,
		);

		// Verify the ID contains all three components
		assert_eq!(&id.0[0..32], &prev_channel.0);
		assert_eq!(&id.0[32..64], &next_channel.0);
		assert_eq!(&id.0[64..72], &bucket_timestamp.to_be_bytes());

		// Verify different buckets create different IDs
		let id2 = ChannelPairStatsId::from_channel_pair_and_bucket(
			&prev_channel,
			&next_channel,
			bucket_timestamp + 3600,
		);
		assert_ne!(id, id2);
	}

	#[test]
	fn test_payments_grouped_into_correct_buckets() {
		use lightning::ln::types::ChannelId;
		use std::collections::HashMap;

		let retention_minutes = 60;
		let bucket_size_secs = retention_minutes * 60;
		let base_time = 3600 * 100; // 360000

		let prev_channel = ChannelId([1u8; 32]);
		let next_channel = ChannelId([2u8; 32]);

		// Create test payments across different time buckets
		let mut payments = Vec::new();

		// 3 payments in bucket 1
		for i in 0..3 {
			payments.push((
				base_time + i * 1000,
				ForwardedPaymentDetails {
					id: ForwardedPaymentId([i as u8; 32]),
					prev_channel_id: prev_channel,
					next_channel_id: next_channel,
					prev_user_channel_id: None,
					next_user_channel_id: None,
					prev_node_id: None,
					next_node_id: None,
					total_fee_earned_msat: Some(1000),
					skimmed_fee_msat: Some(100),
					claim_from_onchain_tx: false,
					outbound_amount_forwarded_msat: Some(10000),
					forwarded_at_timestamp: base_time + i * 1000,
				},
			));
		}

		// 2 payments in bucket 2
		for i in 3..5 {
			payments.push((
				base_time + bucket_size_secs + (i - 3) * 1000,
				ForwardedPaymentDetails {
					id: ForwardedPaymentId([i as u8; 32]),
					prev_channel_id: prev_channel,
					next_channel_id: next_channel,
					prev_user_channel_id: None,
					next_user_channel_id: None,
					prev_node_id: None,
					next_node_id: None,
					total_fee_earned_msat: Some(2000),
					skimmed_fee_msat: Some(200),
					claim_from_onchain_tx: false,
					outbound_amount_forwarded_msat: Some(20000),
					forwarded_at_timestamp: base_time + bucket_size_secs + (i - 3) * 1000,
				},
			));
		}

		// Group payments by bucket (simulating aggregation logic)
		let mut bucket_groups: HashMap<u64, Vec<&ForwardedPaymentDetails>> = HashMap::new();
		for (_, payment) in &payments {
			let bucket_start =
				(payment.forwarded_at_timestamp / bucket_size_secs) * bucket_size_secs;
			bucket_groups.entry(bucket_start).or_insert_with(Vec::new).push(payment);
		}

		// Verify we have 2 distinct buckets
		assert_eq!(bucket_groups.len(), 2, "Should have 2 distinct buckets");

		// Verify bucket 1 has 3 payments
		let bucket1_start = (base_time / bucket_size_secs) * bucket_size_secs;
		assert_eq!(
			bucket_groups.get(&bucket1_start).unwrap().len(),
			3,
			"Bucket 1 should have 3 payments"
		);

		// Verify bucket 2 has 2 payments
		let bucket2_start = ((base_time + bucket_size_secs) / bucket_size_secs) * bucket_size_secs;
		assert_eq!(
			bucket_groups.get(&bucket2_start).unwrap().len(),
			2,
			"Bucket 2 should have 2 payments"
		);
	}

	#[test]
	fn test_bucket_statistics_calculation() {
		use lightning::ln::types::ChannelId;

		let prev_channel = ChannelId([1u8; 32]);
		let next_channel = ChannelId([2u8; 32]);
		let bucket_timestamp = 1738800000u64;

		// Simulate aggregating 3 payments
		let mut total_fee = 0u64;
		let mut total_inbound = 0u64;
		let mut total_outbound = 0u64;

		for i in 1..=3 {
			let fee = 1000 * i;
			let outbound = 10000 * i;
			let inbound = outbound + fee;

			total_fee += fee;
			total_outbound += outbound;
			total_inbound += inbound;
		}

		let payment_count = 3;
		let avg_fee = total_fee / payment_count;
		let avg_inbound = total_inbound / payment_count;

		// Verify calculations
		assert_eq!(total_fee, 6000); // 1000 + 2000 + 3000
		assert_eq!(total_outbound, 60000); // 10000 + 20000 + 30000
		assert_eq!(total_inbound, 66000); // 11000 + 22000 + 33000
		assert_eq!(avg_fee, 2000);
		assert_eq!(avg_inbound, 22000);

		// Create stats entry
		let id = ChannelPairStatsId::from_channel_pair_and_bucket(
			&prev_channel,
			&next_channel,
			bucket_timestamp,
		);

		let stats = ChannelPairForwardingStats {
			id,
			prev_channel_id: prev_channel,
			next_channel_id: next_channel,
			bucket_start_timestamp: bucket_timestamp,
			prev_node_id: None,
			next_node_id: None,
			payment_count,
			total_inbound_amount_msat: total_inbound,
			total_outbound_amount_msat: total_outbound,
			total_fee_earned_msat: total_fee,
			total_skimmed_fee_msat: 0,
			onchain_claims_count: 0,
			avg_fee_msat: avg_fee,
			avg_inbound_amount_msat: avg_inbound,
			first_forwarded_at_timestamp: bucket_timestamp,
			last_forwarded_at_timestamp: bucket_timestamp + 1000,
			aggregated_at_timestamp: bucket_timestamp + 2000,
		};

		assert_eq!(stats.payment_count, 3);
		assert_eq!(stats.avg_fee_msat, 2000);
	}

	#[test]
	fn test_channel_pair_stats_serialization() {
		use lightning::ln::types::ChannelId;
		use lightning::util::ser::{Readable, Writeable};

		let prev_channel = ChannelId([5u8; 32]);
		let next_channel = ChannelId([6u8; 32]);
		let bucket_timestamp = 1738800000u64;

		let id = ChannelPairStatsId::from_channel_pair_and_bucket(
			&prev_channel,
			&next_channel,
			bucket_timestamp,
		);

		let stats = ChannelPairForwardingStats {
			id,
			prev_channel_id: prev_channel,
			next_channel_id: next_channel,
			bucket_start_timestamp: bucket_timestamp,
			prev_node_id: None,
			next_node_id: None,
			payment_count: 10,
			total_inbound_amount_msat: 100000,
			total_outbound_amount_msat: 90000,
			total_fee_earned_msat: 10000,
			total_skimmed_fee_msat: 1000,
			onchain_claims_count: 2,
			avg_fee_msat: 1000,
			avg_inbound_amount_msat: 10000,
			first_forwarded_at_timestamp: bucket_timestamp,
			last_forwarded_at_timestamp: bucket_timestamp + 1000,
			aggregated_at_timestamp: bucket_timestamp + 2000,
		};

		// Test serialization/deserialization
		let encoded = stats.encode();
		let decoded = ChannelPairForwardingStats::read(&mut &encoded[..]).unwrap();

		assert_eq!(stats, decoded);
		assert_eq!(decoded.bucket_start_timestamp, bucket_timestamp);
		assert_eq!(decoded.id.0[64..72], bucket_timestamp.to_be_bytes());
	}
}

/// A unique identifier for a forwarded payment.
///
/// This will be a randomly generated 32-byte identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForwardedPaymentId(pub [u8; 32]);

impl StorableObjectId for ForwardedPaymentId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

impl Writeable for ForwardedPaymentId {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		self.0.write(writer)
	}
}

impl Readable for ForwardedPaymentId {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		Ok(Self(Readable::read(reader)?))
	}
}

/// Details of a payment that has been forwarded through this node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardedPaymentDetails {
	/// A unique identifier for this forwarded payment.
	pub id: ForwardedPaymentId,
	/// The channel id of the incoming channel between the previous node and us.
	pub prev_channel_id: ChannelId,
	/// The channel id of the outgoing channel between the next node and us.
	pub next_channel_id: ChannelId,
	/// The `user_channel_id` of the incoming channel between the previous node and us.
	///
	/// This is only None for events generated or serialized by versions prior to 0.3.0.
	pub prev_user_channel_id: Option<UserChannelId>,
	/// The `user_channel_id` of the outgoing channel between the next node and us.
	///
	/// This will be `None` if the payment was settled via an on-chain transaction or if the
	/// event was generated or serialized by versions prior to 0.3.0.
	pub next_user_channel_id: Option<UserChannelId>,
	/// The node id of the previous node.
	pub prev_node_id: Option<PublicKey>,
	/// The node id of the next node.
	pub next_node_id: Option<PublicKey>,
	/// The total fee, in milli-satoshis, which was earned as a result of the payment.
	///
	/// Note that if we force-closed the channel over which we forwarded an HTLC while the HTLC
	/// was pending, the amount the next hop claimed will have been rounded down to the nearest
	/// whole satoshi. Thus, the fee calculated here may be higher than expected as we still
	/// claimed the full value in millisatoshis from the source.
	///
	/// If the channel which sent us the payment has been force-closed, we will claim the funds
	/// via an on-chain transaction. In that case we do not yet know the on-chain transaction
	/// fees which we will spend and will instead set this to `None`. It is possible duplicate
	/// `PaymentForwarded` events are generated for the same payment iff `total_fee_earned_msat`
	/// is `None`.
	pub total_fee_earned_msat: Option<u64>,
	/// The share of the total fee, in milli-satoshis, which was withheld in addition to the
	/// forwarding fee.
	///
	/// This will be `None` if no fee was skimmed from the forwarded HTLC.
	pub skimmed_fee_msat: Option<u64>,
	/// If this is `true`, the forwarded HTLC was claimed by our counterparty via an on-chain
	/// transaction.
	pub claim_from_onchain_tx: bool,
	/// The final amount forwarded, in milli-satoshis, after the fee is deducted.
	///
	/// The caveat described above the total_fee_earned_msat field applies here as well.
	pub outbound_amount_forwarded_msat: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when the payment was forwarded.
	pub forwarded_at_timestamp: u64,
}

impl_writeable_tlv_based!(ForwardedPaymentDetails, {
	(0, id, required),
	(2, prev_channel_id, required),
	(4, next_channel_id, required),
	(6, prev_user_channel_id, option),
	(8, next_user_channel_id, option),
	(10, prev_node_id, option),
	(12, next_node_id, option),
	(14, total_fee_earned_msat, option),
	(16, skimmed_fee_msat, option),
	(18, claim_from_onchain_tx, required),
	(20, outbound_amount_forwarded_msat, option),
	(22, forwarded_at_timestamp, required),
});

/// A no-op update type for [`ForwardedPaymentDetails`].
///
/// Forwarded payments are immutable once stored, so updates are not supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ForwardedPaymentDetailsUpdate {
	id: ForwardedPaymentId,
}

impl StorableObjectUpdate<ForwardedPaymentDetails> for ForwardedPaymentDetailsUpdate {
	fn id(&self) -> ForwardedPaymentId {
		self.id
	}
}

impl StorableObject for ForwardedPaymentDetails {
	type Id = ForwardedPaymentId;
	type Update = ForwardedPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, _update: &Self::Update) -> bool {
		// Forwarded payments are immutable, so updates are no-ops.
		false
	}

	fn to_update(&self) -> Self::Update {
		ForwardedPaymentDetailsUpdate { id: self.id }
	}
}

/// Aggregate statistics for forwarded payments through a single channel.
///
/// Each channel has one stats entry tracking all forwards where it was either
/// the inbound or outbound channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelForwardingStats {
	/// The channel this stats entry tracks.
	pub channel_id: ChannelId,
	/// The counterparty node id for this channel.
	pub counterparty_node_id: Option<PublicKey>,
	/// Number of payments forwarded where this was the inbound channel.
	pub inbound_payments_forwarded: u64,
	/// Number of payments forwarded where this was the outbound channel.
	pub outbound_payments_forwarded: u64,
	/// Total amount received on this channel for forwarding (msat).
	pub total_inbound_amount_msat: u64,
	/// Total amount sent on this channel for forwarding (msat).
	pub total_outbound_amount_msat: u64,
	/// Total fees earned from forwards where this was the inbound channel (msat).
	pub total_fee_earned_msat: u64,
	/// Total skimmed fees (msat).
	pub total_skimmed_fee_msat: u64,
	/// Number of forwards claimed via onchain tx.
	pub onchain_claims_count: u64,
	/// Timestamp of first forward through this channel.
	pub first_forwarded_at_timestamp: u64,
	/// Timestamp of most recent forward through this channel.
	pub last_forwarded_at_timestamp: u64,
}

impl_writeable_tlv_based!(ChannelForwardingStats, {
	(0, channel_id, required),
	(2, counterparty_node_id, option),
	(4, inbound_payments_forwarded, required),
	(6, outbound_payments_forwarded, required),
	(8, total_inbound_amount_msat, required),
	(10, total_outbound_amount_msat, required),
	(12, total_fee_earned_msat, required),
	(14, total_skimmed_fee_msat, required),
	(16, onchain_claims_count, required),
	(18, first_forwarded_at_timestamp, required),
	(20, last_forwarded_at_timestamp, required),
});

/// Channel pair identifier (prev_channel -> next_channel -> time bucket).
/// Formed by concatenating prev_channel_id, next_channel_id, and bucket_start_timestamp.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelPairStatsId(pub [u8; 72]);

impl ChannelPairStatsId {
	/// Create ID by concatenating prev and next channel IDs with bucket timestamp.
	pub fn from_channel_pair_and_bucket(
		prev: &ChannelId, next: &ChannelId, bucket_start_timestamp: u64,
	) -> Self {
		let mut result = [0u8; 72];
		result[0..32].copy_from_slice(&prev.0);
		result[32..64].copy_from_slice(&next.0);
		result[64..72].copy_from_slice(&bucket_start_timestamp.to_be_bytes());
		Self(result)
	}
}

impl Writeable for ChannelPairStatsId {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		writer.write_all(&self.0)
	}
}

impl Readable for ChannelPairStatsId {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let mut bytes = [0u8; 72];
		reader.read_exact(&mut bytes)?;
		Ok(Self(bytes))
	}
}

impl Display for ChannelPairStatsId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", hex_utils::to_string(&self.0))
	}
}

impl Debug for ChannelPairStatsId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "ChannelPairStatsId({})", hex_utils::to_string(&self.0))
	}
}

impl StorableObjectId for ChannelPairStatsId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

/// Aggregated statistics for a specific channel pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelPairForwardingStats {
	/// The unique identifier for this channel pair (derived from channel IDs and bucket timestamp).
	pub id: ChannelPairStatsId,
	/// The previous (inbound) channel ID.
	pub prev_channel_id: ChannelId,
	/// The next (outbound) channel ID.
	pub next_channel_id: ChannelId,
	/// Start of the time bucket (seconds since UNIX epoch).
	pub bucket_start_timestamp: u64,
	/// The previous (inbound) counterparty node id.
	pub prev_node_id: Option<PublicKey>,
	/// The next (outbound) counterparty node id.
	pub next_node_id: Option<PublicKey>,
	/// Number of payments forwarded through this channel pair.
	pub payment_count: u64,
	/// Total amount received on the inbound channel (msat).
	pub total_inbound_amount_msat: u64,
	/// Total amount sent on the outbound channel (msat).
	pub total_outbound_amount_msat: u64,
	/// Total fees earned from this channel pair (msat).
	pub total_fee_earned_msat: u64,
	/// Total skimmed fees (msat).
	pub total_skimmed_fee_msat: u64,
	/// Number of forwards claimed via onchain tx.
	pub onchain_claims_count: u64,
	/// Average fee per payment (msat).
	pub avg_fee_msat: u64,
	/// Average inbound amount per payment (msat).
	pub avg_inbound_amount_msat: u64,
	/// Timestamp of first forward through this channel pair.
	pub first_forwarded_at_timestamp: u64,
	/// Timestamp of most recent forward through this channel pair.
	pub last_forwarded_at_timestamp: u64,
	/// Timestamp when this entry was aggregated from individual payments.
	pub aggregated_at_timestamp: u64,
}

impl_writeable_tlv_based!(ChannelPairForwardingStats, {
	(0, id, required),
	(2, prev_channel_id, required),
	(4, next_channel_id, required),
	(6, prev_node_id, option),
	(8, next_node_id, option),
	(10, payment_count, required),
	(12, total_inbound_amount_msat, required),
	(14, total_outbound_amount_msat, required),
	(16, total_fee_earned_msat, required),
	(18, total_skimmed_fee_msat, required),
	(20, onchain_claims_count, required),
	(22, avg_fee_msat, required),
	(24, avg_inbound_amount_msat, required),
	(26, first_forwarded_at_timestamp, required),
	(28, last_forwarded_at_timestamp, required),
	(30, aggregated_at_timestamp, required),
	(32, bucket_start_timestamp, required),
});

/// Update type for [`ChannelForwardingStats`] that supports incrementing counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelForwardingStatsUpdate {
	/// The channel ID being updated.
	pub channel_id: ChannelId,
	/// The counterparty node id (used when creating new entry).
	pub counterparty_node_id: Option<PublicKey>,
	/// Increment for inbound payment count.
	pub inbound_payments_increment: u64,
	/// Increment for outbound payment count.
	pub outbound_payments_increment: u64,
	/// Increment for total inbound amount.
	pub inbound_amount_increment_msat: u64,
	/// Increment for total outbound amount.
	pub outbound_amount_increment_msat: u64,
	/// Increment for total fee earned.
	pub fee_earned_increment_msat: u64,
	/// Increment for skimmed fee.
	pub skimmed_fee_increment_msat: u64,
	/// Increment for onchain claims count.
	pub onchain_claims_increment: u64,
	/// Current timestamp for updating first/last timestamps.
	pub timestamp: u64,
}

impl StorableObjectUpdate<ChannelForwardingStats> for ChannelForwardingStatsUpdate {
	fn id(&self) -> ChannelId {
		self.channel_id
	}
}

impl StorableObjectId for ChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

impl StorableObject for ChannelForwardingStats {
	type Id = ChannelId;
	type Update = ChannelForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.channel_id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		debug_assert_eq!(
			self.channel_id, update.channel_id,
			"We should only ever update stats for the same channel id"
		);

		let mut updated = false;

		// Update counterparty if not already set
		if self.counterparty_node_id.is_none() && update.counterparty_node_id.is_some() {
			self.counterparty_node_id = update.counterparty_node_id;
			updated = true;
		}

		// Increment counters
		if update.inbound_payments_increment > 0 {
			self.inbound_payments_forwarded += update.inbound_payments_increment;
			updated = true;
		}
		if update.outbound_payments_increment > 0 {
			self.outbound_payments_forwarded += update.outbound_payments_increment;
			updated = true;
		}
		if update.inbound_amount_increment_msat > 0 {
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			updated = true;
		}
		if update.outbound_amount_increment_msat > 0 {
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			updated = true;
		}
		if update.fee_earned_increment_msat > 0 {
			self.total_fee_earned_msat += update.fee_earned_increment_msat;
			updated = true;
		}
		if update.skimmed_fee_increment_msat > 0 {
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			updated = true;
		}
		if update.onchain_claims_increment > 0 {
			self.onchain_claims_count += update.onchain_claims_increment;
			updated = true;
		}

		// Update timestamps
		if updated {
			self.first_forwarded_at_timestamp =
				self.first_forwarded_at_timestamp.min(update.timestamp);
			self.last_forwarded_at_timestamp =
				self.last_forwarded_at_timestamp.max(update.timestamp);
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		// This creates an update representing the current state as increments.
		// This is primarily used for insert_or_update behavior.
		ChannelForwardingStatsUpdate {
			channel_id: self.channel_id,
			counterparty_node_id: self.counterparty_node_id,
			inbound_payments_increment: self.inbound_payments_forwarded,
			outbound_payments_increment: self.outbound_payments_forwarded,
			inbound_amount_increment_msat: self.total_inbound_amount_msat,
			outbound_amount_increment_msat: self.total_outbound_amount_msat,
			fee_earned_increment_msat: self.total_fee_earned_msat,
			skimmed_fee_increment_msat: self.total_skimmed_fee_msat,
			onchain_claims_increment: self.onchain_claims_count,
			timestamp: self.last_forwarded_at_timestamp,
		}
	}
}

/// Update type for [`ChannelPairForwardingStats`] that supports incrementing counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelPairForwardingStatsUpdate {
	/// The channel pair ID being updated.
	pub id: ChannelPairStatsId,
	/// The previous channel ID.
	pub prev_channel_id: ChannelId,
	/// The next channel ID.
	pub next_channel_id: ChannelId,
	/// Start of the time bucket (seconds since UNIX epoch).
	pub bucket_start_timestamp: u64,
	/// The previous node id (used when creating new entry).
	pub prev_node_id: Option<PublicKey>,
	/// The next node id (used when creating new entry).
	pub next_node_id: Option<PublicKey>,
	/// Increment for payment count.
	pub payment_count_increment: u64,
	/// Increment for total inbound amount.
	pub inbound_amount_increment_msat: u64,
	/// Increment for total outbound amount.
	pub outbound_amount_increment_msat: u64,
	/// Increment for total fee earned.
	pub fee_earned_increment_msat: u64,
	/// Increment for skimmed fee.
	pub skimmed_fee_increment_msat: u64,
	/// Increment for onchain claims count.
	pub onchain_claims_increment: u64,
	/// Current timestamp for updating first/last timestamps.
	pub timestamp: u64,
}

impl StorableObjectUpdate<ChannelPairForwardingStats> for ChannelPairForwardingStatsUpdate {
	fn id(&self) -> ChannelPairStatsId {
		self.id
	}
}

impl StorableObject for ChannelPairForwardingStats {
	type Id = ChannelPairStatsId;
	type Update = ChannelPairForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		debug_assert_eq!(
			self.id, update.id,
			"We should only ever update stats for the same channel pair id"
		);

		let mut updated = false;

		// Update node ids if not already set
		if self.prev_node_id.is_none() && update.prev_node_id.is_some() {
			self.prev_node_id = update.prev_node_id;
			updated = true;
		}
		if self.next_node_id.is_none() && update.next_node_id.is_some() {
			self.next_node_id = update.next_node_id;
			updated = true;
		}

		// Increment counters
		if update.payment_count_increment > 0 {
			self.payment_count += update.payment_count_increment;
			updated = true;
		}
		if update.inbound_amount_increment_msat > 0 {
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			updated = true;
		}
		if update.outbound_amount_increment_msat > 0 {
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			updated = true;
		}
		if update.fee_earned_increment_msat > 0 {
			self.total_fee_earned_msat += update.fee_earned_increment_msat;
			updated = true;
		}
		if update.skimmed_fee_increment_msat > 0 {
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			updated = true;
		}
		if update.onchain_claims_increment > 0 {
			self.onchain_claims_count += update.onchain_claims_increment;
			updated = true;
		}

		// Update timestamps
		if updated {
			if self.first_forwarded_at_timestamp == 0 {
				self.first_forwarded_at_timestamp = update.timestamp;
			}
			self.last_forwarded_at_timestamp = update.timestamp;

			// Recalculate averages
			if self.payment_count > 0 {
				self.avg_fee_msat = self.total_fee_earned_msat / self.payment_count;
				self.avg_inbound_amount_msat = self.total_inbound_amount_msat / self.payment_count;
			}
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		// This creates an update representing the current state as increments.
		// This is primarily used for insert_or_update behavior.
		ChannelPairForwardingStatsUpdate {
			id: self.id,
			prev_channel_id: self.prev_channel_id,
			next_channel_id: self.next_channel_id,
			bucket_start_timestamp: self.bucket_start_timestamp,
			prev_node_id: self.prev_node_id,
			next_node_id: self.next_node_id,
			payment_count_increment: self.payment_count,
			inbound_amount_increment_msat: self.total_inbound_amount_msat,
			outbound_amount_increment_msat: self.total_outbound_amount_msat,
			fee_earned_increment_msat: self.total_fee_earned_msat,
			skimmed_fee_increment_msat: self.total_skimmed_fee_msat,
			onchain_claims_increment: self.onchain_claims_count,
			timestamp: self.last_forwarded_at_timestamp,
		}
	}
}

/// Aggregate expired forwarded payments into time-bucketed channel pair statistics.
///
/// Returns (number of buckets created, number of payments aggregated).
pub(crate) fn aggregate_expired_forwarded_payments(
	forwarded_payment_store: &DataStore<ForwardedPaymentDetails, Arc<Logger>>,
	channel_pair_stats_store: &DataStore<ChannelPairForwardingStats, Arc<Logger>>,
	retention_minutes: u64, logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
	let cutoff = now.saturating_sub(retention_minutes * 60);
	let bucket_size_secs = retention_minutes * 60;

	log_debug!(
		logger,
		"Aggregating forwarded payments older than {retention_minutes} mins (cutoff timestamp: {cutoff})"
	);

	// Get all expired payments
	let expired = forwarded_payment_store.list_filter(|p| p.forwarded_at_timestamp < cutoff);

	if expired.is_empty() {
		log_debug!(logger, "No expired forwarded payments found");
		return Ok((0, 0));
	}

	log_debug!(logger, "Found {} expired forwarded payments to aggregate", expired.len());

	// Group by (channel_pair, bucket_start_timestamp)
	let mut bucket_groups: HashMap<(ChannelId, ChannelId, u64), Vec<&ForwardedPaymentDetails>> =
		HashMap::new();

	for payment in &expired {
		// Calculate which bucket this payment belongs to
		let bucket_start = (payment.forwarded_at_timestamp / bucket_size_secs) * bucket_size_secs;
		let key = (payment.prev_channel_id, payment.next_channel_id, bucket_start);
		bucket_groups.entry(key).or_insert_with(Vec::new).push(payment);
	}

	log_debug!(logger, "Grouped into {} time buckets", bucket_groups.len());

	// Aggregate each bucket
	let mut aggregated_bucket_count = 0u64;
	let mut removed_payment_count = 0u64;

	for ((prev_channel_id, next_channel_id, bucket_start), payments) in bucket_groups {
		debug_assert!(!payments.is_empty(), "Each bucket group should have at least one payment");

		// Calculate aggregated values
		let mut total_inbound_amount_msat = 0u64;
		let mut total_outbound_amount_msat = 0u64;
		let mut total_fee_earned_msat = 0u64;
		let mut total_skimmed_fee_msat = 0u64;
		let mut onchain_claims_count = 0u64;
		let mut first_timestamp = u64::MAX;
		let mut last_timestamp = 0u64;

		// Use first payment for node IDs (they should all be the same for a channel pair)
		let first_payment = payments[0];
		let prev_node_id = first_payment.prev_node_id;
		let next_node_id = first_payment.next_node_id;

		for payment in &payments {
			let outbound = payment.outbound_amount_forwarded_msat.unwrap_or(0);
			let fee = payment.total_fee_earned_msat.unwrap_or(0);
			let skimmed = payment.skimmed_fee_msat.unwrap_or(0);

			total_inbound_amount_msat =
				total_inbound_amount_msat.saturating_add(outbound.saturating_add(fee));
			total_outbound_amount_msat = total_outbound_amount_msat.saturating_add(outbound);
			total_fee_earned_msat = total_fee_earned_msat.saturating_add(fee);
			total_skimmed_fee_msat = total_skimmed_fee_msat.saturating_add(skimmed);
			if payment.claim_from_onchain_tx {
				onchain_claims_count += 1;
			}
			first_timestamp = first_timestamp.min(payment.forwarded_at_timestamp);
			last_timestamp = last_timestamp.max(payment.forwarded_at_timestamp);
		}

		let payment_count = payments.len() as u64;
		let avg_fee_msat = total_fee_earned_msat / payment_count;
		let avg_inbound_amount_msat = total_inbound_amount_msat / payment_count;

		let pair_id = ChannelPairStatsId::from_channel_pair_and_bucket(
			&prev_channel_id,
			&next_channel_id,
			bucket_start,
		);

		// Create the bucket stats entry
		let stats = ChannelPairForwardingStats {
			id: pair_id,
			prev_channel_id,
			next_channel_id,
			bucket_start_timestamp: bucket_start,
			prev_node_id,
			next_node_id,
			payment_count,
			total_inbound_amount_msat,
			total_outbound_amount_msat,
			total_fee_earned_msat,
			total_skimmed_fee_msat,
			onchain_claims_count,
			avg_fee_msat,
			avg_inbound_amount_msat,
			first_forwarded_at_timestamp: first_timestamp,
			last_forwarded_at_timestamp: last_timestamp,
			aggregated_at_timestamp: now,
		};

		// Insert the bucket (should be unique - no update needed)
		channel_pair_stats_store.insert(stats).map_err(|e| {
			log_error!(logger, "Failed to insert channel pair stats bucket for {pair_id:?}: {e}");
			e
		})?;

		aggregated_bucket_count += 1;

		// Remove aggregated payments
		for payment in payments {
			forwarded_payment_store.remove(&payment.id).map_err(|e| {
				log_error!(logger, "Failed to remove forwarded payment {:?}: {}", payment.id, e);
				e
			})?;
			removed_payment_count += 1;
		}
	}

	log_debug!(
		logger,
		"Successfully aggregated {} payments into {} time buckets",
		removed_payment_count,
		aggregated_bucket_count
	);

	Ok((aggregated_bucket_count, removed_payment_count))
}
