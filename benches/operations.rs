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
	expect_event, generate_blocks_and_wait, premine_and_distribute_funds, random_config,
	setup_bitcoind_and_electrsd, setup_node, store_bench_configs, wait_for_payment_success,
};
use criterion::{criterion_group, criterion_main, Criterion};
use electrsd::corepc_node::Node as BitcoinD;
use ldk_node::{Event, Node};
use lightning::ln::channelmanager::PaymentId;
use lightning_invoice::{Bolt11InvoiceDescription, Description};

use crate::common::{open_channel_push_amt, TestChainSource, TestStoreType};

fn operations_benchmark(c: &mut Criterion) {
	forwarding_benchmark(c);
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

fn setup_forwarding_nodes(
	chain_source: &TestChainSource, bitcoind: &BitcoinD, electrsd: &electrsd::ElectrsD,
	store_type: TestStoreType, runtime: &tokio::runtime::Runtime,
) -> Vec<Arc<Node>> {
	let mut nodes = Vec::new();
	for _ in 0..3 {
		let mut config = random_config(true);
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

fn drain_events(node: &Node) {
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}
}

criterion_group!(benches, operations_benchmark);
criterion_main!(benches);
