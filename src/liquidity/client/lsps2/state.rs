use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;

use crate::data_store::{DataStore, StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;

pub(crate) const MIN_LEASE_REMAINING_SECS: u64 = 24 * 60 * 60;

pub(crate) type PaymentLeaseStore<L> = DataStore<PaymentLease, L>;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PaymentLeaseId {
	pub(crate) lsp_node_id: PublicKey,
	pub(crate) intercept_scid: u64,
}

impl_writeable_tlv_based!(PaymentLeaseId, {
	(0, lsp_node_id, required),
	(2, intercept_scid, required),
});

impl StorableObjectId for PaymentLeaseId {
	fn encode_to_hex_str(&self) -> String {
		format!(
			"{}{:016x}",
			hex_utils::to_string(&self.lsp_node_id.serialize()),
			self.intercept_scid
		)
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentLease {
	pub(crate) id: PaymentLeaseId,
	pub(crate) params: LSPS2OpeningFeeParams,
	pub(crate) cltv_expiry_delta: u32,
	pub(crate) payment_size_msat: Option<u64>,
	pub(crate) valid_until: u64,
}

impl_writeable_tlv_based!(PaymentLease, {
	(0, id, required),
	(2, params, required),
	(4, valid_until, required),
	(6, cltv_expiry_delta, required),
	(8, payment_size_msat, option),
});

#[derive(Clone, Debug)]
pub(crate) struct PaymentLeaseUpdate(PaymentLease);

impl StorableObjectUpdate<PaymentLease> for PaymentLeaseUpdate {
	fn id(&self) -> PaymentLeaseId {
		self.0.id
	}
}

impl StorableObject for PaymentLease {
	type Id = PaymentLeaseId;
	type Update = PaymentLeaseUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		if *self == update.0 {
			false
		} else {
			*self = update.0;
			true
		}
	}

	fn to_update(&self) -> Self::Update {
		PaymentLeaseUpdate(self.clone())
	}
}

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

	pub(crate) fn valid(&self, id: &PaymentLeaseId) -> Option<PaymentLease> {
		self.leases.get(id).filter(|lease| is_lease_usable(lease)).cloned()
	}

	pub(crate) fn remove(&mut self, id: &PaymentLeaseId) -> Option<PaymentLease> {
		self.leases.remove(id)
	}

	pub(crate) fn fixed_amount(
		&self, amount_msat: u64, max_fee_msat: Option<u64>,
	) -> Option<(PaymentLease, u64)> {
		let (id, fee_msat) = self
			.leases
			.iter()
			.filter(|(_, lease)| lease.payment_size_msat == Some(amount_msat))
			.filter(|(_, lease)| is_lease_usable(lease))
			.filter_map(|(id, lease)| {
				compute_opening_fee(
					amount_msat,
					lease.params.min_fee_msat,
					lease.params.proportional as u64,
				)
				.map(|fee_msat| (*id, fee_msat))
			})
			.filter(|(_, fee_msat)| max_fee_msat.map_or(true, |max| *fee_msat <= max))
			.min_by_key(|(_, fee_msat)| *fee_msat)?;
		self.leases.get(&id).cloned().map(|lease| (lease, fee_msat))
	}

	pub(crate) fn variable_amount(
		&self, max_total_fee_msat: Option<u64>,
	) -> Option<(PaymentLease, u64)> {
		let (id, proportional_fee) = self
			.leases
			.iter()
			.filter(|(_, lease)| lease.payment_size_msat.is_none())
			.filter(|(_, lease)| is_lease_usable(lease))
			.filter(|(_, lease)| {
				max_total_fee_msat.map_or(true, |max| lease.params.min_fee_msat <= max)
			})
			.map(|(id, lease)| (*id, lease.params.proportional as u64))
			.min_by_key(|(_, fee)| *fee)?;
		self.leases.get(&id).cloned().map(|lease| (lease, proportional_fee))
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
	use bitcoin::secp256k1::{Secp256k1, SecretKey};

	fn lease(
		node_key_byte: u8, intercept_scid: u64, min_fee_msat: u64, payment_size_msat: Option<u64>,
		valid_until: u64,
	) -> PaymentLease {
		PaymentLease {
			id: PaymentLeaseId {
				lsp_node_id: PublicKey::from_secret_key(
					&Secp256k1::new(),
					&SecretKey::from_slice(&[node_key_byte; 32]).unwrap(),
				),
				intercept_scid,
			},
			params: LSPS2OpeningFeeParams {
				min_fee_msat,
				proportional: 1,
				valid_until: "2030-01-01T00:00:00Z".parse().unwrap(),
				min_lifetime: 1,
				max_client_to_self_delay: 1,
				min_payment_size_msat: 1,
				max_payment_size_msat: 2_000,
				promise: String::new(),
			},
			cltv_expiry_delta: 18,
			payment_size_msat,
			valid_until,
		}
	}

	#[test]
	fn leases_are_consumed_once() {
		let lease = lease(2, 42, 1, Some(1_000), now_secs() + MIN_LEASE_REMAINING_SECS + 60);
		let id = lease.id;
		let mut state = LSPS2LeaseState::default();
		state.insert(lease);
		assert!(state.valid(&id).is_some());
		assert!(state.remove(&id).is_some());
		assert!(state.valid(&id).is_none());
	}

	#[test]
	fn prunes_leases_close_to_expiry() {
		let lease = lease(2, 43, 1, Some(1_000), now_secs() + MIN_LEASE_REMAINING_SECS - 1);
		let id = lease.id;
		let mut state = LSPS2LeaseState::default();
		state.insert(lease);
		state.prune();
		assert!(state.valid(&id).is_none());
	}

	#[test]
	fn selects_cheapest_matching_lease_across_lsps() {
		let valid_until = now_secs() + MIN_LEASE_REMAINING_SECS + 60;
		let expensive = lease(2, 44, 100, Some(1_000), valid_until);
		let cheap = lease(3, 45, 50, Some(1_000), valid_until);
		let mut state = LSPS2LeaseState::from_leases(vec![expensive.clone(), cheap.clone()]);

		let (selected, _) = state.fixed_amount(1_000, None).unwrap();
		assert_eq!(selected.id, cheap.id);
		state.remove(&selected.id);
		let (remaining, _) = state.fixed_amount(1_000, None).unwrap();
		assert_eq!(remaining.id, expensive.id);
	}

	#[test]
	fn variable_lease_honors_total_fee_limit() {
		let valid_until = now_secs() + MIN_LEASE_REMAINING_SECS + 60;
		let variable = lease(2, 46, 50, None, valid_until);
		let state = LSPS2LeaseState::from_leases(vec![variable.clone()]);

		assert!(state.variable_amount(Some(49)).is_none());
		assert_eq!(state.variable_amount(Some(50)).unwrap().0.id, variable.id);
	}
}
