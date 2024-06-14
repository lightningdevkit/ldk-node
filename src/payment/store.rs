use crate::hex_utils;
use crate::io::{
	PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, Logger};
use crate::types::{DynStore, TlvEntry};
use crate::Error;

use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::util::ser::{Readable, Writeable};
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based,
	impl_writeable_tlv_based_enum, write_tlv_fields,
};

use std::collections::HashMap;
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time;

/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentDetails {
	/// The identifier of this payment.
	pub id: PaymentId,
	/// The kind of the payment.
	pub kind: PaymentKind,
	/// The amount transferred.
	pub amount_msat: Option<u64>,
	/// The direction of the payment.
	pub direction: PaymentDirection,
	/// The status of the payment.
	pub status: PaymentStatus,
	/// Last update timestamp, as seconds since Unix epoch.
	pub last_update: u64,
	/// Fee paid.
	pub fee_msat: Option<u64>,
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
			(6, self.amount_msat, required),
			(8, self.direction, required),
			(10, self.status, required),
			(131074, Some(self.last_update), option),
			(131076, self.fee_msat, option),
		});
		Ok(())
	}
}

impl Readable for PaymentDetails {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<PaymentDetails, DecodeError> {
		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, id, required), // Used to be `hash`
			(1, lsp_fee_limits, option),
			(2, preimage, required),
			(3, kind_opt, option),
			(4, secret, required),
			(6, amount_msat, required),
			(8, direction, required),
			(10, status, required),
			(131074, last_update, option),
			(131076, fee_msat, option),
		});

		let id: PaymentId = id.0.ok_or(DecodeError::InvalidValue)?;
		let preimage: Option<PaymentPreimage> = preimage.0.ok_or(DecodeError::InvalidValue)?;
		let secret: Option<PaymentSecret> = secret.0.ok_or(DecodeError::InvalidValue)?;
		let amount_msat: Option<u64> = amount_msat.0.ok_or(DecodeError::InvalidValue)?;
		let direction: PaymentDirection = direction.0.ok_or(DecodeError::InvalidValue)?;
		let status: PaymentStatus = status.0.ok_or(DecodeError::InvalidValue)?;
		let last_update: u64 = last_update.unwrap_or(0);

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
					PaymentKind::Bolt11Jit { hash, preimage, secret, lsp_fee_limits }
				} else {
					PaymentKind::Bolt11 { hash, preimage, secret, bolt11_invoice: None }
				}
			} else {
				PaymentKind::Spontaneous { hash, preimage, custom_tlvs: Vec::new() }
			}
		};

		Ok(PaymentDetails { id, kind, amount_msat, direction, status, last_update, fee_msat })
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
	(1, Outbound) => {};
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
	(4, Failed) => {};
);

/// Represents the kind of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentKind {
	/// An on-chain payment.
	Onchain,
	/// A [BOLT 11] payment.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	// TODO: Bolt11 { invoice: Option<Bolt11Invoice> },
	Bolt11 {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The invoice that was paid.
		bolt11_invoice: Option<String>,
	},
	/// A [BOLT 11] payment intended to open an [LSPS 2] just-in-time channel.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [LSPS 2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	// TODO: Bolt11Jit { invoice: Option<Bolt11Invoice> },
	Bolt11Jit {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
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
	/// A spontaneous ("keysend") payment.
	Spontaneous {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// Custom TLVs.
		custom_tlvs: Vec<TlvEntry>,
	},
}

impl_writeable_tlv_based_enum!(PaymentKind,
	(0, Onchain) => {},
	(2, Bolt11) => {
		(0, hash, required),
		(2, preimage, option),
		(4, secret, option),
		(131072, bolt11_invoice, option),
	},
	(4, Bolt11Jit) => {
		(0, hash, required),
		(2, preimage, option),
		(4, secret, option),
		(6, lsp_fee_limits, required),
	},
	(8, Spontaneous) => {
		(0, hash, required),
		(2, preimage, option),
		(131072, custom_tlvs, optional_vec),
	};
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
	pub preimage: Option<Option<PaymentPreimage>>,
	pub secret: Option<Option<PaymentSecret>>,
	pub amount_msat: Option<Option<u64>>,
	pub direction: Option<PaymentDirection>,
	pub status: Option<PaymentStatus>,
	pub fee_msat: Option<Option<u64>>,
}

impl PaymentDetailsUpdate {
	pub fn new(id: PaymentId) -> Self {
		Self {
			id,
			preimage: None,
			secret: None,
			amount_msat: None,
			direction: None,
			status: None,
			fee_msat: None,
		}
	}
}

pub(crate) struct PaymentStore<L: Deref>
where
	L::Target: Logger,
{
	payments: Mutex<HashMap<PaymentId, PaymentDetails>>,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> PaymentStore<L>
where
	L::Target: Logger,
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

	pub(crate) fn remove(&self, id: &PaymentId) -> Result<(), Error> {
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
			})
	}

	pub(crate) fn get(&self, id: &PaymentId) -> Option<PaymentDetails> {
		self.payments.lock().unwrap().get(id).cloned()
	}

	pub(crate) fn update(&self, update: &PaymentDetailsUpdate) -> Result<bool, Error> {
		let mut updated = false;
		let mut locked_payments = self.payments.lock().unwrap();

		if let Some(payment) = locked_payments.get_mut(&update.id) {
			if let Some(preimage_opt) = update.preimage {
				match payment.kind {
					PaymentKind::Bolt11 { ref mut preimage, .. } => *preimage = preimage_opt,
					PaymentKind::Bolt11Jit { ref mut preimage, .. } => *preimage = preimage_opt,
					PaymentKind::Spontaneous { ref mut preimage, .. } => *preimage = preimage_opt,
					_ => {},
				}
			}

			if let Some(secret_opt) = update.secret {
				match payment.kind {
					PaymentKind::Bolt11 { ref mut secret, .. } => *secret = secret_opt,
					PaymentKind::Bolt11Jit { ref mut secret, .. } => *secret = secret_opt,
					_ => {},
				}
			}

			if let Some(amount_opt) = update.amount_msat {
				payment.amount_msat = amount_opt;
			}

			if let Some(status) = update.status {
				payment.status = status;
			}

			if let Some(fee_msat) = update.fee_msat {
				payment.fee_msat = fee_msat;
			}

			payment.last_update = time::SystemTime::now()
				.duration_since(time::UNIX_EPOCH)
				.unwrap_or(time::Duration::ZERO)
				.as_secs();

			self.persist_info(&update.id, payment)?;
			updated = true;
		}

		Ok(updated)
	}

	pub(crate) fn list_filter<F: FnMut(&&PaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<PaymentDetails> {
		self.payments
			.lock()
			.unwrap()
			.iter()
			.map(|(_, p)| p)
			.filter(f)
			.cloned()
			.collect::<Vec<PaymentDetails>>()
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

#[cfg(test)]
mod tests {
	use super::*;
	use lightning::util::{
		ser::Readable,
		test_utils::{TestLogger, TestStore},
	};
	use std::io::Cursor;
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

		let kind = PaymentKind::Bolt11 { hash, preimage: None, secret: None, bolt11_invoice: None };
		let payment = PaymentDetails {
			id,
			kind,
			amount_msat: None,
			direction: PaymentDirection::Inbound,
			status: PaymentStatus::Pending,
			last_update: 0,
			fee_msat: None,
		};

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

		let mut update = PaymentDetailsUpdate::new(id);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(true), payment_store.update(&update));
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
				PaymentKind::Bolt11 { hash: h, preimage: p, secret: s, bolt11_invoice: None } => {
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
				PaymentKind::Bolt11Jit { hash: h, preimage: p, secret: s, lsp_fee_limits: l } => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
					assert_eq!(secret, s);
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
				PaymentKind::Spontaneous { hash: h, preimage: p, custom_tlvs: _ } => {
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
