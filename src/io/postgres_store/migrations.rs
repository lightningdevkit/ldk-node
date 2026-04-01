// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::io;
use tokio_postgres::Client;

pub(super) async fn migrate_schema(
	_client: &Client, _kv_table_name: &str, from_version: u16, to_version: u16,
) -> io::Result<()> {
	assert!(from_version < to_version);
	// Future migrations go here, e.g.:
	// if from_version == 1 && to_version >= 2 {
	//     migrate_v1_to_v2(client, kv_table_name).await?;
	//     from_version = 2;
	// }
	Ok(())
}
