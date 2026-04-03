// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(eclair_test)]

mod common;

use base64::prelude::{Engine as _, BASE64_STANDARD};
use common::eclair::TestEclairNode;
use common::external_node::ExternalNode;
use common::scenarios::channel::{cooperative_close_by_ldk, open_channel_to_external};
use common::scenarios::payment::receive_keysend_payment;
use common::scenarios::setup_interop_test;
use common::scenarios::suite::{run_combos, run_scenarios};
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
async fn test_eclair() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;

	// Eclair-specific: verify keysend receive works
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;
	receive_keysend_payment(&node, &eclair, 5_000_000).await;
	cooperative_close_by_ldk(&node, &eclair, &bitcoind, &electrs, &user_ch).await;

	run_scenarios(&node, &eclair, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_combos() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	run_combos(&node, &eclair, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_send_keysend() {
	let (bitcoind, electrs, eclair) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	let ext_node_id = eclair.get_node_id().await.unwrap();
	node.spontaneous_payment().send(5_000_000, ext_node_id, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	cooperative_close_by_ldk(&node, &eclair, &bitcoind, &electrs, &user_ch).await;
	node.stop().unwrap();
}
