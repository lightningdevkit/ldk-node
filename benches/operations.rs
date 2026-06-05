// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#[path = "../tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::Amount;
use common::{
	expect_channel_pending_event, expect_channel_ready_event, expect_event,
	generate_blocks_and_wait, open_channel_no_wait, premine_and_distribute_funds, random_config,
	random_storage_path, setup_bitcoind_and_electrsd, setup_node, setup_two_nodes_with_store,
	store_bench_configs, wait_for_channel_ready_events, wait_for_payment_success,
};
use criterion::{criterion_group, criterion_main, Criterion};
use electrsd::corepc_node::{Client as BitcoindClient, Node as BitcoinD};
use ldk_node::io::sqlite_store::{SqliteStore, KV_TABLE_NAME, SQLITE_DB_FILE_NAME};
use ldk_node::{Event, Node};
use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::migrate_kv_store_data_async;
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use lightning_persister::fs_store::v2::FilesystemStoreV2;

use crate::common::{
	open_channel_push_amt, StoreBenchConfig, TestChainSource, TestConfig, TestStoreType,
};

#[cfg(feature = "postgres")]
use ldk_node::io::postgres_store::{PostgresStore, POSTGRES_TEST_URL_ENV_VAR};

const STARTUP_SEED_SCENARIOS: [StartupSeedScenario; 6] = [
	StartupSeedScenario { channel_count: 1, payment_count: 2 },
	StartupSeedScenario { channel_count: 1, payment_count: 100 },
	StartupSeedScenario { channel_count: 1, payment_count: 1_000 },
	StartupSeedScenario { channel_count: 10, payment_count: 2 },
	StartupSeedScenario { channel_count: 100, payment_count: 2 },
	StartupSeedScenario { channel_count: 100, payment_count: 1_000 },
];
const STARTUP_SEED_PAYMENT_AMOUNT_MSAT: u64 = 1_000_000;
const STARTUP_SEED_MIN_CHANNEL_FUNDING_SAT: u64 = 100_000;
const STARTUP_SEED_CHANNEL_BUFFER_SAT: u64 = 1_000_000;
const STARTUP_SEED_CHANNEL_BATCH_SIZE: u64 = 2;

#[derive(Clone, Copy)]
struct StartupSeedScenario {
	channel_count: u64,
	payment_count: u64,
}

impl StartupSeedScenario {
	fn bench_name(self, store_name: &str) -> String {
		format!("{}/channels_{}_payments_{}", store_name, self.channel_count, self.payment_count)
	}

	fn runs_in_ci(self) -> bool {
		self.channel_count == 1 && self.payment_count == 2
	}

	fn channel_funding_sat(self) -> u64 {
		let payment_amount_sat = STARTUP_SEED_PAYMENT_AMOUNT_MSAT / 1_000;
		let payment_funding_sat =
			self.payment_count * payment_amount_sat + STARTUP_SEED_CHANNEL_BUFFER_SAT;
		payment_funding_sat.max(STARTUP_SEED_MIN_CHANNEL_FUNDING_SAT)
	}

	fn premine_amount_sat(self) -> u64 {
		self.channel_count * self.channel_funding_sat() + STARTUP_SEED_CHANNEL_BUFFER_SAT
	}
}

fn operations_benchmark(c: &mut Criterion) {
	forwarding_benchmark(c);
	channel_open_benchmark(c);
	startup_benchmark(c);
}

fn forwarding_benchmark(c: &mut Criterion) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);
	let runtime = benchmark_runtime();

	let mut group = c.benchmark_group("forwarding");
	group.sample_size(10);

	for store_config in store_bench_configs() {
		if !should_register_bench("forwarding", store_config.name) {
			continue;
		}
		let nodes = setup_forwarding_nodes(
			&chain_source,
			&bitcoind,
			&electrsd,
			store_config.store_type,
			&runtime,
		);
		let nodes = Arc::new(nodes);

		group.bench_function(store_config.name, |b| {
			b.to_async(&runtime).iter_custom(|iter| {
				let nodes = Arc::clone(&nodes);

				async move {
					let mut total = Duration::ZERO;
					for _ in 0..iter {
						total += send_forwarded_payments(Arc::clone(&nodes)).await;
					}
					total
				}
			});
		});
	}
}

fn benchmark_runtime() -> tokio::runtime::Runtime {
	let mut builder = tokio::runtime::Builder::new_multi_thread();
	builder.worker_threads(4).enable_all();
	#[cfg(tokio_unstable)]
	builder.enable_eager_driver_handoff();
	builder.build().unwrap()
}

fn channel_open_benchmark(c: &mut Criterion) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);
	let runtime = benchmark_runtime();

	let mut group = c.benchmark_group("channel_open");
	group.sample_size(10);

	for store_config in store_bench_configs() {
		if !should_register_bench("channel_open", store_config.name) {
			continue;
		}
		let (node_a, node_b) =
			setup_two_nodes_with_store(&chain_source, false, false, store_config.store_type);
		let node_a = Arc::new(node_a);
		let node_b = Arc::new(node_b);

		// connect nodes
		node_a
			.connect(
				node_b.node_id(),
				node_b.listening_addresses().unwrap().first().unwrap().clone(),
				true,
			)
			.unwrap();

		runtime.block_on(async {
			let address_a = node_a.onchain_payment().new_address().unwrap();
			premine_and_distribute_funds(
				&bitcoind.client,
				&electrsd.client,
				vec![address_a],
				Amount::from_sat(35_000_000),
			)
			.await;
			node_a.sync_wallets().unwrap();
		});

		let node_a = Arc::clone(&node_a);
		let node_b = Arc::clone(&node_b);
		let bitcoind_client = &bitcoind.client;
		let electrsd_ref = &electrsd;

		group.bench_function(store_config.name, |b| {
			b.to_async(&runtime).iter_custom(|iter| {
				let node_a = Arc::clone(&node_a);
				let node_b = Arc::clone(&node_b);

				async move {
					let mut total = Duration::ZERO;
					for _ in 0..iter {
						total += open_channel(
							Arc::clone(&node_a),
							Arc::clone(&node_b),
							bitcoind_client,
							electrsd_ref,
						)
						.await;
					}
					total
				}
			});
		});
	}
}

fn startup_benchmark(c: &mut Criterion) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);
	let runtime = benchmark_runtime();

	let mut group = c.benchmark_group("startup");
	group.sample_size(10);

	for startup_seed_scenario in STARTUP_SEED_SCENARIOS {
		// Larger seeded startup scenarios are useful locally, but take too long to run in CI.
		if is_ci() && !startup_seed_scenario.runs_in_ci() {
			continue;
		}

		let matching_store_configs: Vec<_> = store_bench_configs()
			.into_iter()
			.filter(|store_config| {
				let bench_name = startup_seed_scenario.bench_name(store_config.name);
				should_register_bench("startup", &bench_name)
			})
			.collect();
		if matching_store_configs.is_empty() {
			continue;
		}

		// Seed a canonical sqlite node once, then copy its store into each backend under test. This
		// keeps the channel/payment history identical across stores while avoiding repeated expensive
		// channel and payment setup for every store backend.
		let seeded_config = setup_startup_seed_node(
			&chain_source,
			&bitcoind,
			&electrsd,
			startup_seed_scenario,
			&runtime,
		);
		let startup_configs = migrate_startup_seed_configs(
			&seeded_config,
			startup_seed_scenario,
			matching_store_configs,
			&runtime,
		);

		for (bench_name, config) in startup_configs {
			group.bench_function(bench_name, |b| {
				b.iter_custom(|iter| {
					let mut total = Duration::ZERO;
					for _ in 0..iter {
						let start = Instant::now();
						let node = setup_node(&chain_source, config.clone());
						total += start.elapsed();
						node.stop().unwrap();
					}
					total
				});
			});
		}
	}
}

/// Builds a canonical sqlite node store with the requested channel and payment history.
///
/// Startup benchmarks use this store as the source fixture for every backend so differences in
/// measured startup time come from loading equivalent persisted state, not from different setup
/// runs.
fn setup_startup_seed_node(
	chain_source: &TestChainSource, bitcoind: &BitcoinD, electrsd: &electrsd::ElectrsD,
	seed_scenario: StartupSeedScenario, runtime: &tokio::runtime::Runtime,
) -> TestConfig {
	let mut config_a = random_config();
	config_a.store_type = TestStoreType::Sqlite;
	let node_a = Arc::new(setup_node(chain_source, config_a.clone()));

	let mut config_b = random_config();
	config_b.store_type = TestStoreType::Sqlite;
	let node_b = Arc::new(setup_node(chain_source, config_b));

	runtime.block_on(async {
		let address_a = node_a.onchain_payment().new_address().unwrap();
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![address_a],
			Amount::from_sat(seed_scenario.premine_amount_sat()),
		)
		.await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();

		let funding_amount_sat = seed_scenario.channel_funding_sat();
		let mut remaining_channel_count = seed_scenario.channel_count;
		while remaining_channel_count > 0 {
			let channel_batch_size = remaining_channel_count.min(STARTUP_SEED_CHANNEL_BATCH_SIZE);
			for _ in 0..channel_batch_size {
				node_a
					.open_channel(
						node_b.node_id(),
						node_b.listening_addresses().unwrap().first().unwrap().clone(),
						funding_amount_sat,
						None,
						None,
					)
					.unwrap();
				assert!(node_a.list_peers().iter().any(|peer| peer.node_id == node_b.node_id()));

				let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
				let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
				assert_eq!(funding_txo_a, funding_txo_b);
				node_a.sync_wallets().unwrap();
			}
			generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

			for _ in 0..channel_batch_size {
				node_a.sync_wallets().unwrap();
				node_b.sync_wallets().unwrap();
				wait_for_channel_ready_events(&node_a, node_b.node_id(), 1).await;
				wait_for_channel_ready_events(&node_b, node_a.node_id(), 1).await;
			}
			remaining_channel_count -= channel_batch_size;
		}

		for idx in 0..seed_scenario.payment_count {
			let invoice_description = Bolt11InvoiceDescription::Direct(
				Description::new(format!("startup seed {}", idx + 1)).unwrap(),
			);
			let invoice = node_b
				.bolt11_payment()
				.receive(STARTUP_SEED_PAYMENT_AMOUNT_MSAT, &invoice_description.into(), 9217)
				.unwrap();
			let payment_id = node_a.bolt11_payment().send(&invoice, None).unwrap();
			wait_for_payment_success(&node_a, payment_id).await;
		}

		drain_events(&node_a);
		drain_events(&node_b);
	});

	node_a.stop().unwrap();
	node_b.stop().unwrap();

	config_a
}

/// Produces benchmark configs backed by copies of the canonical seeded store.
///
/// Sqlite can reuse the source store directly. Other store backends get a fresh storage path and a
/// migrated copy of the same key-value data.
fn migrate_startup_seed_configs(
	source_config: &TestConfig, seed_scenario: StartupSeedScenario,
	store_configs: Vec<StoreBenchConfig>, runtime: &tokio::runtime::Runtime,
) -> Vec<(String, TestConfig)> {
	// Open the seeded source store with the same db file and table the node itself uses, otherwise
	// we'd read from an empty default-named store and migrate nothing into the other backends.
	let source_store = SqliteStore::new(
		source_config.node_config.storage_dir_path.clone().into(),
		Some(SQLITE_DB_FILE_NAME.to_string()),
		Some(KV_TABLE_NAME.to_string()),
	)
	.unwrap();

	store_configs
		.into_iter()
		.map(|store_config| {
			let mut config = source_config.clone();
			config.store_type = store_config.store_type;
			if !matches!(store_config.store_type, TestStoreType::Sqlite) {
				config.node_config.storage_dir_path =
					random_storage_path().to_str().unwrap().to_owned();
				migrate_startup_seed_store(&source_store, &config, runtime);
			}

			(seed_scenario.bench_name(store_config.name), config)
		})
		.collect()
}

fn migrate_startup_seed_store(
	source_store: &SqliteStore, destination_config: &TestConfig, runtime: &tokio::runtime::Runtime,
) {
	runtime.block_on(async {
		match destination_config.store_type {
			TestStoreType::Sqlite => {},
			TestStoreType::FilesystemStore => {
				let destination_store_path = std::path::PathBuf::from(
					destination_config.node_config.storage_dir_path.clone(),
				)
				.join("fs_store");
				let destination_store = FilesystemStoreV2::new(destination_store_path).unwrap();
				migrate_kv_store_data_async(source_store, &destination_store).await.unwrap();
			},
			#[cfg(feature = "postgres")]
			TestStoreType::Postgres => {
				let connection_string = postgres_connection_string();
				let table_name = postgres_table_name(destination_config);
				let destination_store =
					PostgresStore::new(connection_string, None, Some(table_name), None)
						.await
						.unwrap();
				migrate_kv_store_data_async(source_store, &destination_store).await.unwrap();
			},
			TestStoreType::TestSyncStore => {
				unreachable!("startup benches do not use TestSyncStore")
			},
		}
	});
}

#[cfg(feature = "postgres")]
fn postgres_connection_string() -> String {
	std::env::var(POSTGRES_TEST_URL_ENV_VAR)
		.unwrap_or_else(|_| "host=localhost user=postgres password=postgres".to_string())
}

#[cfg(feature = "postgres")]
fn postgres_table_name(config: &TestConfig) -> String {
	format!(
		"test_{}",
		config
			.node_config
			.storage_dir_path
			.chars()
			.filter(|c| c.is_ascii_alphanumeric())
			.collect::<String>()
	)
}

/// Returns whether the benchmark identified by `group/name` matches the CLI filters.
///
/// Criterion applies its own filters after benchmark registration, but these benches do expensive
/// setup before registration. Pre-filtering here avoids setting up benchmark cases that cannot run.
/// Only non-flag arguments are considered filters, matching either the full target substring or the
/// group name.
fn should_register_bench(group: &str, name: &str) -> bool {
	let target = format!("{}/{}", group, name);
	let filters: Vec<String> =
		std::env::args().skip(1).filter(|arg| !arg.starts_with('-')).collect();
	filters.is_empty()
		|| filters.iter().any(|filter| {
			target.contains(filter) || (filter == group && target.starts_with(&format!("{group}/")))
		})
}

fn is_ci() -> bool {
	std::env::var("CI").is_ok_and(|value| !value.is_empty() && value != "0" && value != "false")
}

fn setup_forwarding_nodes(
	chain_source: &TestChainSource, bitcoind: &BitcoinD, electrsd: &electrsd::ElectrsD,
	store_type: TestStoreType, runtime: &tokio::runtime::Runtime,
) -> Vec<Arc<Node>> {
	let mut nodes = Vec::new();
	for _ in 0..3 {
		let mut config = random_config();
		config.store_type = store_type;
		nodes.push(Arc::new(setup_node(chain_source, config)));
	}

	runtime.block_on(async {
		let addresses =
			nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect();
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			addresses,
			Amount::from_sat(5_000_000),
		)
		.await;
		for node in &nodes {
			node.sync_wallets().unwrap();
		}

		let funding_amount_sat = 1_000_000;
		let push_amount_msat = Some(funding_amount_sat * 1_000 / 2);
		open_channel_push_amt(
			&nodes[0],
			&nodes[1],
			funding_amount_sat,
			push_amount_msat,
			true,
			electrsd,
		)
		.await;
		open_channel_push_amt(
			&nodes[1],
			&nodes[2],
			funding_amount_sat,
			push_amount_msat,
			true,
			electrsd,
		)
		.await;

		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		for node in &nodes {
			node.sync_wallets().unwrap();
		}

		expect_event!(nodes[0], ChannelReady);
		expect_event!(nodes[1], ChannelReady);
		expect_event!(nodes[1], ChannelReady);
		expect_event!(nodes[2], ChannelReady);

		tokio::time::sleep(Duration::from_secs(1)).await;
		wait_for_forwarding_path(&nodes).await;
	});

	nodes
}

async fn send_forwarded_payments(nodes: Arc<Vec<Arc<Node>>>) -> Duration {
	let total_payments = 25;
	let amount_msat = 5_000;

	let mut total = Duration::ZERO;

	for _ in 0..total_payments {
		let invoice_description =
			Bolt11InvoiceDescription::Direct(Description::new("forwarding".to_string()).unwrap());
		let invoice = nodes[2]
			.bolt11_payment()
			.receive(amount_msat, &invoice_description.into(), 9217)
			.unwrap();

		let start = Instant::now();
		let payment_id = nodes[0].bolt11_payment().send(&invoice, None).unwrap();
		total += wait_for_forwarded_payment(&nodes, payment_id, start).await;
	}

	// return funds and clean up for next run
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new("return".to_string()).unwrap());
	let invoice = nodes[0]
		.bolt11_payment()
		.receive(amount_msat * total_payments, &invoice_description.into(), 9217)
		.unwrap();
	let return_payment_id = nodes[2].bolt11_payment().send(&invoice, None).unwrap();
	wait_for_payment_success(&nodes[2], return_payment_id).await;
	tokio::time::sleep(Duration::from_millis(10)).await;
	for node in nodes.iter() {
		drain_events(node);
	}

	total
}

async fn wait_for_forwarded_payment(
	nodes: &[Arc<Node>], expected_payment_id: PaymentId, start: Instant,
) -> Duration {
	let mut payment_successful = false;
	let mut payment_forwarded = false;

	while !payment_successful || !payment_forwarded {
		tokio::select! {
			event = nodes[0].next_event_async(), if !payment_successful => {
				match event {
					Event::PaymentSuccessful { payment_id: Some(payment_id), .. }
						if payment_id == expected_payment_id =>
					{
						payment_successful = true;
					},
					Event::PaymentFailed { payment_id, payment_hash, .. } => {
						nodes[0].event_handled().unwrap();
						panic!("Forwarded payment {payment_id:?} failed with hash {payment_hash:?}");
					},
					_ => {},
				}
				nodes[0].event_handled().unwrap();
			},
			event = nodes[1].next_event_async(), if !payment_forwarded => {
				if matches!(event, Event::PaymentForwarded { .. }) {
					payment_forwarded = true;
				}
				nodes[1].event_handled().unwrap();
			},
		}
	}

	start.elapsed()
}

/// Sends a payment across the benchmark path before measurements start.
///
/// Channel readiness events alone do not guarantee that the sender can immediately find and use the
/// intended multi-hop path. Waiting for one successful payment keeps route-discovery first-use cost
/// and transient graph propagation failures out of the timed forwarding loop.
async fn wait_for_forwarding_path(nodes: &[Arc<Node>]) {
	for _ in 0..30 {
		let invoice_description =
			Bolt11InvoiceDescription::Direct(Description::new("".to_string()).unwrap());
		let invoice =
			nodes[2].bolt11_payment().receive(5_000, &invoice_description.into(), 9217).unwrap();
		if let Ok(payment_id) = nodes[0].bolt11_payment().send(&invoice, None) {
			wait_for_payment_success(&nodes[0], payment_id).await;
			tokio::time::sleep(Duration::from_millis(50)).await;
			for node in nodes {
				drain_events(node);
			}
			return;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}

	panic!("Timed out waiting for forwarding path readiness");
}

async fn open_channel(
	node_a: Arc<Node>, node_b: Arc<Node>, bitcoind: &BitcoindClient, electrsd: &electrsd::ElectrsD,
) -> Duration {
	let start = Instant::now();
	let funding_txo = open_channel_no_wait(&node_a, &node_b, 100_000, None, false).await;
	let duration = start.elapsed();

	common::wait_for_tx(&electrsd.client, funding_txo.txid).await;

	generate_blocks_and_wait(bitcoind, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_b, node_a.node_id());
	expect_channel_ready_event!(node_a, node_b.node_id());

	duration
}

fn drain_events(node: &Node) {
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}
}

criterion_group!(benches, operations_benchmark);
criterion_main!(benches);
