// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use lightning::io::ErrorKind;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable};

use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;
use crate::Error;

pub(crate) trait StorableObject: Clone + Readable + Writeable {
	type Id: StorableObjectId;
	type Update: StorableObjectUpdate<Self>;

	fn id(&self) -> Self::Id;
	fn update(&mut self, update: Self::Update) -> bool;
	fn to_update(&self) -> Self::Update;
}

pub(crate) trait StorableObjectId: Clone + std::hash::Hash + PartialEq + Eq {
	fn encode_to_hex_str(&self) -> String;
}

pub(crate) trait StorableObjectUpdate<SO: StorableObject> {
	fn id(&self) -> SO::Id;
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum DataStoreUpdateResult {
	Updated,
	Unchanged,
	NotFound,
}

/// How many of a namespace's objects a [`DataStore`] keeps in memory.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum CacheLimit {
	/// Keep every object in memory.
	Unbounded,
	/// Keep at most this many objects in memory.
	Bounded(NonZeroUsize),
}

/// The caching policy of a [`DataStore`].
///
/// This is a type parameter rather than a plain value so that operations which are only
/// meaningful while every object is held in memory — currently [`DataStore::list_filter`] — can be
/// restricted to the stores that satisfy that, and using them elsewhere is a compile error rather
/// than a silently incomplete result.
pub(crate) trait CachePolicy: Send + Sync + 'static {
	fn cache_limit(&self) -> CacheLimit;
}

/// Keeps every object of the namespace in memory.
///
/// Reads are served entirely from memory and never hit the [`KVStore`]. Required for stores whose
/// consumers rely on full scans via [`DataStore::list_filter`], and for stores small enough that
/// bounding them would buy nothing.
pub(crate) struct KeepAllEntries;

impl CachePolicy for KeepAllEntries {
	fn cache_limit(&self) -> CacheLimit {
		CacheLimit::Unbounded
	}
}

/// Keeps at most `capacity` least-recently-used objects in memory, reading through to the
/// [`KVStore`] whenever a lookup misses.
///
/// Suitable for namespaces that grow without bound over a node's lifetime.
#[allow(dead_code)] // Constructed once a store opts into a bounded cache.
pub(crate) struct KeepLeastRecentlyUsed {
	capacity: NonZeroUsize,
}

#[allow(dead_code)] // See above.
impl KeepLeastRecentlyUsed {
	pub(crate) fn new(capacity: NonZeroUsize) -> Self {
		Self { capacity }
	}
}

impl CachePolicy for KeepLeastRecentlyUsed {
	fn cache_limit(&self) -> CacheLimit {
		CacheLimit::Bounded(self.capacity)
	}
}

/// A least-recently-used cache of at most `capacity` objects.
///
/// Recency is tracked with a monotonically increasing sequence number per entry, mirrored in
/// `recency` so that the least recently used entry is the first one in it. This trades `O(1)` for
/// `O(log n)` against a far smaller amount of code than an intrusive list would need, which is a
/// good deal at the cache sizes we expect.
struct LruCache<SO: StorableObject> {
	capacity: NonZeroUsize,
	entries: HashMap<SO::Id, (SO, u64)>,
	recency: BTreeMap<u64, SO::Id>,
	// Wrapping this would take 2^64 mutations of a single store, so we don't guard against it.
	next_seq: u64,
}

impl<SO: StorableObject> LruCache<SO> {
	fn new(capacity: NonZeroUsize) -> Self {
		Self { capacity, entries: HashMap::new(), recency: BTreeMap::new(), next_seq: 0 }
	}

	fn take_seq(&mut self) -> u64 {
		let seq = self.next_seq;
		self.next_seq += 1;
		seq
	}

	fn insert(&mut self, id: SO::Id, object: SO) {
		let seq = self.take_seq();
		if let Some((_, prev_seq)) = self.entries.insert(id.clone(), (object, seq)) {
			self.recency.remove(&prev_seq);
		}
		self.recency.insert(seq, id);
		debug_assert_eq!(self.entries.len(), self.recency.len());

		while self.entries.len() > self.capacity.get() {
			let Some((_, evicted_id)) = self.recency.pop_first() else { break };
			self.entries.remove(&evicted_id);
		}
		debug_assert_eq!(self.entries.len(), self.recency.len());
		debug_assert!(self.entries.len() <= self.capacity.get());
	}

	fn get(&mut self, id: &SO::Id) -> Option<SO> {
		let seq = self.take_seq();
		let (object, prev_seq) = {
			let entry = self.entries.get_mut(id)?;
			let prev_seq = entry.1;
			entry.1 = seq;
			(entry.0.clone(), prev_seq)
		};
		self.recency.remove(&prev_seq);
		self.recency.insert(seq, id.clone());
		debug_assert_eq!(self.entries.len(), self.recency.len());
		Some(object)
	}

	fn remove(&mut self, id: &SO::Id) {
		if let Some((_, seq)) = self.entries.remove(id) {
			self.recency.remove(&seq);
		}
		debug_assert_eq!(self.entries.len(), self.recency.len());
	}
}

/// The in-memory part of a [`DataStore`].
///
/// Modelled as an enum rather than a map plus a policy field so that a [`KeepAllEntries`] store
/// provably pays nothing for the bookkeeping a bounded one needs: its representation is just the
/// map it always was.
enum ObjectCache<SO: StorableObject> {
	KeepAll(HashMap<SO::Id, SO>),
	BoundedLru(LruCache<SO>),
}

impl<SO: StorableObject> ObjectCache<SO> {
	fn new(cache_limit: CacheLimit, objects: Vec<SO>) -> Self {
		match cache_limit {
			CacheLimit::Unbounded => Self::KeepAll(HashMap::from_iter(
				objects.into_iter().map(|object| (object.id(), object)),
			)),
			CacheLimit::Bounded(capacity) => {
				let mut lru = LruCache::new(capacity);
				for object in objects {
					lru.insert(object.id(), object);
				}
				Self::BoundedLru(lru)
			},
		}
	}

	/// Whether the cache holds *every* object of the namespace.
	///
	/// If it does, a miss proves the object is absent from the store, and iterating the cache
	/// yields a complete listing. If it doesn't, both require reading from the store.
	fn is_authoritative(&self) -> bool {
		matches!(self, Self::KeepAll(_))
	}

	/// Returns the cached object for `id`, marking it as most recently used.
	fn get(&mut self, id: &SO::Id) -> Option<SO> {
		match self {
			Self::KeepAll(objects) => objects.get(id).cloned(),
			Self::BoundedLru(lru) => lru.get(id),
		}
	}

	/// Returns whether `id` is cached, without marking it as most recently used.
	fn contains(&self, id: &SO::Id) -> bool {
		match self {
			Self::KeepAll(objects) => objects.contains_key(id),
			Self::BoundedLru(lru) => lru.entries.contains_key(id),
		}
	}

	fn insert(&mut self, id: SO::Id, object: SO) {
		match self {
			Self::KeepAll(objects) => {
				objects.insert(id, object);
			},
			Self::BoundedLru(lru) => lru.insert(id, object),
		}
	}

	fn remove(&mut self, id: &SO::Id) {
		match self {
			Self::KeepAll(objects) => {
				objects.remove(id);
			},
			Self::BoundedLru(lru) => lru.remove(id),
		}
	}

	/// Returns the *cached* objects matching `f`, which is only a complete listing of the
	/// namespace if [`Self::is_authoritative`].
	fn filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Vec<SO> {
		match self {
			Self::KeepAll(objects) => objects.values().filter(f).cloned().collect(),
			Self::BoundedLru(lru) => {
				lru.entries.values().map(|(object, _)| object).filter(f).cloned().collect()
			},
		}
	}

	#[cfg(test)]
	fn len(&self) -> usize {
		match self {
			Self::KeepAll(objects) => objects.len(),
			Self::BoundedLru(lru) => lru.entries.len(),
		}
	}
}

pub(crate) struct DataStore<SO: StorableObject, L: Deref, P: CachePolicy = KeepAllEntries>
where
	L::Target: LdkLogger,
{
	cache: Mutex<ObjectCache<SO>>,
	// Serializes mutations against each other and against readers. Writers hold the write guard
	// across both the store write and the subsequent in-memory update, so readers taking the read
	// guard never observe the window in between, in which the store is already ahead of memory.
	//
	// Note the `cache` lock is always taken *inside* this one, and never held across an `.await`.
	mutation_lock: tokio::sync::RwLock<()>,
	primary_namespace: String,
	secondary_namespace: String,
	kv_store: Arc<DynStore>,
	logger: L,
	cache_policy: PhantomData<P>,
}

impl<SO: StorableObject, L: Deref, P: CachePolicy> DataStore<SO, L, P>
where
	L::Target: LdkLogger,
{
	/// Creates a new store over the given namespace.
	///
	/// `objects` seeds the cache and must already be persisted under that namespace: under a
	/// bounded policy any object beyond `cache_policy`'s capacity is dropped from memory
	/// immediately, and is only recoverable by reading it back from the store.
	pub(crate) fn new(
		objects: Vec<SO>, cache_policy: P, primary_namespace: String, secondary_namespace: String,
		kv_store: Arc<DynStore>, logger: L,
	) -> Self {
		let cache = Mutex::new(ObjectCache::new(cache_policy.cache_limit(), objects));
		Self {
			cache,
			mutation_lock: tokio::sync::RwLock::new(()),
			primary_namespace,
			secondary_namespace,
			kv_store,
			logger,
			cache_policy: PhantomData,
		}
	}

	/// Stores `object`, overwriting any object previously stored under the same id.
	///
	/// Returns whether an object was previously stored under that id.
	pub(crate) async fn insert(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.write().await;

		let id = object.id();
		// Callers treat the return value as "this id was already known", so a cache miss is not
		// enough to answer it under a bounded policy.
		let replaced = self.contains(&id).await?;
		self.persist(&object).await?;
		self.cache.lock().expect("lock").insert(id, object);
		Ok(replaced)
	}

	/// Merges `object` into any object already stored under the same id, or stores it as-is if
	/// there is none.
	///
	/// Returns whether anything was written.
	pub(crate) async fn insert_or_update(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.write().await;

		let id = object.id();
		// Note we have to look through to the store here: merging against a cache miss would
		// overwrite an evicted object with whatever the caller happens to know about it.
		let data_to_persist = match self.lookup(&id).await? {
			Some(mut existing_object) => {
				existing_object.update(object.to_update()).then_some(existing_object)
			},
			None => Some(object),
		};

		match data_to_persist {
			Some(updated_object) => {
				self.persist(&updated_object).await?;
				self.cache.lock().expect("lock").insert(id, updated_object);
				Ok(true)
			},
			None => Ok(false),
		}
	}

	/// Removes the object stored under `id`, if any.
	pub(crate) async fn remove(&self, id: &SO::Id) -> Result<(), Error> {
		let _guard = self.mutation_lock.write().await;

		if !self.contains(id).await? {
			return Ok(());
		}

		let store_key = id.encode_to_hex_str();
		KVStore::remove(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
			false,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Removing object data for key {}/{}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				store_key,
				e
			);
			Error::PersistenceFailed
		})?;
		self.cache.lock().expect("lock").remove(id);
		Ok(())
	}

	/// Returns the object stored under `id`, if any.
	pub(crate) async fn get(&self, id: &SO::Id) -> Result<Option<SO>, Error> {
		let _guard = self.mutation_lock.read().await;
		self.lookup(id).await
	}

	/// Applies `update` to the object stored under its id.
	pub(crate) async fn update(&self, update: SO::Update) -> Result<DataStoreUpdateResult, Error> {
		let _guard = self.mutation_lock.write().await;

		let id = update.id();
		// As in `insert_or_update`, a cache miss is not evidence of absence: reporting `NotFound`
		// for a merely evicted object would drop the update on the floor.
		let Some(mut updated_object) = self.lookup(&id).await? else {
			return Ok(DataStoreUpdateResult::NotFound);
		};
		if !updated_object.update(update) {
			return Ok(DataStoreUpdateResult::Unchanged);
		}

		self.persist(&updated_object).await?;
		self.cache.lock().expect("lock").insert(id, updated_object);
		Ok(DataStoreUpdateResult::Updated)
	}

	/// Returns whether an object is stored under `id`.
	pub(crate) async fn contains_key(&self, id: &SO::Id) -> Result<bool, Error> {
		let _guard = self.mutation_lock.read().await;
		self.contains(id).await
	}

	/// Returns the object stored under `id`, reading through to the [`KVStore`] if the cache is
	/// not authoritative and misses.
	///
	/// The caller must hold `mutation_lock`.
	async fn lookup(&self, id: &SO::Id) -> Result<Option<SO>, Error> {
		let (cached_object, is_authoritative) = {
			let mut locked_cache = self.cache.lock().expect("lock");
			(locked_cache.get(id), locked_cache.is_authoritative())
		};

		if let Some(object) = cached_object {
			return Ok(Some(object));
		}
		if is_authoritative {
			return Ok(None);
		}

		let Some(bytes) = self.read_raw(id).await? else {
			return Ok(None);
		};
		let object = self.decode(id, &bytes)?;
		self.cache.lock().expect("lock").insert(id.clone(), object.clone());
		Ok(Some(object))
	}

	/// Returns whether an object is stored under `id`, without deserializing it or caching it.
	///
	/// The caller must hold `mutation_lock`.
	async fn contains(&self, id: &SO::Id) -> Result<bool, Error> {
		let (is_cached, is_authoritative) = {
			let locked_cache = self.cache.lock().expect("lock");
			(locked_cache.contains(id), locked_cache.is_authoritative())
		};

		if is_cached {
			return Ok(true);
		}
		if is_authoritative {
			return Ok(false);
		}

		Ok(self.read_raw(id).await?.is_some())
	}

	/// Reads the bytes stored under `id`, returning `Ok(None)` if and only if the key is absent.
	///
	/// The caller must hold `mutation_lock`.
	async fn read_raw(&self, id: &SO::Id) -> Result<Option<Vec<u8>>, Error> {
		let store_key = id.encode_to_hex_str();
		match KVStore::read(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
		)
		.await
		{
			Ok(bytes) => Ok(Some(bytes)),
			// An absent key is a legitimate answer, everything else is a failure we must not
			// report as "no such object".
			Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
			Err(e) => {
				log_error!(
					self.logger,
					"Read for key {}/{}/{} failed due to: {}",
					&self.primary_namespace,
					&self.secondary_namespace,
					store_key,
					e
				);
				Err(Error::PersistenceFailed)
			},
		}
	}

	fn decode(&self, id: &SO::Id, bytes: &[u8]) -> Result<SO, Error> {
		SO::read(&mut &bytes[..]).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to deserialize object for key {}/{}/{}: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				id.encode_to_hex_str(),
				e
			);
			Error::PersistenceFailed
		})
	}

	async fn persist(&self, object: &SO) -> Result<(), Error> {
		let (store_key, data) = Self::encode_object(object);
		self.persist_encoded(store_key, data).await
	}

	fn encode_object(object: &SO) -> (String, Vec<u8>) {
		(object.id().encode_to_hex_str(), object.encode())
	}

	async fn persist_encoded(&self, store_key: String, data: Vec<u8>) -> Result<(), Error> {
		KVStore::write(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
			data,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Write for key {}/{}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				store_key,
				e
			);
			Error::PersistenceFailed
		})?;
		Ok(())
	}

	#[cfg(test)]
	fn cached_len(&self) -> usize {
		self.cache.lock().expect("lock").len()
	}

	#[cfg(test)]
	fn is_cached(&self, id: &SO::Id) -> bool {
		self.cache.lock().expect("lock").contains(id)
	}
}

impl<SO: StorableObject, L: Deref> DataStore<SO, L, KeepAllEntries>
where
	L::Target: LdkLogger,
{
	/// Returns all stored objects matching `f`.
	///
	/// Only available on stores that keep every object in memory: answering this on a bounded
	/// store would mean reading its entire namespace back, which is exactly what such a store
	/// exists to avoid.
	pub(crate) async fn list_filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Vec<SO> {
		let _guard = self.mutation_lock.read().await;
		self.cache.lock().expect("lock").filter(f)
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
	use std::time::Duration;

	use lightning::util::persist::{PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning::util::test_utils::TestLogger;
	use lightning::{impl_writeable_tlv_based, io};
	use tokio::sync::Notify;

	use super::*;
	use crate::hex_utils;
	use crate::io::test_utils::InMemoryStore;
	use crate::types::DynStoreWrapper;

	const TEST_PRIMARY_NAMESPACE: &str = "datastore_test_primary";
	const TEST_SECONDARY_NAMESPACE: &str = "datastore_test_secondary";

	fn new_data_store<P: CachePolicy>(
		kv_store: Arc<DynStore>, cache_policy: P, objects: Vec<TestObject>,
	) -> DataStore<TestObject, Arc<TestLogger>, P> {
		DataStore::new(
			objects,
			cache_policy,
			TEST_PRIMARY_NAMESPACE.to_string(),
			TEST_SECONDARY_NAMESPACE.to_string(),
			kv_store,
			Arc::new(TestLogger::new()),
		)
	}

	fn keep_lru(capacity: usize) -> KeepLeastRecentlyUsed {
		KeepLeastRecentlyUsed::new(NonZeroUsize::new(capacity).unwrap())
	}

	fn in_memory_store() -> Arc<DynStore> {
		Arc::new(DynStoreWrapper(InMemoryStore::new()))
	}

	fn test_id(id: u8) -> TestObjectId {
		TestObjectId { id: [id; 4] }
	}

	#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
	struct TestObjectId {
		id: [u8; 4],
	}

	impl StorableObjectId for TestObjectId {
		fn encode_to_hex_str(&self) -> String {
			hex_utils::to_string(&self.id)
		}
	}
	impl_writeable_tlv_based!(TestObjectId, { (0, id, required) });

	struct TestObjectUpdate {
		id: TestObjectId,
		data: [u8; 3],
		/// Only applied when `Some`, mirroring how a real update treats an absent field as "leave
		/// whatever is stored alone".
		extra: Option<u64>,
	}
	impl StorableObjectUpdate<TestObject> for TestObjectUpdate {
		fn id(&self) -> TestObjectId {
			self.id
		}
	}

	#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
	struct TestObject {
		id: TestObjectId,
		data: [u8; 3],
		extra: Option<u64>,
	}

	impl TestObject {
		fn new(id: TestObjectId, data: [u8; 3]) -> Self {
			Self { id, data, extra: None }
		}
	}

	impl StorableObject for TestObject {
		type Id = TestObjectId;
		type Update = TestObjectUpdate;

		fn id(&self) -> Self::Id {
			self.id
		}

		fn update(&mut self, update: Self::Update) -> bool {
			let mut updated = false;
			if self.data != update.data {
				self.data = update.data;
				updated = true;
			}
			if let Some(extra) = update.extra {
				if self.extra != Some(extra) {
					self.extra = Some(extra);
					updated = true;
				}
			}
			updated
		}

		fn to_update(&self) -> Self::Update {
			Self::Update { id: self.id, data: self.data, extra: self.extra }
		}
	}

	impl_writeable_tlv_based!(TestObject, {
		(0, id, required),
		(2, data, required),
		(4, extra, option),
	});

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

	fn new_failing_data_store(objects: Vec<TestObject>) -> DataStore<TestObject, Arc<TestLogger>> {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let logger = Arc::new(TestLogger::new());
		DataStore::new(
			objects,
			KeepAllEntries,
			TEST_PRIMARY_NAMESPACE.to_string(),
			TEST_SECONDARY_NAMESPACE.to_string(),
			store,
			logger,
		)
	}

	/// A store that parks every `write` until it is released, so that tests can hold a write in
	/// flight and observe what concurrent readers see in the meantime.
	struct GatedStore {
		inner: InMemoryStore,
		/// Notified by the store once a `write` has parked.
		write_parked: Arc<Notify>,
		/// Awaited by the store; notify to let the parked `write` proceed.
		release_write: Arc<Notify>,
	}

	impl KVStore for GatedStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			self.inner.read(primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			let write_parked = Arc::clone(&self.write_parked);
			let release_write = Arc::clone(&self.release_write);
			let inner_fut = self.inner.write(primary_namespace, secondary_namespace, key, buf);
			async move {
				write_parked.notify_one();
				release_write.notified().await;
				inner_fut.await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			self.inner.remove(primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			self.inner.list(primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for GatedStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			self.inner.list_paginated(primary_namespace, secondary_namespace, page_token)
		}
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn readers_wait_for_in_flight_writes() {
		let write_parked = Arc::new(Notify::new());
		let release_write = Arc::new(Notify::new());
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(GatedStore {
			inner: InMemoryStore::new(),
			write_parked: Arc::clone(&write_parked),
			release_write: Arc::clone(&release_write),
		}));
		let logger = Arc::new(TestLogger::new());

		let id = TestObjectId { id: [42u8; 4] };
		let old_object = TestObject::new(id, [23u8; 3]);
		let new_object = TestObject::new(id, [24u8; 3]);

		let data_store: Arc<DataStore<TestObject, Arc<TestLogger>>> = Arc::new(DataStore::new(
			vec![old_object],
			KeepAllEntries,
			TEST_PRIMARY_NAMESPACE.to_string(),
			TEST_SECONDARY_NAMESPACE.to_string(),
			store,
			logger,
		));

		let writer_store = Arc::clone(&data_store);
		let writer = tokio::spawn(async move { writer_store.insert(new_object).await });

		// Wait until the write has been handed to the store and parked there, i.e., until the
		// object has been persisted but the in-memory state has not caught up yet.
		write_parked.notified().await;

		// A reader must not be able to observe that window: it has to wait for the writer rather
		// than hand out the pre-write object.
		let read_res = tokio::time::timeout(Duration::from_millis(200), data_store.get(&id)).await;
		assert!(
			read_res.is_err(),
			"Reader observed {:?} while a write was still in flight",
			read_res.unwrap()
		);

		release_write.notify_one();
		assert_eq!(Ok(true), writer.await.unwrap());
		assert_eq!(Some(new_object), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn data_is_persisted() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let primary_namespace = TEST_PRIMARY_NAMESPACE.to_string();
		let secondary_namespace = TEST_SECONDARY_NAMESPACE.to_string();
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			Vec::new(),
			KeepAllEntries,
			primary_namespace.clone(),
			secondary_namespace.clone(),
			Arc::clone(&store),
			logger,
		);

		let id = TestObjectId { id: [42u8; 4] };
		assert!(data_store.get(&id).await.unwrap().is_none());

		let store_key = id.encode_to_hex_str();

		// Check we start empty.
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_err());

		// Check we successfully store an object and return `false`
		let object = TestObject::new(id, [23u8; 3]);
		assert_eq!(Ok(false), data_store.insert(object.clone()).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_ok());

		// Test re-insertion returns `true`
		let mut override_object = object.clone();
		override_object.data = [24u8; 3];
		assert_eq!(Ok(true), data_store.insert(override_object).await);
		assert_eq!(Some(override_object), data_store.get(&id).await.unwrap());

		// Check update returns `Updated`
		let update = TestObjectUpdate { id, data: [25u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::Updated), data_store.update(update).await);
		assert_eq!(data_store.get(&id).await.unwrap().unwrap().data, [25u8; 3]);

		// Check no-op update yields `Unchanged`
		let update = TestObjectUpdate { id, data: [25u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::Unchanged), data_store.update(update).await);

		// Check bogus update yields `NotFound`
		let bogus_id = TestObjectId { id: [84u8; 4] };
		let update = TestObjectUpdate { id: bogus_id, data: [12u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::NotFound), data_store.update(update).await);

		// Check `insert_or_update` inserts unknown objects
		let iou_id = TestObjectId { id: [55u8; 4] };
		let iou_object = TestObject::new(iou_id, [34u8; 3]);
		assert_eq!(Ok(true), data_store.insert_or_update(iou_object.clone()).await);

		// Check `insert_or_update` doesn't update the same object
		assert_eq!(Ok(false), data_store.insert_or_update(iou_object.clone()).await);

		// Check `insert_or_update` updates if object changed
		let mut new_iou_object = iou_object;
		new_iou_object.data[0] += 1;
		assert_eq!(Ok(true), data_store.insert_or_update(new_iou_object).await);
	}

	#[tokio::test]
	async fn insert_or_update_does_not_mutate_memory_if_persist_fails() {
		let existing_id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject::new(existing_id, [23u8; 3]);
		let data_store = new_failing_data_store(vec![existing_object]);

		let updated_object = TestObject::new(existing_id, [24u8; 3]);
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.insert_or_update(updated_object).await
		);
		assert_eq!(Some(existing_object), data_store.get(&existing_id).await.unwrap());

		let new_id = TestObjectId { id: [55u8; 4] };
		let new_object = TestObject::new(new_id, [34u8; 3]);
		assert_eq!(Err(Error::PersistenceFailed), data_store.insert_or_update(new_object).await);
		assert!(data_store.get(&new_id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn insert_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject::new(id, [23u8; 3]);
		let data_store = new_failing_data_store(vec![]);

		assert_eq!(Err(Error::PersistenceFailed), data_store.insert(object).await);
		assert!(data_store.get(&id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn update_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject::new(id, [23u8; 3]);
		let data_store = new_failing_data_store(vec![object]);

		let update = TestObjectUpdate { id, data: [24u8; 3], extra: None };
		assert_eq!(Err(Error::PersistenceFailed), data_store.update(update).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn remove_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject::new(id, [23u8; 3]);
		let data_store = new_failing_data_store(vec![object]);

		assert_eq!(Err(Error::PersistenceFailed), data_store.remove(&id).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
	}

	/// A store that counts how often it is asked to read or list, so that tests can assert a
	/// [`KeepAllEntries`] store never goes to the `KVStore` for a read.
	struct CountingStore {
		inner: InMemoryStore,
		reads: Arc<AtomicUsize>,
		lists: Arc<AtomicUsize>,
	}

	impl KVStore for CountingStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			self.reads.fetch_add(1, Ordering::Relaxed);
			self.inner.read(primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			self.inner.write(primary_namespace, secondary_namespace, key, buf)
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			self.inner.remove(primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			self.lists.fetch_add(1, Ordering::Relaxed);
			self.inner.list(primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for CountingStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			self.lists.fetch_add(1, Ordering::Relaxed);
			self.inner.list_paginated(primary_namespace, secondary_namespace, page_token)
		}
	}

	/// A store whose writes and removals can be made to fail on demand, while reads keep working.
	///
	/// Note a store that fails *reads* would be useless for testing the write paths of a bounded
	/// store, because it would already fail in the read-through that precedes the write.
	struct WriteFailingStore {
		inner: InMemoryStore,
		fail_writes: Arc<AtomicBool>,
	}

	impl KVStore for WriteFailingStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			self.inner.read(primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			let failing = self.fail_writes.load(Ordering::Relaxed);
			let inner_fut = self.inner.write(primary_namespace, secondary_namespace, key, buf);
			async move {
				if failing {
					return Err(io::Error::new(io::ErrorKind::Other, "write failed"));
				}
				inner_fut.await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			let failing = self.fail_writes.load(Ordering::Relaxed);
			let inner_fut = self.inner.remove(primary_namespace, secondary_namespace, key, lazy);
			async move {
				if failing {
					return Err(io::Error::new(io::ErrorKind::Other, "remove failed"));
				}
				inner_fut.await
			}
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			self.inner.list(primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for WriteFailingStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			self.inner.list_paginated(primary_namespace, secondary_namespace, page_token)
		}
	}

	/// Returns a bounded store of the given capacity, together with a handle on the underlying
	/// `KVStore`, and the ids of `num_objects` objects inserted through it.
	///
	/// The objects are inserted in ascending id order, so with `capacity < num_objects` the
	/// lowest ids have been evicted from memory by the time this returns, while remaining
	/// available in the store.
	async fn new_lru_store_with_objects(
		capacity: usize, num_objects: u8,
	) -> (
		DataStore<TestObject, Arc<TestLogger>, KeepLeastRecentlyUsed>,
		Arc<DynStore>,
		Vec<TestObjectId>,
	) {
		let kv_store = in_memory_store();
		let data_store = new_data_store(Arc::clone(&kv_store), keep_lru(capacity), Vec::new());
		let mut ids = Vec::new();
		for i in 0..num_objects {
			let id = test_id(i);
			data_store.insert(TestObject::new(id, [i; 3])).await.unwrap();
			ids.push(id);
		}
		(data_store, kv_store, ids)
	}

	#[tokio::test]
	async fn keep_all_never_reads_from_the_store() {
		let reads = Arc::new(AtomicUsize::new(0));
		let lists = Arc::new(AtomicUsize::new(0));
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(CountingStore {
			inner: InMemoryStore::new(),
			reads: Arc::clone(&reads),
			lists: Arc::clone(&lists),
		}));
		let data_store = new_data_store(kv_store, KeepAllEntries, Vec::new());

		let id = test_id(1);
		let missing_id = test_id(2);
		let object = TestObject::new(id, [23u8; 3]);
		assert_eq!(Ok(false), data_store.insert(object).await);

		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
		assert_eq!(None, data_store.get(&missing_id).await.unwrap());
		assert!(data_store.contains_key(&id).await.unwrap());
		assert!(!data_store.contains_key(&missing_id).await.unwrap());
		assert_eq!(1, data_store.list_filter(|_| true).await.len());
		let no_op = TestObjectUpdate { id, data: [23u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::Unchanged), data_store.update(no_op).await);

		// The whole point of `KeepAllEntries` is that memory is the complete truth, so none of the
		// above may go to the store.
		assert_eq!(0, reads.load(Ordering::Relaxed));
		assert_eq!(0, lists.load(Ordering::Relaxed));
	}

	#[tokio::test]
	async fn lru_evicts_least_recently_used() {
		let (data_store, _kv_store, ids) = new_lru_store_with_objects(2, 3).await;

		assert_eq!(2, data_store.cached_len());
		assert!(!data_store.is_cached(&ids[0]));
		assert!(data_store.is_cached(&ids[1]));
		assert!(data_store.is_cached(&ids[2]));
	}

	#[tokio::test]
	async fn lru_get_marks_an_entry_as_recently_used() {
		let (data_store, _kv_store, ids) = new_lru_store_with_objects(2, 2).await;

		assert!(data_store.get(&ids[0]).await.unwrap().is_some());

		// With `ids[0]` freshly used, inserting a third object must evict `ids[1]` instead.
		let new_id = test_id(9);
		data_store.insert(TestObject::new(new_id, [9u8; 3])).await.unwrap();
		assert!(data_store.is_cached(&ids[0]));
		assert!(!data_store.is_cached(&ids[1]));
		assert!(data_store.is_cached(&new_id));
	}

	#[tokio::test]
	async fn lru_get_reads_through_and_caches() {
		let (data_store, _kv_store, ids) = new_lru_store_with_objects(1, 2).await;
		let evicted_id = ids[0];
		assert!(!data_store.is_cached(&evicted_id));

		let object = data_store.get(&evicted_id).await.unwrap();
		assert_eq!(Some(TestObject::new(evicted_id, [0u8; 3])), object);
		assert!(data_store.is_cached(&evicted_id));
		assert_eq!(1, data_store.cached_len());
	}

	#[tokio::test]
	async fn lru_contains_key_reads_through() {
		let (data_store, _kv_store, ids) = new_lru_store_with_objects(1, 2).await;
		let evicted_id = ids[0];
		assert!(!data_store.is_cached(&evicted_id));

		assert!(data_store.contains_key(&evicted_id).await.unwrap());
		assert!(!data_store.contains_key(&test_id(99)).await.unwrap());
		// A mere existence probe must not displace the working set.
		assert!(!data_store.is_cached(&evicted_id));
	}

	#[tokio::test]
	async fn lru_update_reads_through_evicted_entry() {
		let (data_store, _kv_store, ids) = new_lru_store_with_objects(1, 2).await;
		let evicted_id = ids[0];
		assert!(!data_store.is_cached(&evicted_id));

		// Without reading through, the evicted object would look absent and the update would be
		// dropped on the floor.
		let update = TestObjectUpdate { id: evicted_id, data: [25u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::Updated), data_store.update(update).await);
		assert_eq!([25u8; 3], data_store.get(&evicted_id).await.unwrap().unwrap().data);
	}

	#[tokio::test]
	async fn lru_insert_or_update_does_not_clobber_evicted_entry() {
		let kv_store = in_memory_store();
		let data_store = new_data_store(kv_store, keep_lru(1), Vec::new());

		let id = test_id(1);
		let stored = TestObject { id, data: [23u8; 3], extra: Some(42) };
		data_store.insert(stored).await.unwrap();
		data_store.insert(TestObject::new(test_id(2), [24u8; 3])).await.unwrap();
		assert!(!data_store.is_cached(&id));

		// The incoming object carries no `extra`, so merging must preserve the stored one. Without
		// reading through, the evicted object would be overwritten wholesale and `extra` lost.
		let incoming = TestObject { id, data: [25u8; 3], extra: None };
		assert_eq!(Ok(true), data_store.insert_or_update(incoming).await);

		let merged = data_store.get(&id).await.unwrap().unwrap();
		assert_eq!([25u8; 3], merged.data);
		assert_eq!(Some(42), merged.extra);
	}

	#[tokio::test]
	async fn lru_insert_reports_replacement_of_evicted_entry() {
		let (data_store, _kv_store, ids) = new_lru_store_with_objects(1, 2).await;
		let evicted_id = ids[0];
		assert!(!data_store.is_cached(&evicted_id));

		// Callers rely on this to detect ids they have already seen, so it must not be answered
		// from the cache alone.
		assert_eq!(Ok(true), data_store.insert(TestObject::new(evicted_id, [99u8; 3])).await);
		assert_eq!(Ok(false), data_store.insert(TestObject::new(test_id(99), [99u8; 3])).await);
	}

	#[tokio::test]
	async fn lru_remove_removes_evicted_entry() {
		let (data_store, kv_store, ids) = new_lru_store_with_objects(1, 2).await;
		let evicted_id = ids[0];
		assert!(!data_store.is_cached(&evicted_id));

		data_store.remove(&evicted_id).await.unwrap();

		assert_eq!(None, data_store.get(&evicted_id).await.unwrap());
		let store_key = evicted_id.encode_to_hex_str();
		assert!(KVStore::read(
			&*kv_store,
			TEST_PRIMARY_NAMESPACE,
			TEST_SECONDARY_NAMESPACE,
			&store_key
		)
		.await
		.is_err());
	}

	#[tokio::test]
	async fn lru_seeding_trims_to_capacity() {
		let kv_store = in_memory_store();
		let seed_store = new_data_store(Arc::clone(&kv_store), KeepAllEntries, Vec::new());
		let mut objects = Vec::new();
		for i in 0..5u8 {
			let object = TestObject::new(test_id(i), [i; 3]);
			seed_store.insert(object).await.unwrap();
			objects.push(object);
		}

		let data_store = new_data_store(kv_store, keep_lru(2), objects.clone());
		assert_eq!(2, data_store.cached_len());

		// Everything the cache dropped is still reachable through the store.
		for object in objects {
			assert_eq!(Some(object), data_store.get(&object.id()).await.unwrap());
		}
	}

	#[tokio::test]
	async fn lru_reports_read_failures_rather_than_absence() {
		let data_store =
			new_data_store(Arc::new(DynStoreWrapper(FailingStore)), keep_lru(1), Vec::new());

		let id = test_id(1);
		assert_eq!(Err(Error::PersistenceFailed), data_store.get(&id).await);
		assert_eq!(Err(Error::PersistenceFailed), data_store.contains_key(&id).await);
		assert_eq!(Err(Error::PersistenceFailed), data_store.remove(&id).await);
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.insert_or_update(TestObject::new(id, [1u8; 3])).await
		);
		let update = TestObjectUpdate { id, data: [1u8; 3], extra: None };
		assert_eq!(Err(Error::PersistenceFailed), data_store.update(update).await);

		// By contrast, a store that simply doesn't hold the key must report exactly that.
		let working_store = new_data_store(in_memory_store(), keep_lru(1), Vec::new());
		assert_eq!(Ok(None), working_store.get(&id).await);
		assert_eq!(Ok(false), working_store.contains_key(&id).await);
		assert_eq!(Ok(()), working_store.remove(&id).await);
		let update = TestObjectUpdate { id, data: [1u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::NotFound), working_store.update(update).await);
	}

	#[tokio::test]
	async fn lru_does_not_mutate_memory_if_persist_fails() {
		let fail_writes = Arc::new(AtomicBool::new(false));
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(WriteFailingStore {
			inner: InMemoryStore::new(),
			fail_writes: Arc::clone(&fail_writes),
		}));
		let data_store = new_data_store(kv_store, keep_lru(1), Vec::new());

		let id = test_id(1);
		let stored = TestObject::new(id, [23u8; 3]);
		data_store.insert(stored).await.unwrap();
		// Evict it, so every operation below has to read through first.
		data_store.insert(TestObject::new(test_id(2), [24u8; 3])).await.unwrap();
		assert!(!data_store.is_cached(&id));

		fail_writes.store(true, Ordering::Relaxed);

		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.insert_or_update(TestObject::new(id, [25u8; 3])).await
		);
		let update = TestObjectUpdate { id, data: [26u8; 3], extra: None };
		assert_eq!(Err(Error::PersistenceFailed), data_store.update(update).await);
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.insert(TestObject::new(id, [27u8; 3])).await
		);
		assert_eq!(Err(Error::PersistenceFailed), data_store.remove(&id).await);

		fail_writes.store(false, Ordering::Relaxed);
		assert_eq!(Some(stored), data_store.get(&id).await.unwrap());
	}

	#[test]
	fn lru_cache_keeps_its_indices_in_sync() {
		let mut lru: LruCache<TestObject> = LruCache::new(NonZeroUsize::new(2).unwrap());
		let first = test_id(1);
		let second = test_id(2);
		let third = test_id(3);

		lru.insert(first, TestObject::new(first, [1u8; 3]));
		lru.insert(second, TestObject::new(second, [2u8; 3]));
		assert_eq!(2, lru.entries.len());
		assert_eq!(2, lru.recency.len());

		// Re-inserting a known id must replace rather than grow.
		lru.insert(first, TestObject::new(first, [11u8; 3]));
		assert_eq!(2, lru.entries.len());
		assert_eq!(2, lru.recency.len());

		// `first` was just written, so `second` is the one to go.
		lru.insert(third, TestObject::new(third, [3u8; 3]));
		assert_eq!(2, lru.entries.len());
		assert_eq!(2, lru.recency.len());
		assert!(lru.entries.contains_key(&first));
		assert!(!lru.entries.contains_key(&second));
		assert!(lru.entries.contains_key(&third));

		lru.remove(&first);
		assert_eq!(1, lru.entries.len());
		assert_eq!(1, lru.recency.len());
		// Removing an unknown id is a no-op.
		lru.remove(&second);
		assert_eq!(1, lru.entries.len());
		assert_eq!(1, lru.recency.len());
	}
}
