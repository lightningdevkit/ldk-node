// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to [`PostgresStore`] live here.
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lightning::io;
use lightning::util::persist::{
	KVStore, KVStoreSync, PageToken, PaginatedKVStore, PaginatedKVStoreSync, PaginatedListResponse,
};
use lightning_types::string::PrintableString;
use tokio_postgres::NoTls;

use crate::io::utils::check_namespace_key_validity;

mod migrations;

/// The default table in which we store all data.
pub const DEFAULT_KV_TABLE_NAME: &str = "ldk_data";

// The current schema version for the PostgreSQL store.
const SCHEMA_VERSION: u16 = 1;

// The number of entries returned per page in paginated list operations.
const PAGE_SIZE: usize = 50;

// The number of worker threads for the internal runtime used by sync operations.
const INTERNAL_RUNTIME_WORKERS: usize = 2;

/// A [`KVStoreSync`] implementation that writes to and reads from a [PostgreSQL] database.
///
/// [PostgreSQL]: https://www.postgresql.org
pub struct PostgresStore {
	inner: Arc<PostgresStoreInner>,

	// Version counter to ensure that writes are applied in the correct order. It is assumed that read and list
	// operations aren't sensitive to the order of execution.
	next_write_version: AtomicU64,

	// An internal runtime we use to avoid any deadlocks we could hit when waiting on async
	// operations to finish from a sync context.
	internal_runtime: Option<tokio::runtime::Runtime>,
}

// tokio::sync::Mutex (used for the DB client) contains UnsafeCell which opts out of
// RefUnwindSafe. std::sync::Mutex (used by SqliteStore) doesn't have this issue because
// it poisons on panic. This impl is needed for do_read_write_remove_list_persist which
// requires K: KVStoreSync + RefUnwindSafe.
impl std::panic::RefUnwindSafe for PostgresStore {}

impl PostgresStore {
	/// Constructs a new [`PostgresStore`].
	///
	/// Connects to the PostgreSQL database at the given `connection_string`.
	///
	/// The given `kv_table_name` will be used or default to [`DEFAULT_KV_TABLE_NAME`].
	pub fn new(connection_string: &str, kv_table_name: Option<String>) -> io::Result<Self> {
		let internal_runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.thread_name_fn(|| {
				static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
				let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
				format!("ldk-node-pg-runtime-{id}")
			})
			.worker_threads(INTERNAL_RUNTIME_WORKERS)
			.max_blocking_threads(INTERNAL_RUNTIME_WORKERS)
			.build()
			.unwrap();

		let connection_string = connection_string.to_string();
		let inner = tokio::task::block_in_place(|| {
			internal_runtime.block_on(async {
				PostgresStoreInner::new(&connection_string, kv_table_name).await
			})
		})?;

		let inner = Arc::new(inner);
		let next_write_version = AtomicU64::new(1);
		Ok(Self { inner, next_write_version, internal_runtime: Some(internal_runtime) })
	}

	fn build_locking_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		format!("{primary_namespace}#{secondary_namespace}#{key}")
	}

	fn get_new_version_and_lock_ref(
		&self, locking_key: String,
	) -> (Arc<tokio::sync::Mutex<u64>>, u64) {
		let version = self.next_write_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("PostgresStore version counter overflowed");
		}

		let inner_lock_ref = self.inner.get_inner_lock_ref(locking_key);

		(inner_lock_ref, version)
	}
}

impl Drop for PostgresStore {
	fn drop(&mut self) {
		let internal_runtime = self.internal_runtime.take();
		tokio::task::block_in_place(move || drop(internal_runtime));
	}
}

impl KVStore for PostgresStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		async move { inner.read_internal(&primary_namespace, &secondary_namespace, &key).await }
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		async move {
			inner
				.write_internal(
					inner_lock_ref,
					locking_key,
					version,
					&primary_namespace,
					&secondary_namespace,
					&key,
					buf,
				)
				.await
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		async move {
			inner
				.remove_internal(
					inner_lock_ref,
					locking_key,
					version,
					&primary_namespace,
					&secondary_namespace,
					&key,
				)
				.await
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		async move { inner.list_internal(&primary_namespace, &secondary_namespace).await }
	}
}

impl KVStoreSync for PostgresStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			io::Error::new(io::ErrorKind::Other, "Failed to access internal runtime")
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move {
			inner.read_internal(&primary_namespace, &secondary_namespace, &key).await
		};
		tokio::task::block_in_place(move || internal_runtime.block_on(fut))
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			io::Error::new(io::ErrorKind::Other, "Failed to access internal runtime")
		})?;
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move {
			inner
				.write_internal(
					inner_lock_ref,
					locking_key,
					version,
					&primary_namespace,
					&secondary_namespace,
					&key,
					buf,
				)
				.await
		};
		tokio::task::block_in_place(move || internal_runtime.block_on(fut))
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> io::Result<()> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			io::Error::new(io::ErrorKind::Other, "Failed to access internal runtime")
		})?;
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move {
			inner
				.remove_internal(
					inner_lock_ref,
					locking_key,
					version,
					&primary_namespace,
					&secondary_namespace,
					&key,
				)
				.await
		};
		tokio::task::block_in_place(move || internal_runtime.block_on(fut))
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			io::Error::new(io::ErrorKind::Other, "Failed to access internal runtime")
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut =
			async move { inner.list_internal(&primary_namespace, &secondary_namespace).await };
		tokio::task::block_in_place(move || internal_runtime.block_on(fut))
	}
}

impl PaginatedKVStoreSync for PostgresStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			io::Error::new(io::ErrorKind::Other, "Failed to access internal runtime")
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move {
			inner
				.list_paginated_internal(&primary_namespace, &secondary_namespace, page_token)
				.await
		};
		tokio::task::block_in_place(move || internal_runtime.block_on(fut))
	}
}

impl PaginatedKVStore for PostgresStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		async move {
			inner
				.list_paginated_internal(&primary_namespace, &secondary_namespace, page_token)
				.await
		}
	}
}

struct PostgresStoreInner {
	client: tokio::sync::Mutex<tokio_postgres::Client>,
	kv_table_name: String,
	write_version_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
	next_sort_order: AtomicI64,
}

impl PostgresStoreInner {
	async fn new(connection_string: &str, kv_table_name: Option<String>) -> io::Result<Self> {
		let kv_table_name = kv_table_name.unwrap_or(DEFAULT_KV_TABLE_NAME.to_string());

		let (client, connection) =
			tokio_postgres::connect(connection_string, NoTls).await.map_err(|e| {
				let msg = format!("Failed to connect to PostgreSQL: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

		// Spawn the connection task so it runs in the background.
		tokio::spawn(async move {
			if let Err(e) = connection.await {
				log::error!("PostgreSQL connection error: {e}");
			}
		});

		// Create the KV data table if it doesn't exist.
		let sql = format!(
			"CREATE TABLE IF NOT EXISTS {kv_table_name} (
			primary_namespace TEXT NOT NULL,
			secondary_namespace TEXT NOT NULL DEFAULT '',
			key TEXT NOT NULL CHECK (key <> ''),
			value BYTEA,
			sort_order BIGINT NOT NULL DEFAULT 0,
			PRIMARY KEY (primary_namespace, secondary_namespace, key)
			)"
		);
		client.execute(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to create table {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// Read the schema version from the table comment (analogous to SQLite's PRAGMA user_version).
		let sql = format!("SELECT obj_description('{kv_table_name}'::regclass, 'pg_class')");
		let row = client.query_one(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to read schema version for {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;
		let version_res: u16 = match row.get::<_, Option<&str>>(0) {
			Some(version_str) => version_str.parse().map_err(|_| {
				let msg = format!("Invalid schema version: {version_str}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?,
			None => 0,
		};

		if version_res == 0 {
			// New table, set our SCHEMA_VERSION.
			let sql = format!("COMMENT ON TABLE {kv_table_name} IS '{SCHEMA_VERSION}'");
			client.execute(sql.as_str(), &[]).await.map_err(|e| {
				let msg = format!("Failed to set schema version: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;
		} else if version_res < SCHEMA_VERSION {
			migrations::migrate_schema(&client, &kv_table_name, version_res, SCHEMA_VERSION)
				.await?;
		} else if version_res > SCHEMA_VERSION {
			let msg = format!(
				"Failed to open database: incompatible schema version {version_res}. Expected: {SCHEMA_VERSION}"
			);
			return Err(io::Error::new(io::ErrorKind::Other, msg));
		}

		// Create composite index for paginated listing.
		let sql = format!(
			"CREATE INDEX IF NOT EXISTS idx_{kv_table_name}_paginated ON {kv_table_name} (primary_namespace, secondary_namespace, sort_order DESC, key ASC)"
		);
		client.execute(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to create index on table {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// Initialize next_sort_order from the max existing value.
		let sql = format!("SELECT COALESCE(MAX(sort_order), 0) FROM {kv_table_name}");
		let row = client.query_one(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to read max sort_order from {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;
		let max_sort_order: i64 = row.get(0);
		let next_sort_order = AtomicI64::new(max_sort_order + 1);

		let client = tokio::sync::Mutex::new(client);
		let write_version_locks = Mutex::new(HashMap::new());
		Ok(Self { client, kv_table_name, write_version_locks, next_sort_order })
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<tokio::sync::Mutex<u64>> {
		let mut outer_lock = self.write_version_locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	async fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "read")?;

		let locked_client = self.client.lock().await;
		let sql = format!(
			"SELECT value FROM {} WHERE primary_namespace=$1 AND secondary_namespace=$2 AND key=$3",
			self.kv_table_name
		);

		let row = locked_client
			.query_opt(sql.as_str(), &[&primary_namespace, &secondary_namespace, &key])
			.await
			.map_err(|e| {
				let msg = format!(
					"Failed to read from key {}/{}/{}: {}",
					PrintableString(primary_namespace),
					PrintableString(secondary_namespace),
					PrintableString(key),
					e
				);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

		match row {
			Some(row) => {
				let value: Vec<u8> = row.get(0);
				Ok(value)
			},
			None => {
				let msg = format!(
					"Failed to read as key could not be found: {}/{}/{}",
					PrintableString(primary_namespace),
					PrintableString(secondary_namespace),
					PrintableString(key),
				);
				Err(io::Error::new(io::ErrorKind::NotFound, msg))
			},
		}
	}

	async fn write_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "write")?;

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let locked_client = self.client.lock().await;

			let sort_order = self.next_sort_order.fetch_add(1, Ordering::Relaxed);

			let sql = format!(
				"INSERT INTO {} (primary_namespace, secondary_namespace, key, value, sort_order) \
				 VALUES ($1, $2, $3, $4, $5) \
				 ON CONFLICT (primary_namespace, secondary_namespace, key) DO UPDATE SET value = EXCLUDED.value",
				self.kv_table_name
			);

			locked_client
				.execute(
					sql.as_str(),
					&[
						&primary_namespace,
						&secondary_namespace,
						&key,
						&buf,
						&sort_order,
					],
				)
				.await
				.map(|_| ())
				.map_err(|e| {
					let msg = format!(
						"Failed to write to key {}/{}/{}: {}",
						PrintableString(primary_namespace),
						PrintableString(secondary_namespace),
						PrintableString(key),
						e
					);
					io::Error::new(io::ErrorKind::Other, msg)
				})
		})
		.await
	}

	async fn remove_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "remove")?;

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let locked_client = self.client.lock().await;

			let sql = format!(
				"DELETE FROM {} WHERE primary_namespace=$1 AND secondary_namespace=$2 AND key=$3",
				self.kv_table_name
			);

			locked_client
				.execute(sql.as_str(), &[&primary_namespace, &secondary_namespace, &key])
				.await
				.map_err(|e| {
					let msg = format!(
						"Failed to delete key {}/{}/{}: {}",
						PrintableString(primary_namespace),
						PrintableString(secondary_namespace),
						PrintableString(key),
						e
					);
					io::Error::new(io::ErrorKind::Other, msg)
				})?;
			Ok(())
		})
		.await
	}

	async fn list_internal(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, None, "list")?;

		let locked_client = self.client.lock().await;

		let sql = format!(
			"SELECT key FROM {} WHERE primary_namespace=$1 AND secondary_namespace=$2",
			self.kv_table_name
		);

		let rows = locked_client
			.query(sql.as_str(), &[&primary_namespace, &secondary_namespace])
			.await
			.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

		let keys: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
		Ok(keys)
	}

	async fn list_paginated_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		check_namespace_key_validity(
			primary_namespace,
			secondary_namespace,
			None,
			"list_paginated",
		)?;

		let locked_client = self.client.lock().await;

		// Fetch one extra row beyond PAGE_SIZE to determine whether a next page exists.
		let fetch_limit = (PAGE_SIZE + 1) as i64;

		let mut entries: Vec<(String, i64)> = match page_token {
			Some(ref token) => {
				let token_sort_order: i64 = token.as_str().parse().map_err(|_| {
					let token_str = token.as_str();
					let msg = format!("Invalid page token: {token_str}");
					io::Error::new(io::ErrorKind::InvalidInput, msg)
				})?;
				let sql = format!(
					"SELECT key, sort_order FROM {} \
					 WHERE primary_namespace=$1 \
					 AND secondary_namespace=$2 \
					 AND sort_order < $3 \
					 ORDER BY sort_order DESC, key ASC \
					 LIMIT $4",
					self.kv_table_name
				);

				let rows = locked_client
					.query(
						sql.as_str(),
						&[
							&primary_namespace,
							&secondary_namespace,
							&token_sort_order,
							&fetch_limit,
						],
					)
					.await
					.map_err(|e| {
						let msg = format!("Failed to retrieve queried rows: {e}");
						io::Error::new(io::ErrorKind::Other, msg)
					})?;

				rows.iter().map(|row| (row.get(0), row.get(1))).collect()
			},
			None => {
				let sql = format!(
					"SELECT key, sort_order FROM {} \
					 WHERE primary_namespace=$1 \
					 AND secondary_namespace=$2 \
					 ORDER BY sort_order DESC, key ASC \
					 LIMIT $3",
					self.kv_table_name
				);

				let rows = locked_client
					.query(sql.as_str(), &[&primary_namespace, &secondary_namespace, &fetch_limit])
					.await
					.map_err(|e| {
						let msg = format!("Failed to retrieve queried rows: {e}");
						io::Error::new(io::ErrorKind::Other, msg)
					})?;

				rows.into_iter().map(|row| (row.get(0), row.get(1))).collect()
			},
		};

		let has_more = entries.len() > PAGE_SIZE;
		entries.truncate(PAGE_SIZE);

		let next_page_token = if has_more {
			let (_, last_sort_order) = *entries.last().expect("must be non-empty");
			Some(PageToken::new(last_sort_order.to_string()))
		} else {
			None
		};

		let keys = entries.into_iter().map(|(k, _)| k).collect();
		Ok(PaginatedListResponse { keys, next_page_token })
	}

	async fn execute_locked_write<F: Future<Output = Result<(), io::Error>>, FN: FnOnce() -> F>(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		callback: FN,
	) -> Result<(), io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().await;

			// Check if we already have a newer version written/removed. This is used in async contexts to realize eventual
			// consistency.
			let is_stale_version = version <= *last_written_version;

			// If the version is not stale, we execute the callback. Otherwise, we can and must skip writing.
			if is_stale_version {
				Ok(())
			} else {
				callback().await.map(|_| {
					*last_written_version = version;
				})
			}
		};

		self.clean_locks(&inner_lock_ref, locking_key);

		res
	}

	fn clean_locks(&self, inner_lock_ref: &Arc<tokio::sync::Mutex<u64>>, locking_key: String) {
		// If there are no arcs in use elsewhere, this means that there are no in-flight writes. We can remove the map
		// entry to prevent leaking memory. The two arcs that are expected are the one in the map and the one held here
		// in inner_lock_ref. The outer lock is obtained first, to avoid a new arc being cloned after we've already
		// counted.
		let mut outer_lock = self.write_version_locks.lock().unwrap();

		let strong_count = Arc::strong_count(inner_lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected PostgresStore strong count");

		if strong_count == 2 {
			outer_lock.remove(&locking_key);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::io::test_utils::{do_read_write_remove_list_persist, do_test_store};

	fn test_connection_string() -> String {
		std::env::var("TEST_POSTGRES_URL")
			.unwrap_or_else(|_| "host=localhost user=postgres password=postgres".to_string())
	}

	fn create_test_store(table_name: &str) -> PostgresStore {
		PostgresStore::new(&test_connection_string(), Some(table_name.to_string())).unwrap()
	}

	fn cleanup_store(store: &PostgresStore) {
		if let Some(ref runtime) = store.internal_runtime {
			let kv_table = store.inner.kv_table_name.clone();
			let inner = Arc::clone(&store.inner);
			let _ = tokio::task::block_in_place(|| {
				runtime.block_on(async {
					let client = inner.client.lock().await;
					let _ = client.execute(&format!("DROP TABLE IF EXISTS {kv_table}"), &[]).await;
				})
			});
		}
	}

	#[test]
	fn read_write_remove_list_persist() {
		let store = create_test_store("test_rwrl");
		do_read_write_remove_list_persist(&store);
		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store() {
		let store_0 = create_test_store("test_pg_store_0");
		let store_1 = create_test_store("test_pg_store_1");
		do_test_store(&store_0, &store_1);
		cleanup_store(&store_0);
		cleanup_store(&store_1);
	}

	#[test]
	fn test_postgres_store_paginated_listing() {
		let store = create_test_store("test_pg_paginated");

		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";
		let num_entries = 225;

		for i in 0..num_entries {
			let key = format!("key_{:04}", i);
			let data = vec![i as u8; 32];
			KVStoreSync::write(&store, primary_namespace, secondary_namespace, &key, data).unwrap();
		}

		// Paginate through all entries and collect them
		let mut all_keys = Vec::new();
		let mut page_token = None;
		let mut page_count = 0;

		loop {
			let response = PaginatedKVStoreSync::list_paginated(
				&store,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
			.unwrap();

			all_keys.extend(response.keys.clone());
			page_count += 1;

			match response.next_page_token {
				Some(token) => page_token = Some(token),
				None => break,
			}
		}

		// Verify we got exactly the right number of entries
		assert_eq!(all_keys.len(), num_entries);

		// Verify correct number of pages (225 entries at 50 per page = 5 pages)
		assert_eq!(page_count, 5);

		// Verify no duplicates
		let mut unique_keys = all_keys.clone();
		unique_keys.sort();
		unique_keys.dedup();
		assert_eq!(unique_keys.len(), num_entries);

		// Verify ordering: newest first (highest sort_order first).
		assert_eq!(all_keys[0], format!("key_{:04}", num_entries - 1));
		assert_eq!(all_keys[num_entries - 1], "key_0000");

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_paginated_update_preserves_order() {
		let store = create_test_store("test_pg_paginated_update");

		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";

		KVStoreSync::write(&store, primary_namespace, secondary_namespace, "first", vec![1u8; 8])
			.unwrap();
		KVStoreSync::write(&store, primary_namespace, secondary_namespace, "second", vec![2u8; 8])
			.unwrap();
		KVStoreSync::write(&store, primary_namespace, secondary_namespace, "third", vec![3u8; 8])
			.unwrap();

		// Update the first entry
		KVStoreSync::write(&store, primary_namespace, secondary_namespace, "first", vec![99u8; 8])
			.unwrap();

		// Paginated listing should still show "first" with its original creation order
		let response = PaginatedKVStoreSync::list_paginated(
			&store,
			primary_namespace,
			secondary_namespace,
			None,
		)
		.unwrap();

		// Newest first: third, second, first
		assert_eq!(response.keys, vec!["third", "second", "first"]);

		// Verify the updated value was persisted
		let data =
			KVStoreSync::read(&store, primary_namespace, secondary_namespace, "first").unwrap();
		assert_eq!(data, vec![99u8; 8]);

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_paginated_empty_namespace() {
		let store = create_test_store("test_pg_paginated_empty");

		// Paginating an empty or unknown namespace returns an empty result with no token.
		let response =
			PaginatedKVStoreSync::list_paginated(&store, "nonexistent", "ns", None).unwrap();
		assert!(response.keys.is_empty());
		assert!(response.next_page_token.is_none());

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_paginated_namespace_isolation() {
		let store = create_test_store("test_pg_paginated_isolation");

		KVStoreSync::write(&store, "ns_a", "sub", "key_1", vec![1u8; 8]).unwrap();
		KVStoreSync::write(&store, "ns_a", "sub", "key_2", vec![2u8; 8]).unwrap();
		KVStoreSync::write(&store, "ns_b", "sub", "key_3", vec![3u8; 8]).unwrap();
		KVStoreSync::write(&store, "ns_a", "other", "key_4", vec![4u8; 8]).unwrap();

		// ns_a/sub should only contain key_1 and key_2 (newest first).
		let response = PaginatedKVStoreSync::list_paginated(&store, "ns_a", "sub", None).unwrap();
		assert_eq!(response.keys, vec!["key_2", "key_1"]);
		assert!(response.next_page_token.is_none());

		// ns_b/sub should only contain key_3.
		let response = PaginatedKVStoreSync::list_paginated(&store, "ns_b", "sub", None).unwrap();
		assert_eq!(response.keys, vec!["key_3"]);

		// ns_a/other should only contain key_4.
		let response = PaginatedKVStoreSync::list_paginated(&store, "ns_a", "other", None).unwrap();
		assert_eq!(response.keys, vec!["key_4"]);

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_paginated_removal() {
		let store = create_test_store("test_pg_paginated_removal");

		let ns = "test_ns";
		let sub = "test_sub";

		KVStoreSync::write(&store, ns, sub, "a", vec![1u8; 8]).unwrap();
		KVStoreSync::write(&store, ns, sub, "b", vec![2u8; 8]).unwrap();
		KVStoreSync::write(&store, ns, sub, "c", vec![3u8; 8]).unwrap();

		KVStoreSync::remove(&store, ns, sub, "b", false).unwrap();

		let response = PaginatedKVStoreSync::list_paginated(&store, ns, sub, None).unwrap();
		assert_eq!(response.keys, vec!["c", "a"]);
		assert!(response.next_page_token.is_none());

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_paginated_exact_page_boundary() {
		let store = create_test_store("test_pg_paginated_boundary");

		let ns = "test_ns";
		let sub = "test_sub";

		// Write exactly PAGE_SIZE entries (50).
		for i in 0..PAGE_SIZE {
			let key = format!("key_{:04}", i);
			KVStoreSync::write(&store, ns, sub, &key, vec![i as u8; 8]).unwrap();
		}

		// Exactly PAGE_SIZE entries: all returned in one page with no next-page token.
		let response = PaginatedKVStoreSync::list_paginated(&store, ns, sub, None).unwrap();
		assert_eq!(response.keys.len(), PAGE_SIZE);
		assert!(response.next_page_token.is_none());

		// Add one more entry (PAGE_SIZE + 1 total). First page should now have a token.
		KVStoreSync::write(&store, ns, sub, "key_extra", vec![0u8; 8]).unwrap();
		let response = PaginatedKVStoreSync::list_paginated(&store, ns, sub, None).unwrap();
		assert_eq!(response.keys.len(), PAGE_SIZE);
		assert!(response.next_page_token.is_some());

		// Second page should have exactly 1 entry and no token.
		let response =
			PaginatedKVStoreSync::list_paginated(&store, ns, sub, response.next_page_token)
				.unwrap();
		assert_eq!(response.keys.len(), 1);
		assert!(response.next_page_token.is_none());

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_paginated_fewer_than_page_size() {
		let store = create_test_store("test_pg_paginated_few");

		let ns = "test_ns";
		let sub = "test_sub";

		// Write fewer entries than PAGE_SIZE.
		for i in 0..5 {
			let key = format!("key_{i}");
			KVStoreSync::write(&store, ns, sub, &key, vec![i as u8; 8]).unwrap();
		}

		let response = PaginatedKVStoreSync::list_paginated(&store, ns, sub, None).unwrap();
		assert_eq!(response.keys.len(), 5);
		// Fewer than PAGE_SIZE means no next page.
		assert!(response.next_page_token.is_none());
		// Newest first.
		assert_eq!(response.keys, vec!["key_4", "key_3", "key_2", "key_1", "key_0"]);

		cleanup_store(&store);
	}

	#[test]
	fn test_postgres_store_write_version_persists_across_restart() {
		let table_name = "test_pg_write_version_restart";
		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";

		{
			let store = create_test_store(table_name);

			KVStoreSync::write(
				&store,
				primary_namespace,
				secondary_namespace,
				"key_a",
				vec![1u8; 8],
			)
			.unwrap();
			KVStoreSync::write(
				&store,
				primary_namespace,
				secondary_namespace,
				"key_b",
				vec![2u8; 8],
			)
			.unwrap();

			// Don't clean up since we want to reopen
		}

		// Open a new store instance on the same database table and write more
		{
			let store = create_test_store(table_name);

			KVStoreSync::write(
				&store,
				primary_namespace,
				secondary_namespace,
				"key_c",
				vec![3u8; 8],
			)
			.unwrap();

			// Paginated listing should show newest first: key_c, key_b, key_a
			let response = PaginatedKVStoreSync::list_paginated(
				&store,
				primary_namespace,
				secondary_namespace,
				None,
			)
			.unwrap();

			assert_eq!(response.keys, vec!["key_c", "key_b", "key_a"]);

			cleanup_store(&store);
		}
	}
}
