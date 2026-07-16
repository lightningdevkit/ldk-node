// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Single-use LSPS2 payment leases.
//!
//! A payment lease is the outcome of a successful LSPS2 `buy` request: the opening fee parameters
//! we accepted, together with the intercept SCID and CLTV expiry delta the LSP assigned to them.
//! An invoice that routes payers through the intercept SCID lets the LSP open a just-in-time
//! channel to us once the payment arrives.
//!
//! The LSP opens a single channel per intercept SCID, so each lease may back at most one invoice.
//! Leases are therefore consumed when an invoice is built from them, and become unusable once the
//! opening fee parameters they were negotiated for expire.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;

use crate::data_store::{DataStore, StorableObject, StorableObjectId};
use crate::hex_utils;

pub(crate) const MIN_LEASE_REMAINING_SECS: u64 = 24 * 60 * 60;

pub(crate) type PaymentLeaseStore<L> = DataStore<PaymentLease, L>;

/// Identifies a [`PaymentLease`] by the LSP that granted it and the intercept SCID it assigned.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PaymentLeaseId {
	pub(crate) lsp_node_id: PublicKey,
	pub(crate) intercept_scid: u64,
}

impl_writeable_tlv_based!(PaymentLeaseId, {
	(1, lsp_node_id, required),
	(3, intercept_scid, required),
});

impl StorableObjectId for PaymentLeaseId {
	fn encode_to_hex_str(&self) -> String {
		format!(
			"{}{:016x}",
			hex_utils::to_string(&self.lsp_node_id.serialize()),
			self.intercept_scid
		)
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		let bytes = hex_utils::to_vec(s)?;
		if bytes.len() != 41 {
			return None;
		}
		let lsp_node_id = PublicKey::from_slice(&bytes[..33]).ok()?;
		let intercept_scid = u64::from_be_bytes(bytes[33..].try_into().ok()?);
		Some(Self { lsp_node_id, intercept_scid })
	}
}

/// A negotiated, single-use right to receive a payment over a just-in-time channel.
///
/// See the module-level documentation for details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentLease {
	pub(crate) id: PaymentLeaseId,
	/// The opening fee parameters the LSP agreed to when granting this lease.
	pub(crate) params: LSPS2OpeningFeeParams,
	/// The CLTV expiry delta the LSP requires for payments to the intercept SCID.
	pub(crate) cltv_expiry_delta: u32,
	/// The exact payment amount for a fixed-amount lease, or `None` for a variable-amount lease.
	pub(crate) payment_size_msat: Option<u64>,
	/// The UNIX timestamp, in seconds, at which the opening fee parameters expire.
	pub(crate) valid_until: u64,
}

impl_writeable_tlv_based!(PaymentLease, {
	(1, id, required),
	(3, params, required),
	(5, valid_until, required),
	(7, cltv_expiry_delta, required),
	(9, payment_size_msat, option),
});

impl StorableObject for PaymentLease {
	type Id = PaymentLeaseId;

	fn id(&self) -> Self::Id {
		self.id
	}
}

/// The set of [`PaymentLease`]s that are available to be consumed.
#[derive(Default)]
pub(crate) struct LSPS2LeaseState {
	leases: HashMap<PaymentLeaseId, PaymentLease>,
}

impl LSPS2LeaseState {
	pub(crate) fn from_leases(leases: Vec<PaymentLease>) -> Self {
		Self { leases: leases.into_iter().map(|lease| (lease.id, lease)).collect() }
	}

	pub(crate) fn insert(&mut self, lease: PaymentLease) {
		self.leases.insert(lease.id, lease);
	}

	pub(crate) fn take_valid(&mut self, id: &PaymentLeaseId) -> Option<PaymentLease> {
		let lease = self.leases.remove(id)?;
		if is_lease_usable(&lease) {
			Some(lease)
		} else {
			None
		}
	}

	pub(crate) fn prune(&mut self) {
		self.leases.retain(|_, lease| is_lease_usable(lease));
	}
}

pub(crate) fn is_lease_usable(lease: &PaymentLease) -> bool {
	lease.valid_until.saturating_sub(now_secs()) >= MIN_LEASE_REMAINING_SECS
}

fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn leases_are_consumed_once() {
		let id = PaymentLeaseId {
			lsp_node_id: PublicKey::from_slice(&[2; 33]).unwrap(),
			intercept_scid: 42,
		};
		let mut state = LSPS2LeaseState::default();
		state.insert(PaymentLease {
			id,
			params: LSPS2OpeningFeeParams {
				min_fee_msat: 1,
				proportional: 1,
				valid_until: "2030-01-01T00:00:00Z".parse().unwrap(),
				min_lifetime: 1,
				max_client_to_self_delay: 1,
				min_payment_size_msat: 1,
				max_payment_size_msat: 2,
				promise: String::new(),
			},
			cltv_expiry_delta: 18,
			payment_size_msat: Some(1_000),
			valid_until: now_secs() + MIN_LEASE_REMAINING_SECS + 60,
		});
		assert!(state.take_valid(&id).is_some());
		assert!(state.take_valid(&id).is_none());
	}

	#[test]
	fn prunes_leases_close_to_expiry() {
		let id = PaymentLeaseId {
			lsp_node_id: PublicKey::from_slice(&[2; 33]).unwrap(),
			intercept_scid: 43,
		};
		let mut state = LSPS2LeaseState::default();
		state.insert(PaymentLease {
			id,
			params: LSPS2OpeningFeeParams {
				min_fee_msat: 1,
				proportional: 1,
				valid_until: "2030-01-01T00:00:00Z".parse().unwrap(),
				min_lifetime: 1,
				max_client_to_self_delay: 1,
				min_payment_size_msat: 1,
				max_payment_size_msat: 2,
				promise: String::new(),
			},
			cltv_expiry_delta: 18,
			payment_size_msat: Some(1_000),
			valid_until: now_secs() + MIN_LEASE_REMAINING_SECS - 1,
		});
		state.prune();
		assert!(state.take_valid(&id).is_none());
	}
}
