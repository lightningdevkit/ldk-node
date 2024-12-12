use crate::io::paginated_kv_store::{ListResponse, PaginatedKVStore};
use crate::io::utils::check_namespace_key_validity;
use ldk_node::lightning::types::string::PrintableString;
use rusqlite::{named_params, Connection};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{fs, io};

/// The default database file name.
pub const DEFAULT_SQLITE_DB_FILE_NAME: &str = "ldk_server_data.sqlite";

/// The default table in which we store all the paginated data.
pub const DEFAULT_PAGINATED_KV_TABLE_NAME: &str = "ldk_paginated_data";

// The current SQLite `user_version`, which we can use if we'd ever need to do a schema migration.
const SCHEMA_USER_VERSION: u16 = 1;

// The maximum number of keys retrieved per page in paginated list operation.
const LIST_KEYS_MAX_PAGE_SIZE: i32 = 100;

pub struct SqliteStore {
	connection: Arc<Mutex<Connection>>,
	data_dir: PathBuf,
	paginated_kv_table_name: String,
}

impl SqliteStore {
	/// Constructs a new [`SqliteStore`].
	///
	/// If not already existing, a new SQLite database will be created in the given `data_dir` under the
	/// given `db_file_name` (or the default to [`DEFAULT_SQLITE_DB_FILE_NAME`] if set to `None`).
	///
	/// Similarly, the given `paginated_kv_table_name` will be used or default to [`DEFAULT_PAGINATED_KV_TABLE_NAME`].
	pub fn new(
		data_dir: PathBuf, db_file_name: Option<String>, paginated_kv_table_name: Option<String>,
	) -> io::Result<Self> {
		let db_file_name = db_file_name.unwrap_or(DEFAULT_SQLITE_DB_FILE_NAME.to_string());
		let paginated_kv_table_name =
			paginated_kv_table_name.unwrap_or(DEFAULT_PAGINATED_KV_TABLE_NAME.to_string());

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

		let connection = Connection::open(db_file_path.clone()).map_err(|e| {
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
		} else if version_res > SCHEMA_USER_VERSION {
			let msg = format!(
				"Failed to open database: incompatible schema version {}. Expected: {}",
				version_res, SCHEMA_USER_VERSION
			);
			return Err(io::Error::new(io::ErrorKind::Other, msg));
		}

		let create_paginated_kv_table_sql = format!(
			"CREATE TABLE IF NOT EXISTS {} (
			primary_namespace TEXT NOT NULL,
			secondary_namespace TEXT DEFAULT \"\" NOT NULL,
			key TEXT NOT NULL CHECK (key <> ''),
			creation_time INTEGER NOT NULL,
			value BLOB, PRIMARY KEY ( primary_namespace, secondary_namespace, key )
			);",
			paginated_kv_table_name
		);

		connection.execute(&create_paginated_kv_table_sql, []).map_err(|e| {
			let msg = format!("Failed to create table {}: {}", paginated_kv_table_name, e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let index_creation_time_sql = format!(
			"CREATE INDEX IF NOT EXISTS idx_creation_time ON {} (creation_time);",
			paginated_kv_table_name
		);

		connection.execute(&index_creation_time_sql, []).map_err(|e| {
			let msg = format!(
				"Failed to create index on creation_time, table {}: {}",
				paginated_kv_table_name, e
			);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let connection = Arc::new(Mutex::new(connection));
		Ok(Self { connection, data_dir, paginated_kv_table_name })
	}

	/// Returns the data directory.
	pub fn get_data_dir(&self) -> PathBuf {
		self.data_dir.clone()
	}

	fn read_internal(
		&self, kv_table_name: &str, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "read")?;

		let locked_conn = self.connection.lock().unwrap();
		let sql =
			format!("SELECT value FROM {} WHERE primary_namespace=:primary_namespace AND secondary_namespace=:secondary_namespace AND key=:key;",
							kv_table_name);

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

	fn remove_internal(
		&self, kv_table_name: &str, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "remove")?;

		let locked_conn = self.connection.lock().unwrap();

		let sql = format!("DELETE FROM {} WHERE primary_namespace=:primary_namespace AND secondary_namespace=:secondary_namespace AND key=:key;", kv_table_name);

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
	}
}

impl PaginatedKVStore for SqliteStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		self.read_internal(
			&self.paginated_kv_table_name,
			primary_namespace,
			secondary_namespace,
			key,
		)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, time: i64, buf: &[u8],
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "write")?;

		let locked_conn = self.connection.lock().unwrap();

		let sql = format!(
			"INSERT INTO {} (primary_namespace, secondary_namespace, key, creation_time, value)
         VALUES (:primary_namespace, :secondary_namespace, :key, :creation_time, :value)
         ON CONFLICT(primary_namespace, secondary_namespace, key)
         DO UPDATE SET value = excluded.value;",
			self.paginated_kv_table_name
		);

		let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
			let msg = format!("Failed to prepare statement: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		stmt.execute(named_params! {
			":primary_namespace": primary_namespace,
			":secondary_namespace": secondary_namespace,
			":key": key,
			":creation_time": time,
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
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> io::Result<()> {
		self.remove_internal(
			&self.paginated_kv_table_name,
			primary_namespace,
			secondary_namespace,
			key,
		)
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
		page_token: Option<(String, i64)>,
	) -> io::Result<ListResponse> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, None, "list")?;

		let locked_conn = self.connection.lock().unwrap();

		let sql = format!(
			"SELECT key, creation_time FROM {} WHERE primary_namespace=:primary_namespace AND secondary_namespace=:secondary_namespace \
			AND ( creation_time < :creation_time_token OR (creation_time = :creation_time_token AND key > :key_token) ) \
			ORDER BY creation_time DESC, key ASC LIMIT :page_size",
			self.paginated_kv_table_name
		);

		let mut stmt = locked_conn.prepare_cached(&sql).map_err(|e| {
			let msg = format!("Failed to prepare statement: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		let mut keys: Vec<String> = Vec::new();
		let page_token = page_token.unwrap_or(("".to_string(), i64::MAX));

		let rows_iter = stmt
			.query_map(
				named_params! {
						":primary_namespace": primary_namespace,
						":secondary_namespace": secondary_namespace,
						":key_token": page_token.0,
						":creation_time_token": page_token.1,
						":page_size": LIST_KEYS_MAX_PAGE_SIZE,
				},
				|row| {
					let key: String = row.get(0)?;
					let creation_time: i64 = row.get(1)?;
					Ok((key, creation_time))
				},
			)
			.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {}", e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

		let mut last_creation_time: Option<i64> = None;
		for r in rows_iter {
			let (k, ct) = r.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {}", e);
				io::Error::new(io::ErrorKind::Other, msg)
			})?;
			keys.push(k);
			last_creation_time = Some(ct);
		}

		let last_key = keys.last().cloned();
		let next_page_token = if let (Some(k), Some(ct)) = (last_key, last_creation_time) {
			Some((k, ct))
		} else {
			None
		};

		Ok(ListResponse { keys, next_page_token })
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ldk_node::lightning::util::persist::KVSTORE_NAMESPACE_KEY_MAX_LEN;
	use rand::distributions::Alphanumeric;
	use rand::{thread_rng, Rng};
	use std::panic::RefUnwindSafe;

	impl Drop for SqliteStore {
		fn drop(&mut self) {
			match fs::remove_dir_all(&self.data_dir) {
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

	pub(crate) fn random_storage_path() -> PathBuf {
		let mut temp_path = std::env::temp_dir();
		let mut rng = thread_rng();
		let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		temp_path.push(rand_dir);
		temp_path
	}

	pub(crate) fn do_read_write_remove_list_persist<K: PaginatedKVStore + RefUnwindSafe>(
		kv_store: &K,
	) {
		let data = [42u8; 32];

		let primary_namespace = "testspace";
		let secondary_namespace = "testsubspace";
		let testkey = "testkey_0";

		let list_all_keys = |primary_namespace: &str, secondary_namespace: &str| -> Vec<String> {
			let mut all_keys = Vec::new();
			let mut page_token = None;
			loop {
				let list_response =
					kv_store.list(primary_namespace, secondary_namespace, page_token).unwrap();
				assert!(list_response.keys.len() <= LIST_KEYS_MAX_PAGE_SIZE as usize);
				all_keys.extend(list_response.keys);
				if list_response.next_page_token.is_none() {
					break;
				}
				page_token = list_response.next_page_token;
			}
			all_keys
		};

		// Test the basic KVStore operations.
		for i in 0..110 {
			kv_store
				.write(primary_namespace, secondary_namespace, &format!("testkey_{}", i), 0, &data)
				.unwrap();
		}

		// Test empty primary/secondary namespaces are allowed, but not empty primary namespace and non-empty
		// secondary primary_namespace, and not empty key.
		kv_store.write("", "", testkey, 0, &data).unwrap();
		let res =
			std::panic::catch_unwind(|| kv_store.write("", secondary_namespace, testkey, 0, &data));
		assert!(res.is_err());
		let res = std::panic::catch_unwind(|| {
			kv_store.write(primary_namespace, secondary_namespace, "", 0, &data)
		});
		assert!(res.is_err());

		let listed_keys = list_all_keys(primary_namespace, secondary_namespace);
		assert_eq!(listed_keys.len(), 110);
		assert_eq!(listed_keys[0], testkey);

		let read_data = kv_store.read(primary_namespace, secondary_namespace, testkey).unwrap();
		assert_eq!(data, &*read_data);

		kv_store.remove(primary_namespace, secondary_namespace, testkey, false).unwrap();

		let listed_keys = list_all_keys(primary_namespace, secondary_namespace);
		assert_eq!(listed_keys.len(), 109);

		// Ensure we have no issue operating with primary_namespace/secondary_namespace/key being KVSTORE_NAMESPACE_KEY_MAX_LEN
		let max_chars: String =
			std::iter::repeat('A').take(KVSTORE_NAMESPACE_KEY_MAX_LEN).collect();
		kv_store.write(&max_chars, &max_chars, &max_chars, 0, &data).unwrap();

		println!("{:?}", listed_keys);

		let listed_keys = list_all_keys(&max_chars, &max_chars);
		assert_eq!(listed_keys.len(), 1);
		assert_eq!(listed_keys[0], max_chars);

		let read_data = kv_store.read(&max_chars, &max_chars, &max_chars).unwrap();
		assert_eq!(data, &*read_data);

		kv_store.remove(&max_chars, &max_chars, &max_chars, false).unwrap();

		let listed_keys = list_all_keys(&max_chars, &max_chars);
		assert_eq!(listed_keys.len(), 0);
	}
}
