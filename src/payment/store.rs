// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::hex_utils;
use crate::io::{
	PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;
use crate::Error;

use bitcoin::hashes::Hash;
use bitcoin::io::Read;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::OfferId;
use lightning::offers::refund::Refund;
use lightning::onion_message::dns_resolution::{DNSSECProof, HumanReadableName};
use lightning::util::ser::{Hostname, Readable, Writeable, Writer};
use lightning::util::string::UntrustedString;
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based,
	impl_writeable_tlv_based_enum, write_tlv_fields,
};

use lightning_invoice::Bolt11Invoice;
use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};

use bitcoin::{BlockHash, Txid};

use std::collections::hash_map;
use std::collections::HashMap;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

	pub(crate) fn update(&mut self, update: &PaymentDetailsUpdate) -> bool {
		debug_assert_eq!(
			self.id, update.id,
			"We should only ever override payment data for the same payment id"
		);

		let mut updated = false;

		macro_rules! update_if_necessary {
			($val: expr, $update: expr) => {
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

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum PaymentStoreUpdateResult {
	Updated,
	Unchanged,
	NotFound,
}

pub(crate) struct PaymentStore<L: Deref>
where
	L::Target: LdkLogger,
{
	payments: Mutex<HashMap<PaymentId, PaymentDetails>>,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> PaymentStore<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(payments: Vec<PaymentDetails>, kv_store: Arc<DynStore>, logger: L) -> Self {
		let payments = Mutex::new(HashMap::from_iter(
			payments.into_iter().map(|payment| (payment.id, payment)),
		));
		Self { payments, kv_store, logger }
	}

	pub(crate) fn insert(&self, payment: PaymentDetails) -> Result<bool, Error> {
		let mut locked_payments = self.payments.lock().unwrap();

		let updated = locked_payments.insert(payment.id, payment.clone()).is_some();
		self.persist_info(&payment.id, &payment)?;
		Ok(updated)
	}

	pub(crate) fn insert_or_update(&self, payment: &PaymentDetails) -> Result<bool, Error> {
		let mut locked_payments = self.payments.lock().unwrap();

		let updated;
		match locked_payments.entry(payment.id) {
			hash_map::Entry::Occupied(mut e) => {
				let update = payment.into();
				updated = e.get_mut().update(&update);
				if updated {
					self.persist_info(&payment.id, e.get())?;
				}
			},
			hash_map::Entry::Vacant(e) => {
				e.insert(payment.clone());
				self.persist_info(&payment.id, payment)?;
				updated = true;
			},
		}

		Ok(updated)
	}

	pub(crate) fn remove(&self, id: &PaymentId) -> Result<(), Error> {
		let removed = self.payments.lock().unwrap().remove(id).is_some();
		if removed {
			let store_key = hex_utils::to_string(&id.0);
			self.kv_store
				.remove(
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					&store_key,
					false,
				)
				.map_err(|e| {
					log_error!(
						self.logger,
						"Removing payment data for key {}/{}/{} failed due to: {}",
						PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
						PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
						store_key,
						e
					);
					Error::PersistenceFailed
				})?;
		}
		Ok(())
	}

	pub(crate) fn get(&self, id: &PaymentId) -> Option<PaymentDetails> {
		self.payments.lock().unwrap().get(id).cloned()
	}

	pub(crate) fn update(
		&self, update: &PaymentDetailsUpdate,
	) -> Result<PaymentStoreUpdateResult, Error> {
		let mut locked_payments = self.payments.lock().unwrap();

		if let Some(payment) = locked_payments.get_mut(&update.id) {
			let updated = payment.update(update);
			if updated {
				self.persist_info(&update.id, payment)?;
				Ok(PaymentStoreUpdateResult::Updated)
			} else {
				Ok(PaymentStoreUpdateResult::Unchanged)
			}
		} else {
			Ok(PaymentStoreUpdateResult::NotFound)
		}
	}

	pub(crate) fn list_filter<F: FnMut(&&PaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<PaymentDetails> {
		self.payments.lock().unwrap().values().filter(f).cloned().collect::<Vec<PaymentDetails>>()
	}

	fn persist_info(&self, id: &PaymentId, payment: &PaymentDetails) -> Result<(), Error> {
		let store_key = hex_utils::to_string(&id.0);
		let data = payment.encode();
		self.kv_store
			.write(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key,
				&data,
			)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Write for key {}/{}/{} failed due to: {}",
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					store_key,
					e
				);
				Error::PersistenceFailed
			})?;
		Ok(())
	}
}

/// Represents a payment metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentMetadata {
	/// The identifier of this payment.
	pub id: PaymentId,
	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	pub latest_update_timestamp: u64,
	/// The direction of the payment.
	pub direction: PaymentDirection,
	/// The status of the payment.
	pub status: PaymentStatus,
	/// The metadata detail of the payment.
	///
	/// This can be a BOLT 11 invoice, a BOLT 12 offer, or a BOLT 12 refund.
	pub payment_metadata_detail: PaymentMetadataDetail,
	/// The limits applying to how much fee we allow an LSP to deduct from the payment amount.
	pub jit_channel_fee_limit: Option<JitChannelFeeLimits>,
}

impl PaymentMetadata {
	pub(crate) fn new(
		id: PaymentId, direction: PaymentDirection, payment_metadata_detail: PaymentMetadataDetail,
		jit_channel_fee_limit: Option<JitChannelFeeLimits>, status: PaymentStatus,
	) -> Self {
		let latest_update_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();

		PaymentMetadata {
			id,
			latest_update_timestamp,
			direction,
			payment_metadata_detail,
			jit_channel_fee_limit,
			status,
		}
	}

	pub(crate) fn update(&mut self, update: &PaymentMetadataUpdate) -> bool {
		debug_assert_eq!(
			self.id, update.id,
			"We should only ever override payment metadata data for the same payment id"
		);

		let mut updated = false;

		macro_rules! update_if_necessary {
			($val: expr, $update: expr) => {
				if $val != $update {
					$val = $update;
					updated = true;
				}
			};
		}

		if let Some(preimage_opt) = update.preimage {
			match self.payment_metadata_detail {
				PaymentMetadataDetail::Bolt11 { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, Some(preimage_opt));
				},
				_ => {},
			}
		}

		if let Some(hrn_opt) = update.hrn.clone() {
			match self.payment_metadata_detail {
				PaymentMetadataDetail::Bolt12Offer { ref mut hrn, .. } => {
					update_if_necessary!(*hrn, Some(hrn_opt.clone()));
				},
				PaymentMetadataDetail::Bolt12Refund { ref mut hrn, .. } => {
					update_if_necessary!(*hrn, Some(hrn_opt.clone()));
				},
				_ => {},
			}
		}

		if let Some(dnssec_proof_opt) = update.dnssec_proof.clone() {
			match self.payment_metadata_detail {
				PaymentMetadataDetail::Bolt12Offer { ref mut dnssec_proof, .. } => {
					update_if_necessary!(*dnssec_proof, Some(dnssec_proof_opt.clone()));
				},
				PaymentMetadataDetail::Bolt12Refund { ref mut dnssec_proof, .. } => {
					update_if_necessary!(*dnssec_proof, Some(dnssec_proof_opt.clone()));
				},
				_ => {},
			}
		}

		if let Some(direction) = update.direction {
			update_if_necessary!(self.direction, direction);
		}
		if let Some(counterparty_skimmed_fee_msat) = update.counterparty_skimmed_fee_msat {
			if let Some(jit_channel_fee_limit) = &mut self.jit_channel_fee_limit {
				update_if_necessary!(
					jit_channel_fee_limit.counterparty_skimmed_fee_msat,
					Some(counterparty_skimmed_fee_msat)
				);
			} else {
				updated = true;
			}
		}
		if let Some(state) = &update.status {
			update_if_necessary!(self.status, *state);
		}

		if updated {
			self.latest_update_timestamp = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or(Duration::from_secs(0))
				.as_secs();
		}

		updated
	}
}

impl Writeable for PaymentMetadata {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		write_tlv_fields!(writer, {
			(0, self.id, required),
			(1, self.latest_update_timestamp, required),
			(2, self.direction, required),
			(3, self.status, required),
			(4, self.payment_metadata_detail, required),
			(5, self.jit_channel_fee_limit, option),
		});
		Ok(())
	}
}

impl Readable for PaymentMetadata {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<PaymentMetadata, DecodeError> {
		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, id, required),
			(1, latest_update_timestamp, required),
			(2, direction, required),
			(3, status, required),
			(4, payment_metadata_detail, required),
			(5, jit_channel_fee_limit, option),
		});

		Ok(PaymentMetadata {
			id: id.0.ok_or(DecodeError::InvalidValue)?,
			latest_update_timestamp: latest_update_timestamp.0.ok_or(DecodeError::InvalidValue)?,
			direction: direction.0.ok_or(DecodeError::InvalidValue)?,
			status: status.0.ok_or(DecodeError::InvalidValue)?,
			payment_metadata_detail: payment_metadata_detail.0.ok_or(DecodeError::InvalidValue)?,
			jit_channel_fee_limit,
		})
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Defines fee limits for Just-In-Time (JIT) channels opened by a Lightning Service Provider (LSP)
/// to provide inbound liquidity, as per the LSPS2 protocol (bLIP-52). Used in `PaymentMetadata` to
/// track and constrain costs associated with dynamically opened channels for payments.
pub struct JitChannelFeeLimits {
	/// The maximal total amount we allow any configured LSP withhold from us when forwarding the
	/// payment.
	pub max_total_opening_fee_msat: Option<u64>,
	/// The maximal proportional fee, in parts-per-million millisatoshi, we allow any configured
	/// LSP withhold from us when forwarding the payment.
	pub max_proportional_opening_fee_ppm_msat: Option<u64>,
	/// The value, in thousands of a satoshi, that was deducted from this payment as an extra
	/// fee taken by our channel counterparty.
	///
	/// Will only be `Some` once we received the payment. Will always be `None` for LDK Node
	/// v0.4 and prior.
	pub counterparty_skimmed_fee_msat: Option<u64>,
}
impl_writeable_tlv_based!(JitChannelFeeLimits, {
	(0, max_total_opening_fee_msat, option),
	(1, max_proportional_opening_fee_ppm_msat, option),
	(2, counterparty_skimmed_fee_msat, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
/// Represents the metadata details of a payment.
///
/// This enum encapsulates various types of payment metadata, such as BOLT 11 invoices,
/// BOLT 12 offers, and BOLT 12 refunds, along with their associated details.
pub enum PaymentMetadataDetail {
	/// A [BOLT 11] metadata.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	Bolt11 {
		/// The invoice associated with the payment.
		invoice: String,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
	},
	/// A [BOLT 12] 'offer' payment metadata, i.e., a payment metadata for an [`Offer`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Offer`]: crate::lightning::offers::offer::Offer
	Bolt12Offer {
		/// The ID of the offer this payment is for.
		offer_id: OfferId,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The DNSSEC proof associated with the payment.
		dnssec_proof: Option<DNSSECProofWrapper>,
		/// The human-readable name associated with the payment.
		hrn: Option<HumanReadableName>,
		/// The quantity of an item requested in the offer.
		quantity: Option<u64>,
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: Option<PaymentHash>,
	},
	/// A [BOLT 12] 'refund' payment metadata, i.e., a payment metadata for a [`Refund`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Refund`]: lightning::offers::refund::Refund
	Bolt12Refund {
		/// The refund details associated with the payment.
		refund: Refund,
		/// The human-readable name associated with the refund payment.
		hrn: Option<HumanReadableName>,
		/// The DNSSEC proof associated with the refund payment.
		dnssec_proof: Option<DNSSECProofWrapper>,
		/// The invoice associated with the refund payment.
		invoice: Vec<u8>,
		/// The pre-image used by the refund payment.
		preimage: Option<PaymentPreimage>,
	},
}
impl_writeable_tlv_based_enum! { PaymentMetadataDetail,
	(0, Bolt11) => {
		(0, invoice, required),
		(1, preimage, option),
	},
	(4, Bolt12Offer) => {
		(0, offer_id, required),
		(1, preimage, option),
		(2, dnssec_proof, option),
		(3, hrn, option),
		(4, quantity, option),
		(5, hash, option),
	},
	(6, Bolt12Refund) => {
		(0, refund, required),
		(1, hrn, option),
		(2, dnssec_proof, option),
		(4, invoice, required),
		(5, preimage, option),
	}
}
#[derive(Clone, Debug, PartialEq, Eq)]
/// A wrapper for `DNSSECProof` to enable serialization and deserialization,
/// allowing it to be stored in the payment store.
pub struct DNSSECProofWrapper(pub DNSSECProof);

impl Writeable for DNSSECProofWrapper {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.name.as_str().len() as u8).write(w)?;
		w.write_all(&self.0.name.as_str().as_bytes())?;
		self.0.proof.write(w)?;

		Ok(())
	}
}

impl Readable for DNSSECProofWrapper {
	fn read<R: Read>(r: &mut R) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let s = Hostname::read(r)?;
		let name = s.try_into().map_err(|_| DecodeError::InvalidValue)?;
		let proof = Vec::<u8>::read(r)?;

		Ok(DNSSECProofWrapper(DNSSECProof { name, proof }))
	}
}

pub(crate) struct PaymentMetadataStore<L: Deref>
where
	L::Target: LdkLogger,
{
	metadata: Mutex<HashMap<PaymentId, PaymentMetadata>>,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> PaymentMetadataStore<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(metadata: Vec<PaymentMetadata>, kv_store: Arc<DynStore>, logger: L) -> Self {
		let metadata = Mutex::new(HashMap::from_iter(
			metadata.into_iter().map(|payment| (payment.id, payment)),
		));
		Self { metadata, kv_store, logger }
	}

	pub(crate) fn insert(&self, payment: PaymentMetadata) -> Result<bool, Error> {
		let mut locked_payments = self.metadata.lock().unwrap();

		let updated = locked_payments.insert(payment.id, payment.clone()).is_some();
		self.persist_info(&payment.id, &payment)?;
		Ok(updated)
	}

	pub(crate) fn remove(&self, id: &PaymentId) -> Result<(), Error> {
		let removed = self.metadata.lock().unwrap().remove(id).is_some();
		if removed {
			let store_key = hex_utils::to_string(&id.0);
			self.kv_store
				.remove(
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					&store_key,
					false,
				)
				.map_err(|e| {
					log_error!(
						self.logger,
						"Removing payment data for key {}/{}/{} failed due to: {}",
						PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
						PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
						store_key,
						e
					);
					Error::PersistenceFailed
				})?;
		}
		Ok(())
	}

	pub(crate) fn get(&self, id: &PaymentId) -> Option<PaymentMetadata> {
		self.metadata.lock().unwrap().get(id).cloned()
	}

	pub(crate) fn update(
		&self, update: &PaymentMetadataUpdate,
	) -> Result<PaymentStoreUpdateResult, Error> {
		let mut locked_payments_metadata = self.metadata.lock().unwrap();

		if let Some(payment) = locked_payments_metadata.get_mut(&update.id) {
			let updated = payment.update(update);
			if updated {
				self.persist_info(&update.id, payment)?;
				Ok(PaymentStoreUpdateResult::Updated)
			} else {
				Ok(PaymentStoreUpdateResult::Unchanged)
			}
		} else {
			Ok(PaymentStoreUpdateResult::NotFound)
		}
	}

	pub(crate) fn list_filter<F: FnMut(&&PaymentMetadata) -> bool>(
		&self, f: F,
	) -> Vec<PaymentMetadata> {
		self.metadata.lock().unwrap().values().filter(f).cloned().collect::<Vec<PaymentMetadata>>()
	}

	fn persist_info(&self, id: &PaymentId, payment: &PaymentMetadata) -> Result<(), Error> {
		let store_key = hex_utils::to_string(&id.0);
		let data = payment.encode();
		self.kv_store
			.write(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key,
				&data,
			)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Write for key {}/{}/{} failed due to: {}",
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					store_key,
					e
				);
				Error::PersistenceFailed
			})?;
		Ok(())
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentMetadataUpdate {
	pub id: PaymentId,
	pub direction: Option<PaymentDirection>,
	pub preimage: Option<PaymentPreimage>,
	pub status: Option<PaymentStatus>,
	pub hrn: Option<HumanReadableName>, // Hashed Recipient Name
	pub dnssec_proof: Option<DNSSECProofWrapper>, // Prova DNSSEC
	pub counterparty_skimmed_fee_msat: Option<u64>,
}

impl PaymentMetadataUpdate {
	pub fn new(id: PaymentId) -> Self {
		Self {
			id,
			direction: None,
			preimage: None,
			status: None,
			hrn: None,
			dnssec_proof: None,
			counterparty_skimmed_fee_msat: None,
		}
	}
}

impl From<&PaymentMetadata> for PaymentMetadataUpdate {
	fn from(value: &PaymentMetadata) -> Self {
		let (preimage, hrn, dnssec_proof) = match &value.payment_metadata_detail {
			PaymentMetadataDetail::Bolt11 { preimage, .. } => (preimage, None, None),
			PaymentMetadataDetail::Bolt12Offer { preimage, hrn, dnssec_proof, .. } => {
				(preimage, hrn.clone(), dnssec_proof.clone())
			},
			PaymentMetadataDetail::Bolt12Refund { preimage, hrn, dnssec_proof, .. } => {
				(preimage, hrn.clone(), dnssec_proof.clone())
			},
		};
		let counterparty_skimmed_fee_msat = match value.jit_channel_fee_limit {
			Some(ref limit) => limit.counterparty_skimmed_fee_msat,
			None => None,
		};

		Self {
			id: value.id,
			direction: Some(value.direction),
			preimage: *preimage,
			status: Some(value.status.clone()),
			hrn,
			dnssec_proof,
			counterparty_skimmed_fee_msat,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A struct that holds both `PaymentDetails` and `PaymentMetadata`.
/// Ensures that if one does not exist, the other must exist, aiding in the backward compatibility
/// between `PaymentDetails` and `PaymentMetadata` for pending payments.
pub struct PaymentDataWithFallback {
	pub payment_metadata: Option<PaymentMetadata>,
	pub payment_details: Option<PaymentDetails>,
	pub status: PaymentStatus,
	pub direction: PaymentDirection,
}

impl PaymentDataWithFallback {
	pub(crate) fn new<L, E>(
		payment_id: &PaymentId, logger: Arc<L>, metadata_store: Arc<PaymentMetadataStore<L>>,
		payment_store: Arc<PaymentStore<L>>, error_fn: impl Fn() -> E,
	) -> Result<Self, E>
	where
		L: Deref,
		L::Target: LdkLogger,
	{
		let mut payment_metadata = None;
		let mut payment_details = None;
		let mut status = None;
		let mut direction = None;

		if let Some(metadata) = metadata_store.get(&payment_id) {
			payment_metadata = Some(metadata.clone());
			status = Some(metadata.status);
			direction = Some(metadata.direction);
		}
		if let Some(payment) = payment_store.get(&payment_id) {
			payment_details = Some(payment.clone());
			status = Some(payment.status);
			direction = Some(payment.direction);
		}
		if status.is_none() || direction.is_none() {
			log_error!(
				logger,
				"Payment with id {} not found in either metadata or payment store",
				payment_id
			);
			return Err(error_fn());
		}

		Ok(Self {
			payment_metadata,
			payment_details,
			direction: direction.unwrap(),
			status: status.unwrap(),
		})
	}

	/// Retrieves payment details, creating them from payment metadata if they don't exist.
	/// Returns an error from `error_fn` if neither details nor metadata are available.
	pub fn get_payment_detail<E>(&self, error_fn: impl Fn() -> E) -> Result<PaymentDetails, E> {
		if let Some(detail) = &self.payment_details {
			return Ok(detail.clone());
		} else if let Some(metadata) = &self.payment_metadata {
			let mut amount_msat = None;
			let kind = match &metadata.payment_metadata_detail {
				PaymentMetadataDetail::Bolt11 { invoice, preimage } => {
					let invoice = Bolt11Invoice::from_str(invoice.as_str()).unwrap();
					let payment_hash = PaymentHash(invoice.payment_hash().to_byte_array());
					let payment_secret = invoice.payment_secret();
					amount_msat = invoice.amount_milli_satoshis();
					match &metadata.jit_channel_fee_limit {
						Some(jit_channel_fee_limit) => {
							let lsp_fee_limits = LSPFeeLimits {
								max_proportional_opening_fee_ppm_msat: jit_channel_fee_limit
									.max_proportional_opening_fee_ppm_msat,
								max_total_opening_fee_msat: jit_channel_fee_limit
									.max_total_opening_fee_msat,
							};
							PaymentKind::Bolt11Jit {
								hash: payment_hash,
								secret: Some(payment_secret.clone()),
								preimage: *preimage,
								lsp_fee_limits,
								counterparty_skimmed_fee_msat: jit_channel_fee_limit
									.counterparty_skimmed_fee_msat,
							}
						},
						None => PaymentKind::Bolt11 {
							hash: payment_hash,
							secret: Some(payment_secret.clone()),
							preimage: *preimage,
						},
					}
				},
				PaymentMetadataDetail::Bolt12Offer {
					offer_id, preimage, hash, quantity, ..
				} => PaymentKind::Bolt12Offer {
					hash: *hash,
					offer_id: *offer_id,
					preimage: *preimage,
					quantity: *quantity,
					secret: None,
					payer_note: None,
				},
				PaymentMetadataDetail::Bolt12Refund { refund, preimage, invoice, .. } => {
					let invoice = Bolt12Invoice::try_from(invoice.to_vec()).unwrap();
					let payment_hash = PaymentHash(invoice.payment_hash().0);
					amount_msat = Some(invoice.amount_msats());
					PaymentKind::Bolt12Refund {
						preimage: *preimage,
						hash: Some(payment_hash),
						secret: None,
						payer_note: refund
							.payer_note()
							.map(|note| UntrustedString(note.0.to_string())),
						quantity: refund.quantity(),
					}
				},
			};

			return Ok(PaymentDetails::new(
				metadata.id,
				kind,
				amount_msat,
				None,
				metadata.direction,
				metadata.status,
			));
		} else {
			return Err(error_fn());
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use bitcoin::io::Cursor;
	use lightning::util::{
		ser::Readable,
		test_utils::{TestLogger, TestStore},
	};
	use std::sync::Arc;

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
	fn payment_info_is_persisted() {
		let store: Arc<DynStore> = Arc::new(TestStore::new(false));
		let logger = Arc::new(TestLogger::new());
		let payment_store = PaymentStore::new(Vec::new(), Arc::clone(&store), logger);

		let hash = PaymentHash([42u8; 32]);
		let id = PaymentId([42u8; 32]);
		assert!(payment_store.get(&id).is_none());

		let store_key = hex_utils::to_string(&hash.0);
		assert!(store
			.read(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key
			)
			.is_err());

		let kind = PaymentKind::Bolt11 { hash, preimage: None, secret: None };
		let payment = PaymentDetails::new(
			id,
			kind,
			None,
			None,
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);

		assert_eq!(Ok(false), payment_store.insert(payment.clone()));
		assert!(payment_store.get(&id).is_some());
		assert!(store
			.read(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key
			)
			.is_ok());

		assert_eq!(Ok(true), payment_store.insert(payment));
		assert!(payment_store.get(&id).is_some());

		// Check update returns `Updated`
		let mut update = PaymentDetailsUpdate::new(id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(PaymentStoreUpdateResult::Updated), payment_store.update(&update));

		// Check no-op update yields `Unchanged`
		let mut update = PaymentDetailsUpdate::new(id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(PaymentStoreUpdateResult::Unchanged), payment_store.update(&update));

		// Check bogus update yields `NotFound`
		let bogus_id = PaymentId([84u8; 32]);
		let mut update = PaymentDetailsUpdate::new(bogus_id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(PaymentStoreUpdateResult::NotFound), payment_store.update(&update));

		assert!(payment_store.get(&id).is_some());

		assert_eq!(PaymentStatus::Succeeded, payment_store.get(&id).unwrap().status);
	}

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

	#[test]
	fn payment_metadata_info_is_persisted() {
		let store: Arc<DynStore> = Arc::new(TestStore::new(false));
		let logger = Arc::new(TestLogger::new());
		let metadata_store = PaymentMetadataStore::new(Vec::new(), Arc::clone(&store), logger);

		let hash = PaymentHash([42u8; 32]);
		let id = PaymentId([42u8; 32]);
		assert!(metadata_store.get(&id).is_none());

		let store_key = hex_utils::to_string(&hash.0);
		assert!(store
			.read(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key
			)
			.is_err());
		let payment_metadata_detail =
			PaymentMetadataDetail::Bolt11 { invoice: "test".to_string(), preimage: None };
		let payment_metadata = PaymentMetadata::new(
			id,
			PaymentDirection::Inbound,
			payment_metadata_detail,
			None,
			PaymentStatus::Pending,
		);

		assert_eq!(Ok(false), metadata_store.insert(payment_metadata.clone()));
		assert!(metadata_store.get(&id).is_some());
		assert!(store
			.read(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_METADATA_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key
			)
			.is_ok());

		assert_eq!(Ok(true), metadata_store.insert(payment_metadata.clone()));
		assert!(metadata_store.get(&id).is_some());

		// Check update returns `Updated`
		let mut update = PaymentMetadataUpdate::new(id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(PaymentStoreUpdateResult::Updated), metadata_store.update(&update));

		// Check no-op update yields `Unchanged`
		let mut update = PaymentMetadataUpdate::new(id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(PaymentStoreUpdateResult::Unchanged), metadata_store.update(&update));

		// Check bogus update yields `NotFound`
		let bogus_id = PaymentId([84u8; 32]);
		let mut update = PaymentMetadataUpdate::new(bogus_id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(PaymentStoreUpdateResult::NotFound), metadata_store.update(&update));

		assert!(metadata_store.get(&id).is_some());

		assert_eq!(PaymentStatus::Succeeded, metadata_store.get(&id).unwrap().status);

		// Check remove
		assert_eq!(Ok(()), metadata_store.remove(&id));
		assert!(metadata_store.get(&id).is_none());
	}
}
