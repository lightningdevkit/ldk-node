// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

// The migration test exercises the filesystem, SQLite, and Postgres stores. It is gated on the
// `postgres` feature because Postgres is the only one of the three that needs an external service.
#![cfg(feature = "postgres")]

mod common;

use std::path::PathBuf;

use common::{
	drop_table, expect_channel_ready_event, expect_payment_received_event,
	expect_payment_successful_event, test_connection_string,
};
use ldk_node::entropy::NodeEntropy;
use ldk_node::io::postgres_store::PostgresStore;
use ldk_node::io::sqlite_store::{SqliteStore, KV_TABLE_NAME, SQLITE_DB_FILE_NAME};
use ldk_node::{Builder, Event};
use lightning::util::persist::migrate_kv_store_data_async;
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use lightning_persister::fs_store::v2::FilesystemStoreV2;
use rand::seq::SliceRandom;

async fn drop_tables<'a>(table_names: impl IntoIterator<Item = &'a String>) {
	for table_name in table_names {
		drop_table(table_name).await;
	}
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MigrationBackend {
	FilesystemStore,
	Sqlite,
	Postgres,
}

struct BackendInstance {
	backend: MigrationBackend,
	path: String,
	connection_string: String,
	table: String,
}

impl BackendInstance {
	fn new(
		backend: MigrationBackend, base_dir: &str, connection_string: &str, table: &str,
	) -> Self {
		let path = match backend {
			MigrationBackend::FilesystemStore => format!("{base_dir}/fs_store"),
			MigrationBackend::Sqlite => format!("{base_dir}/sqlite_store"),
			MigrationBackend::Postgres => base_dir.to_string(),
		};
		BackendInstance {
			backend,
			path,
			connection_string: connection_string.to_string(),
			table: table.to_string(),
		}
	}
}

macro_rules! with_opened_store {
	($instance:expr, |$store:ident| $body:expr) => {{
		let instance = $instance;
		match instance.backend {
			MigrationBackend::FilesystemStore => {
				let $store = open_fs_store(&instance.path);
				$body
			},
			MigrationBackend::Sqlite => {
				let $store = open_sqlite_store(&instance.path);
				$body
			},
			MigrationBackend::Postgres => {
				let $store =
					open_postgres_store(&instance.connection_string, &instance.table).await;
				$body
			},
		}
	}};
}

async fn build_migration_node(
	instance: &BackendInstance, node_config: ldk_node::config::Config, node_entropy: NodeEntropy,
	esplora_url: &str,
) -> ldk_node::Node {
	let mut builder = Builder::from_config(node_config);
	builder.set_chain_source_esplora(esplora_url.to_string(), None);
	with_opened_store!(instance, |store| builder.build_with_store(node_entropy, store).unwrap())
}

fn open_fs_store(data_dir: &str) -> FilesystemStoreV2 {
	std::fs::create_dir_all(data_dir).unwrap();
	FilesystemStoreV2::new(PathBuf::from(data_dir)).unwrap()
}

fn open_sqlite_store(data_dir: &str) -> SqliteStore {
	std::fs::create_dir_all(data_dir).unwrap();
	SqliteStore::new(
		PathBuf::from(data_dir),
		Some(SQLITE_DB_FILE_NAME.to_string()),
		Some(KV_TABLE_NAME.to_string()),
	)
	.unwrap()
}

async fn open_postgres_store(connection_string: &str, table: &str) -> PostgresStore {
	PostgresStore::new(connection_string.to_string(), None, Some(table.to_string()), None)
		.await
		.unwrap()
}

/// Migrates all data from a freshly-opened handle on the `source` backend to a freshly-opened
/// handle on the `dest` backend. The node owning the source store must be stopped beforehand.
async fn migrate_between_backends(source: &BackendInstance, dest: &BackendInstance) {
	with_opened_store!(source, |source_store| {
		with_opened_store!(dest, |dest_store| {
			migrate_kv_store_data_async(&source_store, &dest_store).await.unwrap();
		})
	})
}

/// Spins up a node on a KV store backend, creates some on-chain and Lightning transaction history,
/// then migrates its data through every other backend in turn. After each migration it restarts
/// the node on the new backend and verifies that the node identity, on-chain balance, channel, and
/// payment history are all preserved.
///
/// The order in which the backends are visited is randomized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_node_across_all_backends() {
	let mut order =
		[MigrationBackend::FilesystemStore, MigrationBackend::Sqlite, MigrationBackend::Postgres];
	order.shuffle(&mut rand::rng());
	println!("Migrating node across backends in order: {:?}", order);

	// Tables we might use: one per hop plus node B's. (Only the Postgres hops actually use them.)
	let tables: Vec<String> = (0..order.len()).map(|i| format!("migrate_chain_{i}")).collect();
	let node_b_table = "migrate_chain_node_b".to_string();
	drop_tables(tables.iter().chain(std::iter::once(&node_b_table))).await;

	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let connection_string = test_connection_string();

	// Set up node B, the Lightning counterparty.
	let config_b = common::random_config(false);
	let node_b_instance = BackendInstance::new(
		MigrationBackend::Postgres,
		&config_b.node_config.storage_dir_path,
		&connection_string,
		&node_b_table,
	);
	let node_b = build_migration_node(
		&node_b_instance,
		config_b.node_config,
		config_b.node_entropy,
		&esplora_url,
	)
	.await;
	node_b.start().unwrap();

	// Spin up the node we'll migrate on the first backend. The same node config (storage dir,
	// listening addresses, identity) is reused across every hop — only the backend changes — so
	// each backend's store lives in its own subdirectory of the one storage dir.
	let config = common::random_config(false);
	let node_entropy = config.node_entropy;
	let node_config = config.node_config;
	let base_dir = node_config.storage_dir_path.clone();

	let mut current = BackendInstance::new(order[0], &base_dir, &connection_string, &tables[0]);
	let mut node =
		build_migration_node(&current, node_config.clone(), node_entropy, &esplora_url).await;
	node.start().unwrap();
	let expected_node_id = node.node_id();

	// On-chain receive: fund the node.
	let addr = node.onchain_payment().new_address().unwrap();
	common::premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		bitcoin::Amount::from_sat(1_000_000),
	)
	.await;
	node.sync_wallets().unwrap();

	// Open a channel to node B (pushing half so both sides can route) and let it confirm.
	common::open_channel_push_amt(&node, &node_b, 200_000, Some(100_000_000), false, &electrsd)
		.await;
	common::generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node, node_b.node_id());
	expect_channel_ready_event!(node_b, node.node_id());

	// Lightning send: node -> node B.
	let description =
		Bolt11InvoiceDescription::Direct(Description::new("ln send".to_string()).unwrap());
	let invoice = node_b.bolt11_payment().receive(10_000, &description.into(), 3600).unwrap();
	let ln_send_id = node.bolt11_payment().send(&invoice, None).unwrap();
	expect_payment_successful_event!(node, Some(ln_send_id), None);
	expect_payment_received_event!(node_b, 10_000);

	// Lightning receive: node B -> node.
	let description =
		Bolt11InvoiceDescription::Direct(Description::new("ln receive".to_string()).unwrap());
	let invoice = node.bolt11_payment().receive(5_000, &description.into(), 3600).unwrap();
	let ln_receive_id = node_b.bolt11_payment().send(&invoice, None).unwrap();
	expect_payment_successful_event!(node_b, Some(ln_receive_id), None);
	expect_payment_received_event!(node, 5_000);

	// On-chain send: node -> a foreign address.
	let bitcoind_addr = bitcoind.client.new_address().unwrap();
	let txid = node.onchain_payment().send_to_address(&bitcoind_addr, 50_000, None).unwrap();
	common::wait_for_tx(&electrsd.client, txid).await;
	common::generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();

	// Capture the state we expect to survive every migration.
	let expected_balance_sats = node.list_balances().total_onchain_balance_sats;
	let expected_ln_balance_sats = node.list_balances().total_lightning_balance_sats;
	let mut expected_payments = node.list_payments();
	expected_payments.sort_by_key(|p| p.id.0);
	assert!(expected_payments.len() >= 4);

	for (i, &next_backend) in order.iter().enumerate().skip(1) {
		println!("Migrating from {:?} to {:?}", current.backend, next_backend);

		let next = BackendInstance::new(next_backend, &base_dir, &connection_string, &tables[i]);

		// Spin the node down so the source store is no longer being written to.
		node.stop().unwrap();
		drop(node);

		migrate_between_backends(&current, &next).await;

		// Spin the node back up on the new backend.
		node = build_migration_node(&next, node_config.clone(), node_entropy, &esplora_url).await;
		node.start().unwrap();
		node.sync_wallets().unwrap();

		// The balance, channel, and transaction history are preserved across the migration.
		assert_eq!(node.node_id(), expected_node_id);
		assert_eq!(node.list_balances().total_onchain_balance_sats, expected_balance_sats);
		assert_eq!(node.list_balances().total_lightning_balance_sats, expected_ln_balance_sats);
		assert_eq!(node.list_channels().len(), 1);
		let mut migrated_payments = node.list_payments();
		migrated_payments.sort_by_key(|p| p.id.0);
		assert_eq!(migrated_payments, expected_payments);

		current = next;
	}

	node.stop().unwrap();
	node_b.stop().unwrap();

	drop_tables(tables.iter().chain(std::iter::once(&node_b_table))).await;
}
