mod common;

use common::{
	do_channel_full_cycle, expect_event, expect_payment_received_event,
	expect_payment_successful_event, generate_blocks_and_wait, open_channel,
	premine_and_distribute_funds, random_config, setup_bitcoind_and_electrsd, setup_builder,
	setup_node, setup_two_nodes, wait_for_tx, TestSyncStore,
};

use ldk_node::{Builder, Event, NodeError};

use lightning::util::persist::KVStore;

use bitcoin::{Amount, Network};

use std::sync::Arc;

#[test]
fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false);
}

#[test]
fn channel_full_cycle_0conf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, true);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, true)
}

#[test]
fn channel_open_fails_when_funds_insufficient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 100_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	println!("\nA -- connect_open_channel -> B");
	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.connect_open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			120000,
			None,
			None,
			true
		)
	);
}

#[test]
fn multi_hop_sending() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	// Setup and fund 5 nodes
	let mut nodes = Vec::new();
	for _ in 0..5 {
		let config = random_config();
		setup_builder!(builder, config);
		builder.set_esplora_server(esplora_url.clone());
		let node = builder.build().unwrap();
		node.start().unwrap();
		nodes.push(node);
	}

	let addresses = nodes.iter().map(|n| n.onchain_payment().new_address().unwrap()).collect();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		addresses,
		Amount::from_sat(premine_amount_sat),
	);

	for n in &nodes {
		n.sync_wallets().unwrap();
		assert_eq!(n.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
		assert_eq!(n.next_event(), None);
	}

	// Setup channel topology:
	//                    (1M:0)- N2 -(1M:0)
	//                   /                  \
	//  N0 -(100k:0)-> N1                    N4
	//                   \                  /
	//                    (1M:0)- N3 -(1M:0)

	open_channel(&nodes[0], &nodes[1], 100_000, true, &electrsd);
	open_channel(&nodes[1], &nodes[2], 1_000_000, true, &electrsd);
	// We need to sync wallets in-between back-to-back channel opens from the same node so BDK
	// wallet picks up on the broadcast funding tx and doesn't double-spend itself.
	//
	// TODO: Remove once fixed in BDK.
	nodes[1].sync_wallets().unwrap();
	open_channel(&nodes[1], &nodes[3], 1_000_000, true, &electrsd);
	open_channel(&nodes[2], &nodes[4], 1_000_000, true, &electrsd);
	open_channel(&nodes[3], &nodes[4], 1_000_000, true, &electrsd);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	for n in &nodes {
		n.sync_wallets().unwrap();
	}

	expect_event!(nodes[0], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[2], ChannelReady);
	expect_event!(nodes[2], ChannelReady);
	expect_event!(nodes[3], ChannelReady);
	expect_event!(nodes[3], ChannelReady);
	expect_event!(nodes[4], ChannelReady);
	expect_event!(nodes[4], ChannelReady);

	// Sleep a bit for gossip to propagate.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let invoice = nodes[4].bolt11_payment().receive(2_500_000, &"asdf", 9217).unwrap();
	nodes[0].bolt11_payment().send(&invoice).unwrap();

	let payment_hash = expect_payment_received_event!(&nodes[4], 2_500_000);
	let fee_paid_msat = Some(2000);
	expect_payment_successful_event!(nodes[0], payment_hash, fee_paid_msat);
}

#[test]
fn connect_to_public_testnet_esplora() {
	let mut config = random_config();
	config.network = Network::Testnet;
	setup_builder!(builder, config);
	builder.set_esplora_server("https://blockstream.info/testnet/api".to_string());
	let node = builder.build().unwrap();
	node.start().unwrap();
	node.stop().unwrap();
}

#[test]
fn start_stop_reinit() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let test_sync_store: Arc<dyn KVStore + Sync + Send> =
		Arc::new(TestSyncStore::new(config.storage_dir_path.clone().into()));

	setup_builder!(builder, config);
	builder.set_esplora_server(esplora_url.clone());

	let node = builder.build_with_store(Arc::clone(&test_sync_store)).unwrap();
	node.start().unwrap();

	let expected_node_id = node.node_id();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	let funding_address = node.onchain_payment().new_address().unwrap();

	assert_eq!(node.list_balances().total_onchain_balance_sats, 0);

	let expected_amount = Amount::from_sat(100000);
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![funding_address],
		expected_amount,
	);

	node.sync_wallets().unwrap();
	assert_eq!(node.list_balances().spendable_onchain_balance_sats, expected_amount.to_sat());

	let log_file_symlink = format!("{}/logs/ldk_node_latest.log", config.clone().storage_dir_path);
	assert!(std::path::Path::new(&log_file_symlink).is_symlink());

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));

	node.start().unwrap();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));
	drop(node);

	setup_builder!(builder, config);
	builder.set_esplora_server(esplora_url.clone());

	let reinitialized_node = builder.build_with_store(Arc::clone(&test_sync_store)).unwrap();
	reinitialized_node.start().unwrap();
	assert_eq!(reinitialized_node.node_id(), expected_node_id);

	assert_eq!(
		reinitialized_node.list_balances().spendable_onchain_balance_sats,
		expected_amount.to_sat()
	);

	reinitialized_node.sync_wallets().unwrap();
	assert_eq!(
		reinitialized_node.list_balances().spendable_onchain_balance_sats,
		expected_amount.to_sat()
	);

	reinitialized_node.stop().unwrap();
}

#[test]
fn onchain_spend_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b.clone()],
		Amount::from_sat(100000),
	);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, 100000);

	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.onchain_payment().send_to_address(&addr_b, 1000)
	);

	let txid = node_b.onchain_payment().send_to_address(&addr_a, 1000).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 1000);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > 98000);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < 100000);

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_a.list_balances().total_onchain_balance_sats, 0);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > 99000);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < 100000);
}

#[test]
fn sign_verify_msg() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config();
	let node = setup_node(&electrsd, config);

	// Tests arbitrary message signing and later verification
	let msg = "OK computer".as_bytes();
	let sig = node.sign_message(msg).unwrap();
	let pkey = node.node_id();
	assert!(node.verify_signature(msg, sig.as_str(), &pkey));
}

#[test]
fn connection_restart_behavior() {
	do_connection_restart_behavior(true);
	do_connection_restart_behavior(false);
}

fn do_connection_restart_behavior(persist: bool) {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false);

	let node_id_a = node_a.node_id();
	let node_id_b = node_b.node_id();

	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();

	while !node_b.status().is_listening {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	node_a.connect(node_id_b, node_addr_b, persist).unwrap();

	let peer_details_a = node_a.list_peers().first().unwrap().clone();
	assert_eq!(peer_details_a.node_id, node_id_b);
	assert_eq!(peer_details_a.is_persisted, persist);
	assert!(peer_details_a.is_connected);

	let peer_details_b = node_b.list_peers().first().unwrap().clone();
	assert_eq!(peer_details_b.node_id, node_id_a);
	assert_eq!(peer_details_b.is_persisted, false);
	assert!(peer_details_a.is_connected);

	// Restart nodes.
	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_b.start().unwrap();
	node_a.start().unwrap();

	// Sleep a bit to allow for the reconnect to happen.
	std::thread::sleep(std::time::Duration::from_secs(5));

	if persist {
		let peer_details_a = node_a.list_peers().first().unwrap().clone();
		assert_eq!(peer_details_a.node_id, node_id_b);
		assert_eq!(peer_details_a.is_persisted, persist);
		assert!(peer_details_a.is_connected);

		let peer_details_b = node_b.list_peers().first().unwrap().clone();
		assert_eq!(peer_details_b.node_id, node_id_a);
		assert_eq!(peer_details_b.is_persisted, false);
		assert!(peer_details_a.is_connected);
	} else {
		assert!(node_a.list_peers().is_empty());
		assert!(node_b.list_peers().is_empty());
	}
}

#[test]
fn concurrent_connections_succeed() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false);

	let node_a = Arc::new(node_a);
	let node_b = Arc::new(node_b);

	let node_id_b = node_b.node_id();
	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();

	while !node_b.status().is_listening {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	let mut handles = Vec::new();
	for _ in 0..10 {
		let thread_node = Arc::clone(&node_a);
		let thread_addr = node_addr_b.clone();
		let handle = std::thread::spawn(move || {
			thread_node.connect(node_id_b, thread_addr, false).unwrap();
		});
		handles.push(handle);
	}

	for h in handles {
		h.join().unwrap();
	}
}
