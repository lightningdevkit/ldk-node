// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(eclair_test)]

mod common;

use std::str::FromStr;

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

/// Run a shell command via `spawn_blocking` to avoid blocking the tokio runtime.
async fn run_cmd(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
	let program = program.to_string();
	let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
	tokio::task::spawn_blocking(move || std::process::Command::new(&program).args(&args).output())
		.await
		.expect("spawn_blocking panicked")
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

	// Recreate the Eclair container between tests to give a fresh /data
	// directory, a new seed, and a clean initialization against the current
	// chain tip.
	let container_name =
		std::env::var("ECLAIR_CONTAINER_NAME").unwrap_or_else(|_| "ldk-node-eclair-1".to_string());
	run_cmd("docker", &["rm", "-f", &container_name]).await.ok();

	// Unlock UTXOs and start Eclair, retrying if locked UTXOs remain.
	// Force-close transactions can lock new UTXOs between the unlock call
	// and Eclair startup, so we may need multiple attempts.
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
	let mut attempt = 0;
	loop {
		// Unlock any UTXOs left locked in the Eclair wallet.
		run_cmd(
			"curl",
			&[
				"-s",
				"--max-time",
				"5",
				"--user",
				"user:pass",
				"--data-binary",
				r#"{"jsonrpc":"1.0","method":"lockunspent","params":[true]}"#,
				"-H",
				"content-type: text/plain;",
				"http://127.0.0.1:18443/wallet/eclair",
			],
		)
		.await
		.ok();

		if attempt > 0 {
			// On retry, recreate the container since Eclair exited.
			run_cmd("docker", &["rm", "-f", &container_name]).await.ok();
		}
		let output = run_cmd(
			"docker",
			&["compose", "-f", "docker-compose-eclair.yml", "up", "-d", "eclair"],
		)
		.await
		.expect("failed to spawn docker compose");
		assert!(
			output.status.success(),
			"docker compose up failed (exit {}): {}",
			output.status,
			String::from_utf8_lossy(&output.stderr),
		);

		// Wait for Eclair to become ready.
		let mut ready = false;
		for _ in 0..30 {
			if std::time::Instant::now() >= deadline {
				let logs = run_cmd("docker", &["logs", "--tail", "50", &container_name]).await.ok();
				if let Some(l) = logs {
					eprintln!(
						"=== Eclair container logs ===\n{}{}",
						String::from_utf8_lossy(&l.stdout),
						String::from_utf8_lossy(&l.stderr)
					);
				}
				panic!("Eclair did not start within 90s (after {} attempts)", attempt + 1);
			}
			tokio::time::sleep(std::time::Duration::from_secs(1)).await;
			ready = run_cmd(
				"curl",
				&[
					"-s",
					"--max-time",
					"2",
					"-u",
					":eclairpassword",
					"-X",
					"POST",
					"http://127.0.0.1:8080/getinfo",
				],
			)
			.await
			.map(|o| o.status.success() && !o.stdout.is_empty())
			.unwrap_or(false);
			if ready {
				break;
			}
		}

		if ready {
			break;
		}

		// Eclair likely failed due to locked UTXOs — retry.
		attempt += 1;
		eprintln!("Eclair failed to start (attempt {}), retrying after lockunspent...", attempt);
	}

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
