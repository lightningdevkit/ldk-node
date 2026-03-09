// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use bitcoin::{Amount, Network};
use common::{
	expect_channel_pending_event, expect_channel_ready_event, expect_event,
	expect_payment_received_event, expect_payment_successful_event, generate_blocks_and_wait,
	premine_and_distribute_funds, random_storage_path, setup_bitcoind_and_electrsd,
	setup_builder, setup_two_nodes, wait_for_tx, TestChainSource,
};
use ldk_node::config::Config;
use ldk_node::entropy::{generate_entropy_mnemonic, NodeEntropy};
use ldk_node::payment::{PaymentKind, PaymentStatus};
use ldk_node::{Builder, Event};
use lightning_invoice::{Bolt11InvoiceDescription, Description};

/// Smoke test: build and start an ldk-node instance backed by BIP157 compact block filters.
///
/// Uses signet with an empty peer list so kyoto will bootstrap via DNS seeds.
/// This test is expected to fail until `ChainSource::new_kyoto` is fully implemented.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_start_node_with_bip157_backend() {
	let mut config = Config::default();
	config.network = Network::Signet;
	config.storage_dir_path = random_storage_path().to_str().unwrap().to_owned();

	setup_builder!(builder, config);

	// Empty peer list — kyoto will use DNS seeds for signet peer discovery.
	builder.set_chain_source_bip157(vec![]);

	let mnemonic = generate_entropy_mnemonic(None);
	let entropy = NodeEntropy::from_bip39_mnemonic(mnemonic, None);

	let node = builder.build_with_fs_store(entropy.into()).expect("Node build should succeed");
	node.start().expect("Node start should succeed");

	assert!(node.status().is_running);

	node.stop().unwrap();
}

/// Full channel lifecycle test for the BIP-157 chain backend.
///
/// Spins up a local regtest bitcoind with compact block filters enabled, connects two
/// ldk-node instances to it via kyoto, funds them, and drives a complete channel open →
/// payment → close cycle.
///
/// The test is event-driven (no `sync_wallets()` calls): wallet and LDK state are updated
/// by the kyoto background event loop as blocks arrive.
///
/// Expected failures until the kyoto event handlers are implemented:
///   - `ChannelReady` will not fire because `ChainUpdate` in `continuously_sync_wallets`
///     still has a `todo!()` — LDK never receives block confirmations.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_bip157() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Bip157(&bitcoind);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle_bip157(node_a, node_b, &bitcoind.client, &electrsd.client).await;
}

/// Like `do_channel_full_cycle` but without `sync_wallets()` calls.
///
/// For kyoto the wallet is updated by the background event loop so there is no need (and
/// no meaningful way) to manually trigger a sync.  The test paces itself via LDK events
/// (`ChannelPending`, `ChannelReady`, `PaymentSuccessful`, …) instead.
async fn do_channel_full_cycle_bip157<E: electrum_client::ElectrumApi>(
	node_a: ldk_node::Node, node_b: ldk_node::Node, bitcoind: &electrsd::corepc_node::Client,
	electrsd: &E,
) {
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	// Fund both nodes.  Kyoto will pick up the confirmed outputs once the block that
	// contains the funding txs is processed by the background event loop.
	premine_and_distribute_funds(
		bitcoind,
		electrsd,
		vec![addr_a, addr_b],
		Amount::from_sat(2_100_000),
	)
	.await;

	// Open an announced channel from A → B, balanced 50/50.
	println!("\nA -- open_channel -> B");
	let funding_amount_sat = 2_080_000;
	let push_msat = (funding_amount_sat / 2) * 1000;
	node_a
		.open_announced_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			funding_amount_sat,
			Some(push_msat),
			None,
		)
		.unwrap();

	// Both nodes emit ChannelPending as soon as the funding tx is broadcast (no chain
	// confirmation required).
	let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
	let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
	assert_eq!(funding_txo_a, funding_txo_b);
	wait_for_tx(electrsd, funding_txo_a.txid).await;

	// Mine 6 blocks to confirm the channel.  Kyoto will deliver ChainUpdate events to the
	// background loop which notifies LDK via the Listen trait, triggering ChannelReady.
	generate_blocks_and_wait(bitcoind, electrsd, 6).await;

	let user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Send a payment B → A.
	println!("\nA receive");
	let invoice_amount_msat = 2_500_000;
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("test")).unwrap());
	let invoice = node_a
		.bolt11_payment()
		.receive(invoice_amount_msat, &invoice_description.into(), 9217)
		.unwrap();

	println!("\nB send");
	let payment_id = node_b.bolt11_payment().send(&invoice, None).unwrap();

	expect_payment_successful_event!(node_b, Some(payment_id), None);
	expect_payment_received_event!(node_a, invoice_amount_msat);
	assert_eq!(node_b.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);

	// Cooperative close.
	println!("\nA -- close_channel -> B");
	node_a.close_channel(&user_channel_id_a, node_b.node_id()).unwrap();
	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}
