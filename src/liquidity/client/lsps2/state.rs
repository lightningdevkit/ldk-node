use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use lightning::offers::offer::OfferId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;
use tokio::sync::Mutex as AsyncMutex;

use crate::data_store::{DataStore, StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;

pub(crate) const MIN_LEASE_REMAINING_SECS: u64 = 24 * 60 * 60;

pub(crate) type PaymentLeaseStore<L> = DataStore<PaymentLease, L>;
pub(crate) type PendingOfferStore<L> = DataStore<PendingOffer, L>;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PendingOfferId {
	offer_id: [u8; 32],
}

impl From<OfferId> for PendingOfferId {
	fn from(offer_id: OfferId) -> Self {
		Self { offer_id: offer_id.0 }
	}
}

impl From<PendingOfferId> for OfferId {
	fn from(offer_id: PendingOfferId) -> Self {
		Self(offer_id.offer_id)
	}
}

impl_writeable_tlv_based!(PendingOfferId, {
	(0, offer_id, required),
});

impl StorableObjectId for PendingOfferId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.offer_id)
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingOfferAmount {
	Fixed { amount_msat: u64, max_total_fee_msat: Option<u64> },
	Variable { max_proportional_fee_ppm_msat: Option<u64> },
}

impl_writeable_tlv_based_enum!(PendingOfferAmount,
	(0, Fixed) => {
		(0, amount_msat, required),
		(2, max_total_fee_msat, option),
	},
	(2, Variable) => {
		(0, max_proportional_fee_ppm_msat, option),
	},
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingOffer {
	pub(crate) id: PendingOfferId,
	pub(crate) amount: PendingOfferAmount,
	pub(crate) absolute_expiry: Option<u64>,
	last_accessed: u64,
}

impl PendingOffer {
	pub(crate) fn new(
		offer_id: OfferId, amount: PendingOfferAmount, absolute_expiry: Option<u64>,
	) -> Self {
		Self { id: offer_id.into(), amount, absolute_expiry, last_accessed: now_secs() }
	}
}

impl_writeable_tlv_based!(PendingOffer, {
	(0, id, required),
	(2, amount, required),
	(4, absolute_expiry, option),
	(6, last_accessed, required),
});

#[derive(Clone, Debug)]
pub(crate) struct PendingOfferUpdate(PendingOffer);

impl StorableObjectUpdate<PendingOffer> for PendingOfferUpdate {
	fn id(&self) -> PendingOfferId {
		self.0.id
	}
}

impl StorableObject for PendingOffer {
	type Id = PendingOfferId;
	type Update = PendingOfferUpdate;

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
		PendingOfferUpdate(self.clone())
	}
}

pub(crate) struct PendingOfferState {
	offers: HashMap<PendingOfferId, PendingOffer>,
	capacity: usize,
}

impl PendingOfferState {
	pub(crate) fn from_offers(
		offers: Vec<PendingOffer>, capacity: usize, now: u64,
	) -> (Self, Vec<PendingOfferId>) {
		let mut removed = Vec::new();
		let mut offers = offers
			.into_iter()
			.filter_map(|offer| {
				if is_pending_offer_expired(&offer, now) {
					removed.push(offer.id);
					None
				} else {
					Some((offer.id, offer))
				}
			})
			.collect::<HashMap<_, _>>();
		let capacity = capacity.max(1);
		while offers.len() > capacity {
			let id = least_recently_used(&offers).expect("offers are non-empty");
			offers.remove(&id);
			removed.push(id);
		}
		(Self { offers, capacity }, removed)
	}

	pub(crate) fn register(&mut self, offer: PendingOffer) -> Option<PendingOfferId> {
		let is_new = !self.offers.contains_key(&offer.id);
		let evicted = if is_new && self.offers.len() == self.capacity {
			let id = least_recently_used(&self.offers).expect("offers are non-empty");
			self.offers.remove(&id);
			Some(id)
		} else {
			None
		};
		self.offers.insert(offer.id, offer);
		evicted
	}

	pub(crate) fn get(&mut self, id: &PendingOfferId, now: u64) -> Option<PendingOffer> {
		if self.offers.get(id).is_some_and(|offer| is_pending_offer_expired(offer, now)) {
			self.offers.remove(id);
			return None;
		}
		let last_accessed = self.next_access_timestamp(now);
		let offer = self.offers.get_mut(id)?;
		offer.last_accessed = last_accessed;
		Some(offer.clone())
	}

	pub(crate) fn contains(&self, id: &PendingOfferId) -> bool {
		self.offers.contains_key(id)
	}

	pub(crate) fn offers(&self) -> Vec<PendingOffer> {
		self.offers.values().cloned().collect()
	}

	pub(crate) fn prune(&mut self, now: u64) -> Vec<PendingOfferId> {
		let removed = self
			.offers
			.iter()
			.filter(|(_, offer)| is_pending_offer_expired(offer, now))
			.map(|(id, _)| *id)
			.collect::<Vec<_>>();
		for id in &removed {
			self.offers.remove(id);
		}
		removed
	}

	fn next_access_timestamp(&self, now: u64) -> u64 {
		self.offers
			.values()
			.map(|offer| offer.last_accessed)
			.max()
			.map_or(now, |last_accessed| now.max(last_accessed.saturating_add(1)))
	}
}

#[derive(Default)]
pub(crate) struct PendingInvoiceRequestState {
	locks: HashMap<PendingOfferId, Weak<AsyncMutex<()>>>,
}

impl PendingInvoiceRequestState {
	pub(crate) fn request_lock(&mut self, id: PendingOfferId) -> Arc<AsyncMutex<()>> {
		self.prune();
		if let Some(lock) = self.locks.get(&id).and_then(Weak::upgrade) {
			return lock;
		}
		let lock = Arc::new(AsyncMutex::new(()));
		self.locks.insert(id, Arc::downgrade(&lock));
		lock
	}

	pub(crate) fn prune(&mut self) {
		self.locks.retain(|_, lock| lock.strong_count() > 0);
	}

	#[cfg(test)]
	fn is_empty(&self) -> bool {
		self.locks.is_empty()
	}
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum LeaseRequestKey {
	Fixed(u64),
	Variable,
}

#[derive(Default)]
pub(crate) struct PendingLeaseRequestState {
	locks: HashMap<LeaseRequestKey, Weak<AsyncMutex<()>>>,
}

impl PendingLeaseRequestState {
	pub(crate) fn request_lock(&mut self, key: LeaseRequestKey) -> Arc<AsyncMutex<()>> {
		self.prune();
		if let Some(lock) = self.locks.get(&key).and_then(Weak::upgrade) {
			return lock;
		}
		let lock = Arc::new(AsyncMutex::new(()));
		self.locks.insert(key, Arc::downgrade(&lock));
		lock
	}

	pub(crate) fn prune(&mut self) {
		self.locks.retain(|_, lock| lock.strong_count() > 0);
	}
}

fn least_recently_used(offers: &HashMap<PendingOfferId, PendingOffer>) -> Option<PendingOfferId> {
	offers.values().min_by_key(|offer| (offer.last_accessed, offer.id)).map(|offer| offer.id)
}

fn is_pending_offer_expired(offer: &PendingOffer, now: u64) -> bool {
	offer.absolute_expiry.is_some_and(|expiry| expiry <= now)
}

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

	pub(crate) fn has_fixed_amount(&self, amount_msat: u64, max_fee_msat: Option<u64>) -> bool {
		self.leases
			.values()
			.filter(|lease| lease.payment_size_msat == Some(amount_msat))
			.filter(|lease| is_lease_usable(lease))
			.filter_map(|lease| {
				compute_opening_fee(
					amount_msat,
					lease.params.min_fee_msat,
					lease.params.proportional as u64,
				)
			})
			.any(|fee_msat| max_fee_msat.map_or(true, |max| fee_msat <= max))
	}

	pub(crate) fn variable_amount(
		&self, max_proportional_fee_ppm_msat: Option<u64>,
	) -> Option<(PaymentLease, u64)> {
		let (id, proportional_fee) = self
			.leases
			.iter()
			.filter(|(_, lease)| lease.payment_size_msat.is_none())
			.filter(|(_, lease)| is_lease_usable(lease))
			.map(|(id, lease)| (*id, lease.params.proportional as u64))
			.filter(|(_, fee)| max_proportional_fee_ppm_msat.map_or(true, |max| *fee <= max))
			.min_by_key(|(_, fee)| *fee)?;
		self.leases.get(&id).cloned().map(|lease| (lease, proportional_fee))
	}

	pub(crate) fn has_variable_amount(&self, max_proportional_fee_ppm_msat: Option<u64>) -> bool {
		self.leases
			.values()
			.filter(|lease| lease.payment_size_msat.is_none())
			.filter(|lease| is_lease_usable(lease))
			.map(|lease| lease.params.proportional as u64)
			.any(|fee| max_proportional_fee_ppm_msat.map_or(true, |max| fee <= max))
	}

	pub(crate) fn prune(&mut self) {
		self.leases.retain(|_, lease| is_lease_usable(lease));
	}
}

pub(crate) fn is_lease_usable(lease: &PaymentLease) -> bool {
	lease.valid_until.saturating_sub(now_secs()) >= MIN_LEASE_REMAINING_SECS
}

pub(crate) fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs()
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::*;
	use bitcoin::secp256k1::{Secp256k1, SecretKey};
	use lightning::offers::offer::OfferId;
	use lightning::util::ser::{Readable, Writeable};

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
	fn detects_cached_leases_for_refill() {
		let valid_until = now_secs() + MIN_LEASE_REMAINING_SECS + 60;
		let fixed = lease(2, 46, 100, Some(1_000), valid_until);
		let variable = lease(3, 47, 50, None, valid_until);
		let state = LSPS2LeaseState::from_leases(vec![fixed, variable]);

		assert!(state.has_fixed_amount(1_000, Some(100)));
		assert!(!state.has_fixed_amount(1_000, Some(99)));
		assert!(!state.has_fixed_amount(2_000, None));
		assert!(state.has_variable_amount(Some(1)));
		assert!(!state.has_variable_amount(Some(0)));
	}

	fn pending_offer(
		id_byte: u8, amount: PendingOfferAmount, absolute_expiry: Option<u64>, last_accessed: u64,
	) -> PendingOffer {
		PendingOffer {
			id: PendingOfferId::from(OfferId([id_byte; 32])),
			amount,
			absolute_expiry,
			last_accessed,
		}
	}

	#[test]
	fn pending_offers_roundtrip() {
		let offer = pending_offer(
			1,
			PendingOfferAmount::Fixed { amount_msat: 1_000, max_total_fee_msat: Some(100) },
			Some(42),
			7,
		);

		let encoded = offer.encode();
		let decoded = PendingOffer::read(&mut &encoded[..]).unwrap();
		assert_eq!(decoded, offer);
	}

	#[test]
	fn pending_offer_registry_evicts_least_recently_used() {
		let first = pending_offer(
			1,
			PendingOfferAmount::Fixed { amount_msat: 1_000, max_total_fee_msat: None },
			None,
			1,
		);
		let second = pending_offer(
			2,
			PendingOfferAmount::Variable { max_proportional_fee_ppm_msat: None },
			None,
			2,
		);
		let third = pending_offer(
			3,
			PendingOfferAmount::Fixed { amount_msat: 3_000, max_total_fee_msat: None },
			None,
			3,
		);
		let first_id = first.id;
		let second_id = second.id;
		let third_id = third.id;
		let (mut state, removed) = PendingOfferState::from_offers(vec![first, second], 2, 10);
		assert!(removed.is_empty());

		assert!(state.get(&first_id, 11).is_some());
		assert_eq!(state.register(third), Some(second_id));
		assert!(state.get(&first_id, 13).is_some());
		assert!(state.get(&second_id, 14).is_none());
		assert!(state.get(&third_id, 15).is_some());
	}

	#[test]
	fn pending_offer_registry_prunes_only_expired_offers() {
		let expired = pending_offer(
			1,
			PendingOfferAmount::Fixed { amount_msat: 1_000, max_total_fee_msat: None },
			Some(9),
			1,
		);
		let unexpired = pending_offer(
			2,
			PendingOfferAmount::Fixed { amount_msat: 2_000, max_total_fee_msat: None },
			Some(11),
			2,
		);
		let long_lived = pending_offer(
			3,
			PendingOfferAmount::Variable { max_proportional_fee_ppm_msat: Some(100) },
			None,
			3,
		);
		let expired_id = expired.id;
		let unexpired_id = unexpired.id;
		let long_lived_id = long_lived.id;

		let (state, removed) =
			PendingOfferState::from_offers(vec![expired, unexpired, long_lived], 3, 10);

		assert_eq!(removed, vec![expired_id]);
		assert!(state.contains(&unexpired_id));
		assert!(state.contains(&long_lived_id));
	}

	#[test]
	fn pending_offer_registry_lists_cached_offers() {
		let fixed = pending_offer(
			1,
			PendingOfferAmount::Fixed { amount_msat: 1_000, max_total_fee_msat: None },
			None,
			1,
		);
		let variable = pending_offer(
			2,
			PendingOfferAmount::Variable { max_proportional_fee_ppm_msat: None },
			None,
			2,
		);
		let (state, removed) =
			PendingOfferState::from_offers(vec![fixed.clone(), variable.clone()], 2, 10);
		assert!(removed.is_empty());

		let mut ids = state.offers().into_iter().map(|offer| offer.id).collect::<Vec<_>>();
		ids.sort();
		assert_eq!(ids, vec![fixed.id, variable.id]);
	}

	#[test]
	fn invoice_requests_are_serialized_per_offer() {
		let first_id = PendingOfferId::from(OfferId([1; 32]));
		let second_id = PendingOfferId::from(OfferId([2; 32]));
		let mut state = PendingInvoiceRequestState::default();

		let first = state.request_lock(first_id);
		let same_offer = state.request_lock(first_id);
		let other_offer = state.request_lock(second_id);

		assert!(Arc::ptr_eq(&first, &same_offer));
		assert!(!Arc::ptr_eq(&first, &other_offer));

		drop(first);
		drop(same_offer);
		drop(other_offer);
		state.prune();
		assert!(state.is_empty());
	}

	#[test]
	fn lease_requests_are_serialized_per_key() {
		let mut state = PendingLeaseRequestState::default();

		let fixed = state.request_lock(LeaseRequestKey::Fixed(1_000));
		let same_fixed = state.request_lock(LeaseRequestKey::Fixed(1_000));
		let other_fixed = state.request_lock(LeaseRequestKey::Fixed(2_000));
		let variable = state.request_lock(LeaseRequestKey::Variable);

		assert!(Arc::ptr_eq(&fixed, &same_fixed));
		assert!(!Arc::ptr_eq(&fixed, &other_fixed));
		assert!(!Arc::ptr_eq(&fixed, &variable));
	}
}
