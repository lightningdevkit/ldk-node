// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Shared fixtures and helpers for the database (`KVStore`) micro-benchmarks.
//!
//! This lives under `benches/` rather than in the library so the benchmark scaffolding does not
//! bloat the crate's public surface. It deliberately relies only on `ldk-node`'s public API.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::Txid;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::{KVStore, PaginatedKVStore};
use lightning::util::ser::{Readable, Writeable};
use lightning_persister::fs_store::v2::FilesystemStoreV2;
use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};

use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};

#[cfg(feature = "postgres")]
use ldk_node::io::postgres_store::{PostgresStore, DEFAULT_DB_NAME, POSTGRES_TEST_URL_ENV_VAR};

// The persistence namespaces the node uses for (pending) payments. Re-declared here so the
// benchmark exercises realistic keys without depending on crate-internal constants.
const PAYMENT_INFO_PRIMARY_NAMESPACE: &str = "payments";
const PAYMENT_INFO_SECONDARY_NAMESPACE: &str = "";
const PENDING_PAYMENT_INFO_PRIMARY_NAMESPACE: &str = "pending_payments";
const PENDING_PAYMENT_INFO_SECONDARY_NAMESPACE: &str = "";

pub const BATCH_LEN: u64 = 100;
pub const LARGE_PAYMENT_SET_LEN: u64 = 10_000;
pub const PAGINATED_PAGE_LEN: u64 = 50;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct PaymentUpdateBatch(Vec<PaymentDetails>);

#[derive(Clone)]
pub struct PendingPaymentBatch(Vec<PendingPaymentDetails>);

#[derive(Clone)]
pub struct PendingPaymentUpdateBatch(Vec<PendingPaymentDetails>);

#[derive(Clone)]
struct PendingPaymentDetails {
	details: PaymentDetails,
	conflicting_txids: Vec<Txid>,
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
});

#[derive(Clone, Copy)]
pub enum Backend {
	Filesystem,
	Sqlite,
	#[cfg(feature = "postgres")]
	Postgres,
}

impl Backend {
	pub fn name(self) -> &'static str {
		match self {
			Self::Filesystem => "filesystem",
			Self::Sqlite => "sqlite",
			#[cfg(feature = "postgres")]
			Self::Postgres => "postgres",
		}
	}
}

/// A concrete store wrapped for cheap cloning into concurrent benchmark tasks.
///
/// We dispatch over the concrete backends instead of using a trait object so the benchmark only
/// touches `ldk-node`'s public API.
#[derive(Clone)]
enum BenchStore {
	Filesystem(Arc<FilesystemStoreV2>),
	Sqlite(Arc<SqliteStore>),
	#[cfg(feature = "postgres")]
	Postgres(Arc<PostgresStore>),
}

impl BenchStore {
	async fn write(&self, primary: &str, secondary: &str, key: &str, buf: Vec<u8>) {
		match self {
			Self::Filesystem(store) => {
				KVStore::write(store.as_ref(), primary, secondary, key, buf).await.unwrap()
			},
			Self::Sqlite(store) => {
				KVStore::write(store.as_ref(), primary, secondary, key, buf).await.unwrap()
			},
			#[cfg(feature = "postgres")]
			Self::Postgres(store) => {
				KVStore::write(store.as_ref(), primary, secondary, key, buf).await.unwrap()
			},
		}
	}

	async fn read(&self, primary: &str, secondary: &str, key: &str) -> Vec<u8> {
		match self {
			Self::Filesystem(store) => {
				KVStore::read(store.as_ref(), primary, secondary, key).await.unwrap()
			},
			Self::Sqlite(store) => {
				KVStore::read(store.as_ref(), primary, secondary, key).await.unwrap()
			},
			#[cfg(feature = "postgres")]
			Self::Postgres(store) => KVStore::read(store.as_ref(), primary, secondary, key).await.unwrap(),
		}
	}

	async fn remove(&self, primary: &str, secondary: &str, key: &str) {
		match self {
			Self::Filesystem(store) => {
				KVStore::remove(store.as_ref(), primary, secondary, key, false).await.unwrap()
			},
			Self::Sqlite(store) => {
				KVStore::remove(store.as_ref(), primary, secondary, key, false).await.unwrap()
			},
			#[cfg(feature = "postgres")]
			Self::Postgres(store) => {
				KVStore::remove(store.as_ref(), primary, secondary, key, false).await.unwrap()
			},
		}
	}

	async fn list(&self, primary: &str, secondary: &str) -> Vec<String> {
		match self {
			Self::Filesystem(store) => {
				KVStore::list(store.as_ref(), primary, secondary).await.unwrap()
			},
			Self::Sqlite(store) => KVStore::list(store.as_ref(), primary, secondary).await.unwrap(),
			#[cfg(feature = "postgres")]
			Self::Postgres(store) => KVStore::list(store.as_ref(), primary, secondary).await.unwrap(),
		}
	}
}

pub struct StoreFixture {
	store: BenchStore,
	storage_path: PathBuf,
	#[cfg(feature = "postgres")]
	postgres_cleanup: Option<PostgresCleanup>,
}

impl StoreFixture {
	pub async fn new(backend: Backend, benchmark_name: &str) -> Self {
		let store_id = next_store_id();
		let storage_path = bench_storage_path(backend, benchmark_name, &store_id);
		#[cfg(feature = "postgres")]
		let mut postgres_cleanup = None;

		let store = match backend {
			Backend::Filesystem => BenchStore::Filesystem(Arc::new(
				FilesystemStoreV2::new(storage_path.clone()).unwrap(),
			)),
			Backend::Sqlite => BenchStore::Sqlite(Arc::new(
				SqliteStore::new(storage_path.clone(), None, None).unwrap(),
			)),
			#[cfg(feature = "postgres")]
			Backend::Postgres => {
				let connection_string = postgres_connection_string();
				let table_name = postgres_table_name(&store_id);
				let store = PostgresStore::new(
					connection_string.clone(),
					None,
					Some(table_name.clone()),
					None,
				)
				.await
				.unwrap();
				postgres_cleanup = Some(PostgresCleanup { connection_string, table_name });
				BenchStore::Postgres(Arc::new(store))
			},
		};

		Self {
			store,
			storage_path,
			#[cfg(feature = "postgres")]
			postgres_cleanup,
		}
	}

	pub async fn write_payment_batch(&self, payments: Vec<PaymentDetails>) {
		for payment in payments {
			self.write_payment_details(payment).await;
		}
	}

	pub async fn write_payment_batch_from_offset(&self, offset: u64) {
		self.write_payment_batch(payment_details_batch(offset)).await;
	}

	pub async fn write_payment(&self, idx: u64) {
		self.write_payment_details(payment_details(idx)).await;
	}

	pub async fn write_payment_update(&self, idx: u64) {
		self.write_payment_details(payment_update(idx)).await;
	}

	pub async fn write_payment_update_batch(&self, updates: PaymentUpdateBatch) {
		for payment in updates.0 {
			self.write_payment_details(payment).await;
		}
	}

	pub async fn read_payment(&self, idx: u64) -> Vec<u8> {
		self.store
			.read(
				PAYMENT_INFO_PRIMARY_NAMESPACE,
				PAYMENT_INFO_SECONDARY_NAMESPACE,
				&payment_key(idx),
			)
			.await
	}

	pub async fn remove_payment_key(&self, key: &str) {
		self.store
			.remove(PAYMENT_INFO_PRIMARY_NAMESPACE, PAYMENT_INFO_SECONDARY_NAMESPACE, key)
			.await;
	}

	pub async fn reload_payments(&self) -> Vec<PaymentDetails> {
		let keys =
			self.store.list(PAYMENT_INFO_PRIMARY_NAMESPACE, PAYMENT_INFO_SECONDARY_NAMESPACE).await;
		let mut payments = Vec::with_capacity(keys.len());
		for key in keys {
			let bytes = self
				.store
				.read(PAYMENT_INFO_PRIMARY_NAMESPACE, PAYMENT_INFO_SECONDARY_NAMESPACE, &key)
				.await;
			payments.push(PaymentDetails::read(&mut &bytes[..]).unwrap());
		}
		payments
	}

	pub async fn write_payment_batch_concurrent(&self, offset: u64, same_key: bool) {
		let mut handles = Vec::with_capacity(BATCH_LEN as usize);
		for idx in offset..offset + BATCH_LEN {
			let store = self.store.clone();
			let payment = payment_details(idx);
			let key = if same_key {
				"shared_payment_key".to_string()
			} else {
				payment_key_hex(&payment.id)
			};
			handles.push(tokio::spawn(async move {
				store
					.write(
						PAYMENT_INFO_PRIMARY_NAMESPACE,
						PAYMENT_INFO_SECONDARY_NAMESPACE,
						&key,
						payment.encode(),
					)
					.await;
			}));
		}
		for handle in handles {
			handle.await.unwrap();
		}
	}

	pub async fn write_pending_payment_batch(&self, payments: PendingPaymentBatch) {
		for payment in payments.0 {
			self.write_pending_payment_details(payment).await;
		}
	}

	pub async fn write_pending_payment_update_batch(&self, updates: PendingPaymentUpdateBatch) {
		for payment in updates.0 {
			self.write_pending_payment_details(payment).await;
		}
	}

	pub async fn insert_update_read_payment(&self, idx: u64) -> Vec<u8> {
		self.write_payment_details(payment_details(idx)).await;
		self.write_payment_details(payment_update(idx)).await;
		self.read_payment(idx).await
	}

	async fn write_payment_details(&self, payment: PaymentDetails) {
		self.store
			.write(
				PAYMENT_INFO_PRIMARY_NAMESPACE,
				PAYMENT_INFO_SECONDARY_NAMESPACE,
				&payment_key_hex(&payment.id),
				payment.encode(),
			)
			.await;
	}

	async fn write_pending_payment_details(&self, payment: PendingPaymentDetails) {
		self.store
			.write(
				PENDING_PAYMENT_INFO_PRIMARY_NAMESPACE,
				PENDING_PAYMENT_INFO_SECONDARY_NAMESPACE,
				&payment_key_hex(&payment.details.id),
				payment.encode(),
			)
			.await;
	}
}

impl Drop for StoreFixture {
	fn drop(&mut self) {
		#[cfg(feature = "postgres")]
		if let Some(cleanup) = self.postgres_cleanup.take() {
			tokio::spawn(async move {
				cleanup.drop_table().await;
			});
			return;
		}

		let _ = std::fs::remove_dir_all(&self.storage_path);
	}
}

pub struct PaginatedStoreFixture {
	inner: PaginatedStoreFixtureInner,
}

enum PaginatedStoreFixtureInner {
	Filesystem {
		store: FilesystemStoreV2,
		storage_path: PathBuf,
	},
	Sqlite {
		store: SqliteStore,
		storage_path: PathBuf,
	},
	#[cfg(feature = "postgres")]
	Postgres {
		store: PostgresStore,
		runtime: Arc<tokio::runtime::Runtime>,
		cleanup: PostgresCleanup,
	},
}

impl PaginatedStoreFixture {
	pub async fn new(
		backend: Backend, benchmark_name: &str, _runtime: Arc<tokio::runtime::Runtime>,
	) -> Option<Self> {
		let store_id = next_store_id();
		let storage_path = bench_storage_path(backend, benchmark_name, &store_id);

		match backend {
			Backend::Filesystem => {
				let store = FilesystemStoreV2::new(storage_path.clone()).unwrap();
				populate_async_store(&store).await;
				Some(Self { inner: PaginatedStoreFixtureInner::Filesystem { store, storage_path } })
			},
			Backend::Sqlite => {
				let store = SqliteStore::new(storage_path.clone(), None, None).unwrap();
				populate_async_store(&store).await;
				Some(Self { inner: PaginatedStoreFixtureInner::Sqlite { store, storage_path } })
			},
			#[cfg(feature = "postgres")]
			Backend::Postgres => {
				let connection_string = postgres_connection_string();
				let table_name = postgres_table_name(&store_id);
				let store = PostgresStore::new(
					connection_string.clone(),
					None,
					Some(table_name.clone()),
					None,
				)
				.await
				.unwrap();
				populate_async_store(&store).await;
				let cleanup = PostgresCleanup { connection_string, table_name };
				Some(Self {
					inner: PaginatedStoreFixtureInner::Postgres {
						store,
						runtime: _runtime,
						cleanup,
					},
				})
			},
		}
	}

	pub async fn list_first_page(&self) -> usize {
		match &self.inner {
			PaginatedStoreFixtureInner::Filesystem { store, .. } => list_first_page(store).await,
			PaginatedStoreFixtureInner::Sqlite { store, .. } => list_first_page(store).await,
			#[cfg(feature = "postgres")]
			PaginatedStoreFixtureInner::Postgres { store, .. } => list_first_page(store).await,
		}
	}

	pub async fn list_second_page(&self) -> usize {
		match &self.inner {
			PaginatedStoreFixtureInner::Filesystem { store, .. } => list_second_page(store).await,
			PaginatedStoreFixtureInner::Sqlite { store, .. } => list_second_page(store).await,
			#[cfg(feature = "postgres")]
			PaginatedStoreFixtureInner::Postgres { store, .. } => list_second_page(store).await,
		}
	}
}

impl Drop for PaginatedStoreFixture {
	fn drop(&mut self) {
		match &mut self.inner {
			PaginatedStoreFixtureInner::Filesystem { storage_path, .. }
			| PaginatedStoreFixtureInner::Sqlite { storage_path, .. } => {
				let _ = std::fs::remove_dir_all(storage_path);
			},
			#[cfg(feature = "postgres")]
			PaginatedStoreFixtureInner::Postgres { runtime, cleanup, .. } => {
				let cleanup = cleanup.clone();
				runtime.spawn(async move {
					cleanup.drop_table().await;
				});
			},
		}
	}
}

async fn list_first_page<S: PaginatedKVStore>(store: &S) -> usize {
	PaginatedKVStore::list_paginated(
		store,
		PAYMENT_INFO_PRIMARY_NAMESPACE,
		PAYMENT_INFO_SECONDARY_NAMESPACE,
		None,
	)
	.await
	.unwrap()
	.keys
	.len()
}

async fn list_second_page<S: PaginatedKVStore>(store: &S) -> usize {
	let first_page = PaginatedKVStore::list_paginated(
		store,
		PAYMENT_INFO_PRIMARY_NAMESPACE,
		PAYMENT_INFO_SECONDARY_NAMESPACE,
		None,
	)
	.await
	.unwrap();
	let page_token = first_page.next_page_token.expect("first page should have a next token");
	PaginatedKVStore::list_paginated(
		store,
		PAYMENT_INFO_PRIMARY_NAMESPACE,
		PAYMENT_INFO_SECONDARY_NAMESPACE,
		Some(page_token),
	)
	.await
	.unwrap()
	.keys
	.len()
}

pub fn configured_backends() -> Vec<Backend> {
	#[cfg(feature = "postgres")]
	{
		let mut backends = vec![Backend::Filesystem, Backend::Sqlite];
		if std::env::var(POSTGRES_TEST_URL_ENV_VAR).is_ok() {
			backends.push(Backend::Postgres);
		}
		backends
	}

	#[cfg(not(feature = "postgres"))]
	{
		vec![Backend::Filesystem, Backend::Sqlite]
	}
}

async fn populate_async_store<S: KVStore>(store: &S) {
	for idx in 0..LARGE_PAYMENT_SET_LEN {
		let payment = payment_details(idx);
		KVStore::write(
			store,
			PAYMENT_INFO_PRIMARY_NAMESPACE,
			PAYMENT_INFO_SECONDARY_NAMESPACE,
			&payment_key_hex(&payment.id),
			payment.encode(),
		)
		.await
		.unwrap();
	}
}

fn next_store_id() -> String {
	let store_id = NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed);
	format!("{}_{}", std::process::id(), store_id)
}

fn bench_storage_path(backend: Backend, benchmark_name: &str, store_id: &str) -> PathBuf {
	let mut path = std::env::temp_dir();
	path.push("ldk-node-db-benches");
	path.push(backend.name());
	path.push(benchmark_name);
	path.push(store_id);
	path
}

#[cfg(feature = "postgres")]
fn postgres_connection_string() -> String {
	std::env::var(POSTGRES_TEST_URL_ENV_VAR).unwrap_or_else(|_| {
		panic!("{POSTGRES_TEST_URL_ENV_VAR} must be set to run postgres database benchmarks")
	})
}

#[cfg(feature = "postgres")]
#[derive(Clone)]
struct PostgresCleanup {
	connection_string: String,
	table_name: String,
}

#[cfg(feature = "postgres")]
impl PostgresCleanup {
	async fn drop_table(&self) {
		// The fixtures create their tables via `PostgresStore::new(.., None, ..)`, which resolves the
		// database name to `DEFAULT_DB_NAME` when the connection string omits a `dbname`. Mirror that
		// resolution here so cleanup connects to the same database the tables actually live in.
		let Ok(mut config) = self.connection_string.parse::<tokio_postgres::Config>() else {
			return;
		};
		if config.get_dbname().is_none() {
			config.dbname(DEFAULT_DB_NAME);
		}
		let Ok((client, connection)) = config.connect(tokio_postgres::NoTls).await else {
			return;
		};
		tokio::spawn(async move {
			let _ = connection.await;
		});

		let table_name = postgres_identifier(&self.table_name);
		let _ = client.execute(&format!("DROP TABLE IF EXISTS {table_name}"), &[]).await;
	}
}

#[cfg(feature = "postgres")]
fn postgres_table_name(store_id: &str) -> String {
	format!("ldk_node_bench_{store_id}")
}

#[cfg(feature = "postgres")]
fn postgres_identifier(identifier: &str) -> String {
	format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub fn payment_details_batch(offset: u64) -> Vec<PaymentDetails> {
	(offset..offset + BATCH_LEN).map(payment_details).collect()
}

pub fn payment_update_batch_from_offset(offset: u64) -> PaymentUpdateBatch {
	PaymentUpdateBatch((offset..offset + BATCH_LEN).map(payment_update).collect())
}

pub fn pending_payment_details_batch_from_offset(offset: u64) -> PendingPaymentBatch {
	PendingPaymentBatch(
		(offset..offset + BATCH_LEN)
			.map(|idx| {
				let txid = Txid::from_byte_array(filled_bytes(idx + 1));
				PendingPaymentDetails {
					details: payment_details(idx),
					conflicting_txids: vec![txid],
				}
			})
			.collect(),
	)
}

pub fn pending_payment_update_batch_from_offset(offset: u64) -> PendingPaymentUpdateBatch {
	PendingPaymentUpdateBatch(
		(offset..offset + BATCH_LEN)
			.map(|idx| {
				let txid = Txid::from_byte_array(filled_bytes(idx + 3));
				PendingPaymentDetails {
					details: payment_update(idx),
					conflicting_txids: vec![txid],
				}
			})
			.collect(),
	)
}

fn payment_details(idx: u64) -> PaymentDetails {
	PaymentDetails {
		id: payment_id(idx),
		kind: PaymentKind::Bolt11 {
			hash: PaymentHash(filled_bytes(idx + 2)),
			preimage: Some(PaymentPreimage(filled_bytes(idx + 1))),
			secret: Some(PaymentSecret(filled_bytes(idx + 3))),
			counterparty_skimmed_fee_msat: None,
		},
		amount_msat: Some(10_000 + idx),
		fee_paid_msat: None,
		direction: PaymentDirection::Outbound,
		status: PaymentStatus::Pending,
		latest_update_timestamp: 0,
	}
}

/// The same payment as [`payment_details`], updated to a succeeded state. Writing this over an
/// existing entry models the common "update the payment we just persisted" workload.
fn payment_update(idx: u64) -> PaymentDetails {
	let id = payment_id(idx);
	PaymentDetails {
		id,
		kind: PaymentKind::Bolt11 {
			hash: PaymentHash(id.0),
			preimage: Some(PaymentPreimage(filled_bytes(idx + 2))),
			secret: Some(PaymentSecret(id.0)),
			counterparty_skimmed_fee_msat: None,
		},
		amount_msat: Some(10_000),
		fee_paid_msat: Some(42),
		direction: PaymentDirection::Outbound,
		status: PaymentStatus::Succeeded,
		latest_update_timestamp: 0,
	}
}

fn payment_id(idx: u64) -> PaymentId {
	PaymentId(filled_bytes(idx))
}

pub fn payment_key(idx: u64) -> String {
	payment_key_hex(&payment_id(idx))
}

fn payment_key_hex(id: &PaymentId) -> String {
	let mut s = String::with_capacity(id.0.len() * 2);
	for byte in id.0.iter() {
		s.push_str(&format!("{byte:02x}"));
	}
	s
}

fn filled_bytes(idx: u64) -> [u8; 32] {
	let mut bytes = [0u8; 32];
	bytes[..8].copy_from_slice(&idx.to_be_bytes());
	bytes[8..16].copy_from_slice(&(idx.wrapping_mul(0x9e37_79b9)).to_be_bytes());
	bytes
}
