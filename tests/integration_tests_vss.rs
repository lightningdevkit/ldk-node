// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(vss_test)]

mod common;

use std::collections::HashMap;

use ldk_node::entropy::NodeEntropy;
use ldk_node::Builder;
use rand::RngCore;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_with_vss_store() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	println!("== Node A ==");
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let config_a = common::random_config(true);
	let mut builder_a = Builder::from_config(config_a.node_config);
	builder_a.set_chain_source_esplora(esplora_url.clone(), None);
	let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
	let node_a = builder_a
		.build_with_vss_store(
			config_a.node_entropy,
			vss_base_url.clone(),
			"".to_owned(),
			HashMap::new(),
		)
		.unwrap();
	node_a.start().unwrap();

	println!("\n== Node B ==");
	let config_b = common::random_config(true);
	let mut builder_b = Builder::from_config(config_b.node_config);
	builder_b.set_chain_source_esplora(esplora_url.clone(), None);
	let node_b = builder_b
		.build_with_vss_store(config_b.node_entropy, vss_base_url, "".to_owned(), HashMap::new())
		.unwrap();
	node_b.start().unwrap();

	common::do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		true,
		false,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn vss_node_restart() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();

	let storage_path = common::random_storage_path().to_str().unwrap().to_owned();
	let mut seed_bytes = [42u8; 64];
	rand::rng().fill_bytes(&mut seed_bytes);
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes);

	// Setup initial node and fund it.
	let (expected_balance_sats, expected_node_id) = {
		let mut builder = Builder::new();
		builder.set_network(bitcoin::Network::Regtest);
		builder.set_storage_dir_path(storage_path.clone());
		builder.set_chain_source_esplora(esplora_url.clone(), None);
		let node = builder
			.build_with_vss_store(node_entropy, vss_base_url.clone(), "".to_owned(), HashMap::new())
			.unwrap();

		node.start().unwrap();
		let addr = node.onchain_payment().new_address().unwrap();
		common::premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			bitcoin::Amount::from_sat(100_000),
		)
		.await;
		node.sync_wallets().unwrap();

		let balance = node.list_balances().spendable_onchain_balance_sats;
		assert!(balance > 0);
		let node_id = node.node_id();

		node.stop().unwrap();
		(balance, node_id)
	};

	// Verify node can be restarted from VSS backend.
	let mut builder = Builder::new();
	builder.set_network(bitcoin::Network::Regtest);
	builder.set_storage_dir_path(storage_path);
	builder.set_chain_source_esplora(esplora_url, None);

	let node = builder
		.build_with_vss_store(node_entropy, vss_base_url, "".to_owned(), HashMap::new())
		.unwrap();

	node.start().unwrap();
	node.sync_wallets().unwrap();

	assert_eq!(expected_node_id, node.node_id());
	assert_eq!(expected_balance_sats, node.list_balances().spendable_onchain_balance_sats);

	node.stop().unwrap();
}
