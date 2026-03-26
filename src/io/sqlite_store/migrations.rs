// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::io;
use rusqlite::Connection;

pub(super) fn migrate_schema(
	connection: &mut Connection, kv_table_name: &str, mut from_version: u16, to_version: u16,
) -> io::Result<()> {
	assert!(from_version < to_version);
	if from_version == 1 && to_version >= 2 {
		migrate_v1_to_v2(connection, kv_table_name)?;
		from_version = 2;
	}
	if from_version == 2 && to_version >= 3 {
		migrate_v2_to_v3(connection, kv_table_name)?;
	}

	Ok(())
}

fn migrate_v1_to_v2(connection: &mut Connection, kv_table_name: &str) -> io::Result<()> {
	let tx = connection.transaction().map_err(|e| {
		let msg = format!("Failed to migrate table {} from v1 to v2: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	})?;

	// Rename 'namespace' column to 'primary_namespace'
	let sql = format!(
		"ALTER TABLE {}
			RENAME COLUMN namespace TO primary_namespace;",
		kv_table_name
	);

	tx.execute(&sql, []).map_err(|e| {
		let msg = format!("Failed to migrate table {} from v1 to v2: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	})?;

	// Add new 'secondary_namespace' column
	let sql = format!(
		"ALTER TABLE {}
			ADD secondary_namespace TEXT DEFAULT \"\" NOT NULL;",
		kv_table_name
	);

	tx.execute(&sql, []).map_err(|e| {
		let msg = format!("Failed to migrate table {} from v1 to v2: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	})?;

	// Update user_version
	tx.pragma(Some(rusqlite::DatabaseName::Main), "user_version", 2u16, |_| Ok(())).map_err(
		|e| {
			let msg = format!("Failed to upgrade user_version from 1 to 2: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		},
	)?;

	tx.commit().map_err(|e| {
		let msg = format!("Failed to migrate table {} from v1 to v2: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	})?;

	Ok(())
}

fn migrate_v2_to_v3(connection: &mut Connection, kv_table_name: &str) -> io::Result<()> {
	let map_err = |e: rusqlite::Error| -> io::Error {
		let msg = format!("Failed to migrate table {} from v2 to v3: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	};

	// Check whether the primary key already includes secondary_namespace.
	// Tables migrated from v1 have PK (primary_namespace, key) only — missing
	// secondary_namespace. Tables created directly as v2 already have the correct
	// PK (primary_namespace, secondary_namespace, key).
	let needs_table_rewrite = {
		let sql = format!("PRAGMA table_info({})", kv_table_name);
		let mut stmt = connection.prepare(&sql).map_err(map_err)?;
		let mut pk_cols: Vec<(i64, String)> = stmt
			.query_map([], |row| Ok((row.get::<_, i64>(5)?, row.get::<_, String>(1)?)))
			.map_err(map_err)?
			.collect::<Result<Vec<_>, _>>()
			.map_err(map_err)?
			.into_iter()
			.filter(|(pk, _)| *pk > 0)
			.collect();
		pk_cols.sort_by_key(|(pk, _)| *pk);
		let pk_names: Vec<&str> = pk_cols.iter().map(|(_, name)| name.as_str()).collect();
		pk_names != vec!["primary_namespace", "secondary_namespace", "key"]
	};

	let tx = connection.transaction().map_err(|e| {
		let msg = format!("Failed to migrate table {} from v2 to v3: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	})?;

	if needs_table_rewrite {
		// Full table rewrite to fix the primary key.
		let old_table = format!("{}_v2_old", kv_table_name);

		let rename_sql = format!("ALTER TABLE {} RENAME TO {}", kv_table_name, old_table);
		tx.execute(&rename_sql, []).map_err(map_err)?;

		let create_table_sql = format!(
			"CREATE TABLE {} (
				primary_namespace TEXT NOT NULL,
				secondary_namespace TEXT DEFAULT \"\" NOT NULL,
				key TEXT NOT NULL CHECK (key <> ''),
				value BLOB,
				sort_order INTEGER NOT NULL DEFAULT 0,
				PRIMARY KEY (primary_namespace, secondary_namespace, key)
				)",
			kv_table_name
		);
		tx.execute(&create_table_sql, []).map_err(map_err)?;

		// Copy data and backfill sort_order from ROWID for relative ordering
		let copy_sql = format!(
			"INSERT INTO {} (primary_namespace, secondary_namespace, key, value, sort_order)
				SELECT primary_namespace, secondary_namespace, key, value, ROWID FROM {}",
			kv_table_name, old_table
		);
		tx.execute(&copy_sql, []).map_err(map_err)?;

		let drop_old_sql = format!("DROP TABLE {}", old_table);
		tx.execute(&drop_old_sql, []).map_err(map_err)?;
	} else {
		// Primary key is already correct — just add the sort_order column and backfill.
		let add_col_sql = format!(
			"ALTER TABLE {} ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0",
			kv_table_name
		);
		tx.execute(&add_col_sql, []).map_err(map_err)?;

		let backfill_sql = format!("UPDATE {} SET sort_order = ROWID", kv_table_name);
		tx.execute(&backfill_sql, []).map_err(map_err)?;
	}

	// Create composite index for paginated listing
	let sql = format!(
		"CREATE INDEX idx_{}_paginated ON {} (primary_namespace, secondary_namespace, sort_order DESC, key ASC)",
		kv_table_name, kv_table_name
	);
	tx.execute(&sql, []).map_err(map_err)?;

	// Update user_version
	tx.pragma(Some(rusqlite::DatabaseName::Main), "user_version", 3u16, |_| Ok(())).map_err(
		|e| {
			let msg = format!("Failed to upgrade user_version from 2 to 3: {}", e);
			io::Error::new(io::ErrorKind::Other, msg)
		},
	)?;

	tx.commit().map_err(|e| {
		let msg = format!("Failed to migrate table {} from v2 to v3: {}", kv_table_name, e);
		io::Error::new(io::ErrorKind::Other, msg)
	})?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::fs;

	use lightning::util::persist::{KVStoreSync, PaginatedKVStoreSync};
	use rusqlite::{named_params, Connection};

	use crate::io::sqlite_store::SqliteStore;
	use crate::io::test_utils::{do_read_write_remove_list_persist, random_storage_path};

	#[test]
	fn rwrl_post_schema_1_migration() {
		let old_schema_version = 1;

		let mut temp_path = random_storage_path();
		temp_path.push("rwrl_post_schema_1_migration");

		let db_file_name = "test_db".to_string();
		let kv_table_name = "test_table".to_string();

		let test_namespace = "testspace".to_string();
		let test_key = "testkey".to_string();
		let test_data = [42u8; 32];

		{
			// We create a database with a SCHEMA_VERSION 1 table
			fs::create_dir_all(temp_path.clone()).unwrap();
			let mut db_file_path = temp_path.clone();
			db_file_path.push(db_file_name.clone());

			let connection = Connection::open(db_file_path.clone()).unwrap();

			connection
				.pragma(
					Some(rusqlite::DatabaseName::Main),
					"user_version",
					old_schema_version,
					|_| Ok(()),
				)
				.unwrap();

			let sql = format!(
				"CREATE TABLE IF NOT EXISTS {} (
					namespace TEXT NOT NULL,
					key TEXT NOT NULL CHECK (key <> ''),
					value BLOB, PRIMARY KEY ( namespace, key )
					);",
				kv_table_name
			);

			connection.execute(&sql, []).unwrap();

			// We write some data to the table
			let sql = format!(
				"INSERT OR REPLACE INTO {} (namespace, key, value) VALUES (:namespace, :key, :value);",
				kv_table_name
			);
			let mut stmt = connection.prepare_cached(&sql).unwrap();

			stmt.execute(named_params! {
				":namespace": test_namespace,
				":key": test_key,
				":value": test_data,
			})
			.unwrap();

			// We read the just written data back to assert it happened.
			let sql = format!(
				"SELECT value FROM {} WHERE namespace=:namespace AND key=:key;",
				kv_table_name
			);
			let mut stmt = connection.prepare_cached(&sql).unwrap();

			let res: Vec<u8> = stmt
				.query_row(
					named_params! {
						":namespace": test_namespace,
						":key": test_key,
					},
					|row| row.get(0),
				)
				.unwrap();

			assert_eq!(res, test_data);
		}

		// Check we migrate the db just fine without losing our written data.
		let store = SqliteStore::new(temp_path, Some(db_file_name), Some(kv_table_name)).unwrap();
		let res = store.read(&test_namespace, "", &test_key).unwrap();
		assert_eq!(res, test_data);

		// Check we can continue to use the store just fine.
		do_read_write_remove_list_persist(&store);
	}

	#[test]
	fn rwrl_post_schema_2_migration() {
		let old_schema_version = 2u16;

		let mut temp_path = random_storage_path();
		temp_path.push("rwrl_post_schema_2_migration");

		let db_file_name = "test_db".to_string();
		let kv_table_name = "test_table".to_string();

		let test_ns = "testspace";
		let test_sub = "testsub";

		{
			// Create a v2 database manually
			fs::create_dir_all(temp_path.clone()).unwrap();
			let mut db_file_path = temp_path.clone();
			db_file_path.push(db_file_name.clone());

			let connection = Connection::open(db_file_path.clone()).unwrap();

			connection
				.pragma(
					Some(rusqlite::DatabaseName::Main),
					"user_version",
					old_schema_version,
					|_| Ok(()),
				)
				.unwrap();

			let sql = format!(
				"CREATE TABLE IF NOT EXISTS {} (
					primary_namespace TEXT NOT NULL,
					secondary_namespace TEXT DEFAULT \"\" NOT NULL,
					key TEXT NOT NULL CHECK (key <> ''),
					value BLOB, PRIMARY KEY ( primary_namespace, secondary_namespace, key )
					);",
				kv_table_name
			);
			connection.execute(&sql, []).unwrap();

			// Insert 3 rows in a known order
			for i in 0..3 {
				let key = format!("key_{}", i);
				let sql = format!(
					"INSERT INTO {} (primary_namespace, secondary_namespace, key, value) VALUES (:ns, :sub, :key, :value);",
					kv_table_name
				);
				let mut stmt = connection.prepare_cached(&sql).unwrap();
				stmt.execute(named_params! {
					":ns": test_ns,
					":sub": test_sub,
					":key": key,
					":value": vec![i as u8; 8],
				})
				.unwrap();
			}
		}

		// Open with new code, triggering v2→v3 migration
		let store = SqliteStore::new(temp_path, Some(db_file_name), Some(kv_table_name)).unwrap();

		// Verify data survived
		for i in 0..3 {
			let key = format!("key_{}", i);
			let data = store.read(test_ns, test_sub, &key).unwrap();
			assert_eq!(data, vec![i as u8; 8]);
		}

		// Verify paginated listing works and returns entries in ROWID-backfilled order (newest first)
		let response =
			PaginatedKVStoreSync::list_paginated(&store, test_ns, test_sub, None).unwrap();
		assert_eq!(response.keys.len(), 3);
		// ROWIDs were 1, 2, 3 so sort_order was backfilled as 1, 2, 3; newest first
		assert_eq!(response.keys, vec!["key_2", "key_1", "key_0"]);

		// Verify we can write new entries and they get proper ordering
		KVStoreSync::write(&store, test_ns, test_sub, "key_new", vec![99u8; 8]).unwrap();
		let response =
			PaginatedKVStoreSync::list_paginated(&store, test_ns, test_sub, None).unwrap();
		assert_eq!(response.keys[0], "key_new");

		// Check we can continue to use the store just fine.
		do_read_write_remove_list_persist(&store);
	}
}
