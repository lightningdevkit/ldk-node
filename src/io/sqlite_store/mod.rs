// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to [`SqliteStore`] live here.
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lightning::io;
use lightning::util::persist::{
	KVStore, KVStoreSync, PageToken, PaginatedKVStore, PaginatedKVStoreSync, PaginatedListResponse,
};
use lightning_types::string::PrintableString;
use rusqlite::{named_params, Connection};

use crate::io::utils::check_namespace_key_validity;

mod migrations;

/// LDK Node's database file name.
pub const SQLITE_DB_FILE_NAME: &str = "ldk_node_data.sqlite";
/// LDK Node's backup database file name.
pub const SQLITE_BACKUP_DB_FILE_NAME: &str = "ldk_node_data_backup.sqlite";
/// LDK Node's table in which we store all data.
pub const KV_TABLE_NAME: &str = "ldk_node_data";

/// The default database file name.
pub const DEFAULT_SQLITE_DB_FILE_NAME: &str = "ldk_data.sqlite";

/// The default table in which we store all data.
pub const DEFAULT_KV_TABLE_NAME: &str = "ldk_data";

// The current SQLite `user_version`, which we can use if we'd ever need to do a schema migration.
const SCHEMA_USER_VERSION: u16 = 3;

// The number of entries returned per page in paginated list operations.
const PAGE_SIZE: usize = 50;

/// A [`KVStoreSync`] implementation that writes to and reads from an [SQLite] database.
///
/// [SQLite]: https://sqlite.org
pub struct SqliteStore {
	inner: Arc<SqliteStoreInner>,

	// Version counter to ensure that writes are applied in the correct order. It is assumed that read and list
	// operations aren't sensitive to the order of execution.
	next_write_version: AtomicU64,
}

impl SqliteStore {
	/// Constructs a new [`SqliteStore`].
	///
	/// If not already existing, a new SQLite database will be created in the given `data_dir` under the
	/// given `db_file_name` (or the default to [`DEFAULT_SQLITE_DB_FILE_NAME`] if set to `None`).
	///
	/// Similarly, the given `kv_table_name` will be used or default to [`DEFAULT_KV_TABLE_NAME`].
	pub fn new(
		data_dir: PathBuf, db_file_name: Option<String>, kv_table_name: Option<String>,
	) -> io::Result<Self> {
		let inner = Arc::new(SqliteStoreInner::new(data_dir, db_file_name, kv_table_name)?);

		let next_write_version = AtomicU64::new(1);
		Ok(Self { inner, next_write_version })
	}

	fn build_locking_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		format!("{}#{}#{}", primary_namespace, secondary_namespace, key)
	}

	fn get_new_version_and_lock_ref(&self, locking_key: String) -> (Arc<Mutex<u64>>, u64) {
		let version = self.next_write_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("SqliteStore version counter overflowed");
		}

		// Get a reference to the inner lock. We do this early so that the arc can double as an in-flight counter for
		// cleaning up unused locks.
		let inner_lock_ref = self.inner.get_inner_lock_ref(locking_key);

		(inner_lock_ref, version)
	}

	/// Returns the data directory.
	pub fn get_data_dir(&self) -> PathBuf {
		self.inner.data_dir.clone()
	}
}

impl KVStore for SqliteStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.read_internal(&primary_namespace, &secondary_namespace, &key)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
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
		let fut = tokio::task::spawn_blocking(move || {
			inner.write_internal(
				inner_lock_ref,
				locking_key,
				version,
				&primary_namespace,
				&secondary_namespace,
				&key,
				buf,
			)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
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
		let fut = tokio::task::spawn_blocking(move || {
			inner.remove_internal(
				inner_lock_ref,
				locking_key,
				version,
				&primary_namespace,
				&secondary_namespace,
				&key,
			)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.list_internal(&primary_namespace, &secondary_namespace)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}
}

impl KVStoreSync for SqliteStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		self.inner.read_internal(primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		self.inner.write_internal(
			inner_lock_ref,
			locking_key,
			version,
			primary_namespace,
			secondary_namespace,
			key,
			buf,
		)
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> io::Result<()> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		self.inner.remove_internal(
			inner_lock_ref,
			locking_key,
			version,
			primary_namespace,
			secondary_namespace,
			key,
		)
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		self.inner.list_internal(primary_namespace, secondary_namespace)
	}
}

impl PaginatedKVStoreSync for SqliteStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		self.inner.list_paginated_internal(primary_namespace, secondary_namespace, page_token)
	}
}

impl PaginatedKVStore for SqliteStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.list_paginated_internal(&primary_namespace, &secondary_namespace, page_token)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}
}

struct SqliteStoreInner {
	connection: Arc<Mutex<Connection>>,
	data_dir: PathBuf,
	kv_table_name: String,
	write_version_locks: Mutex<HashMap<String, Arc<Mutex<u64>>>>,
	next_sort_order: AtomicI64,
}

impl SqliteStoreInner {
	fn new(
		data_dir: PathBuf, db_file_name: Option<String>, kv_table_name: Option<String>,
	) -> io::Result<Self> {
		let db_file_name = db_file_name.unwrap_or(DEFAULT_SQLITE_DB_FILE_NAME.to_string());
		let kv_table_name = kv_table_name.unwrap_or(DEFAULT_KV_TABLE_NAME.to_string());

		fs::create_dir_all(data_dir.clone()).map_err(|e| {
			let msg = format!(
				"Failed to create database destination directory {}: {}",
				data_dir.display(),
				e
			);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;
		let mut db_file_path = data_dir.clone();
		db_file_path.push(db_file_name);

		let mut connection = Connection::open(db_file_path.clone()).map_err(|e| {
			let msg =
				format!("Failed to open/create database file {}: {}", db_file_path.display(), e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let sql = format!("SELECT user_version FROM pragma_user_version");
		let version_res: u16 = connection.query_row(&sql, [], |row| row.get(0)).map_err(|e| {
			let msg = format!("Failed to read PRAGMA user_version: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		if version_res == 0 {
			// New database, set our SCHEMA_USER_VERSION and continue
			connection
				.pragma(
					Some(rusqlite::DatabaseName::Main),
					"user_version",
					SCHEMA_USER_VERSION,
					|_| Ok(()),
				)
				.map_err(|e| {
					let msg = format!("Failed to set PRAGMA user_version: {}", e);
					io::Error::new(io::ErrorKind::Other, msg)
				})?;
		} else if version_res < SCHEMA_USER_VERSION {
			migrations::migrate_schema(
				&mut connection,
				&kv_table_name,
				version_res,
				SCHEMA_USER_VERSION,
			)?;
		} else if version_res > SCHEMA_USER_VERSION {
			let msg = format!(
				"Failed to open database: incompatible schema version {}. Expected: {}",
				version_res, SCHEMA_USER_VERSION
			);
			return Err(io::Error::new(io::ErrorKind::Other, msg));
		}

		let sql = format!(
			"CREATE TABLE IF NOT EXISTS {} (
			primary_namespace TEXT NOT NULL,
			secondary_namespace TEXT DEFAULT \"\" NOT NULL,
			key TEXT NOT NULL CHECK (key <> ''),
			value BLOB,
			sort_order INTEGER NOT NULL DEFAULT 0,
			PRIMARY KEY ( primary_namespace, secondary_namespace, key )
			);",
			kv_table_name
		);

		connection.execute(&sql, []).map_err(|e| {
			let msg = format!("Failed to create table {}: {}", kv_table_name, e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// Create composite index for paginated listing (IF NOT EXISTS for idempotency)
		let sql = format!(
			"CREATE INDEX IF NOT EXISTS idx_{}_paginated ON {} (primary_namespace, secondary_namespace, sort_order DESC, key ASC)",
			kv_table_name, kv_table_name
		);

		connection.execute(&sql, []).map_err(|e| {
			let msg = format!("Failed to create index on table {}: {}", kv_table_name, e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let max_sort_order: i64 = connection
			.query_row(
				&format!("SELECT COALESCE(MAX(sort_order), 0) FROM {}", kv_table_name),
				[],
				|row| row.get(0),
			)
			.map_err(|e| {
				let msg = format!("Failed to read max sort_order from {}: {}", kv_table_name, e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;
		let next_sort_order = AtomicI64::new(max_sort_order + 1);

		let connection = Arc::new(Mutex::new(connection));
		let write_version_locks = Mutex::new(HashMap::new());
		Ok(Self { connection, data_dir, kv_table_name, write_version_locks, next_sort_order })
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<Mutex<u64>> {
		let mut outer_lock = self.write_version_locks.lock().expect("lock");
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "read")?;

		let locked_conn = self.connection.lock().expect("lock");
		let sql =
			format!("SELECT value FROM {} WHERE primary_namespace=:primary_namespace AND secondary_namespace=:secondary_namespace AND key=:key;",
			self.kv_table_name);

		let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
			let msg = format!("Failed to prepare statement: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let res = stmt
			.query_row(
				named_params! {
					":primary_namespace": primary_namespace,
					":secondary_namespace": secondary_namespace,
					":key": key,
				},
				|row| row.get(0),
			)
			.map_err(|e| match e {
				rusqlite::Error::QueryReturnedNoRows => {
					let msg = format!(
						"Failed to read as key could not be found: {}/{}/{}",
						PrintableString(primary_namespace),
						PrintableString(secondary_namespace),
						PrintableString(key)
					);
					io::Error::new(io::ErrorKind::NotFound, msg)
				},
				e => {
					let msg = format!(
						"Failed to read from key {}/{}/{}: {}",
						PrintableString(primary_namespace),
						PrintableString(secondary_namespace),
						PrintableString(key),
						e
					);
					io::Error::new(io::ErrorKind::Other, msg)
				},
			})?;
		Ok(res)
	}

	fn write_internal(
		&self, inner_lock_ref: Arc<Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "write")?;

		self.execute_locked_write(inner_lock_ref, locking_key, version, || {
			let locked_conn = self.connection.lock().expect("lock");

			let sort_order = self.next_sort_order.fetch_add(1, Ordering::Relaxed);

			let sql = format!(
				"INSERT INTO {} (primary_namespace, secondary_namespace, key, value, sort_order) \
				 VALUES (:primary_namespace, :secondary_namespace, :key, :value, :sort_order) \
				 ON CONFLICT(primary_namespace, secondary_namespace, key) DO UPDATE SET value = excluded.value;",
				self.kv_table_name
			);

			let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
				let msg = format!("Failed to prepare statement: {}", e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

			stmt.execute(named_params! {
				":primary_namespace": primary_namespace,
				":secondary_namespace": secondary_namespace,
				":key": key,
				":value": buf,
				":sort_order": sort_order,
			})
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
	}

	fn remove_internal(
		&self, inner_lock_ref: Arc<Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "remove")?;

		self.execute_locked_write(inner_lock_ref, locking_key, version, || {
			let locked_conn = self.connection.lock().expect("lock");

			let sql = format!("DELETE FROM {} WHERE primary_namespace=:primary_namespace AND secondary_namespace=:secondary_namespace AND key=:key;", self.kv_table_name);

			let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
				let msg = format!("Failed to prepare statement: {}", e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

			stmt.execute(named_params! {
				":primary_namespace": primary_namespace,
				":secondary_namespace": secondary_namespace,
				":key": key,
			})
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
	}

	fn list_internal(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, None, "list")?;

		let locked_conn = self.connection.lock().expect("lock");

		let sql = format!(
			"SELECT key FROM {} WHERE primary_namespace=:primary_namespace AND secondary_namespace=:secondary_namespace",
			self.kv_table_name
		);
		let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
			let msg = format!("Failed to prepare statement: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let mut keys = Vec::new();

		let rows_iter = stmt
			.query_map(
				named_params! {
						":primary_namespace": primary_namespace,
						":secondary_namespace": secondary_namespace,
				},
				|row| row.get(0),
			)
			.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {}", e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

		for k in rows_iter {
			keys.push(k.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {}", e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?);
		}

		Ok(keys)
	}

	fn list_paginated_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		check_namespace_key_validity(
			primary_namespace,
			secondary_namespace,
			None,
			"list_paginated",
		)?;

		let locked_conn = self.connection.lock().expect("lock");

		// Fetch one extra row beyond PAGE_SIZE to determine whether a next page exists.
		let fetch_limit = (PAGE_SIZE + 1) as i64;

		let mut entries: Vec<(String, i64)> = match page_token {
			Some(ref token) => {
				let token_sort_order: i64 = token.as_str().parse().map_err(|_| {
					let msg = format!("Invalid page token: {}", token.as_str());
					io::Error::new(io::ErrorKind::InvalidInput, msg)
				})?;
				let sql = format!(
					"SELECT key, sort_order FROM {} \
					 WHERE primary_namespace=:primary_namespace \
					 AND secondary_namespace=:secondary_namespace \
					 AND sort_order < :token_sort_order \
					 ORDER BY sort_order DESC, key ASC \
					 LIMIT :limit",
					self.kv_table_name
				);
				let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
					let msg = format!("Failed to prepare statement: {}", e);
					io::Error::new(io::ErrorKind::Other, msg)
				})?;

				let rows = stmt
					.query_map(
						named_params! {
							":primary_namespace": primary_namespace,
							":secondary_namespace": secondary_namespace,
							":token_sort_order": token_sort_order,
							":limit": fetch_limit,
						},
						|row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
					)
					.map_err(|e| {
						let msg = format!("Failed to retrieve queried rows: {}", e);
						io::Error::new(io::ErrorKind::Other, msg)
					})?;

				rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
					let msg = format!("Failed to retrieve queried rows: {}", e);
					io::Error::new(io::ErrorKind::Other, msg)
				})?
			},
			None => {
				let sql = format!(
					"SELECT key, sort_order FROM {} \
					 WHERE primary_namespace=:primary_namespace \
					 AND secondary_namespace=:secondary_namespace \
					 ORDER BY sort_order DESC, key ASC \
					 LIMIT :limit",
					self.kv_table_name
				);
				let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
					let msg = format!("Failed to prepare statement: {}", e);
					io::Error::new(io::ErrorKind::Other, msg)
				})?;

				let rows = stmt
					.query_map(
						named_params! {
							":primary_namespace": primary_namespace,
							":secondary_namespace": secondary_namespace,
							":limit": fetch_limit,
						},
						|row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
					)
					.map_err(|e| {
						let msg = format!("Failed to retrieve queried rows: {}", e);
						io::Error::new(io::ErrorKind::Other, msg)
					})?;

				rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
					let msg = format!("Failed to retrieve queried rows: {}", e);
					io::Error::new(io::ErrorKind::Other, msg)
				})?
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

	fn execute_locked_write<F: FnOnce() -> Result<(), lightning::io::Error>>(
		&self, inner_lock_ref: Arc<Mutex<u64>>, locking_key: String, version: u64, callback: F,
	) -> Result<(), lightning::io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().expect("lock");

			// Check if we already have a newer version written/removed. This is used in async contexts to realize eventual
			// consistency.
			let is_stale_version = version <= *last_written_version;

			// If the version is not stale, we execute the callback. Otherwise we can and must skip writing.
			if is_stale_version {
				Ok(())
			} else {
				callback().map(|_| {
					*last_written_version = version;
				})
			}
		};

		self.clean_locks(&inner_lock_ref, locking_key);

		res
	}

	fn clean_locks(&self, inner_lock_ref: &Arc<Mutex<u64>>, locking_key: String) {
		// If there no arcs in use elsewhere, this means that there are no in-flight writes. We can remove the map entry
		// to prevent leaking memory. The two arcs that are expected are the one in the map and the one held here in
		// inner_lock_ref. The outer lock is obtained first, to avoid a new arc being cloned after we've already
		// counted.
		let mut outer_lock = self.write_version_locks.lock().expect("lock");

		let strong_count = Arc::strong_count(&inner_lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected SqliteStore strong count");

		if strong_count == 2 {
			outer_lock.remove(&locking_key);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::io::test_utils::{
		do_read_write_remove_list_persist, do_test_store, random_storage_path,
	};

	impl Drop for SqliteStore {
		fn drop(&mut self) {
			match fs::remove_dir_all(&self.inner.data_dir) {
				Err(e) => println!("Failed to remove test store directory: {}", e),
				_ => {},
			}
		}
	}

	#[test]
	fn read_write_remove_list_persist() {
		let mut temp_path = random_storage_path();
		temp_path.push("read_write_remove_list_persist");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();
		do_read_write_remove_list_persist(&store);
	}

	#[test]
	fn test_sqlite_store() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store");
		let store_0 = SqliteStore::new(
			temp_path.clone(),
			Some("test_db_0".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();
		let store_1 = SqliteStore::new(
			temp_path,
			Some("test_db_1".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();
		do_test_store(&store_0, &store_1)
	}

	#[test]
	fn test_sqlite_store_paginated_listing() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_listing");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

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
		// Since we wrote key_0000 first and key_0249 last, key_0249 should appear first
		// in the paginated results.
		assert_eq!(all_keys[0], format!("key_{:04}", num_entries - 1));
		assert_eq!(all_keys[num_entries - 1], "key_0000");
	}

	#[test]
	fn test_sqlite_store_paginated_update_preserves_order() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_update");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

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
	}

	#[test]
	fn test_sqlite_store_paginated_empty_namespace() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_empty");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

		// Paginating an empty or unknown namespace returns an empty result with no token.
		let response =
			PaginatedKVStoreSync::list_paginated(&store, "nonexistent", "ns", None).unwrap();
		assert!(response.keys.is_empty());
		assert!(response.next_page_token.is_none());
	}

	#[test]
	fn test_sqlite_store_paginated_namespace_isolation() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_isolation");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

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
	}

	#[test]
	fn test_sqlite_store_paginated_removal() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_removal");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

		let ns = "test_ns";
		let sub = "test_sub";

		KVStoreSync::write(&store, ns, sub, "a", vec![1u8; 8]).unwrap();
		KVStoreSync::write(&store, ns, sub, "b", vec![2u8; 8]).unwrap();
		KVStoreSync::write(&store, ns, sub, "c", vec![3u8; 8]).unwrap();

		KVStoreSync::remove(&store, ns, sub, "b", false).unwrap();

		let response = PaginatedKVStoreSync::list_paginated(&store, ns, sub, None).unwrap();
		assert_eq!(response.keys, vec!["c", "a"]);
		assert!(response.next_page_token.is_none());
	}

	#[test]
	fn test_sqlite_store_paginated_exact_page_boundary() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_boundary");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

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
	}

	#[test]
	fn test_sqlite_store_paginated_fewer_than_page_size() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_paginated_few");
		let store = SqliteStore::new(
			temp_path,
			Some("test_db".to_string()),
			Some("test_table".to_string()),
		)
		.unwrap();

		let ns = "test_ns";
		let sub = "test_sub";

		// Write fewer entries than PAGE_SIZE.
		for i in 0..5 {
			let key = format!("key_{}", i);
			KVStoreSync::write(&store, ns, sub, &key, vec![i as u8; 8]).unwrap();
		}

		let response = PaginatedKVStoreSync::list_paginated(&store, ns, sub, None).unwrap();
		assert_eq!(response.keys.len(), 5);
		// Fewer than PAGE_SIZE means no next page.
		assert!(response.next_page_token.is_none());
		// Newest first.
		assert_eq!(response.keys, vec!["key_4", "key_3", "key_2", "key_1", "key_0"]);
	}

	#[test]
	fn test_sqlite_store_write_version_persists_across_restart() {
		let mut temp_path = random_storage_path();
		temp_path.push("test_sqlite_store_write_version_restart");

		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";

		{
			let store = SqliteStore::new(
				temp_path.clone(),
				Some("test_db".to_string()),
				Some("test_table".to_string()),
			)
			.unwrap();

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

			// Don't drop/cleanup since we want to reopen
			std::mem::forget(store);
		}

		// Open a new store instance on the same database and write more
		{
			let store = SqliteStore::new(
				temp_path,
				Some("test_db".to_string()),
				Some("test_table".to_string()),
			)
			.unwrap();

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
		}
	}
}

#[cfg(ldk_bench)]
/// Benches
pub mod bench {
	use criterion::Criterion;

	/// Bench!
	pub fn bench_sends(bench: &mut Criterion) {
		let store_a = super::SqliteStore::new("bench_sqlite_store_a".into(), None, None).unwrap();
		let store_b = super::SqliteStore::new("bench_sqlite_store_b".into(), None, None).unwrap();
		lightning::ln::channelmanager::bench::bench_two_sends(
			bench,
			"bench_sqlite_persisted_sends",
			store_a,
			store_b,
		);
	}
}
