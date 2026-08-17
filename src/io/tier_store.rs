// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.
#![allow(dead_code)] // TODO: Temporal warning silencer. Will be removed in later commit.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bitcoin::hashes::{sha256, Hash, HashEngine};
use lightning::util::persist::{
	KVStore, PageToken, PaginatedKVStore, PaginatedListResponse, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, SCORER_PERSISTENCE_KEY,
	SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
};
use lightning::{io, log_error};
use tokio::sync::Mutex as TokioMutex;

use crate::io::sqlite_store::{SqliteStore, KV_TABLE_NAME, SQLITE_TIER_INDEX_DB_FILE_NAME};
use crate::io::utils::{check_namespace_key_validity, EXTERNAL_PATHFINDING_SCORES_CACHE_KEY};
use crate::logger::{LdkLogger, Logger};
use crate::types::{DynStore, DynStoreWrapper};

const INDEX_DATABASE_ID_LEN: usize = 16;
const INDEX_ENTRIES_PRIMARY_NAMESPACE: &str = "_tier_store_entries";
const INDEX_METADATA_PRIMARY_NAMESPACE: &str = "_tier_store_metadata";
const INDEX_DATABASE_ID_KEY: &str = "index_database_id";
const INDEX_NAMESPACE_READY_KEY_PREFIX: &str = "ready_";
const INDEX_ENTRY_VALUE: &[u8] = &[1];

pub(crate) struct TierStoreIndex {
	// Holding the store keeps its exclusive SQLite lock for the lifetime of the tier store.
	store: Arc<DynStore>,
}

impl TierStoreIndex {
	/// Opens the internal SQLite index and ensures that it has a persistent database identity.
	async fn new(data_dir: PathBuf) -> io::Result<Self> {
		let store = SqliteStore::new_exclusive(
			data_dir,
			Some(SQLITE_TIER_INDEX_DB_FILE_NAME.to_string()),
			Some(KV_TABLE_NAME.to_string()),
		)?;
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(store));
		Self::read_or_create_database_id(store.as_ref()).await?;
		Ok(Self { store })
	}

	/// Constructs an index over the supplied store for tests that do not need SQLite persistence.
	#[cfg(test)]
	fn from_store(store: Arc<DynStore>) -> Self {
		Self { store }
	}

	/// Derives the internal secondary namespace for a logical namespace pair.
	///
	/// Length-prefixing keeps distinct pairs from producing the same hash input. The original pair
	/// is also stored as namespace metadata so that a hash collision can be detected.
	fn namespace_id(primary_namespace: &str, secondary_namespace: &str) -> String {
		let mut engine = sha256::Hash::engine();
		engine.input(&(primary_namespace.len() as u64).to_be_bytes());
		engine.input(primary_namespace.as_bytes());
		engine.input(&(secondary_namespace.len() as u64).to_be_bytes());
		engine.input(secondary_namespace.as_bytes());
		sha256::Hash::from_engine(engine).to_string()
	}

	/// Derives the metadata key that records whether a logical namespace is index-backed.
	fn namespace_ready_key(primary_namespace: &str, secondary_namespace: &str) -> String {
		format!(
			"{}{}",
			INDEX_NAMESPACE_READY_KEY_PREFIX,
			Self::namespace_id(primary_namespace, secondary_namespace)
		)
	}

	/// Encodes the original logical namespace pair for collision detection.
	fn namespace_metadata(primary_namespace: &str, secondary_namespace: &str) -> Vec<u8> {
		// The fixed five-byte overhead is one format-version byte plus two big-endian u16
		// namespace-length prefixes: 1 + 2 + 2 = 5.
		let mut metadata =
			Vec::with_capacity(5 + primary_namespace.len() + secondary_namespace.len());
		metadata.push(1);
		metadata.extend_from_slice(&(primary_namespace.len() as u16).to_be_bytes());
		metadata.extend_from_slice(primary_namespace.as_bytes());
		metadata.extend_from_slice(&(secondary_namespace.len() as u16).to_be_bytes());
		metadata.extend_from_slice(secondary_namespace.as_bytes());
		metadata
	}

	/// Returns whether the namespace's index is authoritative for listing.
	///
	/// Returns an error if the stored namespace metadata does not match the requested namespace,
	/// which indicates either corrupt metadata or a namespace-ID collision.
	async fn is_namespace_ready(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<bool> {
		match KVStore::read(
			self.store.as_ref(),
			INDEX_METADATA_PRIMARY_NAMESPACE,
			"",
			&Self::namespace_ready_key(primary_namespace, secondary_namespace),
		)
		.await
		{
			Ok(metadata)
				if metadata == Self::namespace_metadata(primary_namespace, secondary_namespace) =>
			{
				Ok(true)
			},
			Ok(_) => Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"Tier-store index namespace collision",
			)),
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
			Err(e) => Err(e),
		}
	}

	/// Marks the namespace's index as authoritative by persisting its original namespace pair.
	async fn mark_namespace_ready(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<()> {
		KVStore::write(
			self.store.as_ref(),
			INDEX_METADATA_PRIMARY_NAMESPACE,
			"",
			&Self::namespace_ready_key(primary_namespace, secondary_namespace),
			Self::namespace_metadata(primary_namespace, secondary_namespace),
		)
		.await
	}

	/// Adds a logical key to the namespace's ordered listing index.
	///
	/// Rewriting an existing key preserves its original index-store creation order.
	async fn write_entry(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<()> {
		KVStore::write(
			self.store.as_ref(),
			INDEX_ENTRIES_PRIMARY_NAMESPACE,
			&Self::namespace_id(primary_namespace, secondary_namespace),
			key,
			INDEX_ENTRY_VALUE.to_vec(),
		)
		.await
	}

	/// Removes a logical key from the namespace's listing index.
	async fn remove_entry(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		KVStore::remove(
			self.store.as_ref(),
			INDEX_ENTRIES_PRIMARY_NAMESPACE,
			&Self::namespace_id(primary_namespace, secondary_namespace),
			key,
			lazy,
		)
		.await
	}

	/// Lists all logical keys recorded in the namespace's index.
	async fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		KVStore::list(
			self.store.as_ref(),
			INDEX_ENTRIES_PRIMARY_NAMESPACE,
			&Self::namespace_id(primary_namespace, secondary_namespace),
		)
		.await
	}

	/// Lists logical keys in the index store's creation order.
	async fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		PaginatedKVStore::list_paginated(
			self.store.as_ref(),
			INDEX_ENTRIES_PRIMARY_NAMESPACE,
			&Self::namespace_id(primary_namespace, secondary_namespace),
			page_token,
		)
		.await
	}

	/// Reads the index database identity, creating and persisting one when it is absent.
	async fn read_or_create_database_id(
		store: &DynStore,
	) -> io::Result<[u8; INDEX_DATABASE_ID_LEN]> {
		match KVStore::read(store, INDEX_METADATA_PRIMARY_NAMESPACE, "", INDEX_DATABASE_ID_KEY)
			.await
		{
			Ok(bytes) => bytes.try_into().map_err(|_| {
				io::Error::new(io::ErrorKind::InvalidData, "Invalid tier-store index database ID")
			}),
			Err(e) if e.kind() == io::ErrorKind::NotFound => {
				let mut database_id = [0; INDEX_DATABASE_ID_LEN];
				getrandom::fill(&mut database_id).map_err(|e| {
					io::Error::new(
						io::ErrorKind::Other,
						format!("Failed to generate tier-store index database ID: {e}"),
					)
				})?;
				KVStore::write(
					store,
					INDEX_METADATA_PRIMARY_NAMESPACE,
					"",
					INDEX_DATABASE_ID_KEY,
					database_id.to_vec(),
				)
				.await?;
				Ok(database_id)
			},
			Err(e) => Err(e),
		}
	}
}

/// A 3-tiered [`KVStore`] implementation that routes data across
/// storage backends that may be local or remote:
/// - a primary store for durable, authoritative persistence,
/// - an optional backup store that maintains an additional durable copy of
///   primary-backed data, and
/// - an optional ephemeral store for non-critical, rebuildable cached data.
///
/// When a backup store is configured, writes and removals for primary-backed data
/// are issued to the primary and backup stores concurrently and only succeed once
/// both stores complete successfully.
///
/// Reads and lists do not consult the backup store during normal operation.
/// Ephemeral data is read from and written to the ephemeral store when configured.
/// Namespaces are indexed locally so unpaginated and paginated listings expose the
/// same logical contents in cross-tier creation order. Existing primary-store keys
/// are imported into the index before a namespace is first modified or listed.
///
/// Note that dual-store writes and removals are not atomic across the primary and
/// backup stores. If one store succeeds and the other fails, the operation
/// returns an error even though one store may already reflect the change.
pub(crate) struct TierStore {
	inner: Arc<TierStoreInner>,
}

impl TierStore {
	pub fn new(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		let inner = Arc::new(TierStoreInner::new(primary_store, Arc::clone(&logger)));

		Self { inner }
	}

	/// Configures a backup store for primary-backed data.
	///
	/// Once set, writes and removals targeting the primary tier succeed only if both
	/// the primary and backup stores succeed. The two operations are issued
	/// concurrently, and any failure is returned to the caller.
	///
	/// Note: dual-store writes/removals are not atomic. An error may be returned
	/// after the primary store has already been updated if the backup store fails.
	///
	/// The backup store is not consulted for normal reads or lists.
	pub fn set_backup_store(&mut self, backup: Arc<DynStore>) {
		debug_assert_eq!(Arc::strong_count(&self.inner), 1);

		let inner = Arc::get_mut(&mut self.inner).expect(
			"TierStore should not be shared during configuration. No other references should exist",
		);

		inner.backup_store = Some(backup);
	}

	/// Configures the ephemeral store for non-critical, rebuildable data.
	///
	/// When configured, selected cache-like data is routed to this store instead of
	/// the primary store.
	pub fn set_ephemeral_store(&mut self, ephemeral: Arc<DynStore>) {
		debug_assert_eq!(Arc::strong_count(&self.inner), 1);

		let inner = Arc::get_mut(&mut self.inner).expect(
			"TierStore should not be shared during configuration. No other references should exist",
		);

		inner.ephemeral_store = Some(ephemeral);
	}

	pub(crate) fn set_index_store(&mut self, index: TierStoreIndex) {
		debug_assert_eq!(Arc::strong_count(&self.inner), 1);

		let inner = Arc::get_mut(&mut self.inner).expect(
			"TierStore should not be shared during configuration. No other references should exist",
		);

		inner.index = Some(index);
	}
}

pub(crate) async fn setup_index_store(data_dir: PathBuf) -> io::Result<TierStoreIndex> {
	TierStoreIndex::new(data_dir).await
}

impl KVStore for TierStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move { inner.read_internal(primary_namespace, secondary_namespace, key).await }
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);
		let locking_key = inner.build_locking_key(primary_namespace, secondary_namespace, key);
		let (lock_ref, version) = inner.get_new_version_and_lock_ref(locking_key.clone());

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move {
			inner
				.write_internal(
					primary_namespace,
					secondary_namespace,
					key,
					buf,
					lock_ref,
					locking_key,
					version,
				)
				.await
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);
		let locking_key = inner.build_locking_key(primary_namespace, secondary_namespace, key);
		let (lock_ref, version) = inner.get_new_version_and_lock_ref(locking_key.clone());

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();

		async move {
			inner
				.remove_internal(
					primary_namespace,
					secondary_namespace,
					key,
					lazy,
					lock_ref,
					locking_key,
					version,
				)
				.await
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();

		async move { inner.list_internal(primary_namespace, secondary_namespace).await }
	}
}

impl PaginatedKVStore for TierStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);

		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();

		async move {
			inner.list_paginated_internal(primary_namespace, secondary_namespace, page_token).await
		}
	}
}

struct TierStoreInner {
	/// The authoritative store for durable data.
	primary_store: Arc<DynStore>,
	/// The store used for non-critical, rebuildable cached data.
	ephemeral_store: Option<Arc<DynStore>>,
	/// An optional second durable store for primary-backed data.
	backup_store: Option<Arc<DynStore>>,
	/// The local store used to index the logical contents across tiers.
	index: Option<TierStoreIndex>,
	/// Per-namespace locks for serializing first-use index initialization.
	index_initialization_locks: Mutex<HashMap<String, Arc<TokioMutex<()>>>>,
	/// Per-key locks for serializing primary+backup operations and skipping stale writes.
	locks: Mutex<HashMap<String, Arc<TokioMutex<u64>>>>,
	next_write_version: AtomicU64,
	logger: Arc<Logger>,
}

impl TierStoreInner {
	/// Creates a tier store with the primary data store.
	pub fn new(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> Self {
		Self {
			primary_store,
			ephemeral_store: None,
			backup_store: None,
			index: None,
			index_initialization_locks: Mutex::new(HashMap::new()),
			locks: Mutex::new(HashMap::new()),
			next_write_version: AtomicU64::new(1),
			logger,
		}
	}

	fn get_new_version_and_lock_ref(&self, locking_key: String) -> (Arc<TokioMutex<u64>>, u64) {
		let version = self.next_write_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("TierStore version counter overflowed");
		}

		let mut locks = self.locks.lock().expect("lock");
		let lock_ref =
			Arc::clone(locks.entry(locking_key).or_insert_with(|| Arc::new(TokioMutex::new(0))));

		(lock_ref, version)
	}

	fn clean_locks(&self, lock_ref: &Arc<TokioMutex<u64>>, locking_key: String) {
		let mut locks = self.locks.lock().expect("lock");
		let strong_count = Arc::strong_count(lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected TierStore lock strong count");
		if strong_count == 2 {
			locks.remove(&locking_key);
		}
	}

	/// Returns the lock that serializes initialization of the given logical namespace.
	fn get_index_initialization_lock(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Arc<TokioMutex<()>> {
		let namespace_id = TierStoreIndex::namespace_id(primary_namespace, secondary_namespace);
		let mut locks = self.index_initialization_locks.lock().expect("lock");
		Arc::clone(locks.entry(namespace_id).or_insert_with(|| Arc::new(TokioMutex::new(()))))
	}

	/// Removes an initialization lock after its final active user releases it.
	fn clean_index_initialization_locks(
		&self, lock_ref: &Arc<TokioMutex<()>>, primary_namespace: &str, secondary_namespace: &str,
	) {
		let namespace_id = TierStoreIndex::namespace_id(primary_namespace, secondary_namespace);
		let mut locks = self.index_initialization_locks.lock().expect("lock");
		if Arc::strong_count(lock_ref) == 2 {
			locks.remove(&namespace_id);
		}
	}

	fn build_locking_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		if primary_namespace.is_empty() {
			key.to_owned()
		} else {
			format!("{}#{}#{}", primary_namespace, secondary_namespace, key)
		}
	}

	/// Reads from the primary data store.
	async fn read_primary(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		match KVStore::read(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
		)
		.await
		{
			Ok(data) => Ok(data),
			Err(e) => Err(e),
		}
	}

	/// Lists keys from the primary data store.
	async fn list_primary(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		match KVStore::list(self.primary_store.as_ref(), primary_namespace, secondary_namespace)
			.await
		{
			Ok(keys) => Ok(keys),
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to list from primary store for namespace {}/{}: {}.",
					primary_namespace,
					secondary_namespace,
					e
				);
				Err(e)
			},
		}
	}

	async fn write_primary_backup_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		if let Some(backup_store) = self.backup_store.as_ref() {
			let primary_fut = KVStore::write(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf.clone(),
			);

			let backup_fut = KVStore::write(
				backup_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf,
			);

			let (primary_res, backup_res) = tokio::join!(primary_fut, backup_fut);

			self.handle_primary_backup_results(
				"write",
				primary_namespace,
				secondary_namespace,
				key,
				primary_res,
				backup_res,
			)
		} else {
			KVStore::write(
				self.primary_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				buf,
			)
			.await
		}
	}

	async fn remove_primary_backup_async(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let primary_fut = KVStore::remove(
			self.primary_store.as_ref(),
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		);

		if let Some(backup_store) = self.backup_store.as_ref() {
			let backup_fut = KVStore::remove(
				backup_store.as_ref(),
				primary_namespace,
				secondary_namespace,
				key,
				lazy,
			);

			let (primary_res, backup_res) = tokio::join!(primary_fut, backup_fut);

			self.handle_primary_backup_results(
				"removal",
				primary_namespace,
				secondary_namespace,
				key,
				primary_res,
				backup_res,
			)
		} else {
			primary_fut.await
		}
	}

	async fn execute_locked_write<F, Fut>(
		&self, lock_ref: Arc<TokioMutex<u64>>, locking_key: String, version: u64, callback: F,
	) -> io::Result<()>
	where
		F: FnOnce() -> Fut,
		Fut: Future<Output = io::Result<()>>,
	{
		let res = {
			let mut last_written_version = lock_ref.lock().await;

			if version <= *last_written_version {
				Ok(())
			} else {
				let res = callback().await;
				// A failed multi-store operation may still have updated one of its stores. We record
				// the attempted version regardless so an older operation cannot overwrite newer state.
				*last_written_version = version;
				res
			}
		};

		self.clean_locks(&lock_ref, locking_key);
		res
	}

	async fn read_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"read",
		)?;

		if is_ephemeral_cached_key(&primary_namespace, &secondary_namespace, &key) {
			if let Some(eph_store) = self.ephemeral_store.as_ref() {
				// We don't retry ephemeral-store reads here. Local failures are treated as
				// terminal for this access path rather than falling back to another store.
				return KVStore::read(
					eph_store.as_ref(),
					&primary_namespace,
					&secondary_namespace,
					&key,
				)
				.await;
			}
		}

		self.read_primary(&primary_namespace, &secondary_namespace, &key).await
	}

	async fn write_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
		lock_ref: Arc<TokioMutex<u64>>, locking_key: String, version: u64,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"write",
		)?;
		self.ensure_namespace_indexed(&primary_namespace, &secondary_namespace).await?;

		if is_ephemeral_cached_key(&primary_namespace, &secondary_namespace, &key) {
			if let Some(eph_store) = self.ephemeral_store.as_ref() {
				let eph_store = Arc::clone(eph_store);
				return self
					.execute_locked_write(lock_ref, locking_key, version, || async move {
						KVStore::write(
							eph_store.as_ref(),
							primary_namespace.as_str(),
							secondary_namespace.as_str(),
							key.as_str(),
							buf,
						)
						.await?;
						if let Some(index) = self.index.as_ref() {
							index
								.write_entry(&primary_namespace, &secondary_namespace, &key)
								.await?;
						}
						Ok(())
					})
					.await;
			}
		}

		self.execute_locked_write(lock_ref, locking_key, version, || async move {
			self.write_primary_backup_async(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				buf,
			)
			.await?;
			if let Some(index) = self.index.as_ref() {
				index.write_entry(&primary_namespace, &secondary_namespace, &key).await?;
			}
			Ok(())
		})
		.await
	}

	async fn remove_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
		lock_ref: Arc<TokioMutex<u64>>, locking_key: String, version: u64,
	) -> io::Result<()> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			Some(key.as_str()),
			"remove",
		)?;
		self.ensure_namespace_indexed(&primary_namespace, &secondary_namespace).await?;

		if is_ephemeral_cached_key(&primary_namespace, &secondary_namespace, &key) {
			if let Some(eph_store) = self.ephemeral_store.as_ref() {
				let eph_store = Arc::clone(eph_store);
				return self
					.execute_locked_write(lock_ref, locking_key, version, || async move {
						KVStore::remove(
							eph_store.as_ref(),
							primary_namespace.as_str(),
							secondary_namespace.as_str(),
							key.as_str(),
							lazy,
						)
						.await?;
						if let Some(index) = self.index.as_ref() {
							index
								.remove_entry(&primary_namespace, &secondary_namespace, &key, lazy)
								.await?;
						}
						Ok(())
					})
					.await;
			}
		}

		self.execute_locked_write(lock_ref, locking_key, version, || async move {
			self.remove_primary_backup_async(
				primary_namespace.as_str(),
				secondary_namespace.as_str(),
				key.as_str(),
				lazy,
			)
			.await?;
			if let Some(index) = self.index.as_ref() {
				index.remove_entry(&primary_namespace, &secondary_namespace, &key, lazy).await?;
			}
			Ok(())
		})
		.await
	}

	async fn list_internal(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			None,
			"list",
		)?;

		self.ensure_namespace_indexed(&primary_namespace, &secondary_namespace).await?;
		if let Some(index) = self.index.as_ref() {
			return index.list(&primary_namespace, &secondary_namespace).await;
		}

		self.list_value_stores(&primary_namespace, &secondary_namespace).await
	}

	/// Imports a namespace's existing primary-store keys and makes its index authoritative.
	///
	/// Primary pagination returns keys from newest to oldest, so the complete result is reversed
	/// before insertion to preserve that order in the local index. The readiness marker is written
	/// last so a failed import is safely retried before the index can be observed.
	async fn ensure_namespace_indexed(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<()> {
		let Some(index) = self.index.as_ref() else {
			return Ok(());
		};
		if index.is_namespace_ready(primary_namespace, secondary_namespace).await? {
			return Ok(());
		}

		let lock_ref = self.get_index_initialization_lock(primary_namespace, secondary_namespace);
		let result: io::Result<()> = async {
			let _guard = lock_ref.lock().await;
			if index.is_namespace_ready(primary_namespace, secondary_namespace).await? {
				Ok(())
			} else {
				let mut keys = Vec::new();
				let mut page_token = None;
				loop {
					let page = PaginatedKVStore::list_paginated(
						self.primary_store.as_ref(),
						primary_namespace,
						secondary_namespace,
						page_token,
					)
					.await?;
					keys.extend(page.keys);
					match page.next_page_token {
						Some(next_page_token) => page_token = Some(next_page_token),
						None => break,
					}
				}

				for key in keys.into_iter().rev() {
					index.write_entry(primary_namespace, secondary_namespace, &key).await?;
				}
				index.mark_namespace_ready(primary_namespace, secondary_namespace).await
			}
		}
		.await;
		self.clean_index_initialization_locks(&lock_ref, primary_namespace, secondary_namespace);
		result
	}

	/// Lists the authoritative logical keys directly from the primary and ephemeral value stores.
	///
	/// Ephemeral-routed keys are taken only from the ephemeral store, excluding stale primary
	/// copies. This path is used when no local index store is configured.
	async fn list_value_stores(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		let mut keys = self.list_primary(primary_namespace, secondary_namespace).await?;

		let Some(ephemeral_store) = self.ephemeral_store.as_ref() else {
			return Ok(keys);
		};

		if primary_namespace != NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE
			&& primary_namespace != SCORER_PERSISTENCE_PRIMARY_NAMESPACE
		{
			return Ok(keys);
		}

		// The ephemeral store is authoritative for keys routed there. Exclude stale
		// primary copies, then add only routed keys from the ephemeral store.
		keys.retain(|key| !is_ephemeral_cached_key(primary_namespace, secondary_namespace, key));

		let ephemeral_keys =
			KVStore::list(ephemeral_store.as_ref(), primary_namespace, secondary_namespace).await?;

		for key in ephemeral_keys {
			if is_ephemeral_cached_key(primary_namespace, secondary_namespace, &key)
				&& !keys.contains(&key)
			{
				keys.push(key);
			}
		}

		Ok(keys)
	}

	async fn list_paginated_internal(
		&self, primary_namespace: String, secondary_namespace: String,
		page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		check_namespace_key_validity(
			primary_namespace.as_str(),
			secondary_namespace.as_str(),
			None,
			"list_paginated",
		)?;

		self.ensure_namespace_indexed(&primary_namespace, &secondary_namespace).await?;
		if let Some(index) = self.index.as_ref() {
			return index
				.list_paginated(&primary_namespace, &secondary_namespace, page_token)
				.await;
		}

		PaginatedKVStore::list_paginated(
			self.primary_store.as_ref(),
			&primary_namespace,
			&secondary_namespace,
			page_token,
		)
		.await
	}

	fn handle_primary_backup_results(
		&self, op: &str, primary_namespace: &str, secondary_namespace: &str, key: &str,
		primary_res: io::Result<()>, backup_res: io::Result<()>,
	) -> io::Result<()> {
		match (primary_res, backup_res) {
			(Ok(()), Ok(())) => Ok(()),
			(Err(primary_err), Ok(())) => {
				log_error!(
					self.logger,
					"Primary {} failed after backup {} succeeded for key {}/{}/{}; primary and backup may have diverged: {}",
					op,
					op,
					primary_namespace,
					secondary_namespace,
					key,
					primary_err
				);
				Err(primary_err)
			},
			(Ok(()), Err(backup_err)) => {
				log_error!(
					self.logger,
					"Backup {} failed after primary {} succeeded for key {}/{}/{}; primary and backup may have diverged: {}",
					op,
					op,
					primary_namespace,
					secondary_namespace,
					key,
					backup_err
				);
				Err(backup_err)
			},
			(Err(primary_err), Err(backup_err)) => {
				log_error!(
					self.logger,
					"Primary and backup {}s both failed for key {}/{}/{}: primary={}, backup={}",
					op,
					primary_namespace,
					secondary_namespace,
					key,
					primary_err,
					backup_err
				);
				Err(primary_err)
			},
		}
	}
}

fn is_ephemeral_cached_key(pn: &str, sn: &str, key: &str) -> bool {
	matches!(
		(pn, sn, key),
		(NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, _, NETWORK_GRAPH_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, SCORER_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, EXTERNAL_PATHFINDING_SCORES_CACHE_KEY)
	)
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::panic::RefUnwindSafe;
	use std::path::PathBuf;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Arc;

	use lightning::util::logger::Level;
	use lightning::util::persist::{
		CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE, SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use lightning_persister::fs_store::v2::FilesystemStoreV2;

	use super::*;
	use crate::io::test_utils::{
		do_read_write_remove_list_persist, random_storage_path, InMemoryStore,
	};
	use crate::io::tier_store::TierStore;
	use crate::logger::Logger;
	use crate::types::{DynStore, DynStoreWrapper};

	impl RefUnwindSafe for TierStore {}

	struct CleanupDir(PathBuf);
	impl Drop for CleanupDir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn setup_tier_store(primary_store: Arc<DynStore>, logger: Arc<Logger>) -> TierStore {
		TierStore::new(primary_store, logger)
	}

	fn set_test_index_store(tier: &mut TierStore) {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		tier.set_index_store(TierStoreIndex::from_store(store));
	}

	#[tokio::test]
	async fn index_store_is_internal_persistent_sqlite_store() {
		let base_dir = random_storage_path();
		let _cleanup = CleanupDir(base_dir.clone());

		let index = setup_index_store(base_dir.clone()).await.unwrap();
		assert!(base_dir.join(SQLITE_TIER_INDEX_DB_FILE_NAME).exists());

		let database_id =
			TierStoreIndex::read_or_create_database_id(index.store.as_ref()).await.unwrap();
		let persisted_database_id =
			TierStoreIndex::read_or_create_database_id(index.store.as_ref()).await.unwrap();
		assert_ne!(database_id, [0; INDEX_DATABASE_ID_LEN]);
		assert_eq!(persisted_database_id, database_id);
	}

	#[tokio::test]
	async fn index_store_rejects_second_owner() {
		let base_dir = random_storage_path();
		let _cleanup = CleanupDir(base_dir.clone());

		let _index = setup_index_store(base_dir.clone()).await.unwrap();
		let error = match setup_index_store(base_dir).await {
			Ok(_) => panic!("a second index-store owner must be rejected"),
			Err(e) => e,
		};
		assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
	}

	#[tokio::test]
	async fn indexed_listing_orders_keys_across_primary_and_ephemeral_stores() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());
		let _cleanup = CleanupDir(base_dir);

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let ephemeral_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(primary_store, logger);
		set_test_index_store(&mut tier);
		tier.set_ephemeral_store(ephemeral_store);

		for key in ["primary-a", NETWORK_GRAPH_PERSISTENCE_KEY, "primary-b"] {
			tier.write(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				key,
				vec![1],
			)
			.await
			.unwrap();
		}

		let page = PaginatedKVStore::list_paginated(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();
		assert_eq!(
			page.keys,
			vec![
				"primary-b".to_string(),
				NETWORK_GRAPH_PERSISTENCE_KEY.to_string(),
				"primary-a".to_string(),
			]
		);

		let mut listed = KVStore::list(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();
		listed.sort();
		assert_eq!(
			listed,
			vec![
				NETWORK_GRAPH_PERSISTENCE_KEY.to_string(),
				"primary-a".to_string(),
				"primary-b".to_string(),
			]
		);
	}

	#[tokio::test]
	async fn indexed_listing_preserves_updates_and_reorders_recreated_keys() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());
		let _cleanup = CleanupDir(base_dir);

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(primary_store, logger);
		set_test_index_store(&mut tier);

		for key in ["a", "b"] {
			tier.write("namespace", "", key, vec![1]).await.unwrap();
		}
		tier.write("namespace", "", "a", vec![2]).await.unwrap();

		let page = PaginatedKVStore::list_paginated(&tier, "namespace", "", None).await.unwrap();
		assert_eq!(page.keys, vec!["b".to_string(), "a".to_string()]);

		tier.remove("namespace", "", "a", false).await.unwrap();
		tier.write("namespace", "", "a", vec![3]).await.unwrap();
		let page = PaginatedKVStore::list_paginated(&tier, "namespace", "", None).await.unwrap();
		assert_eq!(page.keys, vec!["a".to_string(), "b".to_string()]);
	}

	#[tokio::test]
	async fn namespace_initialization_preserves_existing_primary_order_across_pages() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());
		let _cleanup = CleanupDir(base_dir);

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		for i in 0..55 {
			primary_store
				.write("namespace", "", &format!("existing-{i:02}"), vec![1])
				.await
				.unwrap();
		}
		let mut tier = setup_tier_store(primary_store, logger);
		set_test_index_store(&mut tier);

		let mut actual = Vec::new();
		let mut page_token = None;
		loop {
			let page =
				PaginatedKVStore::list_paginated(&tier, "namespace", "", page_token).await.unwrap();
			actual.extend(page.keys);
			match page.next_page_token {
				Some(next_page_token) => page_token = Some(next_page_token),
				None => break,
			}
		}

		let expected = (0..55).rev().map(|i| format!("existing-{i:02}")).collect::<Vec<_>>();
		assert_eq!(actual, expected);
		assert_eq!(KVStore::list(&tier, "namespace", "").await.unwrap().len(), 55);
	}

	enum FailureMode {
		List,
		Write { attempts: Arc<AtomicUsize> },
	}

	/// A store that injects a selected failure while delegating other operations to an inner
	/// [`InMemoryStore`].
	struct FailingStore {
		inner: InMemoryStore,
		failure_mode: FailureMode,
	}

	impl FailingStore {
		fn new(failure_mode: FailureMode) -> Self {
			Self { inner: InMemoryStore::new(), failure_mode }
		}
	}

	impl KVStore for FailingStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&self.inner, primary_namespace, secondary_namespace, key)
		}
		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			let write = if let FailureMode::Write { attempts } = &self.failure_mode {
				attempts.fetch_add(1, Ordering::Relaxed);
				None
			} else {
				Some(KVStore::write(&self.inner, primary_namespace, secondary_namespace, key, buf))
			};
			async move {
				match write {
					Some(write) => write.await,
					None => Err(io::Error::new(io::ErrorKind::Other, "write failed")),
				}
			}
		}
		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&self.inner, primary_namespace, secondary_namespace, key, lazy)
		}
		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			let list = match &self.failure_mode {
				FailureMode::List => None,
				FailureMode::Write { .. } => {
					Some(KVStore::list(&self.inner, primary_namespace, secondary_namespace))
				},
			};
			async move {
				match list {
					Some(list) => list.await,
					None => Err(io::Error::new(io::ErrorKind::Other, "list failed")),
				}
			}
		}
	}

	impl PaginatedKVStore for FailingStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			let list = match &self.failure_mode {
				FailureMode::List => None,
				FailureMode::Write { .. } => Some(PaginatedKVStore::list_paginated(
					&self.inner,
					primary_namespace,
					secondary_namespace,
					page_token,
				)),
			};
			async move {
				match list {
					Some(list) => list.await,
					None => Err(io::Error::new(io::ErrorKind::Other, "list_paginated failed")),
				}
			}
		}
	}

	#[tokio::test]
	async fn write_read_list_remove() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let tier = setup_tier_store(primary_store, logger);

		do_read_write_remove_list_persist(&tier).await;
	}

	#[tokio::test]
	async fn ephemeral_routing() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("ephemeral")).unwrap()));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		let data = vec![42u8; 32];

		tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			data.clone(),
		)
		.await
		.unwrap();

		tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			data.clone(),
		)
		.await
		.unwrap();

		let primary_read_ng = primary_store
			.read(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_KEY,
			)
			.await;
		let ephemeral_read_ng = ephemeral_store
			.read(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_KEY,
			)
			.await;

		let primary_read_cm = primary_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.await;
		let ephemeral_read_cm = ephemeral_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.await;

		assert!(primary_read_ng.is_err());
		assert_eq!(ephemeral_read_ng.unwrap(), data);

		assert!(ephemeral_read_cm.is_err());
		assert_eq!(primary_read_cm.unwrap(), data);
	}

	#[tokio::test]
	async fn external_pathfinding_scores_cache_routes_to_ephemeral_store() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("ephemeral")).unwrap()));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		let data = vec![42u8; 32];
		tier.write(
			SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
			data.clone(),
		)
		.await
		.unwrap();

		assert!(primary_store
			.read(
				SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
				SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
				EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
			)
			.await
			.is_err());
		assert_eq!(
			tier.read(
				SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
				SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
				EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
			)
			.await
			.unwrap(),
			data
		);

		tier.remove(
			SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
			false,
		)
		.await
		.unwrap();
		assert!(ephemeral_store
			.read(
				SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
				SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
				EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
			)
			.await
			.is_err());
	}

	#[tokio::test]
	async fn list_exposes_primary_and_routed_ephemeral_keys() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("ephemeral")).unwrap()));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		// A durable root-namespace key, routed to primary since it isn't ephemeral-cached.
		tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			vec![1u8; 32],
		)
		.await
		.unwrap();

		// The ephemeral-cached key, routed to the ephemeral store.
		tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			vec![2u8; 32],
		)
		.await
		.unwrap();

		// A decoy sitting in the ephemeral store under an unrelated namespace. This must
		// never leak into a listing for that namespace just because an ephemeral
		// store happens to be configured.
		ephemeral_store
			.write(
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				"ephemeral-decoy",
				vec![3u8; 32],
			)
			.await
			.unwrap();
		ephemeral_store
			.write(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				"ephemeral-root-decoy",
				vec![4u8; 32],
			)
			.await
			.unwrap();

		// This is `list("", "")`: CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE and
		// NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE are the same empty string, so both
		// keys live in the exact namespace.
		let root_keys = KVStore::list(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();

		// Unpaginated listing exposes the logical view across both tiers without leaking
		// unrelated keys from the ephemeral store.
		assert!(root_keys.contains(&CHANNEL_MANAGER_PERSISTENCE_KEY.to_string()));
		assert!(root_keys.contains(&NETWORK_GRAPH_PERSISTENCE_KEY.to_string()));
		assert!(!root_keys.contains(&"ephemeral-root-decoy".to_string()));

		let monitor_keys = KVStore::list(
			&tier,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();

		// The unrelated-namespace decoy sitting in the ephemeral store must not leak
		// into a listing for a namespace it was never routed to.
		assert!(!monitor_keys.contains(&"ephemeral-decoy".to_string()));
	}

	#[tokio::test]
	async fn list_paginated_only_exposes_primary_keys() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("ephemeral")).unwrap()));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		tier.write(
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			"monitor-key",
			vec![1u8; 32],
		)
		.await
		.unwrap();

		// This decoy uses the same namespace but the opposite physical store, so it
		// would show up if paginated listing routed to the wrong tier.
		ephemeral_store
			.write(
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				"ephemeral-decoy",
				vec![2u8; 32],
			)
			.await
			.unwrap();

		// This key shares the network graph's namespace tuple ("", "") but is not
		// itself an ephemeral-cached key, standing in for durable root-namespace data
		// such as `manager`/`output_sweeper`/`peers`. It must still be listed even
		// though the ephemeral store is configured and authoritative for
		// `network_graph`/`scorer` specifically.
		primary_store
			.write(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				"other-root-namespace-key",
				vec![3u8; 32],
			)
			.await
			.unwrap();

		tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			vec![4u8; 32],
		)
		.await
		.unwrap();

		let primary_response = PaginatedKVStore::list_paginated(
			&tier,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();
		assert_eq!(primary_response.keys, vec!["monitor-key".to_string()]);

		let root_response = PaginatedKVStore::list_paginated(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();

		assert_eq!(root_response.keys, vec!["other-root-namespace-key".to_string()]);
	}

	#[tokio::test]
	async fn listings_only_consult_ephemeral_store_for_routed_namespaces() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		// An ephemeral store whose `list`/`list_paginated` always fail.
		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FailingStore::new(FailureMode::List)));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		// A durable key in a namespace that can never hold an ephemeral-cached key.
		tier.write(
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			"monitor-key",
			vec![1u8; 32],
		)
		.await
		.unwrap();

		// Listing that namespace must not consult (or depend on) the ephemeral store, so it
		// succeeds even though the ephemeral list would fail.
		let monitor_keys = KVStore::list(
			&tier,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();
		assert_eq!(monitor_keys, vec!["monitor-key".to_string()]);

		// The paginated path always exposes only the primary store.
		let monitor_page = PaginatedKVStore::list_paginated(
			&tier,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();
		assert_eq!(monitor_page.keys, vec!["monitor-key".to_string()]);

		// An unpaginated root listing must consult the authoritative ephemeral store.
		assert!(KVStore::list(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.is_err());
	}

	#[tokio::test]
	async fn list_hides_stale_primary_copy_when_ephemeral_key_is_missing() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		// A durable root-namespace key that must always be discoverable.
		tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			vec![1u8; 32],
		)
		.await
		.unwrap();

		// A stale copy of `network_graph` sitting in primary as if it had been persisted there
		// before the ephemeral store was configured. The ephemeral store holds no copy, so
		// `read` routes to ephemeral and would fail for this key.
		primary_store
			.write(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_KEY,
				vec![2u8; 32],
			)
			.await
			.unwrap();

		let root_keys = KVStore::list(
			&tier,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.unwrap();

		// The ephemeral store is authoritative for routed keys, so its missing entry hides
		// the stale primary copy.
		assert!(root_keys.contains(&CHANNEL_MANAGER_PERSISTENCE_KEY.to_string()));
		assert!(!root_keys.contains(&NETWORK_GRAPH_PERSISTENCE_KEY.to_string()));
	}

	#[tokio::test]
	async fn primary_backed_writes_preserve_latest_call_order() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let tier = setup_tier_store(primary_store, logger);

		let old_data = vec![1u8; 32];
		let new_data = vec![2u8; 32];

		let old_write = tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			old_data,
		);
		let new_write = tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			new_data.clone(),
		);

		new_write.await.unwrap();
		old_write.await.unwrap();

		// Stale data doesn't overwrite latest
		let persisted = tier
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.await
			.unwrap();
		assert_eq!(persisted, new_data);
	}

	#[tokio::test]
	async fn failed_newer_backup_write_still_supersedes_older_write() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir);

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let backup_write_attempts = Arc::new(AtomicUsize::new(0));
		let backup_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FailingStore::new(FailureMode::Write {
				attempts: Arc::clone(&backup_write_attempts),
			})));
		tier.set_backup_store(backup_store);

		let old_data = vec![1u8; 32];
		let new_data = vec![2u8; 32];
		let old_write = tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			old_data,
		);
		let new_write = tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			new_data.clone(),
		);

		// The primary write succeeds, but the same newer write fails on the backup.
		assert!(new_write.await.is_err());
		// The older operation must be treated as stale even though the newer operation failed.
		old_write.await.unwrap();

		let persisted = primary_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.await
			.unwrap();
		assert_eq!(persisted, new_data);
		assert_eq!(backup_write_attempts.load(Ordering::Relaxed), 1);
	}

	#[tokio::test]
	async fn ephemeral_writes_preserve_latest_call_order() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(primary_store, logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("ephemeral")).unwrap()));
		tier.set_ephemeral_store(ephemeral_store);

		let old_data = vec![1u8; 32];
		let new_data = vec![2u8; 32];

		let old_write = tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			old_data,
		);
		let new_write = tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			new_data.clone(),
		);

		new_write.await.unwrap();
		old_write.await.unwrap();

		let persisted = tier
			.read(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_KEY,
			)
			.await
			.unwrap();
		assert_eq!(persisted, new_data);
	}

	#[tokio::test]
	async fn ephemeral_removes_preserve_latest_call_order() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(primary_store, logger);

		let ephemeral_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("ephemeral")).unwrap()));
		tier.set_ephemeral_store(ephemeral_store);

		let data = vec![2u8; 32];

		let stale_remove = tier.remove(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			true,
		);
		let new_write = tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			data.clone(),
		);

		new_write.await.unwrap();
		stale_remove.await.unwrap();

		let persisted = tier
			.read(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_KEY,
			)
			.await
			.unwrap();
		assert_eq!(persisted, data);
	}

	#[tokio::test]
	async fn backup_write_is_part_of_success_path() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let backup_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("backup")).unwrap()));
		tier.set_backup_store(Arc::clone(&backup_store));

		let data = vec![42u8; 32];

		tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			data.clone(),
		)
		.await
		.unwrap();

		let primary_read = primary_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.await;
		let backup_read = backup_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.await;

		assert_eq!(primary_read.unwrap(), data);
		assert_eq!(backup_read.unwrap(), data);
	}

	#[tokio::test]
	async fn backup_remove_is_part_of_success_path() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("primary")).unwrap()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let backup_store: Arc<DynStore> =
			Arc::new(DynStoreWrapper(FilesystemStoreV2::new(base_dir.join("backup")).unwrap()));
		tier.set_backup_store(Arc::clone(&backup_store));

		let data = vec![42u8; 32];
		let key = CHANNEL_MANAGER_PERSISTENCE_KEY;

		tier.write(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
			data,
		)
		.await
		.unwrap();

		tier.remove(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			key,
			true,
		)
		.await
		.unwrap();

		let primary_read = primary_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				key,
			)
			.await;
		let backup_read = backup_store
			.read(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				key,
			)
			.await;

		assert!(primary_read.is_err());
		assert!(backup_read.is_err());
	}
}
