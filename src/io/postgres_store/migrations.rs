// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::io;
use tokio_postgres::Transaction;

pub(super) async fn migrate_schema(
	transaction: &Transaction<'_>, kv_table_name: &str, mut from_version: u16, to_version: u16,
) -> io::Result<()> {
	assert!(from_version < to_version);
	if from_version == 1 && to_version >= 2 {
		migrate_v1_to_v2(transaction, kv_table_name).await?;
		from_version = 2;
	}

	if from_version != to_version {
		return Err(io::Error::new(
			io::ErrorKind::Other,
			format!("No PostgreSQL schema migration from version {from_version} to {to_version}"),
		));
	}
	Ok(())
}

async fn migrate_v1_to_v2(transaction: &Transaction<'_>, kv_table_name: &str) -> io::Result<()> {
	// Schema v2 marks the transition from the legacy session advisory lock to fenced node leases.
	// Older releases reject this version instead of reopening the store without lease fencing.
	let sql = format!("COMMENT ON TABLE {kv_table_name} IS '2'");
	transaction.execute(&sql, &[]).await.map_err(|e| {
		io::Error::new(
			io::ErrorKind::Other,
			format!("Failed to set PostgreSQL schema version 2: {e}"),
		)
	})?;
	Ok(())
}
