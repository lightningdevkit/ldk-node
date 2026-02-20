#[path = "../tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::hex::DisplayHex;
use bitcoin::Amount;
use common::{
	expect_channel_ready_event, generate_blocks_and_wait, premine_and_distribute_funds,
	random_chain_source, setup_bitcoind_and_electrsd, setup_two_nodes_with_store,
};
use criterion::{criterion_group, criterion_main, Criterion};
use ldk_node::{Event, Node};
use lightning_types::payment::{PaymentHash, PaymentPreimage};
use rand::RngCore;
use tokio::task::{self};

use crate::common::open_channel_push_amt;

fn spawn_payment(node_a: Arc<Node>, node_b: Arc<Node>, amount_msat: u64) {
	let mut preimage_bytes = [0u8; 32];
	rand::rng().fill_bytes(&mut preimage_bytes);
	let preimage = PaymentPreimage(preimage_bytes);
	let payment_hash: PaymentHash = preimage.into();

	// Spawn each payment as a separate async task
	task::spawn(async move {
		println!("{}: Starting payment", payment_hash.0.as_hex());

		loop {
			// Pre-check the HTLC slots to try to avoid the performance impact of a failed payment.
			while node_a.list_channels()[0].next_outbound_htlc_limit_msat == 0 {
				println!("{}: Waiting for HTLC slots to free up", payment_hash.0.as_hex());
				tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			}

			let payment_id = node_a.spontaneous_payment().send_with_preimage(
				amount_msat,
				node_b.node_id(),
				preimage,
				None,
			);

			match payment_id {
				Ok(payment_id) => {
					println!(
						"{}: Awaiting payment with id {}",
						payment_hash.0.as_hex(),
						payment_id
					);
					break;
				},
				Err(e) => {
					println!("{}: Payment attempt failed: {:?}", payment_hash.0.as_hex(), e);

					tokio::time::sleep(std::time::Duration::from_millis(100)).await;
				},
			}
		}
	});
}

async fn send_payments(node_a: Arc<Node>, node_b: Arc<Node>) -> std::time::Duration {
	let start = Instant::now();

	let total_payments = 1000;
	let amount_msat = 10_000_000;

	let mut success_count = 0;
	for _ in 0..total_payments {
		spawn_payment(node_a.clone(), node_b.clone(), amount_msat);
	}

	while success_count < total_payments {
		match node_a.next_event_async().await {
			Event::PaymentSuccessful { payment_id, payment_hash, .. } => {
				if let Some(id) = payment_id {
					success_count += 1;
					println!("{}: Payment with id {:?} completed", payment_hash.0.as_hex(), id);
				} else {
					println!("Payment completed (no payment_id)");
				}
			},
			Event::PaymentFailed { payment_id, payment_hash, .. } => {
				println!("{}: Payment {:?} failed", payment_hash.unwrap().0.as_hex(), payment_id);

				// The payment failed, so we need to respawn it.
				spawn_payment(node_a.clone(), node_b.clone(), amount_msat);
			},
			ref e => {
				println!("Received non-payment event: {:?}", e);
			},
		}

		node_a.event_handled().unwrap();
	}

	let duration = start.elapsed();
	println!("Time elapsed: {:?}", duration);

	// Send back the money for the next iteration.
	let mut preimage_bytes = [0u8; 32];
	rand::rng().fill_bytes(&mut preimage_bytes);
	node_b
		.spontaneous_payment()
		.send_with_preimage(
			amount_msat * total_payments,
			node_a.node_id(),
			PaymentPreimage(preimage_bytes),
			None,
		)
		.ok()
		.unwrap();

	duration
}

fn payment_benchmark(c: &mut Criterion) {
	// Set up two nodes. Because this is slow, we reuse the same nodes for each sample.
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let (node_a, node_b) = setup_two_nodes_with_store(
		&chain_source,
		false,
		true,
		false,
		common::TestStoreType::Sqlite,
		common::TestStoreType::Sqlite,
	);

	let runtime =
		tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();

	let node_a = Arc::new(node_a);
	let node_b = Arc::new(node_b);

	// Fund the nodes and setup a channel between them. The criterion function cannot be async, so we need to execute
	// the setup using a runtime.
	let node_a_cloned = Arc::clone(&node_a);
	let node_b_cloned = Arc::clone(&node_b);
	runtime.block_on(async move {
		let address_a = node_a_cloned.onchain_payment().new_address().unwrap();
		let premine_sat = 25_000_000;
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![address_a],
			Amount::from_sat(premine_sat),
		)
		.await;
		node_a_cloned.sync_wallets().unwrap();
		node_b_cloned.sync_wallets().unwrap();
		open_channel_push_amt(
			&node_a_cloned,
			&node_b_cloned,
			16_000_000,
			Some(1_000_000_000),
			false,
			&electrsd,
		)
		.await;
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node_a_cloned.sync_wallets().unwrap();
		node_b_cloned.sync_wallets().unwrap();
		expect_channel_ready_event!(node_a_cloned, node_b_cloned.node_id());
		expect_channel_ready_event!(node_b_cloned, node_a_cloned.node_id());
	});

	let mut group = c.benchmark_group("payments");
	group.sample_size(10);

	group.bench_function("payments", |b| {
		// Use custom timing so that sending back the money at the end of each iteration isn't included in the
		// measurement.
		b.to_async(&runtime).iter_custom(|iter| {
			let node_a = Arc::clone(&node_a);
			let node_b = Arc::clone(&node_b);

			async move {
				let mut total = Duration::ZERO;
				for _i in 0..iter {
					let node_a = Arc::clone(&node_a);
					let node_b = Arc::clone(&node_b);

					total += send_payments(node_a, node_b).await;
				}
				total
			}
		});
	});
}

criterion_group!(benches, payment_benchmark);
criterion_main!(benches);
