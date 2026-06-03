// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::{Deref, DerefMut};

use lightning::io;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::{Client, Config, NoTls};

use crate::logger::{log_debug, LdkLogger, Logger};

#[derive(Clone)]
pub(super) enum PgTlsConnector {
	Plain,
	NativeTls(MakeTlsConnector),
}

// Keep the spawned connection driver tied to its Client so reconnects and store teardown don't
// leave detached postgres driver tasks behind.
pub(super) struct ClientConnection {
	client: Client,
	connection_task: tokio::task::JoinHandle<()>,
}

impl ClientConnection {
	fn new(client: Client, connection_task: tokio::task::JoinHandle<()>) -> Self {
		Self { client, connection_task }
	}
}

impl Deref for ClientConnection {
	type Target = Client;

	fn deref(&self) -> &Self::Target {
		&self.client
	}
}

impl DerefMut for ClientConnection {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.client
	}
}

impl Drop for ClientConnection {
	fn drop(&mut self) {
		self.connection_task.abort();
	}
}

// The number of database connections kept in [`SmallPool`]. Reduced under `cfg(test)`
// so the parallel test suite stays comfortably under PostgreSQL's default `max_connections`.
#[cfg(not(test))]
pub(super) const POOL_SIZE: usize = 10;
#[cfg(test)]
pub(super) const POOL_SIZE: usize = 2;

/// A fixed-size pool of mutex-guarded PostgreSQL clients.
///
/// `get` awaits on whichever slot lock becomes available first via [`tokio::select!`]. Up to
/// [`POOL_SIZE`] operations can run concurrently against the database. Reconnection only happens
/// when the chosen client's local mpsc sender is already closed — no per-acquire health-check
/// round trip.
pub(super) struct SmallPool {
	pub(super) connections: [tokio::sync::Mutex<ClientConnection>; POOL_SIZE],
}

impl SmallPool {
	pub(super) async fn new(config: &Config, tls: &PgTlsConnector) -> io::Result<Self> {
		let mut conns: Vec<tokio::sync::Mutex<ClientConnection>> = Vec::with_capacity(POOL_SIZE);
		for _ in 0..POOL_SIZE {
			let connection = make_config_connection(config, tls).await?;
			conns.push(tokio::sync::Mutex::new(connection));
		}
		let connections: [tokio::sync::Mutex<ClientConnection>; POOL_SIZE] =
			conns.try_into().ok().expect("pool built with exactly POOL_SIZE connections");
		Ok(Self { connections })
	}

	pub(super) async fn get(
		&self, config: &Config, tls: &PgTlsConnector, logger: Option<&Logger>,
	) -> io::Result<tokio::sync::MutexGuard<'_, ClientConnection>> {
		#[cfg(test)]
		let mut conn = tokio::select! {
			c = self.connections[0].lock() => c,
			c = self.connections[1].lock() => c,
		};
		#[cfg(not(test))]
		let mut conn = tokio::select! {
			c = self.connections[0].lock() => c,
			c = self.connections[1].lock() => c,
			c = self.connections[2].lock() => c,
			c = self.connections[3].lock() => c,
			c = self.connections[4].lock() => c,
			c = self.connections[5].lock() => c,
			c = self.connections[6].lock() => c,
			c = self.connections[7].lock() => c,
			c = self.connections[8].lock() => c,
			c = self.connections[9].lock() => c,
		};
		if conn.is_closed() {
			if let Some(logger) = logger {
				log_debug!(logger, "Reconnecting to PostgreSQL (client closed)");
			}
			*conn = make_config_connection(config, tls).await?;
		}
		Ok(conn)
	}
}

pub(super) async fn make_config_connection(
	config: &Config, tls: &PgTlsConnector,
) -> io::Result<ClientConnection> {
	// make sure db_name is configured, otherwise it may attempt to use the username instead.
	config
		.get_dbname()
		.expect("database name must be set on config before calling create_database_if_not_exists");

	let err_map = |e| {
		let msg = format!("Failed to connect to PostgreSQL: {e}");
		io::Error::new(io::ErrorKind::Other, msg)
	};

	match tls {
		PgTlsConnector::Plain => {
			let (client, connection) = config.connect(NoTls).await.map_err(err_map)?;
			let connection_task = tokio::spawn(async move {
				if let Err(e) = connection.await {
					log::error!("PostgreSQL connection error: {e}");
				}
			});
			Ok(ClientConnection::new(client, connection_task))
		},
		PgTlsConnector::NativeTls(tls_connector) => {
			let (client, connection) =
				config.connect(tls_connector.clone()).await.map_err(err_map)?;
			let connection_task = tokio::spawn(async move {
				if let Err(e) = connection.await {
					log::error!("PostgreSQL connection error: {e}");
				}
			});
			Ok(ClientConnection::new(client, connection_task))
		},
	}
}
