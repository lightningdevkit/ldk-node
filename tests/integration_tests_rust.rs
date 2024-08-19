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
	setup_node, setup_two_nodes, wait_for_tx, TestSyncStore,
};

use ldk_node::payment::{PaymentKind, QrPaymentResult, SendingParameters};
use ldk_node::{Builder, Event, NodeError};

use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::KVStore;

use bitcoin::{Amount, Network};

use std::sync::Arc;

#[test]
fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false);
}

#[test]
fn channel_full_cycle_force_close() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true);
}

#[test]
fn channel_full_cycle_force_close_trusted_no_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, true);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true);
}

#[test]
fn channel_full_cycle_0conf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, true, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, true, true, false)
}

#[test]
fn channel_full_cycle_legacy_staticremotekey() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, false, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, false, false);
}

#[test]
fn channel_open_fails_when_funds_insufficient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);

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

	let sending_params = SendingParameters {
		max_total_routing_fee_msat: Some(Some(75_000).into()),
		max_total_cltv_expiry_delta: Some(1000),
		max_path_count: Some(10),
		max_channel_saturation_power_of_half: Some(2),
	};

	let invoice = nodes[4].bolt11_payment().receive(2_500_000, &"asdf", 9217).unwrap();
	nodes[0].bolt11_payment().send(&invoice, Some(sending_params)).unwrap();

	let payment_id = expect_payment_received_event!(&nodes[4], 2_500_000);
	let fee_paid_msat = Some(2000);
	expect_payment_successful_event!(nodes[0], payment_id, Some(fee_paid_msat));
}

#[test]
fn connect_to_public_testnet_esplora() {
	let mut config = random_config(true);
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
	let config = random_config(true);

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
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);

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
	let config = random_config(true);
	let node = setup_node(&electrsd, config);

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
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, false, false);

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
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);

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
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);

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
	let node_a_payments = node_a.list_payments();
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
	let node_b_payments = node_b.list_payments();
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
	let node_a_payments = node_a.list_payments_with_filter(|p| p.id == payment_id);
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
	let node_b_payments = node_b.list_payments_with_filter(|p| p.id == node_b_payment_id);
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
		.list_payments_with_filter(|p| p.amount_msat == Some(overpaid_amount))
		.first()
		.unwrap()
		.id;
	expect_payment_successful_event!(node_b, Some(node_b_payment_id), None);

	let node_b_payments = node_b.list_payments_with_filter(|p| p.id == node_b_payment_id);
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
	let node_a_payments = node_a.list_payments_with_filter(|p| p.id == node_a_payment_id);
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
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);

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
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true, false);

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

	// Removed one character from the offer to fall back on to invoice.
	// Still needs work
	let uri_str_with_invalid_offer = &uri_str[..uri_str.len() - 1];
	let invoice_payment_id: PaymentId =
		match node_a.unified_qr_payment().send(uri_str_with_invalid_offer) {
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

	// Removed a character from the offer, so it would move on to the other parameters.
	let txid = match node_a
		.unified_qr_payment()
		.send(&onchain_uqr_payment.as_str()[..onchain_uqr_payment.len() - 1])
	{
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
