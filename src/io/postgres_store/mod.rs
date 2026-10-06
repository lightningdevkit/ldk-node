// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to [`PostgresStore`] live here.
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightning::io;
use lightning::util::persist::{
	KVStore, MigratableKVStore, PageToken, PaginatedKVStore, PaginatedListResponse,
};
use lightning_types::string::PrintableString;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Config, Error as PgError};

use self::pool::{make_config_connection, ClientConnection, PgTlsConnector, SmallPool};
use crate::io::utils::check_namespace_key_validity;
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::runtime::StoreRuntime;

mod migrations;
mod pool;

/// The default database name used when none is specified.
pub const DEFAULT_DB_NAME: &str = "ldk_db";

/// The default table in which we store all data.
pub const DEFAULT_KV_TABLE_NAME: &str = "ldk_data";

/// Environment variable used by PostgreSQL tests and benchmarks.
pub const POSTGRES_TEST_URL_ENV_VAR: &str = "TEST_POSTGRES_URL";

// The current schema version for the PostgreSQL store.
const SCHEMA_VERSION: u16 = 1;

// The number of entries returned per page in paginated list operations.
const PAGE_SIZE: usize = 50;

// Keep this small while still allowing progress if one runtime worker blocks on sync store access.
const INTERNAL_RUNTIME_WORKERS: usize = 2;

const NODE_LEASE_DURATION: Duration = Duration::from_secs(30);
const NODE_LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(10);
const NODE_LEASE_RENEWAL_TIMEOUT: Duration = Duration::from_secs(10);
const NODE_LEASE_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

const NODE_LEASE_TABLE_SUFFIX: &str = "_node_lease";
const POSTGRES_IDENTIFIER_MAX_BYTES: usize = 63;
const MAX_KV_TABLE_NAME_BYTES: usize =
	POSTGRES_IDENTIFIER_MAX_BYTES - NODE_LEASE_TABLE_SUFFIX.len();

fn sql_identifier(identifier: &str) -> io::Result<String> {
	if identifier.is_empty() || identifier.contains('\0') {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			format!("Invalid PostgreSQL identifier: {identifier}"),
		));
	}

	Ok(format!("\"{}\"", identifier.replace('"', "\"\"")))
}

fn sql_table_identifier(table_name: &str) -> io::Result<String> {
	let parts: Vec<&str> = table_name.split('.').collect();
	if parts.is_empty() || parts.len() > 2 || parts.iter().any(|part| part.is_empty()) {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			format!(
				"Invalid PostgreSQL table name: {table_name}. Expected a table name or schema.table"
			),
		));
	}

	let quoted_parts: io::Result<Vec<String>> = parts.into_iter().map(sql_identifier).collect();
	Ok(quoted_parts?.join("."))
}

fn sql_node_lease_table_identifier(table_name: &str) -> io::Result<String> {
	sql_table_identifier(table_name)?;
	let table_part = table_name.rsplit_once('.').map_or(table_name, |(_, table)| table);
	if table_part.len() > MAX_KV_TABLE_NAME_BYTES {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			format!(
				"PostgreSQL KV table name exceeds the maximum of {MAX_KV_TABLE_NAME_BYTES} bytes: {table_name}"
			),
		));
	}

	sql_table_identifier(&format!("{table_name}{NODE_LEASE_TABLE_SUFFIX}"))
}

/// Runs a standalone tokio-postgres query and, if the pooled connection dropped mid-flight,
/// reconnects and retries once. `$store` is the [`PostgresStoreInner`], `$locked` the held client
/// slot guard, `$err_map` an `FnOnce(PgError) -> io::Error`, and `$query` an expression that yields a fresh
/// `Future<Output = Result<_, PgError>>` each time it is evaluated. `$query` may be evaluated up to
/// twice (once normally, once on retry), so it must be side-effect-free outside of issuing the
/// query itself.
macro_rules! query_with_retry {
	($store:expr, $locked:ident, $err_map:expr, $query:expr) => {{
		match $query.await {
			Ok(v) => Ok(v),
			Err(e) if $locked.is_closed() || e.is_closed() => {
				if let Some(logger) = $store.logger.as_ref() {
					log_debug!(logger, "Reconnecting to PostgreSQL after error: {e}");
				}
				*$locked = make_config_connection(&$store.config, &$store.tls).await?;
				$query.await.map_err($err_map)
			},
			Err(e) => Err($err_map(e)),
		}
	}};
}

fn handle_runtime_task_result<T>(
	result: Result<io::Result<T>, tokio::task::JoinError>,
) -> io::Result<T> {
	match result {
		Ok(result) => result,
		Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
		Err(e) => Err(io::Error::new(
			io::ErrorKind::Other,
			format!("PostgreSQL runtime task failed: {e}"),
		)),
	}
}

/// A [`KVStore`] implementation that writes to and reads from a [PostgreSQL] database.
///
/// Maintains an internal runtime for the underlying tokio-postgres connection drivers.
/// Each instance exclusively leases its configured KV table and checks the lease within each KV
/// mutation transaction. Only the background renewal task extends the lease.
/// Lease rejection panics in the calling task; a failed or timed-out
/// background renewal panics in the renewal task. Applications must set `panic = "abort"` in their
/// own Cargo profiles so these panics terminate the process. Recovery requires restarting the
/// process and constructing a fresh node from persisted state.
///
/// [PostgreSQL]: https://www.postgresql.org
pub struct PostgresStore {
	inner: Arc<PostgresStoreInner>,

	// Version counter to ensure that writes are applied in the correct order. It is assumed that read and list
	// operations aren't sensitive to the order of execution.
	next_write_version: AtomicU64,

	// Outstanding mutations retain the lease and its renewal task after the store is dropped.
	resources: Arc<PostgresStoreResources>,
}

struct PostgresStoreResources {
	inner: Arc<PostgresStoreInner>,

	// A store-internal runtime that drives PostgreSQL I/O independently from the node runtime.
	internal_runtime: Option<Arc<StoreRuntime>>,

	lease_renewal_task: tokio::task::JoinHandle<()>,
	lease_shutdown_sender: Option<tokio::sync::oneshot::Sender<()>>,
	lease_shutdown_complete: Mutex<std::sync::mpsc::Receiver<io::Result<()>>>,
}

// tokio::sync::Mutex (used for the DB client) contains UnsafeCell which opts out of
// RefUnwindSafe. std::sync::Mutex (used by SqliteStore) doesn't have this issue because
// it poisons on panic. This impl is needed for do_read_write_remove_list_persist which
// requires K: KVStore + RefUnwindSafe.
#[cfg(test)]
impl std::panic::RefUnwindSafe for PostgresStore {}

impl PostgresStore {
	/// Constructs a new [`PostgresStore`].
	///
	/// Connects to the PostgreSQL database at the given `connection_string`, e.g.,
	/// `"postgres://user:password@localhost/ldk_db"`.
	///
	/// The given `db_name` will be used or default to [`DEFAULT_DB_NAME`]. The
	/// `connection_string` must not include a `dbname` when `db_name` is set, providing both
	/// is an error. The database will be created automatically if it doesn't already exist.
	/// The initial connection is made to the target database, and if it fails we fall back to
	/// the default `postgres` database to create it.
	///
	/// The given `kv_table_name` will be used or default to [`DEFAULT_KV_TABLE_NAME`].
	/// A companion lease table is created by appending `_node_lease` to this name.
	///
	/// Construction acquires an exclusive lease for the selected KV table. Returns an error with
	/// [`io::ErrorKind::AlreadyExists`] while another store holds an unexpired lease.
	///
	/// If `certificate_pem` is `Some`, TLS will be used for database connections and the
	/// provided PEM-encoded CA certificate will be added to the system's default root
	/// certificates (it does not replace them). If `certificate_pem` is `None`, connections
	/// will be unencrypted.
	pub async fn new(
		connection_string: String, db_name: Option<String>, kv_table_name: Option<String>,
		certificate_pem: Option<String>,
	) -> io::Result<Self> {
		Self::new_with_logger(connection_string, db_name, kv_table_name, certificate_pem, None)
			.await
	}

	/// Like [`Self::new`], but lets crate-internal callers route setup logs through the node
	/// logger. This stays separate because [`Logger`] is crate-private and cannot be part of the
	/// public [`PostgresStore::new`] signature.
	pub(crate) async fn new_with_logger(
		connection_string: String, db_name: Option<String>, kv_table_name: Option<String>,
		certificate_pem: Option<String>, logger: Option<Arc<Logger>>,
	) -> io::Result<Self> {
		let internal_runtime = Arc::new(StoreRuntime::new(
			"ldk-node-postgres-runtime",
			INTERNAL_RUNTIME_WORKERS,
			"PostgreSQL",
		)?);
		let tls = Self::build_tls_connector(certificate_pem)?;
		let task = internal_runtime.spawn(async move {
			PostgresStoreInner::new(connection_string, db_name, kv_table_name, tls, logger).await
		});
		let inner = task.await.map_err(|e| {
			io::Error::new(io::ErrorKind::Other, format!("PostgreSQL runtime task failed: {}", e))
		})??;
		let inner = Arc::new(inner);

		let inner_ref = Arc::clone(&inner);
		let (lease_shutdown_sender, shutdown_rx) = tokio::sync::oneshot::channel();
		let (completion_sender, lease_shutdown_complete) = std::sync::mpsc::channel();
		let lease_renewal_task = internal_runtime.spawn(async move {
			let mut interval = tokio::time::interval(NODE_LEASE_RENEWAL_INTERVAL);
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			let lease_duration_secs = NODE_LEASE_DURATION.as_secs() as i64;
			let lease_table = &inner_ref.node_lease_table_name_sql;
			let update_sql = format!(
				"UPDATE {lease_table}
				SET expires_at = clock_timestamp() + ($2::bigint * interval '1 second')
				WHERE id = 1 AND owner_id = $1 AND expires_at > clock_timestamp()"
			);
			let renewal_loop = async {
				loop {
					// The first tick is immediate; later attempts follow the renewal interval.
					interval.tick().await;
					let renewal = async {
						let mut locked = inner_ref.locked_client().await?;
						let err_map = |e| {
							io::Error::new(
								io::ErrorKind::Other,
								format!("Failed to renew node lease: {e}"),
							)
						};
						query_with_retry!(
							inner_ref,
							locked,
							err_map,
							locked.execute(
								&update_sql,
								&[&inner_ref.lease_owner_id.as_slice(), &lease_duration_secs]
							)
						)
					};
					// Bound both the pool wait and query.
					match tokio::time::timeout(NODE_LEASE_RENEWAL_TIMEOUT, renewal).await {
						Ok(Ok(1)) => {},
						Ok(Ok(_)) => panic!(
							"PostgreSQL node lease was lost; continuing may corrupt node state"
						),
						Ok(Err(e)) => panic!("Failed to renew PostgreSQL node lease: {e}"),
						Err(_) => panic!("PostgreSQL node lease renewal timed out"),
					}
				}
			};
			tokio::select! {
				biased;
				_ = shutdown_rx => {},
				_ = renewal_loop => {},
			}

			let result = inner_ref.release_node_lease().await;
			let _ = completion_sender.send(result);
		});

		Ok(Self {
			inner: Arc::clone(&inner),
			next_write_version: AtomicU64::new(1),
			resources: Arc::new(PostgresStoreResources {
				inner,
				internal_runtime: Some(internal_runtime),
				lease_renewal_task,
				lease_shutdown_sender: Some(lease_shutdown_sender),
				lease_shutdown_complete: Mutex::new(lease_shutdown_complete),
			}),
		})
	}

	fn build_tls_connector(certificate_pem: Option<String>) -> io::Result<PgTlsConnector> {
		match certificate_pem {
			Some(pem) => {
				let crt = native_tls::Certificate::from_pem(pem.as_bytes()).map_err(|e| {
					io::Error::new(
						io::ErrorKind::InvalidInput,
						format!("Failed to parse PEM certificate: {e}"),
					)
				})?;
				let connector =
					TlsConnector::builder().add_root_certificate(crt).build().map_err(|e| {
						io::Error::new(
							io::ErrorKind::Other,
							format!("Failed to build TLS connector: {e}"),
						)
					})?;
				Ok(PgTlsConnector::NativeTls(MakeTlsConnector::new(connector)))
			},
			None => Ok(PgTlsConnector::Plain),
		}
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

	fn internal_runtime(&self) -> Arc<StoreRuntime> {
		Arc::clone(
			self.resources.internal_runtime.as_ref().expect("PostgreSQL runtime must be available"),
		)
	}
}

impl Drop for PostgresStoreResources {
	fn drop(&mut self) {
		if let Some(sender) = self.lease_shutdown_sender.take() {
			let _ = sender.send(());
		}
		// The final mutation may release the last resources reference on a store runtime worker.
		// Allow lease shutdown to progress while this worker waits for completion.
		let completion = self.lease_shutdown_complete.get_mut().unwrap();
		let wait = || completion.recv_timeout(NODE_LEASE_RELEASE_TIMEOUT);
		let result = match tokio::runtime::Handle::try_current() {
			Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
				tokio::task::block_in_place(wait)
			},
			_ => wait(),
		};
		if let Some(logger) = self.inner.logger.as_ref() {
			match result {
				Ok(Ok(())) => {},
				Ok(Err(e)) => log_error!(logger, "Failed to release PostgreSQL node lease: {e}"),
				Err(e) => log_error!(logger, "PostgreSQL lease shutdown did not complete: {e}"),
			}
		}
		// Cancel any remaining work if shutdown failed or timed out.
		self.lease_renewal_task.abort();

		if let Some(internal_runtime) = self.internal_runtime.take() {
			if let Ok(internal_runtime) = Arc::try_unwrap(internal_runtime) {
				internal_runtime.shutdown_background();
			}
		}
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
		let runtime = self.internal_runtime();
		async move {
			let task = runtime.spawn(async move {
				inner.read_internal(&primary_namespace, &secondary_namespace, &key).await
			});
			handle_runtime_task_result(task.await)
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
		let runtime = self.internal_runtime();
		let resources = Arc::clone(&self.resources);
		async move {
			let task = runtime.spawn(async move {
				let _resources = resources;
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
			});
			handle_runtime_task_result(task.await)
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
		let runtime = self.internal_runtime();
		let resources = Arc::clone(&self.resources);
		async move {
			let task = runtime.spawn(async move {
				let _resources = resources;
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
			});
			handle_runtime_task_result(task.await)
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		async move {
			let task = runtime.spawn(async move {
				inner.list_internal(&primary_namespace, &secondary_namespace).await
			});
			handle_runtime_task_result(task.await)
		}
	}
}

impl PaginatedKVStore for PostgresStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		async move {
			let task = runtime.spawn(async move {
				inner
					.list_paginated_internal(&primary_namespace, &secondary_namespace, page_token)
					.await
			});
			handle_runtime_task_result(task.await)
		}
	}
}

impl MigratableKVStore for PostgresStore {
	fn list_all_keys(
		&self,
	) -> impl Future<Output = Result<Vec<(String, String, String)>, io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		async move {
			let task = runtime.spawn(async move { inner.list_all_keys_internal().await });
			handle_runtime_task_result(task.await)
		}
	}
}

struct PostgresStoreInner {
	pool: SmallPool,
	config: Config,
	kv_table_name_sql: String,
	node_lease_table_name_sql: String,
	lease_owner_id: [u8; 32],
	tls: PgTlsConnector,
	write_version_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
	logger: Option<Arc<Logger>>,
}

impl PostgresStoreInner {
	async fn new(
		connection_string: String, db_name: Option<String>, kv_table_name: Option<String>,
		tls: PgTlsConnector, logger: Option<Arc<Logger>>,
	) -> io::Result<Self> {
		let kv_table_name = kv_table_name.unwrap_or(DEFAULT_KV_TABLE_NAME.to_string());
		let kv_table_name_sql = sql_table_identifier(&kv_table_name)?;
		let node_lease_table_name_sql = sql_node_lease_table_identifier(&kv_table_name)?;
		let mut lease_owner_id = [0u8; 32];
		getrandom::fill(&mut lease_owner_id).map_err(|e| {
			io::Error::new(io::ErrorKind::Other, format!("Failed to generate lease owner ID: {e}"))
		})?;

		let mut config: Config = connection_string.parse().map_err(|e: PgError| {
			let msg = format!("Failed to parse PostgreSQL connection string: {e}");
			io::Error::new(io::ErrorKind::InvalidInput, msg)
		})?;

		if db_name.is_some() && config.get_dbname().is_some() {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"db_name must not be set when the connection string already contains a dbname",
			));
		}

		// Reconcile the configured TLS connector with the connection string's sslmode.
		// Refuse to silently downgrade an opted-in TLS config to plaintext.
		if matches!(tls, PgTlsConnector::NativeTls(_)) {
			match config.get_ssl_mode() {
				SslMode::Disable => {
					return Err(io::Error::new(
						io::ErrorKind::InvalidInput,
						"certificate_pem was provided but the connection string sets sslmode=disable",
					));
				},
				SslMode::Prefer => {
					config.ssl_mode(SslMode::Require);
				},
				SslMode::Require => {},
				_ => {},
			}
		}

		let db_name = db_name
			.or_else(|| config.get_dbname().map(|s| s.to_string()))
			.unwrap_or(DEFAULT_DB_NAME.to_string());
		config.dbname(&db_name);

		Self::create_database_if_not_exists(&config, &tls, logger.as_deref()).await?;

		let mut client = make_config_connection(&config, &tls).await?;
		let pool = SmallPool::new(&config, &tls).await?;
		let transaction = client.transaction().await.map_err(|e| {
			io::Error::new(
				io::ErrorKind::Other,
				format!("Failed to start PostgreSQL schema setup transaction: {e}"),
			)
		})?;

		// Create the KV data table if it doesn't exist. `sort_order` uses BIGSERIAL so
		// the database assigns a fresh, monotonically increasing value on each INSERT and
		// keeps the previous value untouched on UPSERT-update; the sequence persists across
		// restarts.
		let sql = format!(
			"CREATE TABLE IF NOT EXISTS {kv_table_name_sql} (
			primary_namespace TEXT NOT NULL,
			secondary_namespace TEXT NOT NULL DEFAULT '',
			key TEXT NOT NULL CHECK (key <> ''),
			value BYTEA,
			sort_order BIGSERIAL CHECK (sort_order >= 0),
			PRIMARY KEY (primary_namespace, secondary_namespace, key)
			)"
		);
		transaction.execute(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to create table {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// Read the schema version from the table comment (analogous to SQLite's PRAGMA user_version).
		let row = transaction
			.query_one("SELECT obj_description(to_regclass($1), 'pg_class')", &[&kv_table_name_sql])
			.await
			.map_err(|e| {
				let msg = format!("Failed to read schema version for {kv_table_name}: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;
		let version_res: u16 = match row.get::<_, Option<&str>>(0) {
			Some(version_str) => {
				let version = version_str.parse().map_err(|_| {
					let msg = format!("Invalid schema version: {version_str}");
					io::Error::new(io::ErrorKind::Other, msg)
				})?;

				// We should never expect version 0, our min SCHEMA_VERSION is 1,
				// and version 0 is used to indicate a new table.
				if version == 0 {
					return Err(io::Error::new(
						io::ErrorKind::Other,
						format!("Invalid schema version: {version_str}, cannot be 0"),
					));
				}

				version
			},
			None => 0,
		};

		if version_res == 0 {
			// New table, set our SCHEMA_VERSION.
			let sql = format!("COMMENT ON TABLE {kv_table_name_sql} IS '{SCHEMA_VERSION}'");
			transaction.execute(sql.as_str(), &[]).await.map_err(|e| {
				let msg = format!("Failed to set schema version: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;
		} else if version_res < SCHEMA_VERSION {
			migrations::migrate_schema(
				transaction.client(),
				&kv_table_name_sql,
				version_res,
				SCHEMA_VERSION,
			)
			.await?;
		} else if version_res > SCHEMA_VERSION {
			let msg = format!(
				"Failed to open database: incompatible schema version {version_res}. Expected: {SCHEMA_VERSION}"
			);
			return Err(io::Error::new(io::ErrorKind::Other, msg));
		}

		// Create composite index for paginated listing.
		let index_name_sql = sql_identifier(&format!("idx_{kv_table_name}_paginated"))?;
		let sql = format!(
			"CREATE INDEX IF NOT EXISTS {index_name_sql} ON {kv_table_name_sql} (primary_namespace, secondary_namespace, sort_order DESC, key ASC)"
		);
		transaction.execute(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to create index on table {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// A unique owner ID makes takeover conflict with mutation key-share locks.
		let sql = format!(
			"CREATE TABLE IF NOT EXISTS {node_lease_table_name_sql} (
			id SMALLINT PRIMARY KEY CHECK (id = 1),
			owner_id BYTEA NOT NULL UNIQUE,
			expires_at TIMESTAMPTZ NOT NULL
			)"
		);
		transaction.execute(&sql, &[]).await.map_err(|e| {
			io::Error::new(io::ErrorKind::Other, format!("Failed to create node lease table: {e}"))
		})?;

		// Acquire the lease after schema setup.
		// Schema setup and lease ownership commit or roll back together.
		let lease_duration_secs = NODE_LEASE_DURATION.as_secs() as i64;
		let acquire_sql = format!(
			"INSERT INTO {node_lease_table_name_sql} (id, owner_id, expires_at)
			VALUES (1, $1, clock_timestamp() + ($2::bigint * interval '1 second'))
			ON CONFLICT (id) DO UPDATE SET
			owner_id = EXCLUDED.owner_id,
			expires_at = EXCLUDED.expires_at
			WHERE {node_lease_table_name_sql}.expires_at <= clock_timestamp()"
		);
		let acquired = transaction
			.execute(&acquire_sql, &[&lease_owner_id.as_slice(), &lease_duration_secs])
			.await
			.map_err(|e| {
				io::Error::new(io::ErrorKind::Other, format!("Failed to acquire node lease: {e}"))
			})?;
		if acquired != 1 {
			return Err(io::Error::new(
				io::ErrorKind::AlreadyExists,
				"PostgreSQL node lease is unavailable",
			));
		}

		transaction.commit().await.map_err(|e| {
			io::Error::new(
				io::ErrorKind::Other,
				format!("Failed to commit PostgreSQL schema setup transaction: {e}"),
			)
		})?;

		let write_version_locks = Mutex::new(HashMap::new());
		Ok(Self {
			pool,
			config,
			kv_table_name_sql,
			node_lease_table_name_sql,
			lease_owner_id,
			tls,
			write_version_locks,
			logger,
		})
	}

	async fn create_database_if_not_exists(
		config: &Config, tls: &PgTlsConnector, logger: Option<&Logger>,
	) -> io::Result<()> {
		let db_name = config.get_dbname().expect(
			"database name must be set on config before calling create_database_if_not_exists",
		);

		// Try connecting to the target database directly — if it exists we're done.
		let initial_err = match make_config_connection(config, tls).await {
			Ok(_) => return Ok(()),
			Err(e) => e,
		};
		// `initial_err` is only included in the final error if the bootstrap connect also
		// fails; otherwise it gets dropped silently. Log it at debug! so a later failure
		// (e.g. an auth error that surfaces only when the pool is built) can be traced
		// back to the real cause.
		if let Some(logger) = logger {
			log_debug!(
				logger,
				"Initial connection to '{db_name}' failed: {initial_err}. \
				Falling back to the 'postgres' database to check existence / create it."
			);
		}

		// Target database doesn't exist (or isn't reachable). Connect to the
		// default "postgres" database to create it.
		let mut bootstrap_config = config.clone();
		bootstrap_config.dbname("postgres");
		let client = make_config_connection(&bootstrap_config, tls).await.map_err(|e| {
			io::Error::new(
				io::ErrorKind::Other,
				format!(
					"Failed to connect to database '{db_name}': {initial_err}. \
					Also failed to connect to the 'postgres' database to create it: {e}. \
					You may need to create '{db_name}' manually."
				),
			)
		})?;

		let row = client
			.query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&db_name])
			.await
			.map_err(|e| {
				let msg = format!("Failed to check for database {db_name}: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;

		if row.is_none() {
			let db_name_sql = sql_identifier(db_name)?;
			let sql = format!("CREATE DATABASE {db_name_sql}");
			match client.execute(&sql, &[]).await {
				Ok(_) => {
					if let Some(logger) = logger {
						log_info!(logger, "Created database {db_name}");
					}
				},
				Err(e) => {
					// Another connection may have created it concurrently, that's fine.
					let duplicate = matches!(
						e.code(),
						Some(
							&tokio_postgres::error::SqlState::DUPLICATE_DATABASE
								| &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
						)
					);
					if !duplicate {
						return Err(io::Error::new(
							io::ErrorKind::Other,
							format!("Failed to create database {db_name}: {e}"),
						));
					}
				},
			}
		}

		Ok(())
	}

	async fn locked_client(&self) -> io::Result<tokio::sync::MutexGuard<'_, ClientConnection>> {
		self.pool.get(&self.config, &self.tls, self.logger.as_deref()).await
	}

	async fn release_node_lease(&self) -> io::Result<()> {
		let lease_table = &self.node_lease_table_name_sql;
		let sql = format!("DELETE FROM {lease_table} WHERE id = 1 AND owner_id = $1");
		let mut locked = self.locked_client().await?;
		let err_map =
			|e| io::Error::new(io::ErrorKind::Other, format!("Failed to release node lease: {e}"));
		query_with_retry!(
			self,
			locked,
			err_map,
			locked.execute(&sql, &[&self.lease_owner_id.as_slice()])
		)?;
		Ok(())
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<tokio::sync::Mutex<u64>> {
		let mut outer_lock = self.write_version_locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	async fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "read")?;

		let sql = format!(
			"SELECT value FROM {} WHERE primary_namespace=$1 AND secondary_namespace=$2 AND key=$3",
			self.kv_table_name_sql
		);

		let err_map = |e: PgError| {
			let msg = format!(
				"Failed to read from key {}/{}/{}: {}",
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace),
				PrintableString(key),
				e
			);
			io::Error::new(io::ErrorKind::Other, msg)
		};

		let mut locked = self.locked_client().await?;
		let row = query_with_retry!(
			self,
			locked,
			err_map,
			locked.query_opt(sql.as_str(), &[&primary_namespace, &secondary_namespace, &key])
		)?;

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
			let kv_table = &self.kv_table_name_sql;
			let lease_table = &self.node_lease_table_name_sql;
			// PostgreSQL takes the KV table lock before executing the lease check. The key-share lock
			// allows concurrent mutations and renewal, but blocks takeover until the statement commits,
			// even if the lease expires while the mutation waits for a KV row lock.
			let sql = format!(
				"WITH valid_lease AS (
					SELECT 1 FROM {lease_table}
					WHERE id = 1 AND owner_id = $1 AND expires_at > clock_timestamp() FOR KEY SHARE
				), mutation AS (
					INSERT INTO {kv_table} (primary_namespace, secondary_namespace, key, value)
					SELECT $2, $3, $4, $5 WHERE EXISTS (SELECT 1 FROM valid_lease)
					ON CONFLICT (primary_namespace, secondary_namespace, key) DO UPDATE SET value = EXCLUDED.value
				)
				SELECT 1 FROM valid_lease"
			);

			let err_map = |e: PgError| {
				let msg = format!(
					"Failed to write to key {}/{}/{}: {}",
					PrintableString(primary_namespace),
					PrintableString(secondary_namespace),
					PrintableString(key),
					e
				);
				io::Error::new(io::ErrorKind::Other, msg)
			};

			let mut locked = self.locked_client().await?;
			// Retry only preparation, which cannot mutate data. A failed execution may have committed
			// before its response was lost, so it must not be retried.
			let statement = query_with_retry!(self, locked, &err_map, locked.prepare(&sql))?;
			let lease = locked
				.query_opt(
					&statement,
					&[
						&self.lease_owner_id.as_slice(),
						&primary_namespace,
						&secondary_namespace,
						&key,
						&buf,
					],
				)
				.await
				.map_err(err_map)?;
			if lease.is_none() {
				panic!("PostgreSQL node lease was lost; continuing may corrupt node state");
			}
			Ok(())
		})
		.await
	}

	async fn remove_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "remove")?;

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let kv_table = &self.kv_table_name_sql;
			let lease_table = &self.node_lease_table_name_sql;
			// Return the lease row even when the key does not exist.
			let sql = format!(
				"WITH valid_lease AS (
					SELECT 1 FROM {lease_table}
					WHERE id = 1 AND owner_id = $1 AND expires_at > clock_timestamp() FOR KEY SHARE
				), mutation AS (
					DELETE FROM {kv_table} WHERE primary_namespace=$2 AND secondary_namespace=$3 AND key=$4
					AND EXISTS (SELECT 1 FROM valid_lease)
				)
				SELECT 1 FROM valid_lease"
			);

			let err_map = |e: PgError| {
				let msg = format!(
					"Failed to delete key {}/{}/{}: {}",
					PrintableString(primary_namespace),
					PrintableString(secondary_namespace),
					PrintableString(key),
					e
				);
				io::Error::new(io::ErrorKind::Other, msg)
			};

			let mut locked = self.locked_client().await?;
			// Retry only preparation, which cannot mutate data. A failed execution may have committed
			// before its response was lost, so it must not be retried.
			let statement = query_with_retry!(self, locked, &err_map, locked.prepare(&sql))?;
			let lease = locked
				.query_opt(
					&statement,
					&[
						&self.lease_owner_id.as_slice(),
						&primary_namespace,
						&secondary_namespace,
						&key,
					],
				)
				.await
				.map_err(err_map)?;
			if lease.is_none() {
				panic!("PostgreSQL node lease was lost; continuing may corrupt node state");
			}
			Ok(())
		})
		.await
	}

	async fn list_internal(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, None, "list")?;

		let sql = format!(
			"SELECT key FROM {} WHERE primary_namespace=$1 AND secondary_namespace=$2",
			self.kv_table_name_sql
		);

		let err_map = |e: PgError| {
			let msg = format!("Failed to retrieve queried rows: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		};

		let mut locked = self.locked_client().await?;
		let rows = query_with_retry!(
			self,
			locked,
			err_map,
			locked.query(sql.as_str(), &[&primary_namespace, &secondary_namespace])
		)?;

		let keys: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
		Ok(keys)
	}

	async fn list_all_keys_internal(&self) -> io::Result<Vec<(String, String, String)>> {
		let sql = format!(
			"SELECT primary_namespace, secondary_namespace, key FROM {}",
			self.kv_table_name_sql
		);

		let err_map = |e: PgError| {
			let msg = format!("Failed to retrieve queried rows: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		};

		let mut locked = self.locked_client().await?;
		let rows = query_with_retry!(self, locked, err_map, locked.query(sql.as_str(), &[]))?;

		let keys: Vec<(String, String, String)> =
			rows.iter().map(|row| (row.get(0), row.get(1), row.get(2))).collect();
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

		// Fetch one extra row beyond PAGE_SIZE to determine whether a next page exists.
		let fetch_limit = (PAGE_SIZE + 1) as i64;

		let token_sort_order: Option<i64> = match page_token {
			Some(ref token) => {
				let parsed: i64 = token.as_str().parse().map_err(|_| {
					let token_str = token.as_str();
					let msg = format!("Invalid page token: {token_str}");
					io::Error::new(io::ErrorKind::InvalidInput, msg)
				})?;

				if parsed < 0 {
					return Err(io::Error::new(
						io::ErrorKind::InvalidInput,
						format!(
							"Invalid page token: {}, sort_order cannot be negative",
							token.as_str()
						),
					));
				}
				Some(parsed)
			},
			None => None,
		};

		let err_map = |e: PgError| {
			let msg = format!("Failed to retrieve queried rows: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		};

		let mut locked = self.locked_client().await?;
		let rows = match token_sort_order {
			Some(token_sort_order) => {
				let sql = format!(
					"SELECT key, sort_order FROM {} \
					 WHERE primary_namespace=$1 \
					 AND secondary_namespace=$2 \
					 AND sort_order < $3 \
					 ORDER BY sort_order DESC, key ASC \
					 LIMIT $4",
					self.kv_table_name_sql
				);
				let params: [&(dyn tokio_postgres::types::ToSql + Sync); 4] =
					[&primary_namespace, &secondary_namespace, &token_sort_order, &fetch_limit];

				query_with_retry!(self, locked, err_map, locked.query(sql.as_str(), &params))?
			},
			None => {
				let sql = format!(
					"SELECT key, sort_order FROM {} \
					 WHERE primary_namespace=$1 \
					 AND secondary_namespace=$2 \
					 ORDER BY sort_order DESC, key ASC \
					 LIMIT $3",
					self.kv_table_name_sql
				);
				let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] =
					[&primary_namespace, &secondary_namespace, &fetch_limit];

				query_with_retry!(self, locked, err_map, locked.query(sql.as_str(), &params))?
			},
		};

		let has_more = rows.len() > PAGE_SIZE;
		let next_page_token = if has_more {
			let last_sort_order = rows[PAGE_SIZE - 1].get::<_, i64>(1);
			Some(PageToken::new(last_sort_order.to_string()))
		} else {
			None
		};

		let keys = rows.into_iter().take(PAGE_SIZE).map(|row| row.get(0)).collect();
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
		std::env::var(POSTGRES_TEST_URL_ENV_VAR)
			.unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ldk_node_tests".to_string())
	}

	async fn create_test_store(table_name: &str) -> PostgresStore {
		PostgresStore::new(test_connection_string(), None, Some(table_name.to_string()), None)
			.await
			.unwrap()
	}

	async fn cleanup_store(store: &PostgresStore) {
		// Stop renewal before removing its table.
		store.resources.lease_renewal_task.abort();
		let kv_table = store.inner.kv_table_name_sql.clone();
		let lease_table = &store.inner.node_lease_table_name_sql;
		let client = store.inner.pool.connections[0].lock().await;
		let _ =
			client.execute(&format!("DROP TABLE IF EXISTS {kv_table}, {lease_table}"), &[]).await;
	}

	#[test]
	fn test_postgres_identifier_quoting() {
		assert_eq!(sql_identifier("tenant-1").unwrap(), "\"tenant-1\"");
		assert_eq!(sql_identifier("select").unwrap(), "\"select\"");
		assert_eq!(sql_identifier("tenant\"one").unwrap(), "\"tenant\"\"one\"");
		assert_eq!(
			sql_table_identifier("tenant_schema.tenant-1").unwrap(),
			"\"tenant_schema\".\"tenant-1\""
		);
		assert!(sql_identifier("").is_err());
		assert!(sql_table_identifier("too.many.parts").is_err());
		assert!(sql_table_identifier("schema.").is_err());
		assert_eq!(sql_node_lease_table_identifier("tenant-1").unwrap(), "\"tenant-1_node_lease\"");
		assert_eq!(
			sql_node_lease_table_identifier("tenant.select").unwrap(),
			"\"tenant\".\"select_node_lease\""
		);
		assert!(sql_node_lease_table_identifier(&"a".repeat(MAX_KV_TABLE_NAME_BYTES)).is_ok());
		assert!(sql_node_lease_table_identifier(&"a".repeat(MAX_KV_TABLE_NAME_BYTES + 1)).is_err());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_lease() {
		let table_name = "test_pg_lease";
		let store = create_test_store(table_name).await;
		let kv_table = &store.inner.kv_table_name_sql;
		let mut client =
			make_config_connection(&store.inner.config, &store.inner.tls).await.unwrap();
		// Make the second store attempt a schema change before its lease is rejected.
		client.execute(&format!("COMMENT ON TABLE {kv_table} IS NULL"), &[]).await.unwrap();

		let err =
			PostgresStore::new(test_connection_string(), None, Some(table_name.to_string()), None)
				.await
				.err()
				.expect("a second store using the same database and table must fail");
		assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
		let row = client
			.query_one("SELECT obj_description(to_regclass($1), 'pg_class')", &[kv_table])
			.await
			.unwrap();
		assert_eq!(row.get::<_, Option<&str>>(0), None);

		// A write must complete while another transaction holds renewal's non-key update lock.
		let transaction = client.transaction().await.unwrap();
		let lease_table = &store.inner.node_lease_table_name_sql;
		transaction
			.query_one(&format!("SELECT 1 FROM {lease_table} WHERE id = 1 FOR NO KEY UPDATE"), &[])
			.await
			.unwrap();
		tokio::time::timeout(
			Duration::from_secs(5),
			KVStore::write(&store, "test_ns", "", "key", vec![1]),
		)
		.await
		.expect("renewal's row lock must not block writes")
		.unwrap();
		transaction.commit().await.unwrap();

		// Changing the owner must wait for a mutation's key-share lock to be released.
		let transaction = client.transaction().await.unwrap();
		transaction
			.query_one(&format!("SELECT 1 FROM {lease_table} WHERE id = 1 FOR KEY SHARE"), &[])
			.await
			.unwrap();
		let mut contender =
			make_config_connection(&store.inner.config, &store.inner.tls).await.unwrap();
		contender.batch_execute("SET lock_timeout = '1s'").await.unwrap();
		let takeover_sql = format!("UPDATE {lease_table} SET owner_id = $1 WHERE id = 1");
		let new_owner_id = [42u8; 32];
		let err = contender.execute(&takeover_sql, &[&new_owner_id.as_slice()]).await.unwrap_err();
		assert_eq!(err.code(), Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE));
		transaction.commit().await.unwrap();

		let takeover = contender.transaction().await.unwrap();
		assert_eq!(takeover.execute(&takeover_sql, &[&new_owner_id.as_slice()]).await.unwrap(), 1);
		takeover.rollback().await.unwrap();

		// Mutation futures retain the lease until they finish, even after dropping the store.
		let write = KVStore::write(&store, "test_ns", "", "key", vec![2]);
		let read = KVStore::read(&store, "test_ns", "", "key");
		drop(store);
		write.await.unwrap();
		assert_eq!(read.await.unwrap(), vec![2]);

		let store = create_test_store(table_name).await;
		let remove = KVStore::remove(&store, "test_ns", "", "key", false);
		drop(store);
		remove.await.unwrap();

		// The last mutation releases the lease so another store can acquire it immediately.
		let store = create_test_store(table_name).await;
		assert_eq!(
			KVStore::read(&store, "test_ns", "", "key").await.unwrap_err().kind(),
			io::ErrorKind::NotFound
		);
		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_background_renewal_and_lease_loss() {
		let mut store = create_test_store("test_pg_background_lease_loss").await;
		let mut client = store.inner.pool.connections[0].lock().await;
		let transaction = client.transaction().await.unwrap();
		let lease_table = &store.inner.node_lease_table_name_sql;
		let sql = format!("SELECT expires_at FROM {lease_table} WHERE id = 1 FOR KEY SHARE");
		let mut expires_at =
			transaction.query_one(&sql, &[]).await.unwrap().get::<_, std::time::SystemTime>(0);
		// Observe two renewals while holding a mutation fence, without KV writes.
		tokio::time::timeout(
			NODE_LEASE_RENEWAL_INTERVAL * 2 + NODE_LEASE_RENEWAL_TIMEOUT + Duration::from_secs(5),
			async {
				for _ in 0..2 {
					loop {
						let renewed_until = transaction
							.query_one(&sql, &[])
							.await
							.unwrap()
							.get::<_, std::time::SystemTime>(0);
						if renewed_until > expires_at {
							expires_at = renewed_until;
							break;
						}
						tokio::time::sleep(Duration::from_millis(50)).await;
					}
				}
			},
		)
		.await
		.expect("background renewal must extend the lease while a mutation fence is held");
		transaction.commit().await.unwrap();
		drop(client);

		expire_lease(&store).await;
		let error = tokio::time::timeout(
			NODE_LEASE_RENEWAL_INTERVAL + NODE_LEASE_RENEWAL_TIMEOUT + Duration::from_secs(5),
			&mut Arc::get_mut(&mut store.resources).unwrap().lease_renewal_task,
		)
		.await
		.expect("background renewal must detect lease loss without another store operation")
		.expect_err("background renewal must panic on lease loss");
		let panic = error.into_panic();
		assert!(panic.downcast_ref::<&str>().unwrap().contains("PostgreSQL node lease was lost"));
		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_background_renewal_panics_on_timeout() {
		let mut store = create_test_store("test_pg_background_lease_timeout").await;
		// Exhaust the pool to verify the renewal timeout includes waiting for a connection.
		let first = store.inner.pool.connections[0].lock().await;
		let second = store.inner.pool.connections[1].lock().await;
		let error = tokio::time::timeout(
			NODE_LEASE_RENEWAL_INTERVAL + NODE_LEASE_RENEWAL_TIMEOUT + Duration::from_secs(5),
			&mut Arc::get_mut(&mut store.resources).unwrap().lease_renewal_task,
		)
		.await
		.expect("background renewal must time out while waiting for the pool")
		.expect_err("background renewal must panic on timeout");
		let panic = error.into_panic();
		assert!(panic
			.downcast_ref::<&str>()
			.unwrap()
			.contains("PostgreSQL node lease renewal timed out"));
		drop(first);
		drop(second);
		// The panicked renewal task cannot release its lease during shutdown.
		store.inner.release_node_lease().await.unwrap();
		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_drop_with_blocked_pool() {
		let store = create_test_store("test_pg_drop_blocked_pool").await;
		let inner = Arc::clone(&store.inner);
		let client = make_config_connection(&inner.config, &inner.tls).await.unwrap();
		let _first = inner.pool.connections[0].lock().await;
		let _second = inner.pool.connections[1].lock().await;
		let start = std::time::Instant::now();
		drop(store);
		assert!(start.elapsed() < NODE_LEASE_RELEASE_TIMEOUT + Duration::from_secs(2));

		client
			.execute(
				&format!(
					"DROP TABLE {}, {}",
					inner.kv_table_name_sql, inner.node_lease_table_name_sql
				),
				&[],
			)
			.await
			.unwrap();
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_rolls_back_failed_schema_setup() {
		let table_name = "test_pg_schema_setup_rollback";
		let store = create_test_store(table_name).await;
		let kv_table = store.inner.kv_table_name_sql.clone();
		let lease_table = store.inner.node_lease_table_name_sql.clone();
		let client = make_config_connection(&store.inner.config, &store.inner.tls).await.unwrap();
		drop(store);

		// Force index creation to fail after setup writes the schema version.
		client
			.batch_execute(&format!(
				"COMMENT ON TABLE {kv_table} IS NULL; ALTER TABLE {kv_table} DROP COLUMN sort_order"
			))
			.await
			.unwrap();
		let err =
			PostgresStore::new(test_connection_string(), None, Some(table_name.to_string()), None)
				.await
				.err()
				.expect("schema setup must fail without sort_order");
		assert!(err.to_string().contains("Failed to create index"));

		let row = client
			.query_one("SELECT obj_description(to_regclass($1), 'pg_class')", &[&kv_table])
			.await
			.unwrap();
		client.execute(&format!("DROP TABLE {kv_table}, {lease_table}"), &[]).await.unwrap();
		assert_eq!(row.get::<_, Option<&str>>(0), None);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn read_write_remove_list_persist() {
		let store = create_test_store("test_rwrl").await;
		do_read_write_remove_list_persist(&store).await;
		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store() {
		let store_0 = create_test_store("test_pg_store_0").await;
		let store_1 = create_test_store("test_pg_store_1").await;
		do_test_store(&store_0, &store_1);
		cleanup_store(&store_0).await;
		cleanup_store(&store_1).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_list_all_keys() {
		let store = create_test_store("test_pg_list_all_keys").await;

		KVStore::write(&store, "ns_a", "sub_a", "key_a", vec![1u8]).await.unwrap();
		KVStore::write(&store, "ns_a", "sub_b", "key_b", vec![2u8]).await.unwrap();
		KVStore::write(&store, "ns_b", "", "key_c", vec![3u8]).await.unwrap();

		let mut keys = MigratableKVStore::list_all_keys(&store).await.unwrap();
		keys.sort();

		assert_eq!(
			keys,
			vec![
				("ns_a".to_string(), "sub_a".to_string(), "key_a".to_string()),
				("ns_a".to_string(), "sub_b".to_string(), "key_b".to_string()),
				("ns_b".to_string(), "".to_string(), "key_c".to_string()),
			]
		);

		cleanup_store(&store).await;
	}

	async fn kill_connection(store: &PostgresStore) {
		// Terminate each pooled backend to exercise reconnection. Background renewal may
		// reconnect a slot before the next store operation.
		for mutex in &store.inner.pool.connections {
			let client = mutex.lock().await;
			let _ = client.execute("SELECT pg_terminate_backend(pg_backend_pid())", &[]).await;
		}
	}

	async fn expire_lease(store: &PostgresStore) {
		let client = store.inner.pool.connections[0].lock().await;
		let lease_table = &store.inner.node_lease_table_name_sql;
		client
			.execute(
				&format!(
			"UPDATE {lease_table} SET expires_at = clock_timestamp() - interval '1 second' WHERE id = 1"
		),
				&[],
			)
			.await
			.unwrap();
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_auto_reconnect() {
		let store = create_test_store("test_pg_reconnect").await;

		let ns = "test_ns";
		let sub = "test_sub";

		// Write a value before disconnecting.
		KVStore::write(&store, ns, sub, "key_a", vec![1u8; 8]).await.unwrap();

		// Read should auto-reconnect and return the previously written value.
		kill_connection(&store).await;
		let data = KVStore::read(&store, ns, sub, "key_a").await.unwrap();
		assert_eq!(data, vec![1u8; 8]);

		// Write should auto-reconnect without a preceding read.
		kill_connection(&store).await;
		KVStore::write(&store, ns, sub, "key_b", vec![2u8; 8]).await.unwrap();
		let data = KVStore::read(&store, ns, sub, "key_b").await.unwrap();
		assert_eq!(data, vec![2u8; 8]);

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_queued_write_rechecks_lease() {
		let table_name = "test_pg_queued_write_lease";
		let store = create_test_store(table_name).await;
		KVStore::remove(&store, "test_ns", "test_sub", "missing", false).await.unwrap();
		let locking_key = store.build_locking_key("test_ns", "test_sub", "key");
		let inner_lock_ref = store.inner.get_inner_lock_ref(locking_key);
		let inner_lock = inner_lock_ref.lock().await;

		let mut write = Box::pin(KVStore::write(&store, "test_ns", "test_sub", "key", vec![1u8]));
		// Start the write while holding the per-key lock so it cannot proceed before lease expiry.
		std::future::poll_fn(|cx| match write.as_mut().poll(cx) {
			std::task::Poll::Pending => std::task::Poll::Ready(()),
			std::task::Poll::Ready(result) => {
				panic!("the write must be queued on the per-key lock, got {result:?}")
			},
		})
		.await;

		expire_lease(&store).await;
		// Even a delete that affects no rows must reject an expired lease before takeover.
		let remove = tokio::spawn(KVStore::remove(&store, "test_ns", "test_sub", "missing", false));
		assert!(remove.await.unwrap_err().is_panic());
		let second_store = create_test_store(table_name).await;
		drop(inner_lock);

		let write_task = tokio::spawn(write);
		let err = write_task.await.expect_err("the queued write must panic after the lock is lost");
		assert!(err.is_panic());
		let err = KVStore::read(&second_store, "test_ns", "test_sub", "key")
			.await
			.expect_err("the queued write must not persist data");
		assert_eq!(err.kind(), io::ErrorKind::NotFound);

		KVStore::write(&second_store, "test_ns", "test_sub", "key", vec![2]).await.unwrap();
		let remove = tokio::spawn(KVStore::remove(&store, "test_ns", "test_sub", "key", false));
		assert!(remove.await.unwrap_err().is_panic());
		// Exercise release even if the old renewal task has already panicked. Neither release
		// nor dropping the old owner may remove the replacement's lease.
		store.inner.release_node_lease().await.unwrap();
		drop(store);
		assert_eq!(
			KVStore::read(&second_store, "test_ns", "test_sub", "key").await.unwrap(),
			vec![2]
		);
		KVStore::write(&second_store, "test_ns", "test_sub", "key", vec![3]).await.unwrap();
		cleanup_store(&second_store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_listing() {
		let store = create_test_store("test_pg_paginated").await;

		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";
		let num_entries = 225;

		for i in 0..num_entries {
			let key = format!("key_{:04}", i);
			let data = vec![i as u8; 32];
			KVStore::write(&store, primary_namespace, secondary_namespace, &key, data)
				.await
				.unwrap();
		}

		// Paginate through all entries and collect them
		let mut all_keys = Vec::new();
		let mut page_token = None;
		let mut page_count = 0;

		loop {
			let response = PaginatedKVStore::list_paginated(
				&store,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
			.await
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

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_update_preserves_order() {
		let store = create_test_store("test_pg_paginated_update").await;

		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";

		KVStore::write(&store, primary_namespace, secondary_namespace, "first", vec![1u8; 8])
			.await
			.unwrap();
		KVStore::write(&store, primary_namespace, secondary_namespace, "second", vec![2u8; 8])
			.await
			.unwrap();
		KVStore::write(&store, primary_namespace, secondary_namespace, "third", vec![3u8; 8])
			.await
			.unwrap();

		// Update the first entry
		KVStore::write(&store, primary_namespace, secondary_namespace, "first", vec![99u8; 8])
			.await
			.unwrap();

		// Paginated listing should still show "first" with its original creation order
		let response =
			PaginatedKVStore::list_paginated(&store, primary_namespace, secondary_namespace, None)
				.await
				.unwrap();

		// Newest first: third, second, first
		assert_eq!(response.keys, vec!["third", "second", "first"]);

		// Verify the updated value was persisted
		let data =
			KVStore::read(&store, primary_namespace, secondary_namespace, "first").await.unwrap();
		assert_eq!(data, vec![99u8; 8]);

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_empty_namespace() {
		let store = create_test_store("test_pg_paginated_empty").await;

		// Paginating an empty or unknown namespace returns an empty result with no token.
		let response =
			PaginatedKVStore::list_paginated(&store, "nonexistent", "ns", None).await.unwrap();
		assert!(response.keys.is_empty());
		assert!(response.next_page_token.is_none());

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_namespace_isolation() {
		let store = create_test_store("test_pg_paginated_isolation").await;

		KVStore::write(&store, "ns_a", "sub", "key_1", vec![1u8; 8]).await.unwrap();
		KVStore::write(&store, "ns_a", "sub", "key_2", vec![2u8; 8]).await.unwrap();
		KVStore::write(&store, "ns_b", "sub", "key_3", vec![3u8; 8]).await.unwrap();
		KVStore::write(&store, "ns_a", "other", "key_4", vec![4u8; 8]).await.unwrap();

		// ns_a/sub should only contain key_1 and key_2 (newest first).
		let response = PaginatedKVStore::list_paginated(&store, "ns_a", "sub", None).await.unwrap();
		assert_eq!(response.keys, vec!["key_2", "key_1"]);
		assert!(response.next_page_token.is_none());

		// ns_b/sub should only contain key_3.
		let response = PaginatedKVStore::list_paginated(&store, "ns_b", "sub", None).await.unwrap();
		assert_eq!(response.keys, vec!["key_3"]);

		// ns_a/other should only contain key_4.
		let response =
			PaginatedKVStore::list_paginated(&store, "ns_a", "other", None).await.unwrap();
		assert_eq!(response.keys, vec!["key_4"]);

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_removal() {
		let store = create_test_store("test_pg_paginated_removal").await;

		let ns = "test_ns";
		let sub = "test_sub";

		KVStore::write(&store, ns, sub, "a", vec![1u8; 8]).await.unwrap();
		KVStore::write(&store, ns, sub, "b", vec![2u8; 8]).await.unwrap();
		KVStore::write(&store, ns, sub, "c", vec![3u8; 8]).await.unwrap();

		KVStore::remove(&store, ns, sub, "b", false).await.unwrap();

		let response = PaginatedKVStore::list_paginated(&store, ns, sub, None).await.unwrap();
		assert_eq!(response.keys, vec!["c", "a"]);
		assert!(response.next_page_token.is_none());

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_exact_page_boundary() {
		let store = create_test_store("test_pg_paginated_boundary").await;

		let ns = "test_ns";
		let sub = "test_sub";

		// Write exactly PAGE_SIZE entries (50).
		for i in 0..PAGE_SIZE {
			let key = format!("key_{:04}", i);
			KVStore::write(&store, ns, sub, &key, vec![i as u8; 8]).await.unwrap();
		}

		// Exactly PAGE_SIZE entries: all returned in one page with no next-page token.
		let response = PaginatedKVStore::list_paginated(&store, ns, sub, None).await.unwrap();
		assert_eq!(response.keys.len(), PAGE_SIZE);
		assert!(response.next_page_token.is_none());

		// Add one more entry (PAGE_SIZE + 1 total). First page should now have a token.
		KVStore::write(&store, ns, sub, "key_extra", vec![0u8; 8]).await.unwrap();
		let response = PaginatedKVStore::list_paginated(&store, ns, sub, None).await.unwrap();
		assert_eq!(response.keys.len(), PAGE_SIZE);
		assert!(response.next_page_token.is_some());

		// Second page should have exactly 1 entry and no token.
		let response = PaginatedKVStore::list_paginated(&store, ns, sub, response.next_page_token)
			.await
			.unwrap();
		assert_eq!(response.keys.len(), 1);
		assert!(response.next_page_token.is_none());

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_paginated_fewer_than_page_size() {
		let store = create_test_store("test_pg_paginated_few").await;

		let ns = "test_ns";
		let sub = "test_sub";

		// Write fewer entries than PAGE_SIZE.
		for i in 0..5 {
			let key = format!("key_{i}");
			KVStore::write(&store, ns, sub, &key, vec![i as u8; 8]).await.unwrap();
		}

		let response = PaginatedKVStore::list_paginated(&store, ns, sub, None).await.unwrap();
		assert_eq!(response.keys.len(), 5);
		// Fewer than PAGE_SIZE means no next page.
		assert!(response.next_page_token.is_none());
		// Newest first.
		assert_eq!(response.keys, vec!["key_4", "key_3", "key_2", "key_1", "key_0"]);

		cleanup_store(&store).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_postgres_store_write_version_persists_across_restart() {
		let table_name = "test_pg_write_version_restart";
		let primary_namespace = "test_ns";
		let secondary_namespace = "test_sub";

		{
			let store = create_test_store(table_name).await;

			KVStore::write(&store, primary_namespace, secondary_namespace, "key_a", vec![1u8; 8])
				.await
				.unwrap();
			KVStore::write(&store, primary_namespace, secondary_namespace, "key_b", vec![2u8; 8])
				.await
				.unwrap();

			// Don't clean up since we want to reopen
		}

		// Open a new store instance on the same database table and write more
		{
			let store = create_test_store(table_name).await;

			KVStore::write(&store, primary_namespace, secondary_namespace, "key_c", vec![3u8; 8])
				.await
				.unwrap();

			// Paginated listing should show newest first: key_c, key_b, key_a
			let response = PaginatedKVStore::list_paginated(
				&store,
				primary_namespace,
				secondary_namespace,
				None,
			)
			.await
			.unwrap();

			assert_eq!(response.keys, vec!["key_c", "key_b", "key_a"]);

			cleanup_store(&store).await;
		}
	}

	#[test]
	fn test_tls_config_none_builds_plain_connector() {
		let connector = PostgresStore::build_tls_connector(None).unwrap();
		assert!(matches!(connector, PgTlsConnector::Plain));
	}

	#[test]
	fn test_tls_config_invalid_pem_returns_error() {
		let result = PostgresStore::build_tls_connector(Some("not-a-valid-pem".to_string()));
		assert!(result.is_err());
	}
}
