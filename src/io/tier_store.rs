// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lightning::util::persist::{
	KVStore, PageToken, PaginatedKVStore, PaginatedListResponse, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, SCORER_PERSISTENCE_KEY,
	SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
};
use lightning::{io, log_error};
use tokio::sync::Mutex as TokioMutex;

use crate::io::utils::check_namespace_key_validity;
use crate::logger::{LdkLogger, Logger};
use crate::types::DynStore;

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
				if res.is_ok() {
					*last_written_version = version;
				}
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
						.await
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
			.await
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
						.await
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
			.await
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

		let mut keys = self.list_primary(&primary_namespace, &secondary_namespace).await?;

		self.apply_ephemeral_overlay(&primary_namespace, &secondary_namespace, &mut keys, true)
			.await?;

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

		let mut response = PaginatedKVStore::list_paginated(
			self.primary_store.as_ref(),
			&primary_namespace,
			&secondary_namespace,
			page_token,
		)
		.await?;

		// Filter stale primary copies of ephemeral-cached keys from every page, and append the
		// live ephemeral overlay only once primary pagination is exhausted (`next_page_token`
		// is `None`).
		//
		// Because filtering can drop entries from a full page, a non-terminal page can come back
		// shorter than the nominal page size: callers must not treat `next_page_token.is_some()`
		// as a guarantee of a full page. Conversely, appending the overlay onto an already
		// full-sized terminal page can make it up to `MAX_EPHEMERAL_CACHED_KEYS_PER_NAMESPACE`
		// entries larger than the nominal page size. Both effects are bounded by that constant.
		let append_live = response.next_page_token.is_none();
		self.apply_ephemeral_overlay(
			&primary_namespace,
			&secondary_namespace,
			&mut response.keys,
			append_live,
		)
		.await?;

		Ok(response)
	}

	/// Reconciles a set of keys already listed from the primary store with the ephemeral store.
	///
	/// This keeps `list`/`list_paginated` consistent with the key-level routing in
	/// `read`/`write`/`remove`: once an ephemeral store is configured it is authoritative for
	/// ephemeral-cached keys (`network_graph`/`scorer`). Any copy of such a key still held by the
	/// primary store is a stale leftover from before the ephemeral store was configured, so we
	/// drop it here; when `append_live` is set we then append the live ephemeral copy.
	///
	/// The ephemeral store is only consulted for namespaces that can actually contain an
	/// ephemeral-cached key (see [`namespace_may_hold_ephemeral_cached_key`]). For every other
	/// namespace this is a no-op, so listing durable, primary-backed data never depends on the
	/// optional ephemeral backend being reachable.
	async fn apply_ephemeral_overlay(
		&self, primary_namespace: &str, secondary_namespace: &str, keys: &mut Vec<String>,
		append_live: bool,
	) -> io::Result<()> {
		let Some(eph_store) = self.ephemeral_store.as_ref() else {
			return Ok(());
		};

		if !namespace_may_hold_ephemeral_cached_key(primary_namespace) {
			return Ok(());
		}

		keys.retain(|key| !is_ephemeral_cached_key(primary_namespace, secondary_namespace, key));

		if !append_live {
			return Ok(());
		}

		// We don't retry ephemeral-store lists here. Local failures are treated as terminal for
		// this access path rather than falling back to another store.
		let cached_keys: Vec<String> =
			KVStore::list(eph_store.as_ref(), primary_namespace, secondary_namespace)
				.await?
				.into_iter()
				.filter(|key| is_ephemeral_cached_key(primary_namespace, secondary_namespace, key))
				.collect();

		debug_assert!(
			cached_keys.len() <= MAX_EPHEMERAL_CACHED_KEYS_PER_NAMESPACE,
			"ephemeral-cached key overlay ({} keys) exceeded the bound this pagination \
			 design assumes -- see MAX_EPHEMERAL_CACHED_KEYS_PER_NAMESPACE",
			cached_keys.len(),
		);

		for key in cached_keys {
			// Guards against `list` on the ephemeral store itself returning a duplicate entry.
			if !keys.contains(&key) {
				keys.push(key);
			}
		}

		Ok(())
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

/// The maximum number of distinct keys [`is_ephemeral_cached_key`] can ever match for a
/// single namespace pair -- one per matched key literal (`network_graph`, `scorer`).
///
/// `apply_ephemeral_overlay` reads the ephemeral-cached overlay unpaginated and appends
/// it onto the terminal primary page in a single shot, reusing primary's own page token
/// unmodified rather than tracking a cross-store cursor. That's only sound while this
/// stays small. If a future key is added to `is_ephemeral_cached_key`, bump this constant
/// to match, and re-examine whether `list_paginated_internal` still needs revisiting -- a
/// larger or unbounded ephemeral-cached set can silently blow past the nominal page size
/// and can't correctly report `next_page_token` for a partial overlay.
const MAX_EPHEMERAL_CACHED_KEYS_PER_NAMESPACE: usize = 2;

fn is_ephemeral_cached_key(pn: &str, sn: &str, key: &str) -> bool {
	matches!(
		(pn, sn, key),
		(NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, _, NETWORK_GRAPH_PERSISTENCE_KEY)
			| (SCORER_PERSISTENCE_PRIMARY_NAMESPACE, _, SCORER_PERSISTENCE_KEY)
	)
}

/// Whether a primary namespace can ever contain a key that [`is_ephemeral_cached_key`] matches.
///
/// Listing paths use this to gate the (optional) ephemeral-store lookup: for any namespace that
/// cannot hold an ephemeral-cached key, the overlay is skipped entirely, so durable,
/// primary-backed listings neither pay for an extra ephemeral list nor fail when the ephemeral
/// backend is unreachable.
fn namespace_may_hold_ephemeral_cached_key(pn: &str) -> bool {
	pn == NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE || pn == SCORER_PERSISTENCE_PRIMARY_NAMESPACE
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::panic::RefUnwindSafe;
	use std::path::PathBuf;
	use std::sync::Arc;

	use lightning::util::logger::Level;
	use lightning::util::persist::{
		CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use lightning_persister::fs_store::v2::FilesystemStoreV2;

	use super::*;
	use crate::io::test_utils::{
		do_read_write_remove_list_persist, random_storage_path, InMemoryStore, IN_MEMORY_PAGE_SIZE,
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

	/// A store whose `list`/`list_paginated` always fail while every other operation is delegated
	/// to an inner [`InMemoryStore`]. Used to prove that a failing ephemeral list does not sink a
	/// listing for a namespace that can never hold an ephemeral-cached key.
	struct FailingListStore {
		inner: InMemoryStore,
	}

	impl FailingListStore {
		fn new() -> Self {
			Self { inner: InMemoryStore::new() }
		}
	}

	impl KVStore for FailingListStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&self.inner, primary_namespace, secondary_namespace, key)
		}
		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::write(&self.inner, primary_namespace, secondary_namespace, key, buf)
		}
		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&self.inner, primary_namespace, secondary_namespace, key, lazy)
		}
		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "list failed")) }
		}
	}

	impl PaginatedKVStore for FailingListStore {
		fn list_paginated(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
			_page_token: Option<PageToken>,
		) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "list_paginated failed")) }
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
	async fn list_discovers_durable_keys_alongside_ephemeral_cache() {
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

		// The durable primary-backed key and the ephemeral-cached key must both be
		// discoverable from a single call.
		assert!(root_keys.contains(&CHANNEL_MANAGER_PERSISTENCE_KEY.to_string()));
		assert!(root_keys.contains(&NETWORK_GRAPH_PERSISTENCE_KEY.to_string()));

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
	async fn list_paginated_routes_to_selected_tier() {
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

		let ephemeral_response = PaginatedKVStore::list_paginated(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();

		// The durable root-namespace key surfaces from the primary store, and the
		// ephemeral-cached `network_graph` key is appended once primary's pagination
		// is exhausted.
		assert_eq!(
			ephemeral_response.keys,
			vec!["other-root-namespace-key".to_string(), NETWORK_GRAPH_PERSISTENCE_KEY.to_string()]
		);
	}

	#[tokio::test]
	async fn list_paginated_filters_stale_primary_copies_across_every_page() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		let ephemeral_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		tier.set_ephemeral_store(Arc::clone(&ephemeral_store));

		// Exactly IN_MEMORY_PAGE_SIZE unrelated, durable root-namespace keys written
		// directly to primary, so a single additional entry (the stale copy below)
		// pushes the namespace to exactly two pages.
		for i in 0..IN_MEMORY_PAGE_SIZE {
			primary_store
				.write(
					NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
					NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
					&format!("filler-{i:02}"),
					vec![0u8; 32],
				)
				.await
				.unwrap();
		}

		// A stale copy of `network_graph`, written directly to primary as if it had
		// been persisted there before the ephemeral store was configured. Writing it
		// last gives it the highest creation order, guaranteeing it lands on the
		// *first* page rather than the last -- filtering only the terminal page
		// would miss it entirely.
		primary_store
			.write(
				NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
				NETWORK_GRAPH_PERSISTENCE_KEY,
				vec![1u8; 32],
			)
			.await
			.unwrap();

		// The live, authoritative copy, written through the tier so it is routed to
		// the ephemeral store.
		tier.write(
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_KEY,
			vec![2u8; 32],
		)
		.await
		.unwrap();

		let page_one = PaginatedKVStore::list_paginated(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();

		// The stale primary copy must be filtered out of the first page even though
		// the walk isn't done yet -- it should never be visible once an ephemeral
		// store is configured.
		let expected_page_one: Vec<String> =
			(1..IN_MEMORY_PAGE_SIZE).rev().map(|i| format!("filler-{i:02}")).collect();
		assert_eq!(page_one.keys, expected_page_one);
		assert!(page_one.next_page_token.is_some());

		let page_two = PaginatedKVStore::list_paginated(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			page_one.next_page_token,
		)
		.await
		.unwrap();

		// The final page carries the one remaining filler key plus the ephemeral
		// store's live copy, appended exactly once now that primary pagination is
		// exhausted.
		assert_eq!(
			page_two.keys,
			vec!["filler-00".to_string(), NETWORK_GRAPH_PERSISTENCE_KEY.to_string()]
		);
		assert!(page_two.next_page_token.is_none());

		// Across the full walk, `network_graph` appears exactly once -- this is the
		// property that would have caught the duplicate-key bug.
		let occurrences = page_one
			.keys
			.iter()
			.chain(page_two.keys.iter())
			.filter(|k| k.as_str() == NETWORK_GRAPH_PERSISTENCE_KEY)
			.count();
		assert_eq!(occurrences, 1);
	}

	#[tokio::test]
	async fn list_unrelated_namespace_survives_ephemeral_list_failure() {
		let base_dir = random_storage_path();
		let log_path = base_dir.join("tier_store_test.log").to_string_lossy().into_owned();
		let logger = Arc::new(Logger::new_fs_writer(log_path, Level::Trace).unwrap());

		let _cleanup = CleanupDir(base_dir.clone());

		let primary_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let mut tier = setup_tier_store(Arc::clone(&primary_store), logger);

		// An ephemeral store whose `list`/`list_paginated` always fail.
		let ephemeral_store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingListStore::new()));
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

		// The paginated path is gated identically.
		let monitor_page = PaginatedKVStore::list_paginated(
			&tier,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			None,
		)
		.await
		.unwrap();
		assert_eq!(monitor_page.keys, vec!["monitor-key".to_string()]);

		// Sanity check the fixture: listing a namespace that *can* hold an ephemeral-cached key
		// still surfaces the ephemeral failure rather than silently swallowing it.
		assert!(KVStore::list(
			&tier,
			NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
		)
		.await
		.is_err());
	}

	#[tokio::test]
	async fn list_filters_stale_primary_copy_when_ephemeral_missing() {
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

		// The durable key is still listed, and the stale primary copy of `network_graph` is
		// filtered out so `list` cannot report a key that `read` would fail to fetch -- matching
		// the paginated path.
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
