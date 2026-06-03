// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to [`PostgresStore`] live here.
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lightning::io;
use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
use lightning_types::string::PrintableString;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Config, Error as PgError};

use self::pool::{make_config_connection, ClientConnection, PgTlsConnector, SmallPool};
use crate::io::utils::check_namespace_key_validity;
use crate::logger::{log_debug, log_info, LdkLogger, Logger};

mod migrations;
mod pool;

/// The default database name used when none is specified.
pub const DEFAULT_DB_NAME: &str = "ldk_db";

/// The default table in which we store all data.
pub const DEFAULT_KV_TABLE_NAME: &str = "ldk_data";

// The current schema version for the PostgreSQL store.
const SCHEMA_VERSION: u16 = 1;

// The number of entries returned per page in paginated list operations.
const PAGE_SIZE: usize = 50;

// Keep this small while still allowing progress if one runtime worker blocks on sync store access.
const INTERNAL_RUNTIME_WORKERS: usize = 2;

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

/// Runs a tokio-postgres query and, if the connection dropped mid-flight, reconnects and retries
/// once. `$store` is the [`PostgresStoreInner`], `$locked` the held client slot guard,
/// `$err_map` an `Fn(PgError) -> io::Error` (called at most once), and `$query` an expression
/// that yields a fresh `Future<Output = Result<_, PgError>>` each time it's evaluated. `$query`
/// may be evaluated up to twice (once normally, once on retry), so it must be side-effect-free
/// outside of issuing the query itself.
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

/// A [`KVStore`] implementation that writes to and reads from a [PostgreSQL] database.
///
/// Maintains an internal runtime for the underlying tokio-postgres connection drivers.
///
/// [PostgreSQL]: https://www.postgresql.org
pub struct PostgresStore {
	inner: Arc<PostgresStoreInner>,

	// Version counter to ensure that writes are applied in the correct order. It is assumed that read and list
	// operations aren't sensitive to the order of execution.
	next_write_version: AtomicU64,

	// A store-internal runtime used for setup and connection driver tasks.
	internal_runtime: Option<tokio::runtime::Runtime>,
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
		let internal_runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.thread_name_fn(|| {
				static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
				let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
				format!("ldk-node-postgres-runtime-{}", id)
			})
			.worker_threads(INTERNAL_RUNTIME_WORKERS)
			.max_blocking_threads(INTERNAL_RUNTIME_WORKERS)
			.build()
			.map_err(|e| {
				io::Error::new(
					io::ErrorKind::Other,
					format!("Failed to build PostgreSQL runtime: {e}"),
				)
			})?;
		let tls = Self::build_tls_connector(certificate_pem)?;
		let runtime_handle = internal_runtime.handle();
		let inner = tokio::task::block_in_place(|| {
			runtime_handle.block_on(async {
				PostgresStoreInner::new(connection_string, db_name, kv_table_name, tls, logger)
					.await
			})
		})?;
		let inner = Arc::new(inner);
		let next_write_version = AtomicU64::new(1);
		Ok(Self { inner, next_write_version, internal_runtime: Some(internal_runtime) })
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
}

impl Drop for PostgresStore {
	fn drop(&mut self) {
		if let Some(internal_runtime) = self.internal_runtime.take() {
			internal_runtime.shutdown_background();
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
		async move { inner.read_internal(&primary_namespace, &secondary_namespace, &key).await }
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
		async move {
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
		async move {
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
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		async move { inner.list_internal(&primary_namespace, &secondary_namespace).await }
	}
}

impl PostgresStore {
	fn internal_runtime(&self) -> io::Result<&tokio::runtime::Runtime> {
		self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal PostgreSQL runtime");
			io::Error::new(io::ErrorKind::Other, "Failed to access internal PostgreSQL runtime")
		})
	}
}

impl PaginatedKVStore for PostgresStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		async move {
			inner
				.list_paginated_internal(&primary_namespace, &secondary_namespace, page_token)
				.await
		}
	}
}

struct PostgresStoreInner {
	pool: SmallPool,
	config: Config,
	kv_table_name_sql: String,
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

		let client = make_config_connection(&config, &tls).await?;

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
		client.execute(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to create table {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// Read the schema version from the table comment (analogous to SQLite's PRAGMA user_version).
		let row = client
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
			client.execute(sql.as_str(), &[]).await.map_err(|e| {
				let msg = format!("Failed to set schema version: {e}");
				io::Error::new(io::ErrorKind::Other, msg)
			})?;
		} else if version_res < SCHEMA_VERSION {
			migrations::migrate_schema(&client, &kv_table_name_sql, version_res, SCHEMA_VERSION)
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
		client.execute(sql.as_str(), &[]).await.map_err(|e| {
			let msg = format!("Failed to create index on table {kv_table_name}: {e}");
			io::Error::new(io::ErrorKind::Other, msg)
		})?;

		// Drop the setup client; the pool builds its own POOL_SIZE fresh connections.
		drop(client);
		let pool = SmallPool::new(&config, &tls).await?;

		let write_version_locks = Mutex::new(HashMap::new());
		Ok(Self { pool, config, kv_table_name_sql, tls, write_version_locks, logger })
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
			let sql = format!(
				"INSERT INTO {} (primary_namespace, secondary_namespace, key, value) \
				 VALUES ($1, $2, $3, $4) \
				 ON CONFLICT (primary_namespace, secondary_namespace, key) DO UPDATE SET value = EXCLUDED.value",
				self.kv_table_name_sql
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
			query_with_retry!(
				self,
				locked,
				err_map,
				locked.execute(
					sql.as_str(),
					&[&primary_namespace, &secondary_namespace, &key, &buf],
				)
			)
			.map(|_| ())
		})
		.await
	}

	async fn remove_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "remove")?;

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let sql = format!(
				"DELETE FROM {} WHERE primary_namespace=$1 AND secondary_namespace=$2 AND key=$3",
				self.kv_table_name_sql
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
			query_with_retry!(
				self,
				locked,
				err_map,
				locked.execute(sql.as_str(), &[&primary_namespace, &secondary_namespace, &key])
			)
			.map(|_| ())
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
		std::env::var("TEST_POSTGRES_URL")
			.unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ldk_node_tests".to_string())
	}

	async fn create_test_store(table_name: &str) -> PostgresStore {
		PostgresStore::new(test_connection_string(), None, Some(table_name.to_string()), None)
			.await
			.unwrap()
	}

	async fn cleanup_store(store: &PostgresStore) {
		let kv_table = store.inner.kv_table_name_sql.clone();
		let client = store.inner.pool.connections[0].lock().await;
		let _ = client.execute(&format!("DROP TABLE IF EXISTS {kv_table}"), &[]).await;
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

	async fn kill_connection(store: &PostgresStore) {
		// Terminate every backend in the pool so the next op deterministically
		// hits a closed connection regardless of which slot `get` selects.
		for mutex in &store.inner.pool.connections {
			let client = mutex.lock().await;
			let _ = client.execute("SELECT pg_terminate_backend(pg_backend_pid())", &[]).await;
		}
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
