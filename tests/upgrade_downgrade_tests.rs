// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

// This file is a downgrade monitoring canary for serialized LDK Node state, not a
// compatibility test for the filesystem-store IO layout itself. The current
// `build_with_fs_store` path writes filesystem-store v2 data, while LDK Node v0.7.0
// reads filesystem-store v1 data. There is no supported v2-to-v1 IO-layer downgrade:
// v2 stores empty namespaces under `[empty]`, which v1 readers do not look up.
//
// To keep monitoring whether the serialized node/channel/payment state remains
// understandable by v0.7.0, these tests intentionally write current state through
// the legacy v1 filesystem-store implementation via `build_with_store`, then
// reopen it with v0.7.0's `build_with_fs_store`.

#[allow(unused_imports, unused_macros)]
mod common;

use std::path::PathBuf;
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
use common::{
	generate_blocks_and_wait, generate_listening_addresses, premine_and_distribute_funds,
	random_storage_path, setup_bitcoind_and_electrsd, wait_for_tx,
};
use ldk_node::config::{Config, EsploraSyncConfig};
use ldk_node::entropy::NodeEntropy;
use ldk_node::lightning::ln::msgs::SocketAddress as CurrentSocketAddress;
use ldk_node::lightning_invoice::{
	Bolt11InvoiceDescription as CurrentBolt11InvoiceDescription, Description as CurrentDescription,
};
use lightning_persister::fs_store::v1::FilesystemStore;

#[cfg(feature = "uniffi")]
type CurrentNode = std::sync::Arc<ldk_node::Node>;
#[cfg(not(feature = "uniffi"))]
type CurrentNode = ldk_node::Node;

const NODE_A_SEED_BYTES: [u8; 64] = [42; 64];
const NODE_B_SEED_BYTES: [u8; 64] = [43; 64];
const FUNDING_AMOUNT_SAT: u64 = 2_000_000;
const CHANNEL_AMOUNT_SAT: u64 = 1_000_000;
const PUSH_AMOUNT_MSAT: u64 = 500_000_000;
const PRE_DOWNGRADE_PAYMENT_MSAT: u64 = 100_000;
const POST_DOWNGRADE_PAYMENT_MSAT: u64 = 200_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn monitor_v0_7_0_serialization_downgrade_channel_payment() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let storage_path_a = random_storage_path().to_str().unwrap().to_owned();
	let storage_path_b = random_storage_path().to_str().unwrap().to_owned();
	let current_addresses_a = generate_listening_addresses();
	let current_addresses_b = generate_listening_addresses();
	let v070_addresses_a = to_v070_socket_addresses(&current_addresses_a);
	let v070_addresses_b = to_v070_socket_addresses(&current_addresses_b);

	let node_id_a;
	let node_id_b;
	let pre_downgrade_payment_id;

	{
		let node_a = build_current_node(
			storage_path_a.clone(),
			NODE_A_SEED_BYTES,
			current_addresses_a.clone(),
			"downgrade-a",
			&esplora_url,
		);
		let node_b = build_current_node(
			storage_path_b.clone(),
			NODE_B_SEED_BYTES,
			current_addresses_b.clone(),
			"downgrade-b",
			&esplora_url,
		);
		node_id_a = node_a.node_id();
		node_id_b = node_b.node_id();

		let addr_a = node_a.onchain_payment().new_address().unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr_a, addr_b],
			Amount::from_sat(FUNDING_AMOUNT_SAT),
		)
		.await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, FUNDING_AMOUNT_SAT);
		assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, FUNDING_AMOUNT_SAT);

		let funding_txo = open_current_channel(&node_a, &node_b).await;
		wait_for_tx(&electrsd.client, funding_txo.txid).await;
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		expect_current_channel_ready(&node_a, node_id_b).await;
		expect_current_channel_ready(&node_b, node_id_a).await;
		assert_current_channel_ready(&node_a, node_id_b);
		assert_current_channel_ready(&node_b, node_id_a);

		pre_downgrade_payment_id = send_current_bolt11_payment(
			&node_a,
			&node_b,
			PRE_DOWNGRADE_PAYMENT_MSAT,
			"pre-downgrade",
		)
		.await;

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	let node_a_v070 = build_v070_node(
		storage_path_a,
		NODE_A_SEED_BYTES,
		v070_addresses_a.clone(),
		"downgrade-a",
		&esplora_url,
	);
	let node_b_v070 = build_v070_node(
		storage_path_b,
		NODE_B_SEED_BYTES,
		v070_addresses_b.clone(),
		"downgrade-b",
		&esplora_url,
	);

	assert_eq!(node_a_v070.node_id(), node_id_a);
	assert_eq!(node_b_v070.node_id(), node_id_b);

	let pre_downgrade_payment_id =
		ldk_node_070::lightning::ln::channelmanager::PaymentId(pre_downgrade_payment_id.0);
	assert_v070_bolt11_payment(
		&node_a_v070,
		&pre_downgrade_payment_id,
		ldk_node_070::payment::PaymentDirection::Outbound,
		PRE_DOWNGRADE_PAYMENT_MSAT,
	);
	assert_v070_bolt11_payment(
		&node_b_v070,
		&pre_downgrade_payment_id,
		ldk_node_070::payment::PaymentDirection::Inbound,
		PRE_DOWNGRADE_PAYMENT_MSAT,
	);

	node_a_v070.sync_wallets().unwrap();
	node_b_v070.sync_wallets().unwrap();
	node_a_v070.connect(node_id_b, v070_addresses_b.first().unwrap().clone(), true).unwrap();
	wait_for_v070_usable_channel(&node_a_v070, node_id_b).await;
	wait_for_v070_usable_channel(&node_b_v070, node_id_a).await;
	drain_v070_events(&node_a_v070).await;
	drain_v070_events(&node_b_v070).await;

	send_v070_bolt11_payment(
		&node_a_v070,
		&node_b_v070,
		POST_DOWNGRADE_PAYMENT_MSAT,
		"post-downgrade",
	)
	.await;

	node_a_v070.stop().unwrap();
	node_b_v070.stop().unwrap();
}

fn build_current_node(
	storage_path: String, seed_bytes: [u8; 64], listening_addresses: Vec<CurrentSocketAddress>,
	alias: &str, esplora_url: &str,
) -> CurrentNode {
	let mut config = Config::default();
	config.network = bitcoin::Network::Regtest;
	config.storage_dir_path = storage_path;
	config.listening_addresses = Some(listening_addresses);
	config.anchor_channels_config = None;

	// Use the v1 filesystem layout that v0.7.0's filesystem builder can reopen.
	let mut fs_store_path = PathBuf::from(&config.storage_dir_path);
	fs_store_path.push("fs_store");
	#[allow(unused_mut)]
	let mut builder = ldk_node::Builder::from_config(config);
	builder.set_node_alias(alias.to_string()).unwrap();

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;
	builder.set_chain_source_esplora(esplora_url.to_owned(), Some(sync_config));

	#[cfg(feature = "uniffi")]
	let node_entropy = std::sync::Arc::new(NodeEntropy::from_seed_bytes(seed_bytes.to_vec()).unwrap());
	#[cfg(not(feature = "uniffi"))]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes);

	let kv_store = FilesystemStore::new(fs_store_path);
	let node = builder.build_with_store(node_entropy.into(), kv_store).unwrap();
	node.start().unwrap();
	node
}

fn build_v070_node(
	storage_path: String, seed_bytes: [u8; 64],
	listening_addresses: Vec<ldk_node_070::lightning::ln::msgs::SocketAddress>, alias: &str,
	esplora_url: &str,
) -> ldk_node_070::Node {
	let mut builder = ldk_node_070::Builder::new();
	builder.set_network(bitcoin::Network::Regtest);
	builder.set_storage_dir_path(storage_path);
	builder.set_entropy_seed_bytes(seed_bytes);
	builder.set_listening_addresses(listening_addresses).unwrap();
	builder.set_node_alias(alias.to_string()).unwrap();
	builder.set_chain_source_esplora(esplora_url.to_owned(), None);
	let node = builder.build_with_fs_store().unwrap();
	node.start().unwrap();
	node
}

async fn open_current_channel(node_a: &CurrentNode, node_b: &CurrentNode) -> bitcoin::OutPoint {
	node_a
		.open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			CHANNEL_AMOUNT_SAT,
			Some(PUSH_AMOUNT_MSAT),
			None,
		)
		.unwrap();

	let funding_txo_a = expect_current_channel_pending(node_a, node_b.node_id()).await;
	let funding_txo_b = expect_current_channel_pending(node_b, node_a.node_id()).await;
	assert_eq!(funding_txo_a, funding_txo_b);
	funding_txo_a
}

async fn send_current_bolt11_payment(
	payer: &CurrentNode, payee: &CurrentNode, amount_msat: u64, description: &str,
) -> ldk_node::lightning::ln::channelmanager::PaymentId {
	let invoice_description = CurrentBolt11InvoiceDescription::Direct(
		CurrentDescription::new(description.to_owned()).unwrap(),
	);
	let invoice = payee
		.bolt11_payment()
		.receive(amount_msat, &invoice_description.clone().into(), 3600)
		.unwrap();
	let payment_id = payer.bolt11_payment().send(&invoice, None).unwrap();
	expect_current_payment_successful(payer, &payment_id).await;
	expect_current_payment_received(payee, amount_msat).await;
	assert_eq!(
		payer.payment(&payment_id).unwrap().status,
		ldk_node::payment::PaymentStatus::Succeeded
	);
	payment_id
}

async fn send_v070_bolt11_payment(
	payer: &ldk_node_070::Node, payee: &ldk_node_070::Node, amount_msat: u64, description: &str,
) {
	let invoice_description = ldk_node_070::lightning_invoice::Bolt11InvoiceDescription::Direct(
		ldk_node_070::lightning_invoice::Description::new(description.to_owned()).unwrap(),
	);
	let invoice = payee.bolt11_payment().receive(amount_msat, &invoice_description, 3600).unwrap();
	let payment_id = payer.bolt11_payment().send(&invoice, None).unwrap();
	expect_v070_payment_successful(payer, &payment_id).await;
	expect_v070_payment_received(payee, amount_msat).await;
	assert_eq!(
		payer.payment(&payment_id).unwrap().status,
		ldk_node_070::payment::PaymentStatus::Succeeded
	);
}

async fn expect_current_channel_pending(
	node: &CurrentNode, expected_counterparty: PublicKey,
) -> bitcoin::OutPoint {
	match next_current_event(node).await {
		ldk_node::Event::ChannelPending { counterparty_node_id, funding_txo, .. } => {
			assert_eq!(counterparty_node_id, expected_counterparty);
			node.event_handled().unwrap();
			funding_txo
		},
		event => panic!("{} got unexpected event: {:?}", node.node_id(), event),
	}
}

async fn expect_current_channel_ready(node: &CurrentNode, expected_counterparty: PublicKey) {
	match next_current_event(node).await {
		ldk_node::Event::ChannelReady { counterparty_node_id, .. } => {
			assert_eq!(counterparty_node_id, expected_counterparty);
			node.event_handled().unwrap();
		},
		event => panic!("{} got unexpected event: {:?}", node.node_id(), event),
	}
}

async fn expect_current_payment_successful(
	node: &CurrentNode, expected_payment_id: &ldk_node::lightning::ln::channelmanager::PaymentId,
) {
	match next_current_event(node).await {
		ldk_node::Event::PaymentSuccessful { payment_id, .. } => {
			assert_eq!(payment_id.as_ref(), Some(expected_payment_id));
			node.event_handled().unwrap();
		},
		event => panic!("{} got unexpected event: {:?}", node.node_id(), event),
	}
}

async fn expect_current_payment_received(node: &CurrentNode, expected_amount_msat: u64) {
	match next_current_event(node).await {
		ldk_node::Event::PaymentReceived { amount_msat, payment_id, .. } => {
			assert_eq!(amount_msat, expected_amount_msat);
			assert!(payment_id.is_some());
			node.event_handled().unwrap();
		},
		event => panic!("{} got unexpected event: {:?}", node.node_id(), event),
	}
}

async fn expect_v070_payment_successful(
	node: &ldk_node_070::Node,
	expected_payment_id: &ldk_node_070::lightning::ln::channelmanager::PaymentId,
) {
	match next_v070_event(node).await {
		ldk_node_070::Event::PaymentSuccessful { payment_id, .. } => {
			assert_eq!(payment_id.as_ref(), Some(expected_payment_id));
			node.event_handled().unwrap();
		},
		event => panic!("{} got unexpected event: {:?}", node.node_id(), event),
	}
}

async fn expect_v070_payment_received(node: &ldk_node_070::Node, expected_amount_msat: u64) {
	match next_v070_event(node).await {
		ldk_node_070::Event::PaymentReceived { amount_msat, payment_id, .. } => {
			assert_eq!(amount_msat, expected_amount_msat);
			assert!(payment_id.is_some());
			node.event_handled().unwrap();
		},
		event => panic!("{} got unexpected event: {:?}", node.node_id(), event),
	}
}

async fn next_current_event(node: &CurrentNode) -> ldk_node::Event {
	tokio::time::timeout(Duration::from_secs(common::INTEROP_TIMEOUT_SECS), node.next_event_async())
		.await
		.unwrap_or_else(|_| panic!("{} timed out waiting for event", node.node_id()))
}

async fn next_v070_event(node: &ldk_node_070::Node) -> ldk_node_070::Event {
	tokio::time::timeout(Duration::from_secs(common::INTEROP_TIMEOUT_SECS), node.next_event_async())
		.await
		.unwrap_or_else(|_| panic!("{} timed out waiting for event", node.node_id()))
}

async fn drain_v070_events(node: &ldk_node_070::Node) {
	while tokio::time::timeout(Duration::from_millis(250), node.next_event_async()).await.is_ok() {
		node.event_handled().unwrap();
	}
}

async fn wait_for_v070_usable_channel(node: &ldk_node_070::Node, counterparty_node_id: PublicKey) {
	for _ in 0..40 {
		let channels = node.list_channels();
		if let Some(channel) =
			channels.iter().find(|c| c.counterparty_node_id == counterparty_node_id)
		{
			assert_eq!(channel.channel_value_sats, CHANNEL_AMOUNT_SAT);
			if channel.is_channel_ready && channel.is_usable {
				return;
			}
		}
		tokio::time::sleep(Duration::from_millis(250)).await;
	}

	panic!(
		"{} failed to restore a usable v0.7.0 channel with {}",
		node.node_id(),
		counterparty_node_id
	);
}

fn assert_current_channel_ready(node: &CurrentNode, counterparty_node_id: PublicKey) {
	let channels = node.list_channels();
	let channel = channels.iter().find(|c| c.counterparty_node_id == counterparty_node_id).unwrap();
	assert_eq!(channel.channel_value_sats, CHANNEL_AMOUNT_SAT);
	assert!(channel.is_channel_ready);
}

fn assert_v070_bolt11_payment(
	node: &ldk_node_070::Node, payment_id: &ldk_node_070::lightning::ln::channelmanager::PaymentId,
	expected_direction: ldk_node_070::payment::PaymentDirection, expected_amount_msat: u64,
) {
	let payment = node.payment(payment_id).unwrap();
	assert_eq!(payment.amount_msat, Some(expected_amount_msat));
	assert_eq!(payment.direction, expected_direction);
	assert_eq!(payment.status, ldk_node_070::payment::PaymentStatus::Succeeded);
	assert!(matches!(payment.kind, ldk_node_070::payment::PaymentKind::Bolt11 { .. }));
}

fn to_v070_socket_addresses(
	addresses: &[CurrentSocketAddress],
) -> Vec<ldk_node_070::lightning::ln::msgs::SocketAddress> {
	addresses
		.iter()
		.map(|address| match address {
			CurrentSocketAddress::TcpIpV4 { addr, port } => {
				ldk_node_070::lightning::ln::msgs::SocketAddress::TcpIpV4 {
					addr: *addr,
					port: *port,
				}
			},
			_ => panic!("unexpected non-IPv4 test address: {:?}", address),
		})
		.collect()
}
