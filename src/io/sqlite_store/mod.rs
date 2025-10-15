// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to [`SqliteStore`] live here.
use std::boxed::Box;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lightning::io;
use lightning::util::persist::{KVStore, KVStoreSync};
use lightning_types::string::PrintableString;
use rusqlite::{named_params, Connection};

use crate::io::utils::check_namespace_key_validity;

mod migrations;

/// LDK Node's database file name.
pub const SQLITE_DB_FILE_NAME: &str = "ldk_node_data.sqlite";
/// LDK Node's table in which we store all data.
pub const KV_TABLE_NAME: &str = "ldk_node_data";

/// The default database file name.
pub const DEFAULT_SQLITE_DB_FILE_NAME: &str = "ldk_data.sqlite";

/// The default table in which we store all data.
pub const DEFAULT_KV_TABLE_NAME: &str = "ldk_data";

// The current SQLite `user_version`, which we can use if we'd ever need to do a schema migration.
const SCHEMA_USER_VERSION: u16 = 2;

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
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + Send>> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.read_internal(&primary_namespace, &secondary_namespace, &key)
		});
		Box::pin(async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		})
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> {
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
		Box::pin(async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		})
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> {
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
		Box::pin(async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		})
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + Send>> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.list_internal(&primary_namespace, &secondary_namespace)
		});
		Box::pin(async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		})
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
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
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

struct SqliteStoreInner {
	connection: Arc<Mutex<Connection>>,
	data_dir: PathBuf,
	kv_table_name: String,
	write_version_locks: Mutex<HashMap<String, Arc<Mutex<u64>>>>,
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
		let version_res: u16 = connection.query_row(&sql, [], |row| row.get(0)).unwrap();

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
			value BLOB, PRIMARY KEY ( primary_namespace, secondary_namespace, key )
			);",
			kv_table_name
		);

		connection.execute(&sql, []).map_err(|e| {
			let msg = format!("Failed to create table {}: {}", kv_table_name, e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let connection = Arc::new(Mutex::new(connection));
		let write_version_locks = Mutex::new(HashMap::new());
		Ok(Self { connection, data_dir, kv_table_name, write_version_locks })
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<Mutex<u64>> {
		let mut outer_lock = self.write_version_locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "read")?;

		let locked_conn = self.connection.lock().unwrap();
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
			let locked_conn = self.connection.lock().unwrap();

			let sql = format!(
				"INSERT OR REPLACE INTO {} (primary_namespace, secondary_namespace, key, value) VALUES (:primary_namespace, :secondary_namespace, :key, :value);",
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
			let locked_conn = self.connection.lock().unwrap();

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

		let locked_conn = self.connection.lock().unwrap();

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

	fn execute_locked_write<F: FnOnce() -> Result<(), lightning::io::Error>>(
		&self, inner_lock_ref: Arc<Mutex<u64>>, locking_key: String, version: u64, callback: F,
	) -> Result<(), lightning::io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().unwrap();

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
		let mut outer_lock = self.write_version_locks.lock().unwrap();

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
