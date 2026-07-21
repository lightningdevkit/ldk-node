use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable};
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;

use crate::data_store::{DataStore, StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::io::{
	LSPS2_CACHE_TARGET_PERSISTENCE_KEY, LSPS2_CACHE_TARGET_PERSISTENCE_PRIMARY_NAMESPACE,
	LSPS2_CACHE_TARGET_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;
use crate::Error;

pub(crate) const MIN_LEASE_REMAINING_SECS: u64 = 24 * 60 * 60;

pub(crate) type PaymentLeaseStore<L> = DataStore<PaymentLease, L>;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LeaseCacheTargetId {
	Fixed { amount_msat: u64 },
	Variable,
}

impl_writeable_tlv_based_enum!(LeaseCacheTargetId,
	(0, Fixed) => {
		(0, amount_msat, required),
	},
	(2, Variable) => {},
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseCacheTarget {
	pub(crate) id: LeaseCacheTargetId,
	pub(crate) absolute_expiry: Option<u64>,
	last_accessed: u64,
}

impl LeaseCacheTarget {
	pub(crate) fn new(id: LeaseCacheTargetId, absolute_expiry: Option<u64>) -> Self {
		Self { id, absolute_expiry, last_accessed: now_secs() }
	}
}

impl_writeable_tlv_based!(LeaseCacheTarget, {
	(0, id, required),
	(2, absolute_expiry, option),
	(4, last_accessed, required),
});

#[derive(Clone, Debug, PartialEq, Eq)]
struct LeaseCacheTargets {
	targets: Vec<LeaseCacheTarget>,
}

impl_writeable_tlv_based!(LeaseCacheTargets, {
	(0, targets, required_vec),
});

#[derive(Clone)]
pub(crate) struct LeaseCacheTargetState {
	targets: HashMap<LeaseCacheTargetId, LeaseCacheTarget>,
	capacity: usize,
}

impl LeaseCacheTargetState {
	pub(crate) fn from_targets(
		targets: Vec<LeaseCacheTarget>, capacity: usize, now: u64,
	) -> (Self, Vec<LeaseCacheTargetId>) {
		let mut removed = Vec::new();
		let mut targets = targets
			.into_iter()
			.filter_map(|target| {
				if is_cache_target_expired(&target, now) {
					removed.push(target.id);
					None
				} else {
					Some((target.id, target))
				}
			})
			.collect::<HashMap<_, _>>();
		let capacity = capacity.max(1);
		while targets.len() > capacity {
			let id = least_recently_used(&targets).expect("targets are non-empty");
			targets.remove(&id);
			removed.push(id);
		}
		(Self { targets, capacity }, removed)
	}

	pub(crate) fn register(&mut self, mut target: LeaseCacheTarget) -> Option<LeaseCacheTargetId> {
		let last_accessed = self.next_access_timestamp(target.last_accessed);
		if let Some(existing) = self.targets.get_mut(&target.id) {
			existing.absolute_expiry =
				merge_absolute_expiry(existing.absolute_expiry, target.absolute_expiry);
			existing.last_accessed = last_accessed;
			return None;
		}

		let evicted = if self.targets.len() == self.capacity {
			let id = least_recently_used(&self.targets).expect("targets are non-empty");
			self.targets.remove(&id);
			Some(id)
		} else {
			None
		};
		target.last_accessed = last_accessed;
		self.targets.insert(target.id, target);
		evicted
	}

	pub(crate) fn targets(&self) -> Vec<LeaseCacheTarget> {
		let mut targets = self.targets.values().cloned().collect::<Vec<_>>();
		targets.sort_unstable_by_key(|target| target.id);
		targets
	}

	#[cfg(test)]
	pub(crate) fn contains(&self, id: &LeaseCacheTargetId) -> bool {
		self.targets.contains_key(id)
	}

	pub(crate) fn prune(&mut self, now: u64) -> Vec<LeaseCacheTargetId> {
		let removed = self
			.targets
			.iter()
			.filter(|(_, target)| is_cache_target_expired(target, now))
			.map(|(id, _)| *id)
			.collect::<Vec<_>>();
		for id in &removed {
			self.targets.remove(id);
		}
		removed
	}

	fn next_access_timestamp(&self, now: u64) -> u64 {
		self.targets
			.values()
			.map(|target| target.last_accessed)
			.max()
			.map_or(now, |last_accessed| now.max(last_accessed.saturating_add(1)))
	}
}

pub(crate) struct LeaseCacheTargetStore<L: Deref>
where
	L::Target: LdkLogger,
{
	state: Mutex<LeaseCacheTargetState>,
	mutation_lock: tokio::sync::Mutex<()>,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> LeaseCacheTargetStore<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(state: LeaseCacheTargetState, kv_store: Arc<DynStore>, logger: L) -> Self {
		Self {
			state: Mutex::new(state),
			mutation_lock: tokio::sync::Mutex::new(()),
			kv_store,
			logger,
		}
	}

	pub(crate) async fn register(&self, target: LeaseCacheTarget) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let mut updated_state = self.state.lock().expect("lock").clone();
		updated_state.register(target);
		self.persist_state(&updated_state).await?;
		*self.state.lock().expect("lock") = updated_state;
		Ok(())
	}

	pub(crate) async fn prune(&self, now: u64) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let mut updated_state = self.state.lock().expect("lock").clone();
		if updated_state.prune(now).is_empty() {
			return Ok(());
		}
		self.persist_state(&updated_state).await?;
		*self.state.lock().expect("lock") = updated_state;
		Ok(())
	}

	pub(crate) async fn persist(&self) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let state = self.state.lock().expect("lock").clone();
		self.persist_state(&state).await
	}

	pub(crate) fn targets(&self) -> Vec<LeaseCacheTarget> {
		self.state.lock().expect("lock").targets()
	}

	async fn persist_state(&self, state: &LeaseCacheTargetState) -> Result<(), Error> {
		let data = LeaseCacheTargets { targets: state.targets() }.encode();
		KVStore::write(
			&*self.kv_store,
			LSPS2_CACHE_TARGET_PERSISTENCE_PRIMARY_NAMESPACE,
			LSPS2_CACHE_TARGET_PERSISTENCE_SECONDARY_NAMESPACE,
			LSPS2_CACHE_TARGET_PERSISTENCE_KEY,
			data,
		)
		.await
		.map_err(|error| {
			log_error!(self.logger, "Writing LSPS2 lease cache targets failed due to: {}", error);
			Error::PersistenceFailed
		})
	}
}

pub(crate) async fn read_lease_cache_targets<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<Vec<LeaseCacheTarget>, lightning::io::Error>
where
	L::Target: LdkLogger,
{
	match KVStore::read(
		kv_store,
		LSPS2_CACHE_TARGET_PERSISTENCE_PRIMARY_NAMESPACE,
		LSPS2_CACHE_TARGET_PERSISTENCE_SECONDARY_NAMESPACE,
		LSPS2_CACHE_TARGET_PERSISTENCE_KEY,
	)
	.await
	{
		Ok(data) => LeaseCacheTargets::read(&mut &data[..]).map(|targets| targets.targets).map_err(
			|error| {
				log_error!(logger, "Failed to deserialize LSPS2 lease cache targets: {}", error);
				lightning::io::Error::new(
					lightning::io::ErrorKind::InvalidData,
					"Failed to deserialize LSPS2 lease cache targets",
				)
			},
		),
		Err(error) if error.kind() == lightning::io::ErrorKind::NotFound => Ok(Vec::new()),
		Err(error) => Err(error),
	}
}

fn least_recently_used(
	targets: &HashMap<LeaseCacheTargetId, LeaseCacheTarget>,
) -> Option<LeaseCacheTargetId> {
	targets.values().min_by_key(|target| (target.last_accessed, target.id)).map(|target| target.id)
}

fn merge_absolute_expiry(current: Option<u64>, new: Option<u64>) -> Option<u64> {
	match (current, new) {
		(None, _) | (_, None) => None,
		(Some(current), Some(new)) => Some(current.max(new)),
	}
}

fn is_cache_target_expired(target: &LeaseCacheTarget, now: u64) -> bool {
	target.absolute_expiry.is_some_and(|expiry| expiry <= now)
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

pub(crate) fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs()
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::*;
	use bitcoin::secp256k1::{Secp256k1, SecretKey};
	use lightning::io;
	use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning::util::ser::{Readable, Writeable};
	use lightning::util::test_utils::TestLogger;

	use crate::io::test_utils::InMemoryStore;
	use crate::io::{
		LSPS2_CACHE_TARGET_PERSISTENCE_KEY, LSPS2_CACHE_TARGET_PERSISTENCE_PRIMARY_NAMESPACE,
		LSPS2_CACHE_TARGET_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use crate::types::{DynStore, DynStoreWrapper};

	struct FailingStore;

	impl KVStore for FailingStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "read failed")) }
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "write failed")) }
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "remove failed")) }
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "list failed")) }
		}
	}

	impl PaginatedKVStore for FailingStore {
		fn list_paginated(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
			_page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			async { Err(io::Error::new(io::ErrorKind::Other, "list_paginated failed")) }
		}
	}

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

	fn cache_target(
		id: LeaseCacheTargetId, absolute_expiry: Option<u64>, last_accessed: u64,
	) -> LeaseCacheTarget {
		LeaseCacheTarget { id, absolute_expiry, last_accessed }
	}

	#[test]
	fn cache_targets_roundtrip() {
		let target = cache_target(LeaseCacheTargetId::Fixed { amount_msat: 1_000 }, Some(42), 7);

		let encoded = target.encode();
		let decoded = LeaseCacheTarget::read(&mut &encoded[..]).unwrap();
		assert_eq!(decoded, target);
	}

	#[tokio::test]
	async fn cache_targets_persist_as_single_blob() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let (state, removed) = LeaseCacheTargetState::from_targets(Vec::new(), 2, 0);
		assert!(removed.is_empty());
		let store =
			LeaseCacheTargetStore::new(state, Arc::clone(&kv_store), Arc::new(TestLogger::new()));
		store
			.register(cache_target(LeaseCacheTargetId::Fixed { amount_msat: 1_000 }, None, 1))
			.await
			.unwrap();
		store.register(cache_target(LeaseCacheTargetId::Variable, None, 2)).await.unwrap();

		let keys = KVStore::list(
			&*kv_store,
			LSPS2_CACHE_TARGET_PERSISTENCE_PRIMARY_NAMESPACE,
			LSPS2_CACHE_TARGET_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();
		assert_eq!(keys, vec![LSPS2_CACHE_TARGET_PERSISTENCE_KEY.to_string()]);

		let restored =
			read_lease_cache_targets(&*kv_store, Arc::new(TestLogger::new())).await.unwrap();
		assert_eq!(restored, store.targets());
	}

	#[tokio::test]
	async fn cache_target_store_keeps_memory_on_persist_failure() {
		let initial = cache_target(LeaseCacheTargetId::Variable, None, 1);
		let (state, removed) = LeaseCacheTargetState::from_targets(vec![initial.clone()], 2, 0);
		assert!(removed.is_empty());
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let store = LeaseCacheTargetStore::new(state, kv_store, Arc::new(TestLogger::new()));

		let result = store
			.register(cache_target(LeaseCacheTargetId::Fixed { amount_msat: 1_000 }, None, 2))
			.await;

		assert_eq!(result, Err(Error::PersistenceFailed));
		assert_eq!(store.targets(), vec![initial]);
	}

	#[test]
	fn cache_target_registry_evicts_least_recently_used() {
		let first = cache_target(LeaseCacheTargetId::Fixed { amount_msat: 1_000 }, None, 1);
		let second = cache_target(LeaseCacheTargetId::Variable, None, 2);
		let third = cache_target(LeaseCacheTargetId::Fixed { amount_msat: 3_000 }, None, 3);
		let first_id = first.id;
		let second_id = second.id;
		let third_id = third.id;
		let (mut state, removed) = LeaseCacheTargetState::from_targets(vec![first, second], 2, 10);
		assert!(removed.is_empty());

		assert_eq!(state.register(cache_target(first_id, None, 11)), None);
		assert_eq!(state.register(third), Some(second_id));
		assert!(state.contains(&first_id));
		assert!(!state.contains(&second_id));
		assert!(state.contains(&third_id));
	}

	#[test]
	fn cache_target_registry_prunes_only_expired_targets() {
		let expired = cache_target(LeaseCacheTargetId::Fixed { amount_msat: 1_000 }, Some(9), 1);
		let unexpired = cache_target(LeaseCacheTargetId::Fixed { amount_msat: 2_000 }, Some(11), 2);
		let long_lived = cache_target(LeaseCacheTargetId::Variable, None, 3);
		let expired_id = expired.id;
		let unexpired_id = unexpired.id;
		let long_lived_id = long_lived.id;

		let (state, removed) =
			LeaseCacheTargetState::from_targets(vec![expired, unexpired, long_lived], 3, 10);

		assert_eq!(removed, vec![expired_id]);
		assert!(state.contains(&unexpired_id));
		assert!(state.contains(&long_lived_id));
	}

	#[test]
	fn cache_target_registry_keeps_longest_expiry() {
		let id = LeaseCacheTargetId::Fixed { amount_msat: 1_000 };
		let (mut state, removed) =
			LeaseCacheTargetState::from_targets(vec![cache_target(id, Some(20), 1)], 2, 10);
		assert!(removed.is_empty());

		state.register(cache_target(id, Some(30), 2));
		assert_eq!(state.targets().first().unwrap().absolute_expiry, Some(30));
		state.register(cache_target(id, None, 3));
		assert_eq!(state.targets().first().unwrap().absolute_expiry, None);
	}
}
