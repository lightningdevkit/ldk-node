// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(cln_test)]

mod common;

use std::str::FromStr;

use common::cln::TestClnNode;
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

async fn setup_clients() -> (BitcoindClient, ElectrumClient, TestClnNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	let cln = TestClnNode::from_env();
	(bitcoind, electrs, cln)
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
async fn test_cln_basic_channel_cycle() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;

	let (user_channel_id, _ext_channel_id) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	// LDK -> CLN payment
	let invoice = cln.create_invoice(10_000_000, "cln-test-send").await.unwrap();
	let parsed = lightning_invoice::Bolt11Invoice::from_str(&invoice).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	// CLN -> LDK payment
	receive_bolt11_payment(&node, &cln, 10_000_000).await;

	cooperative_close_by_ldk(&node, &cln, &bitcoind, &electrs, &user_channel_id).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_disconnect_reconnect() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	disconnect_reconnect_idle(&node, &cln, &bitcoind, &electrs, &Side::Ldk).await;
	disconnect_reconnect_idle(&node, &cln, &bitcoind, &electrs, &Side::External).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_force_close_by_ldk() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	force_close_by_ldk(&node, &cln, &bitcoind, &electrs, &user_ch).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_force_close_by_external() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	force_close_by_external(&node, &cln, &bitcoind, &electrs, &ext_ch).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "CLN splicing requires --experimental-splicing flag and CLN v25+"]
async fn test_cln_splice() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	splice_from_external(&node, &cln, &bitcoind, &electrs, &ext_ch).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "CLN splicing requires --experimental-splicing flag and CLN v25+"]
async fn test_cln_splice_then_payment() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	splice_then_payment(&node, &cln, &bitcoind, &electrs, &ext_ch).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_inbound_channel() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	run_inbound_channel_test(
		&node,
		&cln,
		&bitcoind,
		&electrs,
		CloseType::Cooperative,
		Side::External,
	)
	.await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_receive_payments() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	receive_bolt11_payment(&node, &cln, 5_000_000).await;

	node.stop().unwrap();
}

/// CLN v24.08+ includes a `payment_secret` in outbound keysend HTLCs.
/// LDK treats any inbound HTLC with `payment_secret` as a BOLT11 payment and
/// verifies it against a stored invoice — which fails for spontaneous payments.
/// Upstream LDK fix needed: skip payment_secret verification when a valid
/// `keysend_preimage` TLV is present.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "CLN v24.08+ sends payment_secret in keysend — LDK rejects (upstream fix needed)"]
async fn test_cln_receive_keysend() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	receive_keysend_payment(&node, &cln, 5_000_000).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_bidirectional_payments() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	bidirectional_payments(&node, &cln, &ext_ch, 5_000_000).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_pay_expired_invoice() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	pay_expired_invoice(&node, &cln).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_concurrent_payments() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	concurrent_payments(&node, &cln, 5, 1_000_000).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_cooperative_close_after_fee_change() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	cooperative_close_after_fee_change(&node, &cln, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_force_close_after_fee_change() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	force_close_after_fee_change(&node, &cln, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

interop_combo_tests!(test_cln, setup_clients, setup_ldk_node);
