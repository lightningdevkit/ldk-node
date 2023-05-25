use super::*;

use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;

use rusqlite::{named_params, Connection};

use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// The database file name.
const SQLITE_DB_FILE: &str = "ldk_node.sqlite";

// The table in which we store all data.
const KV_TABLE_NAME: &str = "ldk_node_data";

// The current SQLite `user_version`, which we can use if we'd ever need to do a schema migration.
const SCHEMA_USER_VERSION: u16 = 1;

/// A [`KVStore`] implementation that writes to and reads from an [SQLite] database.
///
/// [SQLite]: https://sqlite.org
pub struct SqliteStore {
	connection: Arc<Mutex<Connection>>,
}

impl SqliteStore {
	pub(crate) fn new(dest_dir: PathBuf) -> Self {
		fs::create_dir_all(dest_dir.clone()).unwrap_or_else(|_| {
			panic!("Failed to create database destination directory: {}", dest_dir.display())
		});
		let mut db_file_path = dest_dir.clone();
		db_file_path.push(SQLITE_DB_FILE);

		let connection = Connection::open(db_file_path.clone()).unwrap_or_else(|_| {
			panic!("Failed to open/create database file: {}", db_file_path.display())
		});

		connection
			.pragma(Some(rusqlite::DatabaseName::Main), "user_version", SCHEMA_USER_VERSION, |_| {
				Ok(())
			})
			.unwrap_or_else(|_| panic!("Failed to set PRAGMA user_version"));

		let sql = format!(
			"CREATE TABLE IF NOT EXISTS {} (
			namespace TEXT NOT NULL,
			key TEXT NOT NULL CHECK (key <> ''),
			value BLOB, PRIMARY KEY ( namespace, key )
		);",
			KV_TABLE_NAME
		);
		connection
			.execute(&sql, [])
			.unwrap_or_else(|_| panic!("Failed to create table: {}", KV_TABLE_NAME));

		let connection = Arc::new(Mutex::new(connection));
		Self { connection }
	}
}

impl KVStore for SqliteStore {
	type Reader = Cursor<Vec<u8>>;

	fn read(&self, namespace: &str, key: &str) -> std::io::Result<Self::Reader> {
		let locked_conn = self.connection.lock().unwrap();
		let sql =
			format!("SELECT value FROM {} WHERE namespace=:namespace AND key=:key;", KV_TABLE_NAME);

		let res = locked_conn
			.query_row(
				&sql,
				named_params! {
					":namespace": namespace,
					":key": key,
				},
				|row| row.get(0),
			)
			.map_err(|e| match e {
				rusqlite::Error::QueryReturnedNoRows => {
					let msg =
						format!("Failed to read as key could not be found: {}/{}", namespace, key);
					std::io::Error::new(std::io::ErrorKind::NotFound, msg)
				}
				e => {
					let msg = format!("Failed to read from key {}/{}: {}", namespace, key, e);
					std::io::Error::new(std::io::ErrorKind::Other, msg)
				}
			})?;
		Ok(Cursor::new(res))
	}

	fn write(&self, namespace: &str, key: &str, buf: &[u8]) -> std::io::Result<()> {
		let locked_conn = self.connection.lock().unwrap();

		let sql = format!(
			"INSERT OR REPLACE INTO {} (namespace, key, value) VALUES (:namespace, :key, :value);",
			KV_TABLE_NAME
		);

		locked_conn
			.execute(
				&sql,
				named_params! {
					":namespace": namespace,
					":key": key,
					":value": buf,
				},
			)
			.map(|_| ())
			.map_err(|e| {
				let msg = format!("Failed to write to key {}/{}: {}", namespace, key, e);
				std::io::Error::new(std::io::ErrorKind::Other, msg)
			})
	}

	fn remove(&self, namespace: &str, key: &str) -> std::io::Result<bool> {
		let locked_conn = self.connection.lock().unwrap();

		let sql = format!("DELETE FROM {} WHERE namespace=:namespace AND key=:key;", KV_TABLE_NAME);
		let changes = locked_conn
			.execute(
				&sql,
				named_params! {
					":namespace": namespace,
					":key": key,
				},
			)
			.map_err(|e| {
				let msg = format!("Failed to delete key {}/{}: {}", namespace, key, e);
				std::io::Error::new(std::io::ErrorKind::Other, msg)
			})?;

		let was_present = changes != 0;

		Ok(was_present)
	}

	fn list(&self, namespace: &str) -> std::io::Result<Vec<String>> {
		let locked_conn = self.connection.lock().unwrap();

		let sql = format!("SELECT key FROM {} WHERE namespace=:namespace", KV_TABLE_NAME);
		let mut stmt = locked_conn.prepare(&sql).map_err(|e| {
			let msg = format!("Failed to prepare statement: {}", e);
			std::io::Error::new(std::io::ErrorKind::Other, msg)
		})?;

		let mut keys = Vec::new();

		let rows_iter = stmt
			.query_map(named_params! {":namespace": namespace, }, |row| row.get(0))
			.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {}", e);
				std::io::Error::new(std::io::ErrorKind::Other, msg)
			})?;

		for k in rows_iter {
			keys.push(k.map_err(|e| {
				let msg = format!("Failed to retrieve queried rows: {}", e);
				std::io::Error::new(std::io::ErrorKind::Other, msg)
			})?);
		}

		Ok(keys)
	}
}

impl KVStorePersister for SqliteStore {
	fn persist<W: Writeable>(&self, prefixed_key: &str, object: &W) -> lightning::io::Result<()> {
		let (namespace, key) = get_namespace_and_key_from_prefixed(prefixed_key)?;
		self.write(&namespace, &key, &object.encode())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test::utils::random_storage_path;

	use proptest::prelude::*;
	proptest! {
		#[test]
		fn read_write_remove_list_persist(data in any::<[u8; 32]>()) {
			let rand_dir = random_storage_path();
			let sqlite_store = SqliteStore::new(rand_dir.into());

			do_read_write_remove_list_persist(&data, &sqlite_store);
		}
	}
}
