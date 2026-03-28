// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(eclair_test)]

mod common;

use std::str::FromStr;

use base64::prelude::{Engine as _, BASE64_STANDARD};
use common::eclair::TestEclairNode;
use common::external_node::ExternalNode;
use common::scenarios::channel::{
	cooperative_close_after_fee_change, cooperative_close_by_ldk, force_close_after_fee_change,
	force_close_by_external, force_close_by_ldk, open_channel_to_external,
	run_inbound_channel_test,
};
use common::scenarios::combo::run_interop_combo_test;
use common::scenarios::disconnect::disconnect_reconnect_idle;
use common::scenarios::payment::{
	bidirectional_payments, concurrent_payments, pay_expired_invoice, receive_bolt11_payment,
	receive_keysend_payment,
};
use common::scenarios::splice::{splice_from_external, splice_then_payment};
use common::scenarios::{setup_interop_test, CloseType, PayType, Phase, Side};
use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;
use ldk_node::{Builder, Event};

/// Unlock all UTXOs in the given bitcoind wallet via JSON-RPC.
async fn unlock_utxos(wallet_url: &str, user: &str, pass: &str) {
	let auth = BASE64_STANDARD.encode(format!("{}:{}", user, pass));
	let body = r#"{"jsonrpc":"1.0","method":"lockunspent","params":[true]}"#;
	let _ = bitreq::post(wallet_url)
		.with_header("Authorization", format!("Basic {}", auth))
		.with_header("Content-Type", "text/plain")
		.with_body(body)
		.with_timeout(5)
		.send_async()
		.await;
}

async fn setup_clients() -> (BitcoindClient, ElectrumClient, TestEclairNode) {
	// Use wallet-specific RPC URL to avoid multi-wallet conflicts.
	// Eclair loads its own "eclair" wallet on bitcoind, and our tests
	// create "ldk_node_test". With two wallets loaded, plain RPC calls
	// fail with "Wallet file not specified". Using the wallet URL
	// ensures our calls go to the right wallet.
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443/wallet/ldk_node_test",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	// Unlock any UTXOs left locked by previous force-close tests.
	unlock_utxos("http://127.0.0.1:18443/wallet/eclair", "user", "pass").await;

	let eclair = TestEclairNode::from_env();
	(bitcoind, electrs, eclair)
}

fn setup_ldk_node() -> ldk_node::Node {
	let config = common::random_config(true);
	let mut builder = Builder::from_config(config.node_config);
	builder.set_chain_source_electrum("tcp://127.0.0.1:50001".to_string(), None);
	let node = builder.build(config.node_entropy).unwrap();
	node.start().unwrap();
	node
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_basic_channel_cycle() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;

	let (user_channel_id, _ext_channel_id) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	// LDK -> Eclair payment
	let invoice = eclair.create_invoice(10_000_000, "eclair-test-send").await.unwrap();
	let parsed = lightning_invoice::Bolt11Invoice::from_str(&invoice).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	// Eclair -> LDK payment
	receive_bolt11_payment(&node, &eclair, 10_000_000).await;

	cooperative_close_by_ldk(&node, &eclair, &bitcoind, &electrs, &user_channel_id).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_disconnect_reconnect() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	disconnect_reconnect_idle(&node, &eclair, &bitcoind, &electrs, &Side::Ldk).await;
	disconnect_reconnect_idle(&node, &eclair, &bitcoind, &electrs, &Side::External).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_force_close_by_ldk() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	force_close_by_ldk(&node, &eclair, &bitcoind, &electrs, &user_ch).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_force_close_by_external() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	force_close_by_external(&node, &eclair, &bitcoind, &electrs, &ext_ch).await;
	node.stop().unwrap();
}

/// Eclair 0.8.0 rejects LDK keysend with `InvalidOnionPayload(8,0)` — LDK includes
/// `payment_data` (TLV type 8) in keysend onions, which Eclair considers invalid for
/// spontaneous payments. Eclair 0.14.0+ may handle this differently.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "Eclair 0.8.0 rejects LDK keysend with InvalidOnionPayload(8,0)"]
async fn test_eclair_send_keysend() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	let ext_node_id = eclair.get_node_id().await.unwrap();
	node.spontaneous_payment().send(5_000_000, ext_node_id, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "Eclair 0.8.0 does not support splicing (introduced in v0.10.0+)"]
async fn test_eclair_splice() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	splice_from_external(&node, &eclair, &bitcoind, &electrs, &ext_ch).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "Eclair 0.8.0 does not support splicing (introduced in v0.10.0+)"]
async fn test_eclair_splice_then_payment() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	splice_then_payment(&node, &eclair, &bitcoind, &electrs, &ext_ch).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_inbound_channel() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	run_inbound_channel_test(
		&node,
		&eclair,
		&bitcoind,
		&electrs,
		CloseType::Cooperative,
		Side::External,
	)
	.await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_receive_payments() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	receive_bolt11_payment(&node, &eclair, 5_000_000).await;
	receive_keysend_payment(&node, &eclair, 5_000_000).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_bidirectional_payments() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	bidirectional_payments(&node, &eclair, &ext_ch, 5_000_000).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_pay_expired_invoice() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	pay_expired_invoice(&node, &eclair).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_concurrent_payments() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	concurrent_payments(&node, &eclair, 5, 1_000_000).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_cooperative_close_after_fee_change() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	cooperative_close_after_fee_change(&node, &eclair, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_force_close_after_fee_change() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	force_close_after_fee_change(&node, &eclair, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

interop_combo_tests!(test_eclair, setup_clients, setup_ldk_node);
