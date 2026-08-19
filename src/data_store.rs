// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use lightning::io::ErrorKind;
use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore};
use lightning::util::ser::{Readable, Writeable};

use crate::io::utils::process_kv_store_reads;
use crate::logger::{log_debug, log_error, LdkLogger};
use crate::types::DynStore;
use crate::Error;

/// An object a store can read, write and delete as a whole, keyed by its own id.
pub(crate) trait StorableObject: Clone + Readable + Writeable {
	type Id: StorableObjectId;

	fn id(&self) -> Self::Id;
}

/// A [`StorableObject`] that a [`DataStore`] can merge an update into in place.
///
/// Separate from [`StorableObject`] because stores that only ever replace whole objects have no use
/// for this, and requiring it of them would mean supplying an update representation nothing calls.
pub(crate) trait UpdatableObject: StorableObject {
	type Update: StorableObjectUpdate<Self>;

	fn update(&mut self, update: Self::Update) -> bool;
	fn to_update(&self) -> Self::Update;
}

pub(crate) trait StorableObjectId: Clone + std::hash::Hash + PartialEq + Eq + Sized {
	fn encode_to_hex_str(&self) -> String;

	/// Recovers an id from the representation produced by [`Self::encode_to_hex_str`].
	///
	/// Returns `None` if `s` is not one. Callers listing a namespace must treat that as a cache
	/// miss and read the object instead, whose own id is authoritative, rather than assume the
	/// store only ever hands back keys we wrote.
	fn decode_from_hex_str(s: &str) -> Option<Self>;
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
pub(crate) struct KeepLeastRecentlyUsed {
	capacity: NonZeroUsize,
}

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

	fn is_keep_all(&self) -> bool {
		matches!(self, Self::KeepAll(_))
	}

	/// Returns the cached object for `id`, marking it as most recently used.
	fn get(&mut self, id: &SO::Id) -> Option<SO> {
		match self {
			Self::KeepAll(objects) => objects.get(id).cloned(),
			Self::BoundedLru(lru) => lru.get(id),
		}
	}

	/// Returns the cached object for `id`, without marking it as most recently used.
	fn peek(&self, id: &SO::Id) -> Option<SO> {
		match self {
			Self::KeepAll(objects) => objects.get(id).cloned(),
			Self::BoundedLru(lru) => lru.entries.get(id).map(|(object, _)| object.clone()),
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
	/// namespace if [`Self::is_keep_all`].
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

/// A page of objects, as returned by [`DataStore::list_page`].
pub(crate) struct DataStorePage<SO> {
	/// The objects in this page, ordered from most recently created to least recently created.
	pub objects: Vec<SO>,
	/// The token to pass to the next [`DataStore::list_page`] call, or `None` if this was the
	/// last page.
	pub next_page_token: Option<PageToken>,
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
	///
	/// They are taken in ascending order of recency, i.e., the last one given is treated as the
	/// most recently used and is therefore the last to be evicted. Callers seeding from a
	/// newest-first source have to reverse it.
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
	pub(crate) async fn insert(&self, object: SO) -> Result<(), Error> {
		let _guard = self.mutation_lock.write().await;

		let id = object.id();
		self.persist(&object).await?;
		self.cache.lock().expect("lock").insert(id, object);
		Ok(())
	}

	/// Removes the object stored under `id`, if any.
	pub(crate) async fn remove(&self, id: &SO::Id) -> Result<(), Error> {
		let _guard = self.mutation_lock.write().await;

		let known_absent = {
			let cache = self.cache.lock().expect("lock");
			cache.is_keep_all() && !cache.contains(id)
		};
		if known_absent {
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

	/// Atomically transforms the entry for `id` through `f` and persists the result.
	///
	/// `f` receives the current entry (`None` when absent) and returns the new state to write;
	/// returning `None` leaves the store untouched. The read, the closure, and the write share
	/// one critical section of the mutation lock, so no concurrent writer can land in between —
	/// unlike a separate [`Self::get`] followed by an insert or update.
	///
	/// The closure runs on a clone of the entry with the in-memory cache lock released, so it may
	/// freely inspect other in-memory state without ordering cache locks against each other. Keep
	/// it cheap and non-blocking.
	///
	/// Returns the written object, or `None` when the closure declined to write.
	pub(crate) async fn mutate<F: FnOnce(Option<&SO>) -> Option<SO>>(
		&self, id: &SO::Id, f: F,
	) -> Result<Option<SO>, Error> {
		self.mutate_with(id, |current| async move { Ok(f(current.as_ref())) }).await
	}

	async fn mutate_with<F, Fut>(&self, id: &SO::Id, f: F) -> Result<Option<SO>, Error>
	where
		F: FnOnce(Option<SO>) -> Fut,
		Fut: Future<Output = Result<Option<SO>, Error>>,
	{
		let _guard = self.mutation_lock.write().await;

		let current = self.lookup(id).await?;
		let new_object = match f(current).await? {
			Some(new_object) => new_object,
			None => return Ok(None),
		};
		debug_assert!(new_object.id() == *id, "mutate closure must not change the object's id");

		self.persist(&new_object).await?;
		self.cache.lock().expect("lock").insert(new_object.id(), new_object.clone());
		Ok(Some(new_object))
	}

	/// Like [`Self::mutate`], but allows the transformation to await fallible reads.
	///
	/// The mutation lock remains held while `f` runs. This is useful when the new state must be
	/// decided from an async read of another store without letting a concurrent writer invalidate
	/// that decision. Callers must keep cross-store lock ordering consistent to avoid deadlocks.
	pub(crate) async fn mutate_async<F, Fut>(&self, id: &SO::Id, f: F) -> Result<Option<SO>, Error>
	where
		F: FnOnce(Option<SO>) -> Fut,
		Fut: Future<Output = Result<Option<SO>, Error>>,
	{
		self.mutate_with(id, f).await
	}

	/// Returns whether an object is stored under `id`.
	pub(crate) async fn contains_key(&self, id: &SO::Id) -> Result<bool, Error> {
		let _guard = self.mutation_lock.read().await;
		self.contains(id).await
	}

	/// Returns a page of objects, ordered from most recently created to least recently created.
	///
	/// Pass `None` to start at the most recently created object, and the returned
	/// [`DataStorePage::next_page_token`] to continue from where the previous call left off.
	///
	/// The ordering and the tokens are the storage backend's own: we hand its opaque token back to
	/// it unchanged and never derive an order of our own. This keeps pagination independent of our
	/// caching, while the token lifetime remains the storage backend's own. The backend's creation
	/// ordering is also why an object updated mid-pagination cannot shift position and so be skipped
	/// or returned twice.
	///
	/// Note this deliberately does not hold the mutation lock across its reads: a listing must not
	/// block every writer for the duration of a round trip to a remote backend. Objects created or
	/// removed while paginating may or may not be observed. Likewise, a page is not a point-in-time
	/// snapshot: concurrently updated objects may reflect different moments depending on whether
	/// they came from the cache or the storage backend.
	///
	/// Note also that a page may hold fewer objects than the backend's page size, because objects
	/// removed between listing the keys and reading them are skipped. Iterate until
	/// `next_page_token` is `None` rather than until a short page.
	pub(crate) async fn list_page(
		&self, page_token: Option<PageToken>,
	) -> Result<DataStorePage<SO>, Error> {
		let response = PaginatedKVStore::list_paginated(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			page_token,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Listing objects under {}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				e
			);
			// The backend rejects a token it didn't issue, which is the caller's problem rather
			// than a persistence failure.
			if e.kind() == ErrorKind::InvalidInput {
				Error::InvalidPageToken
			} else {
				Error::PersistenceFailed
			}
		})?;

		// Serve whatever we already hold, and note the rest to read below. We take the mutation
		// lock only for this, so that we observe a consistent view of the cache without holding up
		// writers while we read.
		let mut objects: Vec<Option<SO>> = vec![None; response.keys.len()];
		let mut missing = Vec::with_capacity(response.keys.len());
		{
			let _guard = self.mutation_lock.read().await;
			let locked_cache = self.cache.lock().expect("lock");
			for (idx, key) in response.keys.iter().enumerate() {
				// Note we deliberately peek rather than `get` here: a listing sweep walks the whole
				// namespace, so letting it count as "use" would evict the working set it walks past.
				match SO::Id::decode_from_hex_str(key).and_then(|id| locked_cache.peek(&id)) {
					Some(object) => objects[idx] = Some(object),
					None => missing.push((idx, key.clone())),
				}
			}
		}

		self.read_missing(&mut objects, missing).await?;

		Ok(DataStorePage {
			objects: objects.into_iter().flatten().collect(),
			next_page_token: response.next_page_token,
		})
	}

	/// Reads the objects we couldn't serve from the cache into their slots in `objects`.
	///
	/// Reads run concurrently but are tracked by slot, as the order in which they finish says
	/// nothing about the order of the page. Note the objects read here are deliberately *not*
	/// cached, see [`Self::list_page`].
	async fn read_missing(
		&self, objects: &mut [Option<SO>], missing: Vec<(usize, String)>,
	) -> Result<(), Error> {
		process_kv_store_reads(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			missing,
			|idx, key, read_res| {
				match read_res {
					Ok(bytes) => match SO::read(&mut &bytes[..]) {
						Ok(object) => objects[idx] = Some(object),
						Err(e) => {
							log_error!(
								self.logger,
								"Failed to deserialize object for key {}/{}/{}: {}",
								&self.primary_namespace,
								&self.secondary_namespace,
								key,
								e
							);
							return Err(Error::PersistenceFailed);
						},
					},
					// The object was removed between us listing the keys and reading it, which is
					// indistinguishable from it having been removed just before the listing. Skip it.
					Err(e) if e.kind() == ErrorKind::NotFound => {
						log_debug!(
							self.logger,
							"Skipping concurrently removed key {}/{}/{}",
							&self.primary_namespace,
							&self.secondary_namespace,
							key
						);
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Read for key {}/{}/{} failed due to: {}",
							&self.primary_namespace,
							&self.secondary_namespace,
							key,
							e
						);
						return Err(Error::PersistenceFailed);
					},
				}
				Ok(())
			},
			|e| {
				log_error!(self.logger, "Failed to join object read task: {}", e);
				Error::PersistenceFailed
			},
		)
		.await
	}

	/// Returns the object stored under `id`, reading through to the [`KVStore`] if the cache is
	/// not authoritative and misses.
	///
	/// The caller must hold `mutation_lock`.
	async fn lookup(&self, id: &SO::Id) -> Result<Option<SO>, Error> {
		let (cached_object, is_keep_all) = {
			let mut locked_cache = self.cache.lock().expect("lock");
			(locked_cache.get(id), locked_cache.is_keep_all())
		};

		if let Some(object) = cached_object {
			return Ok(Some(object));
		}
		if is_keep_all {
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
		let (is_cached, is_keep_all) = {
			let locked_cache = self.cache.lock().expect("lock");
			(locked_cache.contains(id), locked_cache.is_keep_all())
		};

		if is_cached {
			return Ok(true);
		}
		if is_keep_all {
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
	pub(crate) fn cached_len(&self) -> usize {
		self.cache.lock().expect("lock").len()
	}

	#[cfg(test)]
	pub(crate) fn is_cached(&self, id: &SO::Id) -> bool {
		self.cache.lock().expect("lock").contains(id)
	}
}

impl<SO: UpdatableObject, L: Deref, P: CachePolicy> DataStore<SO, L, P>
where
	L::Target: LdkLogger,
{
	/// Like [`Self::insert`], but when an entry with the object's id already exists, merges the
	/// object's full update ([`UpdatableObject::to_update`]) into it instead of replacing it.
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

	/// Applies `update` to the object stored under its id.
	pub(crate) async fn update(&self, update: SO::Update) -> Result<DataStoreUpdateResult, Error> {
		let _guard = self.mutation_lock.write().await;

		let id = update.id();
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
	use crate::io::test_utils::{InMemoryStore, IN_MEMORY_PAGE_SIZE};
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

		fn decode_from_hex_str(s: &str) -> Option<Self> {
			hex_utils::to_vec(s)?.try_into().ok().map(|id| Self { id })
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

		fn id(&self) -> Self::Id {
			self.id
		}
	}

	impl UpdatableObject for TestObject {
		type Update = TestObjectUpdate;

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
		assert_eq!(Ok(()), writer.await.unwrap());
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

		// Check we successfully store an object.
		let object = TestObject::new(id, [23u8; 3]);
		assert_eq!(Ok(()), data_store.insert(object.clone()).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_ok());

		// Test re-insertion overwrites the object.
		let mut override_object = object.clone();
		override_object.data = [24u8; 3];
		assert_eq!(Ok(()), data_store.insert(override_object).await);
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
	async fn mutate_inserts_when_absent() {
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
		let object = TestObject::new(id, [23u8; 3]);
		let result = data_store
			.mutate(&id, |existing| {
				assert!(existing.is_none());
				Some(object)
			})
			.await;
		assert_eq!(Ok(Some(object)), result);

		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
		let store_key = id.encode_to_hex_str();
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_ok());
	}

	#[tokio::test]
	async fn mutate_transforms_existing_entry() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject::new(id, [23u8; 3]);
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![existing_object],
			KeepAllEntries,
			TEST_PRIMARY_NAMESPACE.to_string(),
			TEST_SECONDARY_NAMESPACE.to_string(),
			store,
			logger,
		);

		// The closure sees the current entry and derives the new state from it.
		let result = data_store
			.mutate(&id, |existing| {
				let mut new_object = *existing.unwrap();
				new_object.data[0] += 1;
				Some(new_object)
			})
			.await;
		let expected = TestObject::new(id, [24u8, 23u8, 23u8]);
		assert_eq!(Ok(Some(expected)), result);
		assert_eq!(Some(expected), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn mutate_async_awaits_fallible_reads() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let id = TestObjectId { id: [42u8; 4] };
		let other_id = TestObjectId { id: [43u8; 4] };
		let existing_object = TestObject::new(id, [23u8; 3]);
		let other_object = TestObject::new(other_id, [24u8; 3]);
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![existing_object],
			KeepAllEntries,
			TEST_PRIMARY_NAMESPACE.to_string(),
			TEST_SECONDARY_NAMESPACE.to_string(),
			Arc::clone(&store),
			Arc::clone(&logger),
		);
		let other_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![other_object],
			KeepAllEntries,
			"other_datastore_test_primary".to_string(),
			"other_datastore_test_secondary".to_string(),
			store,
			logger,
		);

		let result = data_store
			.mutate_async(&id, |existing| async move {
				let mut updated = existing.unwrap();
				updated.data = other_store.get(&other_id).await?.unwrap().data;
				Ok(Some(updated))
			})
			.await;
		let expected = TestObject::new(id, [24u8; 3]);
		assert_eq!(Ok(Some(expected)), result);
		assert_eq!(Some(expected), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn mutate_runs_the_closure_without_the_cache_lock() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject::new(id, [23u8; 3]);
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![existing_object],
			KeepAllEntries,
			TEST_PRIMARY_NAMESPACE.to_string(),
			TEST_SECONDARY_NAMESPACE.to_string(),
			store,
			logger,
		);

		// Closures gate cross-store decisions on reads of other stores, which lock their own
		// in-memory caches. Holding this store's cache lock across the closure would order it
		// before theirs and invite lock-order inversions, so the closure must run with the cache
		// lock released.
		let result = data_store
			.mutate(&id, |existing| {
				assert_eq!(Some(&existing_object), existing);
				assert!(data_store.cache.try_lock().is_ok());
				None
			})
			.await;
		assert_eq!(Ok(None), result);
	}

	#[tokio::test]
	async fn mutate_persists_nothing_when_closure_declines() {
		let id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject::new(id, [23u8; 3]);
		let data_store = new_failing_data_store(vec![existing_object]);

		// Returning `None` must not attempt a write (the store fails all writes) nor touch memory.
		let result = data_store
			.mutate(&id, |existing| {
				assert_eq!(Some(&existing_object), existing);
				None
			})
			.await;
		assert_eq!(Ok(None), result);
		assert_eq!(Some(existing_object), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn mutate_does_not_mutate_memory_if_persist_fails() {
		let existing_id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject::new(existing_id, [23u8; 3]);
		let data_store = new_failing_data_store(vec![existing_object]);

		let changed = TestObject::new(existing_id, [24u8; 3]);
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.mutate(&existing_id, |_| Some(changed)).await
		);
		assert_eq!(Some(existing_object), data_store.get(&existing_id).await.unwrap());

		let new_id = TestObjectId { id: [55u8; 4] };
		let new_object = TestObject::new(new_id, [34u8; 3]);
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.mutate(&new_id, |_| Some(new_object)).await
		);
		assert!(data_store.get(&new_id).await.unwrap().is_none());
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

	/// A store that counts how often it is asked to read, write, remove, or list.
	struct CountingStore {
		inner: InMemoryStore,
		reads: Arc<AtomicUsize>,
		writes: Arc<AtomicUsize>,
		removes: Arc<AtomicUsize>,
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
			self.writes.fetch_add(1, Ordering::Relaxed);
			self.inner.write(primary_namespace, secondary_namespace, key, buf)
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			self.removes.fetch_add(1, Ordering::Relaxed);
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
		let writes = Arc::new(AtomicUsize::new(0));
		let lists = Arc::new(AtomicUsize::new(0));
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(CountingStore {
			inner: InMemoryStore::new(),
			reads: Arc::clone(&reads),
			writes: Arc::clone(&writes),
			removes: Arc::new(AtomicUsize::new(0)),
			lists: Arc::clone(&lists),
		}));
		let data_store = new_data_store(kv_store, KeepAllEntries, Vec::new());

		let id = test_id(1);
		let missing_id = test_id(2);
		let object = TestObject::new(id, [23u8; 3]);
		assert_eq!(Ok(()), data_store.insert(object).await);

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
	async fn lru_insert_writes_without_reading() {
		let reads = Arc::new(AtomicUsize::new(0));
		let writes = Arc::new(AtomicUsize::new(0));
		let lists = Arc::new(AtomicUsize::new(0));
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(CountingStore {
			inner: InMemoryStore::new(),
			reads: Arc::clone(&reads),
			writes: Arc::clone(&writes),
			removes: Arc::new(AtomicUsize::new(0)),
			lists,
		}));
		let data_store = new_data_store(kv_store, keep_lru(1), Vec::new());

		data_store.insert(TestObject::new(test_id(1), [1u8; 3])).await.unwrap();

		assert_eq!(0, reads.load(Ordering::Relaxed), "insert must not read from the store");
		assert_eq!(1, writes.load(Ordering::Relaxed), "insert must write exactly once");
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
	async fn lru_remove_removes_without_reading() {
		let reads = Arc::new(AtomicUsize::new(0));
		let removes = Arc::new(AtomicUsize::new(0));
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(CountingStore {
			inner: InMemoryStore::new(),
			reads: Arc::clone(&reads),
			writes: Arc::new(AtomicUsize::new(0)),
			removes: Arc::clone(&removes),
			lists: Arc::new(AtomicUsize::new(0)),
		}));
		let data_store = new_data_store(Arc::clone(&kv_store), keep_lru(1), Vec::new());
		let evicted_id = test_id(1);
		data_store.insert(TestObject::new(evicted_id, [1u8; 3])).await.unwrap();
		data_store.insert(TestObject::new(test_id(2), [2u8; 3])).await.unwrap();
		assert!(!data_store.is_cached(&evicted_id));

		data_store.remove(&evicted_id).await.unwrap();

		assert_eq!(0, reads.load(Ordering::Relaxed), "remove must not read from the store");
		assert_eq!(1, removes.load(Ordering::Relaxed), "remove must execute exactly once");
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
	async fn lru_seeding_treats_the_last_object_as_most_recently_used() {
		// The builder relies on this to hand a newest-first read over in reverse: whichever
		// objects are given last must be the ones that survive, or seeding the cache would
		// preferentially throw away the newest entries.
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
		assert!(data_store.is_cached(&objects[3].id));
		assert!(data_store.is_cached(&objects[4].id));
		for object in objects.iter().take(3) {
			assert!(!data_store.is_cached(&object.id));
		}
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

	/// A store that reports one key from `list_paginated` that it will then fail to read, standing
	/// in for an entry removed between the two calls.
	struct PhantomKeyStore {
		inner: InMemoryStore,
		phantom_key: String,
	}

	impl KVStore for PhantomKeyStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
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
			self.inner.list(primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for PhantomKeyStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			let phantom_key = self.phantom_key.clone();
			let inner_fut =
				self.inner.list_paginated(primary_namespace, secondary_namespace, page_token);
			async move {
				let mut response = inner_fut.await?;
				response.keys.insert(0, phantom_key);
				Ok(response)
			}
		}
	}

	/// Sweeps every page and returns the objects in the order they were listed.
	async fn list_all_pages<P: CachePolicy>(
		data_store: &DataStore<TestObject, Arc<TestLogger>, P>,
	) -> Vec<TestObject> {
		let mut all = Vec::new();
		let mut page_token = None;
		loop {
			let page = data_store.list_page(page_token).await.unwrap();
			all.extend(page.objects);
			match page.next_page_token {
				Some(token) => page_token = Some(token),
				None => break,
			}
		}
		all
	}

	/// Inserts `num_objects` objects with ascending ids through `data_store`.
	async fn insert_ascending<P: CachePolicy>(
		data_store: &DataStore<TestObject, Arc<TestLogger>, P>, num_objects: usize,
	) -> Vec<TestObject> {
		let mut objects = Vec::new();
		for i in 0..num_objects {
			let id = TestObjectId { id: (i as u32).to_be_bytes() };
			let object = TestObject::new(id, [7u8; 3]);
			data_store.insert(object).await.unwrap();
			objects.push(object);
		}
		objects
	}

	#[tokio::test]
	async fn list_page_walks_pages_in_reverse_creation_order() {
		let data_store = new_data_store(in_memory_store(), KeepAllEntries, Vec::new());
		let num_objects = 2 * IN_MEMORY_PAGE_SIZE + 25;
		let inserted = insert_ascending(&data_store, num_objects).await;

		// Check the pages themselves are sized and terminated as expected.
		let first = data_store.list_page(None).await.unwrap();
		assert_eq!(IN_MEMORY_PAGE_SIZE, first.objects.len());
		let second = data_store.list_page(first.next_page_token).await.unwrap();
		assert_eq!(IN_MEMORY_PAGE_SIZE, second.objects.len());
		let third = data_store.list_page(second.next_page_token).await.unwrap();
		assert_eq!(25, third.objects.len());
		assert!(third.next_page_token.is_none());

		let mut expected = inserted;
		expected.reverse();
		assert_eq!(expected, list_all_pages(&data_store).await);
	}

	#[tokio::test]
	async fn list_page_orders_by_creation_not_by_update() {
		let data_store = new_data_store(in_memory_store(), KeepAllEntries, Vec::new());
		let inserted = insert_ascending(&data_store, IN_MEMORY_PAGE_SIZE + 10).await;

		// Touch the oldest object. Ordering by update time would move it to the front and so drop
		// or duplicate entries across pages; ordering by creation must leave it where it is.
		let oldest = inserted.first().unwrap();
		let update = TestObjectUpdate { id: oldest.id, data: [99u8; 3], extra: None };
		assert_eq!(Ok(DataStoreUpdateResult::Updated), data_store.update(update).await);

		let listed = list_all_pages(&data_store).await;
		assert_eq!(inserted.len(), listed.len());
		assert_eq!(oldest.id, listed.last().unwrap().id);
		assert_eq!([99u8; 3], listed.last().unwrap().data);
	}

	#[tokio::test]
	async fn list_page_token_survives_a_restart() {
		let kv_store = in_memory_store();
		let num_objects = IN_MEMORY_PAGE_SIZE + 10;
		let inserted = {
			let data_store = new_data_store(Arc::clone(&kv_store), KeepAllEntries, Vec::new());
			insert_ascending(&data_store, num_objects).await
		};

		let first_page = {
			let data_store = new_data_store(Arc::clone(&kv_store), KeepAllEntries, Vec::new());
			data_store.list_page(None).await.unwrap()
		};
		let token = first_page.next_page_token.clone().unwrap();

		// Resume from a *fresh* store, i.e., with nothing in memory, as an app would after being
		// restarted between two pages. Because the ordering and the token are the backend's own,
		// and not something we number ourselves, the continuation must still line up exactly.
		let data_store = new_data_store(Arc::clone(&kv_store), KeepAllEntries, Vec::new());
		let second_page = data_store.list_page(Some(token)).await.unwrap();

		let mut expected = inserted;
		expected.reverse();
		assert_eq!(expected[..IN_MEMORY_PAGE_SIZE], first_page.objects[..]);
		assert_eq!(expected[IN_MEMORY_PAGE_SIZE..], second_page.objects[..]);
		assert!(second_page.next_page_token.is_none());
	}

	#[tokio::test]
	async fn list_page_does_not_repeat_entries_after_removals_and_a_restart() {
		let kv_store = in_memory_store();
		let num_objects = IN_MEMORY_PAGE_SIZE + 10;
		let inserted = {
			let data_store = new_data_store(Arc::clone(&kv_store), KeepAllEntries, Vec::new());
			insert_ascending(&data_store, num_objects).await
		};

		let data_store = new_data_store(Arc::clone(&kv_store), KeepAllEntries, inserted.clone());
		let first_page = data_store.list_page(None).await.unwrap();
		let token = first_page.next_page_token.clone().unwrap();
		let seen: Vec<TestObjectId> = first_page.objects.iter().map(|o| o.id).collect();

		// Remove the oldest objects, including the one the token points at, then resume from a
		// fresh store. An implementation that renumbered its own ordering on load would hand back
		// entries from the first page again here.
		let cursor_id = seen.last().copied().unwrap();
		data_store.remove(&cursor_id).await.unwrap();
		for object in inserted.iter().take(5) {
			data_store.remove(&object.id).await.unwrap();
		}

		let resumed = new_data_store(Arc::clone(&kv_store), KeepAllEntries, Vec::new());
		let second_page = resumed.list_page(Some(token)).await.unwrap();
		for object in &second_page.objects {
			assert!(
				!seen.contains(&object.id),
				"Object {:?} was returned on more than one page",
				object.id
			);
		}
	}

	#[tokio::test]
	async fn list_page_serves_entries_that_are_not_in_memory() {
		// The point of doing this against the store rather than an ordering of our own: a bounded
		// store can only hold a fraction of the namespace, yet must still list all of it.
		let data_store = new_data_store(in_memory_store(), keep_lru(10), Vec::new());
		let inserted = insert_ascending(&data_store, 2 * IN_MEMORY_PAGE_SIZE + 25).await;
		assert_eq!(10, data_store.cached_len());

		let mut expected = inserted;
		expected.reverse();
		assert_eq!(expected, list_all_pages(&data_store).await);
	}

	#[tokio::test]
	async fn list_page_does_not_disturb_the_cache() {
		let data_store = new_data_store(in_memory_store(), keep_lru(2), Vec::new());
		let first = test_id(1);
		let second = test_id(2);
		data_store.insert(TestObject::new(first, [1u8; 3])).await.unwrap();
		data_store.insert(TestObject::new(second, [2u8; 3])).await.unwrap();

		// Make `first` the most recently used, then sweep the whole namespace.
		assert!(data_store.get(&first).await.unwrap().is_some());
		assert_eq!(2, list_all_pages(&data_store).await.len());
		assert_eq!(2, data_store.cached_len());

		// The sweep must not have counted as use, so `second` is still the one to evict.
		let third = test_id(3);
		data_store.insert(TestObject::new(third, [3u8; 3])).await.unwrap();
		assert!(data_store.is_cached(&first));
		assert!(!data_store.is_cached(&second));
		assert!(data_store.is_cached(&third));
	}

	#[tokio::test]
	async fn list_page_skips_concurrently_removed_keys() {
		let phantom_id = test_id(200);
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(PhantomKeyStore {
			inner: InMemoryStore::new(),
			phantom_key: phantom_id.encode_to_hex_str(),
		}));
		let data_store = new_data_store(kv_store, keep_lru(1), Vec::new());
		let inserted = insert_ascending(&data_store, 3).await;

		// A key that vanished between being listed and being read is skipped rather than failing
		// the whole listing.
		let page = data_store.list_page(None).await.unwrap();
		assert_eq!(3, page.objects.len());
		for object in inserted {
			assert!(page.objects.contains(&object));
		}
	}

	#[tokio::test]
	async fn list_page_reports_failures() {
		let failing =
			new_data_store(Arc::new(DynStoreWrapper(FailingStore)), KeepAllEntries, Vec::new());
		assert_eq!(Err(Error::PersistenceFailed), failing.list_page(None).await.map(|_| ()));
	}

	#[tokio::test]
	async fn list_page_rejects_a_malformed_token() {
		let data_store = new_data_store(in_memory_store(), KeepAllEntries, Vec::new());
		insert_ascending(&data_store, 1).await;

		let token = PageToken::new("not-a-token".to_string());
		assert_eq!(
			Err(Error::InvalidPageToken),
			data_store.list_page(Some(token)).await.map(|_| ())
		);
	}

	#[tokio::test]
	async fn list_page_reports_undecodable_objects() {
		let kv_store = in_memory_store();
		let data_store = new_data_store(Arc::clone(&kv_store), keep_lru(1), Vec::new());
		insert_ascending(&data_store, 2).await;

		// Corrupt an object that is no longer cached, so that listing has to read it back.
		let corrupted_id = TestObjectId { id: 0u32.to_be_bytes() };
		assert!(!data_store.is_cached(&corrupted_id));
		KVStore::write(
			&*kv_store,
			TEST_PRIMARY_NAMESPACE,
			TEST_SECONDARY_NAMESPACE,
			&corrupted_id.encode_to_hex_str(),
			vec![0xff; 3],
		)
		.await
		.unwrap();

		assert_eq!(Err(Error::PersistenceFailed), data_store.list_page(None).await.map(|_| ()));
	}

	#[tokio::test]
	async fn list_page_on_an_empty_store() {
		let data_store = new_data_store(in_memory_store(), KeepAllEntries, Vec::new());
		let page = data_store.list_page(None).await.unwrap();
		assert!(page.objects.is_empty());
		assert!(page.next_page_token.is_none());
	}
}
