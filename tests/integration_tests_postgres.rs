// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(feature = "storage-postgres")]

mod common;

use common::{configure_chain_source, drop_table, random_chain_source, test_connection_string};
use ldk_node::Builder;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_full_cycle_with_postgres_store() {
	drop_table("channel_cycle_a").await;
	drop_table("channel_cycle_b").await;

	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	println!("== Node A ==");
	let config_a = common::random_config();
	let mut builder_a = Builder::from_config(config_a.node_config.clone());
	configure_chain_source(&chain_source, &mut builder_a, &config_a);
	let node_a = builder_a
		.build_with_postgres_store(
			config_a.node_entropy.into(),
			test_connection_string(),
			None,
			Some("channel_cycle_a".to_string()),
			None,
		)
		.unwrap();
	node_a.start().unwrap();

	println!("\n== Node B ==");
	let mut config_b = common::random_config();
	config_b.node_config.manually_handle_unknown_bolt11_payments = true;
	let mut builder_b = Builder::from_config(config_b.node_config.clone());
	configure_chain_source(&chain_source, &mut builder_b, &config_b);
	let node_b = builder_b
		.build_with_postgres_store(
			config_b.node_entropy.into(),
			test_connection_string(),
			None,
			Some("channel_cycle_b".to_string()),
			None,
		)
		.unwrap();
	node_b.start().unwrap();

	common::do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		true,
		true,
		false,
	)
	.await;

	drop_table("channel_cycle_a").await;
	drop_table("channel_cycle_b").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_node_restart() {
	drop_table("restart_test").await;

	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let connection_string = test_connection_string();

	let storage_path = common::random_storage_path().to_str().unwrap().to_owned();
	let mut config = common::random_config();
	config.node_config.storage_dir_path = storage_path;
	let node_entropy = config.node_entropy;

	// Setup initial node and fund it.
	let (expected_balance_sats, expected_node_id) = {
		let mut builder = Builder::from_config(config.node_config.clone());
		configure_chain_source(&chain_source, &mut builder, &config);
		let node = builder
			.build_with_postgres_store(
				node_entropy.into(),
				connection_string.clone(),
				None,
				Some("restart_test".to_string()),
				None,
			)
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

	// Verify node can be restarted from PostgreSQL backend.
	let mut builder = Builder::from_config(config.node_config.clone());
	configure_chain_source(&chain_source, &mut builder, &config);

	let node = builder
		.build_with_postgres_store(
			node_entropy.into(),
			connection_string.clone(),
			None,
			Some("restart_test".to_string()),
			None,
		)
		.unwrap();

	node.start().unwrap();
	node.sync_wallets().unwrap();

	assert_eq!(expected_node_id, node.node_id());
	assert_eq!(expected_balance_sats, node.list_balances().spendable_onchain_balance_sats);

	node.stop().unwrap();

	drop_table("restart_test").await;
}
