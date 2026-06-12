// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::Txid;
use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::{KVStore, PaginatedKVStore};
use lightning::util::ser::Writeable;
use lightning_persister::fs_store::v2::FilesystemStoreV2;
use lightning_types::payment::{PaymentHash, PaymentPreimage};

use crate::data_store::StorableObjectId;
use crate::io::sqlite_store::SqliteStore;
use crate::io::utils::read_all_objects;
use crate::io::{
	PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::Logger;
use crate::payment::pending_payment_store::{PendingPaymentDetails, PendingPaymentDetailsUpdate};
use crate::payment::store::{
	PaymentDetails, PaymentDetailsUpdate, PaymentDirection, PaymentKind, PaymentStatus,
};
use crate::types::{DynStore, DynStoreRef, DynStoreWrapper};

pub const BATCH_LEN: u64 = 100;
pub const LARGE_PAYMENT_SET_LEN: u64 = 10_000;
pub const PAGINATED_PAGE_LEN: u64 = 50;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct PaymentUpdateBatch(Vec<PaymentDetailsUpdate>);

#[derive(Clone)]
pub struct PendingPaymentBatch(Vec<PendingPaymentDetails>);

#[derive(Clone)]
pub struct PendingPaymentUpdateBatch(Vec<PendingPaymentDetailsUpdate>);

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

pub struct StoreFixture {
	store: Arc<DynStore>,
	logger: Arc<Logger>,
	storage_path: PathBuf,
	#[cfg(feature = "postgres")]
	postgres_cleanup: Option<PostgresCleanup>,
}

impl StoreFixture {
	pub async fn new(backend: Backend, benchmark_name: &str) -> Self {
		let store_id = next_store_id();
		let storage_path = bench_storage_path(backend, benchmark_name, &store_id);
		let logger = Arc::new(Logger::new_log_facade());
		#[cfg(feature = "postgres")]
		let mut postgres_cleanup = None;

		let store: Arc<DynStore> = match backend {
			Backend::Filesystem => {
				let store = FilesystemStoreV2::new(storage_path.clone()).unwrap();
				Arc::new(DynStoreWrapper(store))
			},
			Backend::Sqlite => {
				let store = SqliteStore::new(storage_path.clone(), None, None).unwrap();
				Arc::new(DynStoreWrapper(store))
			},
			#[cfg(feature = "postgres")]
			Backend::Postgres => {
				let connection_string =
					std::env::var(crate::io::postgres_store::POSTGRES_TEST_URL_ENV_VAR)
						.unwrap_or_else(|_| {
							panic!(
								"{} must be set to run postgres database benchmarks",
								crate::io::postgres_store::POSTGRES_TEST_URL_ENV_VAR
							)
						});
				let table_name = postgres_table_name(&store_id);
				let store = crate::io::postgres_store::PostgresStore::new(
					connection_string.clone(),
					None,
					Some(table_name.clone()),
					None,
				)
				.await
				.unwrap();
				postgres_cleanup = Some(PostgresCleanup { connection_string, table_name });
				Arc::new(DynStoreWrapper(store))
			},
		};

		Self {
			store,
			logger,
			storage_path,
			#[cfg(feature = "postgres")]
			postgres_cleanup,
		}
	}

	pub async fn write_payment_batch(&self, payments: Vec<PaymentDetails>) {
		write_payment_batch(&*self.store, payments).await;
	}

	pub async fn write_payment_batch_from_offset(&self, offset: u64) {
		self.write_payment_batch(payment_details_batch(offset)).await;
	}

	pub async fn write_payment(&self, idx: u64) {
		write_payment(&*self.store, payment_details(idx)).await;
	}

	pub async fn write_payment_update(&self, idx: u64) {
		write_payment_update(&*self.store, payment_update(idx)).await;
	}

	pub async fn write_payment_update_batch(&self, updates: PaymentUpdateBatch) {
		write_payment_update_batch(&*self.store, updates.0).await;
	}

	pub async fn read_payment(&self, idx: u64) -> Vec<u8> {
		read_payment(&*self.store, payment_key(idx)).await.unwrap()
	}

	pub async fn remove_payment_key(&self, key: &str) {
		remove_payment(&*self.store, key).await;
	}

	pub async fn reload_payments(&self) -> Vec<PaymentDetails> {
		read_all_objects::<PaymentDetails, _>(
			&*self.store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			Arc::clone(&self.logger),
		)
		.await
		.unwrap()
	}

	pub async fn write_payment_batch_concurrent(&self, offset: u64, same_key: bool) {
		write_payment_batch_concurrent(DynStoreRef(Arc::clone(&self.store)), offset, same_key)
			.await;
	}

	pub async fn write_pending_payment_batch(&self, payments: PendingPaymentBatch) {
		write_pending_payment_batch(&*self.store, payments.0).await;
	}

	pub async fn write_pending_payment_update_batch(&self, updates: PendingPaymentUpdateBatch) {
		write_pending_payment_update_batch(&*self.store, updates.0).await;
	}

	pub async fn insert_update_read_payment(&self, idx: u64) -> Vec<u8> {
		let payment_key = payment_key(idx);
		write_payment(&*self.store, payment_details(idx)).await;
		write_payment_update(&*self.store, payment_update(idx)).await;
		read_payment(&*self.store, payment_key).await.unwrap()
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
		store: crate::io::postgres_store::PostgresStore,
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
				let connection_string =
					std::env::var(crate::io::postgres_store::POSTGRES_TEST_URL_ENV_VAR)
						.unwrap_or_else(|_| {
							panic!(
								"{} must be set to run postgres database benchmarks",
								crate::io::postgres_store::POSTGRES_TEST_URL_ENV_VAR
							)
						});
				let table_name = postgres_table_name(&store_id);
				let store = crate::io::postgres_store::PostgresStore::new(
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
			PaginatedStoreFixtureInner::Filesystem { store, .. } => {
				PaginatedKVStore::list_paginated(
					store,
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					None,
				)
				.await
				.unwrap()
				.keys
				.len()
			},
			PaginatedStoreFixtureInner::Sqlite { store, .. } => PaginatedKVStore::list_paginated(
				store,
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				None,
			)
			.await
			.unwrap()
			.keys
			.len(),
			#[cfg(feature = "postgres")]
			PaginatedStoreFixtureInner::Postgres { store, .. } => PaginatedKVStore::list_paginated(
				store,
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				None,
			)
			.await
			.unwrap()
			.keys
			.len(),
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

async fn list_second_page<S: PaginatedKVStore>(store: &S) -> usize {
	let first_page = PaginatedKVStore::list_paginated(
		store,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		None,
	)
	.await
	.unwrap();
	let page_token = first_page.next_page_token.expect("first page should have a next token");
	PaginatedKVStore::list_paginated(
		store,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
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
		if std::env::var(crate::io::postgres_store::POSTGRES_TEST_URL_ENV_VAR).is_ok() {
			backends.push(Backend::Postgres);
		}
		backends
	}

	#[cfg(not(feature = "postgres"))]
	{
		vec![Backend::Filesystem, Backend::Sqlite]
	}
}

async fn write_payment_batch<S: KVStore + ?Sized>(store: &S, payments: Vec<PaymentDetails>) {
	for payment in payments {
		write_payment(store, payment).await;
	}
}

async fn write_payment<S: KVStore + ?Sized>(store: &S, payment: PaymentDetails) {
	let key = payment.id.encode_to_hex_str();
	KVStore::write(
		store,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		&key,
		payment.encode(),
	)
	.await
	.unwrap();
}

async fn write_payment_update_batch<S: KVStore + ?Sized>(
	store: &S, updates: Vec<PaymentDetailsUpdate>,
) {
	for update in updates {
		write_payment_update(store, update).await;
	}
}

async fn write_payment_update<S: KVStore + ?Sized>(store: &S, update: PaymentDetailsUpdate) {
	write_payment(store, payment_details_from_update(update)).await;
}

async fn read_payment<S: KVStore + ?Sized>(store: &S, key: String) -> bitcoin::io::Result<Vec<u8>> {
	KVStore::read(
		store,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		&key,
	)
	.await
}

async fn remove_payment<S: KVStore + ?Sized>(store: &S, key: &str) {
	KVStore::remove(
		store,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		key,
		false,
	)
	.await
	.unwrap();
}

async fn write_payment_batch_concurrent(store: DynStoreRef, offset: u64, same_key: bool) {
	let mut handles = Vec::with_capacity(BATCH_LEN as usize);
	for idx in offset..offset + BATCH_LEN {
		let store = store.clone();
		let payment = payment_details(idx);
		let key = if same_key {
			"shared_payment_key".to_string()
		} else {
			payment.id.encode_to_hex_str()
		};
		handles.push(tokio::spawn(async move {
			KVStore::write(
				&store,
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
				payment.encode(),
			)
			.await
			.unwrap();
		}));
	}
	for handle in handles {
		handle.await.unwrap();
	}
}

async fn write_pending_payment_batch<S: KVStore + ?Sized>(
	store: &S, payments: Vec<PendingPaymentDetails>,
) {
	for payment in payments {
		let key = payment.details.id.encode_to_hex_str();
		KVStore::write(
			store,
			PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&key,
			payment.encode(),
		)
		.await
		.unwrap();
	}
}

async fn write_pending_payment_update_batch<S: KVStore + ?Sized>(
	store: &S, updates: Vec<PendingPaymentDetailsUpdate>,
) {
	for update in updates {
		let payment = pending_payment_details_from_update(update);
		let key = payment.details.id.encode_to_hex_str();
		KVStore::write(
			store,
			PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&key,
			payment.encode(),
		)
		.await
		.unwrap();
	}
}

async fn populate_async_store<S: KVStore>(store: &S) {
	for idx in 0..LARGE_PAYMENT_SET_LEN {
		let payment = payment_details(idx);
		KVStore::write(
			store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&payment_id(idx).encode_to_hex_str(),
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
#[derive(Clone)]
struct PostgresCleanup {
	connection_string: String,
	table_name: String,
}

#[cfg(feature = "postgres")]
impl PostgresCleanup {
	async fn drop_table(&self) {
		let Ok((client, connection)) =
			tokio_postgres::connect(&self.connection_string, tokio_postgres::NoTls).await
		else {
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
	PaymentUpdateBatch(payment_update_batch(offset))
}

pub fn pending_payment_details_batch_from_offset(offset: u64) -> PendingPaymentBatch {
	PendingPaymentBatch(pending_payment_details_batch(offset))
}

pub fn pending_payment_update_batch_from_offset(offset: u64) -> PendingPaymentUpdateBatch {
	PendingPaymentUpdateBatch(pending_payment_update_batch(offset))
}

fn pending_payment_details_batch(offset: u64) -> Vec<PendingPaymentDetails> {
	payment_details_batch(offset)
		.into_iter()
		.enumerate()
		.map(|(idx, payment)| {
			let txid = Txid::from_byte_array(filled_bytes((idx as u64) + 1));
			PendingPaymentDetails::new(payment, vec![txid])
		})
		.collect()
}

fn payment_update_batch(offset: u64) -> Vec<PaymentDetailsUpdate> {
	(offset..offset + BATCH_LEN).map(payment_update).collect()
}

fn payment_update(idx: u64) -> PaymentDetailsUpdate {
	let mut update = PaymentDetailsUpdate::new(payment_id(idx));
	update.status = Some(PaymentStatus::Succeeded);
	update.fee_paid_msat = Some(Some(42));
	update.preimage = Some(Some(PaymentPreimage(filled_bytes(idx + 2))));
	update
}

fn pending_payment_update_batch(offset: u64) -> Vec<PendingPaymentDetailsUpdate> {
	payment_update_batch(offset)
		.into_iter()
		.enumerate()
		.map(|(idx, payment_update)| PendingPaymentDetailsUpdate {
			id: payment_update.id,
			payment_update: Some(payment_update),
			conflicting_txids: Some(vec![Txid::from_byte_array(filled_bytes(idx as u64 + 3))]),
		})
		.collect()
}

fn payment_details(idx: u64) -> PaymentDetails {
	let preimage = PaymentPreimage(filled_bytes(idx + 1));
	let hash = PaymentHash(filled_bytes(idx + 2));
	let secret = lightning_types::payment::PaymentSecret(filled_bytes(idx + 3));
	payment_details_with_parts(
		payment_id(idx),
		hash,
		Some(preimage),
		Some(secret),
		Some(10_000 + idx),
		None,
		PaymentStatus::Pending,
	)
}

fn payment_details_from_update(update: PaymentDetailsUpdate) -> PaymentDetails {
	let id = update.id;
	let hash = update.hash.flatten().unwrap_or(PaymentHash(id.0));
	let preimage = update.preimage.unwrap_or(Some(PaymentPreimage(id.0)));
	let secret = update.secret.unwrap_or(Some(lightning_types::payment::PaymentSecret(id.0)));
	let amount_msat = update.amount_msat.unwrap_or(Some(10_000));
	let fee_paid_msat = update.fee_paid_msat.unwrap_or(None);
	let status = update.status.unwrap_or(PaymentStatus::Pending);
	payment_details_with_parts(id, hash, preimage, secret, amount_msat, fee_paid_msat, status)
}

fn pending_payment_details_from_update(
	update: PendingPaymentDetailsUpdate,
) -> PendingPaymentDetails {
	let id = update.id;
	let details = update
		.payment_update
		.map(payment_details_from_update)
		.unwrap_or_else(|| payment_details_from_update(PaymentDetailsUpdate::new(id)));
	let conflicting_txids = update.conflicting_txids.unwrap_or_default();
	PendingPaymentDetails::new(details, conflicting_txids)
}

fn payment_details_with_parts(
	id: PaymentId, hash: PaymentHash, preimage: Option<PaymentPreimage>,
	secret: Option<lightning_types::payment::PaymentSecret>, amount_msat: Option<u64>,
	fee_paid_msat: Option<u64>, status: PaymentStatus,
) -> PaymentDetails {
	PaymentDetails::new(
		id,
		PaymentKind::Bolt11 { hash, preimage, secret, counterparty_skimmed_fee_msat: None },
		amount_msat,
		fee_paid_msat,
		PaymentDirection::Outbound,
		status,
	)
}

fn payment_id(idx: u64) -> PaymentId {
	PaymentId(filled_bytes(idx))
}

pub fn payment_key(idx: u64) -> String {
	payment_id(idx).encode_to_hex_str()
}

fn filled_bytes(idx: u64) -> [u8; 32] {
	let mut bytes = [0u8; 32];
	bytes[..8].copy_from_slice(&idx.to_be_bytes());
	bytes[8..16].copy_from_slice(&(idx.wrapping_mul(0x9e37_79b9)).to_be_bytes());
	bytes
}
