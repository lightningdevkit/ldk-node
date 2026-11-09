// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#[cfg(test)]
mod tests {
	use ldk_node::io::sqlite_store::{
		SqliteStore, DEFAULT_KV_TABLE_NAME, DEFAULT_SQLITE_DB_FILE_NAME,
	};
	use ldk_node::lightning::util::persist::KVStore;

	#[tokio::test]
	async fn persists_with_the_applications_rusqlite() {
		let data_dir =
			std::env::temp_dir().join(format!("ldk-node-rusqlite-compat-{}", std::process::id()));
		let store = SqliteStore::new(data_dir.clone(), None, None).unwrap();
		KVStore::write(&store, "test", "", "key", vec![42]).await.unwrap();
		drop(store);

		let store = SqliteStore::new(data_dir.clone(), None, None).unwrap();
		assert_eq!(KVStore::read(&store, "test", "", "key").await.unwrap(), vec![42]);
		drop(store);

		let connection =
			rusqlite::Connection::open(data_dir.join(DEFAULT_SQLITE_DB_FILE_NAME)).unwrap();
		let value: Vec<u8> = connection
			.query_row(
				&format!("SELECT value FROM {DEFAULT_KV_TABLE_NAME} WHERE key = 'key'"),
				[],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(value, vec![42]);
		drop(connection);
		std::fs::remove_dir_all(data_dir).unwrap();
	}
}
