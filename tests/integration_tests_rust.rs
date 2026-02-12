// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::hashes::Hash;
use bitcoin::{Address, Amount, ScriptBuf};
use common::logging::{init_log_logger, validate_log_entry, MultiNodeLogger, TestLogWriter};
use common::{
	bump_fee_and_broadcast, distribute_funds_unconfirmed, do_channel_full_cycle,
	expect_channel_pending_event, expect_channel_ready_event, expect_event,
	expect_payment_claimable_event, expect_payment_received_event, expect_payment_successful_event,
	expect_splice_pending_event, generate_blocks_and_wait, open_channel, open_channel_push_amt,
	open_channel_with_all, premine_and_distribute_funds, premine_blocks, prepare_rbf,
	random_config, random_listening_addresses, setup_bitcoind_and_electrsd, setup_builder,
	setup_node, setup_node_for_async_payments, setup_two_nodes, wait_for_tx, TestChainSource,
	TestStoreType, TestSyncStore,
};
use ldk_node::config::{AsyncPaymentsRole, EsploraSyncConfig};
use ldk_node::entropy::NodeEntropy;
use ldk_node::liquidity::LSPS2ServiceConfig;
use ldk_node::payment::{
	ConfirmationStatus, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
	UnifiedPaymentResult,
};
use ldk_node::{Builder, Event, NodeError};
use lightning::ln::channelmanager::PaymentId;
use lightning::routing::gossip::{NodeAlias, NodeId};
use lightning::routing::router::RouteParametersConfig;
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use lightning_types::payment::{PaymentHash, PaymentPreimage};
use log::LevelFilter;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_electrum() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Electrum(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_bitcoind_rpc_sync() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_bitcoind_rest_sync() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRestSync(&bitcoind);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_force_close() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_force_close_trusted_no_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, true);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_0conf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, true, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, true, true, false)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_legacy_staticremotekey() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, false, false)
		.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_open_fails_when_funds_insufficient() {
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
	)
	.await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn multi_hop_sending() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	// Setup and fund 5 nodes
	let mut nodes = Vec::new();
	for _ in 0..5 {
		let config = random_config(true);
		let mut sync_config = EsploraSyncConfig::default();
		sync_config.background_sync_config = None;
		setup_builder!(builder, config.node_config);
		builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
		let node = builder.build(config.node_entropy.into()).unwrap();
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
	)
	.await;

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

	open_channel(&nodes[0], &nodes[1], 100_000, true, &electrsd).await;
	open_channel(&nodes[1], &nodes[2], 1_000_000, true, &electrsd).await;
	// We need to sync wallets in-between back-to-back channel opens from the same node so BDK
	// wallet picks up on the broadcast funding tx and doesn't double-spend itself.
	//
	// TODO: Remove once fixed in BDK.
	nodes[1].sync_wallets().unwrap();
	open_channel(&nodes[1], &nodes[3], 1_000_000, true, &electrsd).await;
	open_channel(&nodes[2], &nodes[4], 1_000_000, true, &electrsd).await;
	open_channel(&nodes[3], &nodes[4], 1_000_000, true, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

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
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let route_params = RouteParametersConfig {
		max_total_routing_fee_msat: Some(75_000),
		max_total_cltv_expiry_delta: 1000,
		max_path_count: 10,
		max_channel_saturation_power_of_half: 2,
	};

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let invoice = nodes[4]
		.bolt11_payment()
		.receive(2_500_000, &invoice_description.clone().into(), 9217)
		.unwrap();
	nodes[0].bolt11_payment().send(&invoice, Some(route_params)).unwrap();

	expect_event!(nodes[1], PaymentForwarded);

	// We expect that the payment goes through N2 or N3, so we check both for the PaymentForwarded event.
	let node_2_fwd_event = matches!(nodes[2].next_event(), Some(Event::PaymentForwarded { .. }));
	let node_3_fwd_event = matches!(nodes[3].next_event(), Some(Event::PaymentForwarded { .. }));
	assert!(node_2_fwd_event || node_3_fwd_event);

	let payment_id = expect_payment_received_event!(&nodes[4], 2_500_000);
	let fee_paid_msat = Some(2000);
	expect_payment_successful_event!(nodes[0], payment_id, Some(fee_paid_msat));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn start_stop_reinit() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config(true);

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let test_sync_store = TestSyncStore::new(config.node_config.storage_dir_path.clone().into());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;
	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

	let node =
		builder.build_with_store(config.node_entropy.into(), test_sync_store.clone()).unwrap();
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
	)
	.await;

	node.sync_wallets().unwrap();
	assert_eq!(node.list_balances().spendable_onchain_balance_sats, expected_amount.to_sat());

	let log_file = format!("{}/ldk_node.log", config.node_config.clone().storage_dir_path);
	assert!(std::path::Path::new(&log_file).exists());

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));

	node.start().unwrap();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));
	drop(node);

	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

	let reinitialized_node =
		builder.build_with_store(config.node_entropy.into(), test_sync_store).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	// This is a Bitcoin Testnet address. Sending funds to this address from the Regtest network will fail
	let static_address = "tb1q0d40e5rta4fty63z64gztf8c3v20cvet6v2jdh";
	let unchecked_address = Address::<NetworkUnchecked>::from_str(static_address).unwrap();
	let addr_c = unchecked_address.assume_checked();

	let premine_amount_sat = 1_100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

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
	open_channel(&node_b, &node_a, channel_amount_sat, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

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

	assert_eq!(
		Err(NodeError::InvalidAddress),
		node_a.onchain_payment().send_to_address(&addr_c, expected_node_a_balance + 1, None)
	);

	assert_eq!(
		Err(NodeError::InvalidAddress),
		node_a.onchain_payment().send_all_to_address(&addr_c, true, None)
	);

	let amount_to_send_sats = 54321;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
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

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
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
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	wait_for_tx(&electrsd.client, txid).await;

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
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	wait_for_tx(&electrsd.client, txid).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_send_all_retains_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	// Setup nodes
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;
	let reserve_amount_sat = 25_000;
	let onchain_fee_buffer_sat = 1000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	// Send all over, with 0 reserve as we don't have any channels open.
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, true, None).unwrap();

	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	// Check node a sent all and node b received it
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat * 2 - onchain_fee_buffer_sat)..=(premine_amount_sat * 2))
		.contains(&node_b.list_balances().spendable_onchain_balance_sats));

	// Refill to make sure we have enough reserve for the channel open.
	let txid = bitcoind
		.client
		.send_to_address(&addr_a, Amount::from_sat(reserve_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, reserve_amount_sat);

	// Open a channel.
	open_channel(&node_b, &node_a, premine_amount_sat, false, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Check node a sent all and node b received it
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat)
		..=premine_amount_sat)
		.contains(&node_b.list_balances().spendable_onchain_balance_sats));

	// Send all over again, this time ensuring the reserve is accounted for
	let txid = node_b.onchain_payment().send_all_to_address(&addr_a, true, None).unwrap();

	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Check node b sent all and node a received it
	assert_eq!(node_b.list_balances().total_onchain_balance_sats, reserve_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat)
		..=premine_amount_sat)
		.contains(&node_a.list_balances().spendable_onchain_balance_sats));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_wallet_recovery() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let chain_source = TestChainSource::Esplora(&electrsd);

	let original_config = random_config(true);
	let original_node_entropy = original_config.node_entropy;
	let original_node = setup_node(&chain_source, original_config);

	let premine_amount_sat = 100_000;

	let addr_1 = original_node.onchain_payment().new_address().unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_1],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	original_node.sync_wallets().unwrap();
	assert_eq!(original_node.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let addr_2 = original_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(&addr_2, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	original_node.sync_wallets().unwrap();
	assert_eq!(
		original_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	original_node.stop().unwrap();
	drop(original_node);

	// Now we start from scratch, only the seed remains the same.
	let mut recovered_config = random_config(true);
	recovered_config.node_entropy = original_node_entropy;
	let recovered_node = setup_node(&chain_source, recovered_config);

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
		.send_to_address(&addr_6, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 3
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_via_mempool() {
	run_rbf_test(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_via_direct_block_insertion() {
	run_rbf_test(true).await;
}

// `is_insert_block`:
// - `true`: transaction is mined immediately (no mempool), testing confirmed-Tx handling.
// - `false`: transaction stays in mempool until confirmation, testing unconfirmed-Tx handling.
async fn run_rbf_test(is_insert_block: bool) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
	let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
	let chain_source_esplora = TestChainSource::Esplora(&electrsd);

	macro_rules! config_node {
		($chain_source:expr, $anchor_channels:expr) => {{
			let config_a = random_config($anchor_channels);
			let node = setup_node(&$chain_source, config_a);
			node
		}};
	}
	let anchor_channels = false;
	let nodes = vec![
		config_node!(chain_source_electrsd, anchor_channels),
		config_node!(chain_source_bitcoind, anchor_channels),
		config_node!(chain_source_esplora, anchor_channels),
	];

	let (bitcoind, electrs) = (&bitcoind.client, &electrsd.client);
	premine_blocks(bitcoind, electrs).await;

	// Helpers declaration before starting the test
	let all_addrs =
		nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect::<Vec<_>>();
	let amount_sat = 2_100_000;
	let mut txid;
	macro_rules! distribute_funds_all_nodes {
		() => {
			txid = distribute_funds_unconfirmed(
				bitcoind,
				electrs,
				all_addrs.clone(),
				Amount::from_sat(amount_sat),
			)
			.await;
		};
	}
	macro_rules! validate_balances {
		($expected_balance_sat:expr, $is_spendable:expr) => {
			let spend_balance = if $is_spendable { $expected_balance_sat } else { 0 };
			for node in &nodes {
				node.sync_wallets().unwrap();
				let balances = node.list_balances();
				assert_eq!(balances.spendable_onchain_balance_sats, spend_balance);
				assert_eq!(balances.total_onchain_balance_sats, $expected_balance_sat);
			}
		};
	}

	let scripts_buf: HashSet<ScriptBuf> =
		all_addrs.iter().map(|addr| addr.script_pubkey()).collect();
	let mut tx;
	let mut fee_output_index;

	// Modify the output to the nodes
	distribute_funds_all_nodes!();
	validate_balances!(amount_sat, false);
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	tx.output.iter_mut().for_each(|output| {
		if scripts_buf.contains(&output.script_pubkey) {
			let new_addr = bitcoind.new_address().unwrap();
			output.script_pubkey = new_addr.script_pubkey();
		}
	});
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	validate_balances!(0, is_insert_block);

	// Not modifying the output scripts, but still bumping the fee.
	distribute_funds_all_nodes!();
	validate_balances!(amount_sat, false);
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	validate_balances!(amount_sat, is_insert_block);

	let mut final_amount_sat = amount_sat * 2;
	let value_sat = 21_000;

	// Increase the value of the nodes' outputs
	distribute_funds_all_nodes!();
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	tx.output.iter_mut().for_each(|output| {
		if scripts_buf.contains(&output.script_pubkey) {
			output.value = Amount::from_sat(output.value.to_sat() + value_sat);
		}
	});
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	final_amount_sat += value_sat;
	validate_balances!(final_amount_sat, is_insert_block);

	// Decreases the value of the nodes' outputs
	distribute_funds_all_nodes!();
	final_amount_sat += amount_sat;
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	tx.output.iter_mut().for_each(|output| {
		if scripts_buf.contains(&output.script_pubkey) {
			output.value = Amount::from_sat(output.value.to_sat() - value_sat);
		}
	});
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	final_amount_sat -= value_sat;
	validate_balances!(final_amount_sat, is_insert_block);

	if !is_insert_block {
		generate_blocks_and_wait(bitcoind, electrs, 1).await;
		validate_balances!(final_amount_sat, true);
	}

	// Check if it is possible to send all funds from the node
	let mut txids = Vec::new();
	let addr = bitcoind.new_address().unwrap();
	nodes.iter().for_each(|node| {
		let txid = node.onchain_payment().send_all_to_address(&addr, true, None).unwrap();
		txids.push(txid);
	});
	for txid in txids {
		wait_for_tx(electrs, txid).await;
	}
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	validate_balances!(0, true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sign_verify_msg() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config(true);
	let chain_source = TestChainSource::Esplora(&electrsd);
	let node = setup_node(&chain_source, config);

	// Tests arbitrary message signing and later verification
	let msg = "OK computer".as_bytes();
	let sig = node.sign_message(msg);
	let pkey = node.node_id();
	assert!(node.verify_signature(msg, sig.as_str(), &pkey));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn connection_multi_listen() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);

	let node_id_b = node_b.node_id();

	let node_addrs_b = node_b.listening_addresses().unwrap();
	for node_addr_b in &node_addrs_b {
		node_a.connect(node_id_b, node_addr_b.clone(), false).unwrap();
		node_a.disconnect(node_id_b).unwrap();
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn connection_restart_behavior() {
	do_connection_restart_behavior(true).await;
	do_connection_restart_behavior(false).await;
}

async fn do_connection_restart_behavior(persist: bool) {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);

	let node_id_a = node_a.node_id();
	let node_id_b = node_b.node_id();

	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();
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
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn concurrent_connections_succeed() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let node_a = Arc::new(node_a);
	let node_b = Arc::new(node_b);

	let node_id_b = node_b.node_id();
	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();

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

async fn run_splice_channel_test(bitcoind_chain_source: bool) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = if bitcoind_chain_source {
		TestChainSource::BitcoindRpcSync(&bitcoind)
	} else {
		TestChainSource::Esplora(&electrsd)
	};
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let address_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a, address_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_a.list_balances().total_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().total_onchain_balance_sats, premine_amount_sat);

	open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;

	// Open a channel with Node A contributing the funding
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	let opening_transaction_fee_sat = 156;
	let closing_transaction_fee_sat = 614;
	let anchor_output_sat = 330;

	assert_eq!(
		node_a.list_balances().total_onchain_balance_sats,
		premine_amount_sat - 4_000_000 - opening_transaction_fee_sat
	);
	assert_eq!(
		node_a.list_balances().total_lightning_balance_sats,
		4_000_000 - closing_transaction_fee_sat - anchor_output_sat
	);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 0);

	// Test that splicing and payments fail when there are insufficient funds
	let address = node_b.onchain_payment().new_address().unwrap();
	let amount_msat = 400_000_000;

	assert_eq!(
		node_b.splice_in(&user_channel_id_b, node_b.node_id(), 5_000_000),
		Err(NodeError::ChannelSplicingFailed),
	);
	assert_eq!(
		node_b.splice_out(&user_channel_id_b, node_b.node_id(), &address, amount_msat / 1000),
		Err(NodeError::ChannelSplicingFailed),
	);
	assert_eq!(
		node_b.spontaneous_payment().send(amount_msat, node_a.node_id(), None),
		Err(NodeError::PaymentSendingFailed)
	);

	// Splice-in funds for Node B so that it has outbound liquidity to make a payment
	node_b.splice_in(&user_channel_id_b, node_a.node_id(), 4_000_000).unwrap();

	let txo = expect_splice_pending_event!(node_a, node_b.node_id());
	expect_splice_pending_event!(node_b, node_a.node_id());

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let expected_splice_in_fee_sat = 252;

	let payments = node_b.list_payments();
	let payment =
		payments.into_iter().find(|p| p.id == PaymentId(txo.txid.to_byte_array())).unwrap();
	assert_eq!(payment.fee_paid_msat, Some(expected_splice_in_fee_sat * 1_000));

	assert_eq!(
		node_b.list_balances().total_onchain_balance_sats,
		premine_amount_sat - 4_000_000 - expected_splice_in_fee_sat
	);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 4_000_000);

	let payment_id =
		node_b.spontaneous_payment().send(amount_msat, node_a.node_id(), None).unwrap();

	expect_payment_successful_event!(node_b, Some(payment_id), None);
	expect_payment_received_event!(node_a, amount_msat);

	// Mine a block to give time for the HTLC to resolve
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	assert_eq!(
		node_a.list_balances().total_lightning_balance_sats,
		4_000_000 - closing_transaction_fee_sat - anchor_output_sat + amount_msat / 1000
	);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 4_000_000 - amount_msat / 1000);

	// Splice-out funds for Node A from the payment sent by Node B
	let address = node_a.onchain_payment().new_address().unwrap();
	node_a.splice_out(&user_channel_id_a, node_b.node_id(), &address, amount_msat / 1000).unwrap();

	let txo = expect_splice_pending_event!(node_a, node_b.node_id());
	expect_splice_pending_event!(node_b, node_a.node_id());

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let expected_splice_out_fee_sat = 183;

	let payments = node_a.list_payments();
	let payment =
		payments.into_iter().find(|p| p.id == PaymentId(txo.txid.to_byte_array())).unwrap();
	assert_eq!(payment.fee_paid_msat, Some(expected_splice_out_fee_sat * 1_000));

	assert_eq!(
		node_a.list_balances().total_onchain_balance_sats,
		premine_amount_sat - 4_000_000 - opening_transaction_fee_sat + amount_msat / 1000
	);
	assert_eq!(
		node_a.list_balances().total_lightning_balance_sats,
		4_000_000 - closing_transaction_fee_sat - anchor_output_sat - expected_splice_out_fee_sat
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn splice_channel() {
	run_splice_channel_test(false).await;
	run_splice_channel_test(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn simple_bolt12_send_receive() {
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
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcasted a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}

	// Sleep one more sec to make sure the node announcement propagates.
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let expected_amount_msat = 100_000_000;
	let offer =
		node_b.bolt12_payment().receive(expected_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let payment_id = node_a
		.bolt12_payment()
		.send(&offer, expected_quantity, expected_payer_note.clone(), None)
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
			// TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			// API currently doesn't allow to do that.
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
		.send_using_amount(&offer, less_than_offer_amount, None, None, None)
		.is_err());
	let payment_id = node_a
		.bolt12_payment()
		.send_using_amount(
			&offer,
			expected_amount_msat,
			expected_quantity,
			expected_payer_note.clone(),
			None,
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
			// TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			// API currently doesn't allow to do that.
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
		.initiate_refund(
			overpaid_amount,
			3600,
			expected_quantity,
			expected_payer_note.clone(),
			None,
		)
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
			// TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			// API currently doesn't allow to do that.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn async_payment() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config_sender = random_config(true);
	config_sender.node_config.listening_addresses = None;
	config_sender.node_config.node_alias = None;
	config_sender.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("sender      ".to_string())));
	let node_sender = setup_node_for_async_payments(
		&chain_source,
		config_sender,
		Some(AsyncPaymentsRole::Client),
	);

	let mut config_sender_lsp = random_config(true);
	config_sender_lsp.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("sender_lsp  ".to_string())));
	let node_sender_lsp = setup_node_for_async_payments(
		&chain_source,
		config_sender_lsp,
		Some(AsyncPaymentsRole::Server),
	);

	let mut config_receiver_lsp = random_config(true);
	config_receiver_lsp.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("receiver_lsp".to_string())));

	let node_receiver_lsp = setup_node_for_async_payments(
		&chain_source,
		config_receiver_lsp,
		Some(AsyncPaymentsRole::Server),
	);

	let mut config_receiver = random_config(true);
	config_receiver.node_config.listening_addresses = None;
	config_receiver.node_config.node_alias = None;
	config_receiver.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("receiver    ".to_string())));
	let node_receiver = setup_node(&chain_source, config_receiver);

	let address_sender = node_sender.onchain_payment().new_address().unwrap();
	let address_sender_lsp = node_sender_lsp.onchain_payment().new_address().unwrap();
	let address_receiver_lsp = node_receiver_lsp.onchain_payment().new_address().unwrap();
	let address_receiver = node_receiver.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 4_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_sender, address_sender_lsp, address_receiver_lsp, address_receiver],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_sender.sync_wallets().unwrap();
	node_sender_lsp.sync_wallets().unwrap();
	node_receiver_lsp.sync_wallets().unwrap();
	node_receiver.sync_wallets().unwrap();

	open_channel(&node_sender, &node_sender_lsp, 400_000, false, &electrsd).await;
	open_channel(&node_sender_lsp, &node_receiver_lsp, 400_000, true, &electrsd).await;
	open_channel_push_amt(
		&node_receiver,
		&node_receiver_lsp,
		400_000,
		Some(200_000_000),
		false,
		&electrsd,
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_sender.sync_wallets().unwrap();
	node_sender_lsp.sync_wallets().unwrap();
	node_receiver_lsp.sync_wallets().unwrap();
	node_receiver.sync_wallets().unwrap();

	expect_channel_ready_event!(node_sender, node_sender_lsp.node_id());
	expect_channel_ready_event!(node_sender_lsp, node_sender.node_id());
	expect_channel_ready_event!(node_sender_lsp, node_receiver_lsp.node_id());
	expect_channel_ready_event!(node_receiver_lsp, node_sender_lsp.node_id());
	expect_channel_ready_event!(node_receiver_lsp, node_receiver.node_id());
	expect_channel_ready_event!(node_receiver, node_receiver_lsp.node_id());

	let has_node_announcements = |node: &ldk_node::Node| {
		node.network_graph()
			.list_nodes()
			.iter()
			.filter(|n| {
				node.network_graph().node(n).map_or(false, |info| info.announcement_info.is_some())
			})
			.count() >= 2
	};

	// Wait for everyone to see all channels and node announcements.
	while node_sender.network_graph().list_channels().len() < 1
		|| node_sender_lsp.network_graph().list_channels().len() < 1
		|| node_receiver_lsp.network_graph().list_channels().len() < 1
		|| node_receiver.network_graph().list_channels().len() < 1
		|| !has_node_announcements(&node_sender)
		|| !has_node_announcements(&node_sender_lsp)
		|| !has_node_announcements(&node_receiver_lsp)
		|| !has_node_announcements(&node_receiver)
	{
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	}

	let recipient_id = vec![1, 2, 3];
	let blinded_paths =
		node_receiver_lsp.bolt12_payment().blinded_paths_for_async_recipient(recipient_id).unwrap();
	node_receiver.bolt12_payment().set_paths_to_static_invoice_server(blinded_paths).unwrap();

	let offer = loop {
		if let Ok(offer) = node_receiver.bolt12_payment().receive_async() {
			break offer;
		}

		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	};

	node_receiver.stop().unwrap();

	let payment_id =
		node_sender.bolt12_payment().send_using_amount(&offer, 5_000, None, None, None).unwrap();

	// Sleep to allow the payment reach a state where the htlc is held and waiting for the receiver to come online.
	tokio::time::sleep(std::time::Duration::from_millis(3000)).await;

	node_receiver.start().unwrap();

	expect_payment_successful_event!(node_sender, Some(payment_id), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_node_announcement_propagation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Node A will use both listening and announcement addresses
	let mut config_a = random_config(true);
	let node_a_alias_string = "ldk-node-a".to_string();
	let mut node_a_alias_bytes = [0u8; 32];
	node_a_alias_bytes[..node_a_alias_string.as_bytes().len()]
		.copy_from_slice(node_a_alias_string.as_bytes());
	let node_a_node_alias = Some(NodeAlias(node_a_alias_bytes));
	let node_a_announcement_addresses = random_listening_addresses();
	config_a.node_config.node_alias = node_a_node_alias.clone();
	config_a.node_config.listening_addresses = Some(random_listening_addresses());
	config_a.node_config.announcement_addresses = Some(node_a_announcement_addresses.clone());

	// Node B will only use listening addresses
	let mut config_b = random_config(true);
	let node_b_alias_string = "ldk-node-b".to_string();
	let mut node_b_alias_bytes = [0u8; 32];
	node_b_alias_bytes[..node_b_alias_string.as_bytes().len()]
		.copy_from_slice(node_b_alias_string.as_bytes());
	let node_b_node_alias = Some(NodeAlias(node_b_alias_bytes));
	let node_b_listening_addresses = random_listening_addresses();
	config_b.node_config.node_alias = node_b_node_alias.clone();
	config_b.node_config.listening_addresses = Some(node_b_listening_addresses.clone());
	config_b.node_config.announcement_addresses = None;

	let node_a = setup_node(&chain_source, config_a);
	let node_b = setup_node(&chain_source, config_b);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();

	// Open an announced channel from node_a to node_b
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Wait until node_b broadcasts a node announcement
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}

	// Sleep to make sure the node announcement propagates
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	// Get node info from the other node's perspective
	let node_a_info = node_b.network_graph().node(&NodeId::from_pubkey(&node_a.node_id())).unwrap();
	let node_a_announcement_info = node_a_info.announcement_info.as_ref().unwrap();

	let node_b_info = node_a.network_graph().node(&NodeId::from_pubkey(&node_b.node_id())).unwrap();
	let node_b_announcement_info = node_b_info.announcement_info.as_ref().unwrap();

	// Assert that the aliases and addresses match the expected values
	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_a_announcement_info.alias(), &node_a_node_alias.unwrap());
	#[cfg(feature = "uniffi")]
	assert_eq!(node_a_announcement_info.alias, node_a_alias_string);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_a_announcement_info.addresses(), &node_a_announcement_addresses);
	#[cfg(feature = "uniffi")]
	assert_eq!(node_a_announcement_info.addresses, node_a_announcement_addresses);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_b_announcement_info.alias(), &node_b_node_alias.unwrap());
	#[cfg(feature = "uniffi")]
	assert_eq!(node_b_announcement_info.alias, node_b_alias_string);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_b_announcement_info.addresses(), &node_b_listening_addresses);
	#[cfg(feature = "uniffi")]
	assert_eq!(node_b_announcement_info.addresses, node_b_listening_addresses);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn generate_bip21_uri() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	// Test 1: Verify URI generation (on-chain + BOLT11) works
	// even before any channels are opened. This checks the graceful fallback behavior.
	let initial_uni_payment = node_b
		.unified_payment()
		.receive(expected_amount_sats, "asdf", expiry_sec)
		.expect("Failed to generate URI");
	println!("Initial URI (no channels): {}", initial_uni_payment);

	assert!(initial_uni_payment.contains("bitcoin:"));
	assert!(initial_uni_payment.contains("lightning="));
	assert!(!initial_uni_payment.contains("lno=")); // BOLT12 requires channels

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Test 2: Verify URI generation (on-chain + BOLT11 + BOLT12) works after channels are established.
	let uni_payment = node_b
		.unified_payment()
		.receive(expected_amount_sats, "asdf", expiry_sec)
		.expect("Failed to generate URI");

	println!("Generated URI: {}", uni_payment);
	assert!(uni_payment.contains("bitcoin:"));
	assert!(uni_payment.contains("lightning="));
	assert!(uni_payment.contains("lno="));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unified_send_receive_bip21_uri() {
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
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcast a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}

	// Sleep one more sec to make sure the node announcement propagates.
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	let uni_payment = node_b.unified_payment().receive(expected_amount_sats, "asdf", expiry_sec);
	let uri_str = uni_payment.clone().unwrap();
	let offer_payment_id: PaymentId =
		match node_a.unified_payment().send(&uri_str, None, None).await {
			Ok(UnifiedPaymentResult::Bolt12 { payment_id }) => {
				println!("\nBolt12 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(UnifiedPaymentResult::Bolt11 { payment_id: _ }) => {
				panic!("Expected Bolt12 payment but got Bolt11");
			},
			Ok(UnifiedPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt12 payment but got On-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt12 payment but got error: {:?}", e);
			},
		};

	expect_payment_successful_event!(node_a, Some(offer_payment_id), None);

	// Cut off the BOLT12 part to fallback to BOLT11.
	let uri_str_without_offer = uri_str.split("&lno=").next().unwrap();
	let invoice_payment_id: PaymentId =
		match node_a.unified_payment().send(uri_str_without_offer, None, None).await {
			Ok(UnifiedPaymentResult::Bolt12 { payment_id: _ }) => {
				panic!("Expected Bolt11 payment but got Bolt12");
			},
			Ok(UnifiedPaymentResult::Bolt11 { payment_id }) => {
				println!("\nBolt11 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(UnifiedPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt11 payment but got on-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt11 payment but got error: {:?}", e);
			},
		};
	expect_payment_successful_event!(node_a, Some(invoice_payment_id), None);

	let expect_onchain_amount_sats = 800_000;
	let onchain_uni_payment =
		node_b.unified_payment().receive(expect_onchain_amount_sats, "asdf", 4_000).unwrap();

	// Cut off any lightning part to fallback to on-chain only.
	let uri_str_without_lightning = onchain_uni_payment.split("&lightning=").next().unwrap();
	let txid = match node_a.unified_payment().send(&uri_str_without_lightning, None, None).await {
		Ok(UnifiedPaymentResult::Bolt12 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt12")
		},
		Ok(UnifiedPaymentResult::Bolt11 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt11");
		},
		Ok(UnifiedPaymentResult::Onchain { txid }) => {
			println!("\nOn-chain transaction successful with Txid: {}", txid);
			txid
		},
		Err(e) => {
			panic!("Expected on-chain payment but got error: {:?}", e);
		},
	};

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	wait_for_tx(&electrsd.client, txid).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_b.list_balances().total_onchain_balance_sats, 800_000);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 200_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_client_service_integration() {
	do_lsps2_client_service_integration(true).await;
	do_lsps2_client_service_integration(false).await;
}

async fn do_lsps2_client_service_integration(client_trusts_lsp: bool) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp,
	};

	let service_config = random_config(true);
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.set_liquidity_provider_lsps2(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();

	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config(true);
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.set_liquidity_source_lsps2(service_node_id, service_addr, None);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();

	let payer_config = random_config(true);
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let service_addr = service_node.onchain_payment().new_address().unwrap();
	let client_addr = client_node.onchain_payment().new_address().unwrap();
	let payer_addr = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr, client_addr, payer_addr],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();

	// Open a channel payer -> service that will allow paying the JIT invoice
	println!("Opening channel payer_node -> service_node!");
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel(jit_amount_msat, &invoice_description.into(), 1024, None)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_event!(service_node, PaymentForwarded);
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	let client_payment_id =
		expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();
	let client_payment = client_node.payment(&client_payment_id).unwrap();
	match client_payment.kind {
		PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(service_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let expected_channel_overprovisioning_msat =
		(expected_received_amount_msat * channel_over_provisioning_ppm as u64) / 1_000_000;
	let expected_channel_size_sat =
		(expected_received_amount_msat + expected_channel_overprovisioning_msat) / 1000;
	let channel_value_sats = client_node.list_channels().first().unwrap().channel_value_sats;
	assert_eq!(channel_value_sats, expected_channel_size_sat);

	println!("Generating regular invoice!");
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap()).into();
	let amount_msat = 5_000_000;
	let invoice =
		client_node.bolt11_payment().receive(amount_msat, &invoice_description, 1024).unwrap();

	// Have the payer_node pay the invoice, to check regular forwards service_node -> client_node
	// are working as expected.
	println!("Paying regular invoice!");
	let payment_id = payer_node.bolt11_payment().send(&invoice, None).unwrap();
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	expect_event!(service_node, PaymentForwarded);
	expect_payment_received_event!(client_node, amount_msat);

	////////////////////////////////////////////////////////////////////////////
	// receive_via_jit_channel_for_hash and claim_for_hash
	////////////////////////////////////////////////////////////////////////////
	println!("Generating JIT invoice!");
	// Increase the amount to make sure it does not fit into the existing channels.
	let jit_amount_msat = 200_000_000;
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description,
			1024,
			None,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	let claimable_amount_msat = expect_payment_claimable_event!(
		client_node,
		payment_id,
		manual_payment_hash,
		expected_received_amount_msat
	);
	println!("Claiming payment!");
	client_node
		.bolt11_payment()
		.claim_for_hash(manual_payment_hash, claimable_amount_msat, manual_preimage)
		.unwrap();

	expect_event!(service_node, PaymentForwarded);
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	let client_payment_id =
		expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();
	let client_payment = client_node.payment(&client_payment_id).unwrap();
	match client_payment.kind {
		PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(service_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	////////////////////////////////////////////////////////////////////////////
	// receive_via_jit_channel_for_hash and fail_for_hash
	////////////////////////////////////////////////////////////////////////////
	println!("Generating JIT invoice!");
	// Increase the amount to make sure it does not fit into the existing channels.
	let jit_amount_msat = 400_000_000;
	let manual_preimage = PaymentPreimage([43u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description,
			1024,
			None,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	expect_payment_claimable_event!(
		client_node,
		payment_id,
		manual_payment_hash,
		expected_received_amount_msat
	);
	println!("Failing payment!");
	client_node.bolt11_payment().fail_for_hash(manual_payment_hash).unwrap();

	expect_event!(payer_node, PaymentFailed);
	assert_eq!(client_node.payment(&payment_id).unwrap().status, PaymentStatus::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn facade_logging() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let logger = init_log_logger(LevelFilter::Trace);
	let mut config = random_config(false);
	config.log_writer = TestLogWriter::LogFacade;

	println!("== Facade logging starts ==");
	let _node = setup_node(&chain_source, config);

	assert!(!logger.retrieve_logs().is_empty());
	for (_, entry) in logger.retrieve_logs().iter().enumerate() {
		validate_log_entry(entry);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn spontaneous_send_with_custom_preimage() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_sat = 1_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 500_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let seed = b"test_payment_preimage";
	let bytes: Sha256Hash = Sha256Hash::hash(seed);
	let custom_bytes = bytes.to_byte_array();
	let custom_preimage = PaymentPreimage(custom_bytes);

	let amount_msat = 100_000;
	let payment_id = node_a
		.spontaneous_payment()
		.send_with_preimage(amount_msat, node_b.node_id(), custom_preimage, None)
		.unwrap();

	// check payment status and verify stored preimage
	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let details: PaymentDetails =
		node_a.list_payments_with_filter(|p| p.id == payment_id).first().unwrap().clone();
	assert_eq!(details.status, PaymentStatus::Succeeded);
	if let PaymentKind::Spontaneous { preimage: Some(pi), .. } = details.kind {
		assert_eq!(pi.0, custom_bytes);
	} else {
		panic!("Expected a spontaneous PaymentKind with a preimage");
	}

	// Verify receiver side (node_b)
	expect_payment_received_event!(node_b, amount_msat);
	let receiver_payments: Vec<PaymentDetails> = node_b.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Inbound
			&& matches!(p.kind, PaymentKind::Spontaneous { .. })
	});

	assert_eq!(receiver_payments.len(), 1);
	let receiver_details = &receiver_payments[0];
	assert_eq!(receiver_details.status, PaymentStatus::Succeeded);
	assert_eq!(receiver_details.amount_msat, Some(amount_msat));
	assert_eq!(receiver_details.direction, PaymentDirection::Inbound);

	// Verify receiver also has the same preimage
	if let PaymentKind::Spontaneous { preimage: Some(pi), .. } = &receiver_details.kind {
		assert_eq!(pi.0, custom_bytes);
	} else {
		panic!("Expected receiver to have spontaneous PaymentKind with preimage");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn drop_in_async_context() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(true);
	let node = setup_node(&chain_source, config);
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_client_trusts_lsp() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: true,
	};

	let service_config = random_config(true);
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.set_liquidity_provider_lsps2(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();
	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config(true);
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.set_liquidity_source_lsps2(service_node_id, service_addr.clone(), None);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();
	let client_node_id = client_node.node_id();

	let payer_config = random_config(true);
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let service_addr_onchain = service_node.onchain_payment().new_address().unwrap();
	let client_addr_onchain = client_node.onchain_payment().new_address().unwrap();
	let payer_addr_onchain = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr_onchain, client_addr_onchain, payer_addr_onchain],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	println!("Premine complete!");
	// Open a channel payer -> service that will allow paying the JIT invoice
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let res = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description.into(),
			1024,
			None,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&res, None).unwrap();
	println!("Payment ID: {:?}", payment_id);
	let funding_txo = expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	// Check the funding transaction hasn't been broadcasted yet and nodes aren't seeing it.
	println!("Try to find funding tx... It won't be found yet, as the client has not claimed it.");
	tokio::time::sleep(std::time::Duration::from_secs(3)).await;
	let mempool = bitcoind.client.get_raw_mempool().unwrap().into_model().unwrap();
	let funding_tx_found = mempool.0.iter().any(|txid| *txid == funding_txo.txid);
	assert!(!funding_tx_found, "Funding transaction should NOT be broadcast yet");

	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	assert_eq!(
		client_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty_node_id == service_node_id)
			.unwrap()
			.confirmations,
		Some(0)
	);
	assert_eq!(
		service_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty_node_id == client_node_id)
			.unwrap()
			.confirmations,
		Some(0)
	);

	// Now claim the JIT payment, which should release the funding transaction
	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;

	let _ = expect_payment_claimable_event!(
		client_node,
		payment_id,
		manual_payment_hash,
		expected_received_amount_msat
	);

	client_node
		.bolt11_payment()
		.claim_for_hash(manual_payment_hash, jit_amount_msat, manual_preimage)
		.unwrap();

	expect_payment_successful_event!(payer_node, Some(payment_id), None);

	let _ = expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();

	// Check the nodes pick up on the confirmed funding tx now.
	wait_for_tx(&electrsd.client, funding_txo.txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	assert_eq!(
		client_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty_node_id == service_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
	assert_eq!(
		service_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty_node_id == client_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_lsp_trusts_client_but_client_does_not_claim() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: false,
	};

	let service_config = random_config(true);
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.set_liquidity_provider_lsps2(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();

	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config(true);
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.set_liquidity_source_lsps2(service_node_id, service_addr.clone(), None);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();

	let client_node_id = client_node.node_id();

	let payer_config = random_config(true);
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let service_addr_onchain = service_node.onchain_payment().new_address().unwrap();
	let client_addr_onchain = client_node.onchain_payment().new_address().unwrap();
	let payer_addr_onchain = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr_onchain, client_addr_onchain, payer_addr_onchain],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	println!("Premine complete!");
	// Open a channel payer -> service that will allow paying the JIT invoice
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let res = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description.into(),
			1024,
			None,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let _payment_id = payer_node.bolt11_payment().send(&res, None).unwrap();
	let funding_txo = expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());
	println!("Waiting for funding transaction to be broadcast...");

	// Check the nodes pick up on the confirmed funding tx now.
	wait_for_tx(&electrsd.client, funding_txo.txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	assert_eq!(
		client_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty_node_id == service_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
	assert_eq!(
		service_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty_node_id == client_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn payment_persistence_after_restart() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup nodes manually so we can restart node_a with the same config
	println!("== Node A ==");
	let mut config_a = random_config(true);
	config_a.store_type = TestStoreType::Sqlite;

	let num_payments = 200;
	let payment_amount_msat = 1_000_000; // 1000 sats per payment

	{
		let node_a = setup_node(&chain_source, config_a.clone());

		println!("\n== Node B ==");
		let config_b = random_config(true);
		let node_b = setup_node(&chain_source, config_b);

		let addr_a = node_a.onchain_payment().new_address().unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		// Premine sufficient funds for a large channel and many payments
		let premine_amount_sat = 10_000_000;
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr_a, addr_b],
			Amount::from_sat(premine_amount_sat),
		)
		.await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
		assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

		// Open a large channel from node_a to node_b
		let channel_amount_sat = 5_000_000;
		open_channel(&node_a, &node_b, channel_amount_sat, true, &electrsd).await;
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		expect_channel_ready_event!(node_a, node_b.node_id());
		expect_channel_ready_event!(node_b, node_a.node_id());

		// Send 200 payments from node_a to node_b
		println!("\nSending {} payments from A to B...", num_payments);
		let invoice_description =
			Bolt11InvoiceDescription::Direct(Description::new(String::from("test")).unwrap());

		for i in 0..num_payments {
			let invoice = node_b
				.bolt11_payment()
				.receive(payment_amount_msat, &invoice_description.clone().into(), 3600)
				.unwrap();
			let payment_id = node_a.bolt11_payment().send(&invoice, None).unwrap();
			expect_event!(node_a, PaymentSuccessful);
			expect_event!(node_b, PaymentReceived);

			if (i + 1) % 50 == 0 {
				println!("Completed {} payments", i + 1);
			}

			// Verify payment succeeded
			assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
		}
		println!("All {} payments completed successfully", num_payments);

		// Verify node_a has 200 outbound Bolt11 payments before shutdown
		let outbound_payments_before = node_a.list_payments_with_filter(|p| {
			p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Bolt11 { .. })
		});
		assert_eq!(outbound_payments_before.len(), num_payments);

		// Shut down both nodes
		println!("\nShutting down nodes...");
		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	// Restart node_a with the same config
	println!("\nRestarting node A...");
	let restarted_node_a = setup_node(&chain_source, config_a);

	// Assert all 200 payments are still in the store
	let outbound_payments_after = restarted_node_a.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Outbound && matches!(p.kind, PaymentKind::Bolt11 { .. })
	});
	assert_eq!(
		outbound_payments_after.len(),
		num_payments,
		"Expected {} payments after restart, found {}",
		num_payments,
		outbound_payments_after.len()
	);

	// Verify all payments have the correct status
	for payment in &outbound_payments_after {
		assert_eq!(
			payment.status,
			PaymentStatus::Succeeded,
			"Payment {:?} has unexpected status {:?}",
			payment.id,
			payment.status
		);
		assert_eq!(payment.amount_msat, Some(payment_amount_msat));
	}

	println!(
		"Successfully verified {} payments persisted after restart",
		outbound_payments_after.len()
	);

	restarted_node_a.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn persistence_backwards_compatibility() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let storage_path = common::random_storage_path().to_str().unwrap().to_owned();
	let seed_bytes = [42u8; 64];

	// Setup a v0.6.2 `Node`
	let (old_balance, old_node_id) = {
		let mut builder_old = ldk_node_062::Builder::new();
		builder_old.set_network(bitcoin::Network::Regtest);
		builder_old.set_storage_dir_path(storage_path.clone());
		builder_old.set_entropy_seed_bytes(seed_bytes);
		builder_old.set_chain_source_esplora(esplora_url.clone(), None);
		let node_old = builder_old.build().unwrap();

		node_old.start().unwrap();
		let addr_old = node_old.onchain_payment().new_address().unwrap();
		common::premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr_old],
			bitcoin::Amount::from_sat(100_000),
		)
		.await;
		node_old.sync_wallets().unwrap();

		let balance = node_old.list_balances().spendable_onchain_balance_sats;
		assert!(balance > 0);
		let node_id = node_old.node_id();

		node_old.stop().unwrap();

		(balance, node_id)
	};

	// Now ensure we can still reinit from the same backend.
	#[cfg(feature = "uniffi")]
	let builder_new = Builder::new();
	#[cfg(not(feature = "uniffi"))]
	let mut builder_new = Builder::new();
	builder_new.set_network(bitcoin::Network::Regtest);
	builder_new.set_storage_dir_path(storage_path);
	builder_new.set_chain_source_esplora(esplora_url, None);

	#[cfg(feature = "uniffi")]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes.to_vec()).unwrap();
	#[cfg(not(feature = "uniffi"))]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes);
	let node_new = builder_new.build(node_entropy.into()).unwrap();

	node_new.start().unwrap();
	node_new.sync_wallets().unwrap();

	let new_balance = node_new.list_balances().spendable_onchain_balance_sats;
	let new_node_id = node_new.node_id();

	assert_eq!(old_node_id, new_node_id);
	assert_eq!(old_balance, new_balance);

	node_new.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn open_channel_with_all_with_anchors() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let funding_txo = open_channel_with_all(&node_a, &node_b, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let _user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	// After opening a channel with all balance, the remaining on-chain balance should only
	// be the anchor reserve (25k sats by default) plus a small margin for change
	let anchor_reserve_sat = 25_000;
	let remaining_balance = node_a.list_balances().spendable_onchain_balance_sats;
	assert!(
		remaining_balance < anchor_reserve_sat + 500,
		"Remaining balance {remaining_balance} should be close to the anchor reserve {anchor_reserve_sat}"
	);

	// Verify a channel was opened with most of the funds
	let channels = node_a.list_channels();
	assert_eq!(channels.len(), 1);
	let channel = &channels[0];
	assert!(channel.channel_value_sats > premine_amount_sat - anchor_reserve_sat - 500);
	assert_eq!(channel.counterparty_node_id, node_b.node_id());
	assert_eq!(channel.funding_txo.unwrap(), funding_txo);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn open_channel_with_all_without_anchors() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let funding_txo = open_channel_with_all(&node_a, &node_b, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let _user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	// Without anchors, there should be no remaining balance
	let remaining_balance = node_a.list_balances().spendable_onchain_balance_sats;
	assert_eq!(
		remaining_balance, 0,
		"Remaining balance {remaining_balance} should be zero without anchor reserve"
	);

	// Verify a channel was opened with all the funds accounting for fees
	let channels = node_a.list_channels();
	assert_eq!(channels.len(), 1);
	let channel = &channels[0];
	assert!(channel.channel_value_sats > premine_amount_sat - 500);
	assert_eq!(channel.counterparty_node_id, node_b.node_id());
	assert_eq!(channel.funding_txo.unwrap(), funding_txo);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}
