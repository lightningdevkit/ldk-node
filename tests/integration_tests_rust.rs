// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use common::{
	do_channel_full_cycle, expect_channel_ready_event, expect_event, expect_payment_received_event,
	expect_payment_successful_event, generate_blocks_and_wait, open_channel,
	premine_and_distribute_funds, random_config, setup_bitcoind_and_electrsd, setup_builder,
	setup_node, setup_two_nodes, wait_for_tx, TestChainSource, TestSyncStore,
};

use ldk_node::config::EsploraSyncConfig;
use ldk_node::payment::{
	ConfirmationStatus, PaymentDirection, PaymentKind, PaymentStatus, QrPaymentResult,
	SendingParameters,
};
use ldk_node::{Builder, Event, NodeError};

use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::KVStore;

use bitcoincore_rpc::RpcApi;

use bitcoin::hashes::Hash;
use bitcoin::Amount;
use lightning_invoice::{Bolt11InvoiceDescription, Description};

use std::sync::Arc;

#[test]
fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false);
}

#[test]
fn channel_full_cycle_bitcoind() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpc(&bitcoind);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false);
}

#[test]
fn channel_full_cycle_force_close() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true);
}

#[test]
fn channel_full_cycle_force_close_trusted_no_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, true);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true);
}

#[test]
fn channel_full_cycle_0conf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, true, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, true, true, false)
}

#[test]
fn channel_full_cycle_legacy_staticremotekey() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, false, false);
}

#[test]
fn channel_open_fails_when_funds_insufficient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

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

	println!("\nA -- open_channel -> B");
	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			120000,
			None,
			None,
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
		let config = random_config(true);
		let mut sync_config = EsploraSyncConfig::default();
		sync_config.onchain_wallet_sync_interval_secs = 100000;
		sync_config.lightning_wallet_sync_interval_secs = 100000;
		setup_builder!(builder, config);
		builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
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

	let sending_params = SendingParameters {
		max_total_routing_fee_msat: Some(Some(75_000).into()),
		max_total_cltv_expiry_delta: Some(1000),
		max_path_count: Some(10),
		max_channel_saturation_power_of_half: Some(2),
	};

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let invoice = nodes[4]
		.bolt11_payment()
		.receive(2_500_000, &invoice_description.clone().into(), 9217)
		.unwrap();
	nodes[0].bolt11_payment().send(&invoice, Some(sending_params)).unwrap();

	expect_event!(nodes[1], PaymentForwarded);
	expect_event!(nodes[2], PaymentForwarded);

	let payment_id = expect_payment_received_event!(&nodes[4], 2_500_000);
	let fee_paid_msat = Some(2000);
	expect_payment_successful_event!(nodes[0], payment_id, Some(fee_paid_msat));
}

#[test]
fn start_stop_reinit() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config(true);

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let test_sync_store: Arc<dyn KVStore + Sync + Send> =
		Arc::new(TestSyncStore::new(config.storage_dir_path.clone().into()));

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.onchain_wallet_sync_interval_secs = 100000;
	sync_config.lightning_wallet_sync_interval_secs = 100000;
	setup_builder!(builder, config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

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

	let log_file = format!("{}/ldk_node.log", config.clone().storage_dir_path);
	assert!(std::path::Path::new(&log_file).exists());

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));

	node.start().unwrap();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));
	drop(node);

	setup_builder!(builder, config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

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
fn onchain_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let node_a_payments = node_a.list_payments();
	let node_b_payments = node_b.list_payments();
	for payments in [&node_a_payments, &node_b_payments] {
		assert_eq!(payments.len(), 1)
	}
	for p in [node_a_payments.first().unwrap(), node_b_payments.first().unwrap()] {
		assert_eq!(p.amount_msat, Some(premine_amount_sat * 1000));
		assert_eq!(p.direction, PaymentDirection::Inbound);
		// We got only 1-conf here, so we're only pending for now.
		assert_eq!(p.status, PaymentStatus::Pending);
		match p.kind {
			PaymentKind::Onchain { status, .. } => {
				assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			},
			_ => panic!("Unexpected payment kind"),
		}
	}

	let channel_amount_sat = 1_000_000;
	let reserve_amount_sat = 25_000;
	open_channel(&node_b, &node_a, channel_amount_sat, true, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 1);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 2);

	let onchain_fee_buffer_sat = 1000;
	let expected_node_a_balance = premine_amount_sat - reserve_amount_sat;
	let expected_node_b_balance_lower =
		premine_amount_sat - channel_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat;
	let expected_node_b_balance_upper =
		premine_amount_sat - channel_amount_sat - reserve_amount_sat;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.onchain_payment().send_to_address(&addr_b, expected_node_a_balance + 1, None)
	);

	let amount_to_send_sats = 54321;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None).unwrap();
	wait_for_tx(&electrsd.client, txid);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(txid.to_byte_array());
	let payment_a = node_a.payment(&payment_id).unwrap();
	assert_eq!(payment_a.status, PaymentStatus::Pending);
	match payment_a.kind {
		PaymentKind::Onchain { status, .. } => {
			assert!(matches!(status, ConfirmationStatus::Unconfirmed));
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert!(payment_a.fee_paid_msat > Some(0));
	let payment_b = node_b.payment(&payment_id).unwrap();
	assert_eq!(payment_b.status, PaymentStatus::Pending);
	match payment_a.kind {
		PaymentKind::Onchain { status, .. } => {
			assert!(matches!(status, ConfirmationStatus::Unconfirmed));
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert!(payment_b.fee_paid_msat > Some(0));
	assert_eq!(payment_a.amount_msat, Some(amount_to_send_sats * 1000));
	assert_eq!(payment_a.amount_msat, payment_b.amount_msat);
	assert_eq!(payment_a.fee_paid_msat, payment_b.fee_paid_msat);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_a_balance = expected_node_a_balance + amount_to_send_sats;
	let expected_node_b_balance_lower = expected_node_b_balance_lower - amount_to_send_sats;
	let expected_node_b_balance_upper = expected_node_b_balance_upper - amount_to_send_sats;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 2);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 3);

	let payment_a = node_a.payment(&payment_id).unwrap();
	match payment_a.kind {
		PaymentKind::Onchain { txid: _txid, status } => {
			assert_eq!(_txid, txid);
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let payment_b = node_a.payment(&payment_id).unwrap();
	match payment_b.kind {
		PaymentKind::Onchain { txid: _txid, status } => {
			assert_eq!(_txid, txid);
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, true, None).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_b_balance_lower = expected_node_b_balance_lower + expected_node_a_balance;
	let expected_node_b_balance_upper = expected_node_b_balance_upper + expected_node_a_balance;
	let expected_node_a_balance = 0;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, reserve_amount_sat);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 3);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 4);

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, false, None).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_b_balance_lower = expected_node_b_balance_lower + reserve_amount_sat;
	let expected_node_b_balance_upper = expected_node_b_balance_upper + reserve_amount_sat;
	let expected_node_a_balance = 0;

	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 4);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 5);
}

#[test]
fn onchain_wallet_recovery() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let chain_source = TestChainSource::Esplora(&electrsd);

	let seed_bytes = vec![42u8; 64];

	let original_config = random_config(true);
	let original_node = setup_node(&chain_source, original_config, Some(seed_bytes.clone()));

	let premine_amount_sat = 100_000;

	let addr_1 = original_node.onchain_payment().new_address().unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_1],
		Amount::from_sat(premine_amount_sat),
	);
	original_node.sync_wallets().unwrap();
	assert_eq!(original_node.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let addr_2 = original_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(
			&addr_2,
			Amount::from_sat(premine_amount_sat),
			None,
			None,
			None,
			None,
			None,
			None,
		)
		.unwrap();
	wait_for_tx(&electrsd.client, txid);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);

	original_node.sync_wallets().unwrap();
	assert_eq!(
		original_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	original_node.stop().unwrap();
	drop(original_node);

	// Now we start from scratch, only the seed remains the same.
	let recovered_config = random_config(true);
	let recovered_node = setup_node(&chain_source, recovered_config, Some(seed_bytes));

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	// Check we sync even when skipping some addresses.
	let _addr_3 = recovered_node.onchain_payment().new_address().unwrap();
	let _addr_4 = recovered_node.onchain_payment().new_address().unwrap();
	let _addr_5 = recovered_node.onchain_payment().new_address().unwrap();
	let addr_6 = recovered_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(
			&addr_6,
			Amount::from_sat(premine_amount_sat),
			None,
			None,
			None,
			None,
			None,
			None,
		)
		.unwrap();
	wait_for_tx(&electrsd.client, txid);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 3
	);
}

#[test]
fn sign_verify_msg() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config(true);
	let chain_source = TestChainSource::Esplora(&electrsd);
	let node = setup_node(&chain_source, config, None);

	// Tests arbitrary message signing and later verification
	let msg = "OK computer".as_bytes();
	let sig = node.sign_message(msg);
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
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);

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
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

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

#[test]
fn simple_bolt12_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	);

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcasted a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	// Sleep one more sec to make sure the node announcement propagates.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let expected_amount_msat = 100_000_000;
	let offer =
		node_b.bolt12_payment().receive(expected_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let payment_id = node_a
		.bolt12_payment()
		.send(&offer, expected_quantity, expected_payer_note.clone())
		.unwrap();

	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt12Offer { .. }));
	assert_eq!(node_a_payments.len(), 1);
	match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer {
			hash,
			preimage,
			secret: _,
			offer_id,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(offer_id, offer.id());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0);
			//TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			//API currently doesn't allow to do that.
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	expect_payment_received_event!(node_b, expected_amount_msat);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt12Offer { .. }));
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer { hash, preimage, secret, offer_id, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
			assert_eq!(offer_id, offer.id());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	// Test send_using_amount
	let offer_amount_msat = 100_000_000;
	let less_than_offer_amount = offer_amount_msat - 10_000;
	let expected_amount_msat = offer_amount_msat + 10_000;
	let offer = node_b.bolt12_payment().receive(offer_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	assert!(node_a
		.bolt12_payment()
		.send_using_amount(&offer, less_than_offer_amount, None, None)
		.is_err());
	let payment_id = node_a
		.bolt12_payment()
		.send_using_amount(
			&offer,
			expected_amount_msat,
			expected_quantity,
			expected_payer_note.clone(),
		)
		.unwrap();

	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let node_a_payments = node_a.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Offer { .. }) && p.id == payment_id
	});
	assert_eq!(node_a_payments.len(), 1);
	let payment_hash = match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer {
			hash,
			preimage,
			secret: _,
			offer_id,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(offer_id, offer.id());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0);
			//TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			//API currently doesn't allow to do that.
			hash.unwrap()
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	};
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	expect_payment_received_event!(node_b, expected_amount_msat);
	let node_b_payment_id = PaymentId(payment_hash.0);
	let node_b_payments = node_b.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Offer { .. }) && p.id == node_b_payment_id
	});
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer { hash, preimage, secret, offer_id, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
			assert_eq!(offer_id, offer.id());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	// Now node_b refunds the amount node_a just overpaid.
	let overpaid_amount = expected_amount_msat - offer_amount_msat;
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let refund = node_b
		.bolt12_payment()
		.initiate_refund(overpaid_amount, 3600, expected_quantity, expected_payer_note.clone())
		.unwrap();
	let invoice = node_a.bolt12_payment().request_refund_payment(&refund).unwrap();
	expect_payment_received_event!(node_a, overpaid_amount);

	let node_b_payment_id = node_b
		.list_payments_with_filter(|p| {
			matches!(p.kind, PaymentKind::Bolt12Refund { .. })
				&& p.amount_msat == Some(overpaid_amount)
		})
		.first()
		.unwrap()
		.id;
	expect_payment_successful_event!(node_b, Some(node_b_payment_id), None);

	let node_b_payments = node_b.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Refund { .. }) && p.id == node_b_payment_id
	});
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Refund {
			hash,
			preimage,
			secret: _,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0)
			//TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			//API currently doesn't allow to do that.
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(overpaid_amount));

	let node_a_payment_id = PaymentId(invoice.payment_hash().0);
	let node_a_payments = node_a.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Refund { .. }) && p.id == node_a_payment_id
	});
	assert_eq!(node_a_payments.len(), 1);
	match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Refund { hash, preimage, secret, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(overpaid_amount));
}

#[test]
fn generate_bip21_uri() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	);

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	let uqr_payment = node_b.unified_qr_payment().receive(expected_amount_sats, "asdf", expiry_sec);

	match uqr_payment.clone() {
		Ok(ref uri) => {
			println!("Generated URI: {}", uri);
			assert!(uri.contains("bitcoin:"));
			assert!(uri.contains("lightning="));
			assert!(uri.contains("lno="));
		},
		Err(e) => panic!("Failed to generate URI: {:?}", e),
	}
}

#[test]
fn unified_qr_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	);

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcast a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	// Sleep one more sec to make sure the node announcement propagates.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	let uqr_payment = node_b.unified_qr_payment().receive(expected_amount_sats, "asdf", expiry_sec);
	let uri_str = uqr_payment.clone().unwrap();
	let offer_payment_id: PaymentId = match node_a.unified_qr_payment().send(&uri_str) {
		Ok(QrPaymentResult::Bolt12 { payment_id }) => {
			println!("\nBolt12 payment sent successfully with PaymentID: {:?}", payment_id);
			payment_id
		},
		Ok(QrPaymentResult::Bolt11 { payment_id: _ }) => {
			panic!("Expected Bolt12 payment but got Bolt11");
		},
		Ok(QrPaymentResult::Onchain { txid: _ }) => {
			panic!("Expected Bolt12 payment but get On-chain transaction");
		},
		Err(e) => {
			panic!("Expected Bolt12 payment but got error: {:?}", e);
		},
	};

	expect_payment_successful_event!(node_a, Some(offer_payment_id), None);

	// Cut off the BOLT12 part to fallback to BOLT11.
	let uri_str_without_offer = uri_str.split("&lno=").next().unwrap();
	let invoice_payment_id: PaymentId =
		match node_a.unified_qr_payment().send(uri_str_without_offer) {
			Ok(QrPaymentResult::Bolt12 { payment_id: _ }) => {
				panic!("Expected Bolt11 payment but got Bolt12");
			},
			Ok(QrPaymentResult::Bolt11 { payment_id }) => {
				println!("\nBolt11 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(QrPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt11 payment but got on-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt11 payment but got error: {:?}", e);
			},
		};
	expect_payment_successful_event!(node_a, Some(invoice_payment_id), None);

	let expect_onchain_amount_sats = 800_000;
	let onchain_uqr_payment =
		node_b.unified_qr_payment().receive(expect_onchain_amount_sats, "asdf", 4_000).unwrap();

	// Cut off any lightning part to fallback to on-chain only.
	let uri_str_without_lightning = onchain_uqr_payment.split("&lightning=").next().unwrap();
	let txid = match node_a.unified_qr_payment().send(&uri_str_without_lightning) {
		Ok(QrPaymentResult::Bolt12 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt12")
		},
		Ok(QrPaymentResult::Bolt11 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt11");
		},
		Ok(QrPaymentResult::Onchain { txid }) => {
			println!("\nOn-chain transaction successful with Txid: {}", txid);
			txid
		},
		Err(e) => {
			panic!("Expected on-chain payment but got error: {:?}", e);
		},
	};

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_b.list_balances().total_onchain_balance_sats, 800_000);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 200_000);
}
