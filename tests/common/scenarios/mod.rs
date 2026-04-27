// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Shared interop test scenarios, generic over `ExternalNode`.
//!
//! - `channel` / `payment` / `connectivity` -- composable building blocks
//! - `interop_tests!` macro -- emits one `#[tokio::test]` per scenario

pub(crate) mod channel;
pub(crate) mod connectivity;
pub(crate) mod payment;

use std::future::Future;
use std::time::Duration;

use bitcoin::Amount;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};

use super::external_node::ExternalNode;
use super::{generate_blocks_and_wait, premine_and_distribute_funds};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Side {
	Ldk,
	External,
}

/// Retry an async operation with 1s delay; used for ops that may fail due to gossip delay.
pub(crate) async fn retry_until_ok<F, Fut, T, E>(max_attempts: u32, operation: &str, mut f: F) -> T
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<T, E>>,
	E: std::fmt::Display,
{
	for attempt in 1..=max_attempts {
		match f().await {
			Ok(val) => return val,
			Err(e) => {
				if attempt == max_attempts {
					panic!("{} failed after {} attempts: {}", operation, max_attempts, e);
				}
				tokio::time::sleep(Duration::from_secs(1)).await;
			},
		}
	}
	unreachable!()
}

/// Sync wallets, retrying on `WalletOperationTimeout`.
pub(crate) async fn sync_wallets_with_retry(node: &Node) {
	for attempt in 0..3 {
		match node.sync_wallets() {
			Ok(()) => return,
			Err(ldk_node::NodeError::WalletOperationTimeout) if attempt < 2 => {
				tokio::time::sleep(Duration::from_secs(5)).await;
			},
			Err(e) => panic!("sync_wallets failed: {:?}", e),
		}
	}
}

/// Wait until the peer reports 0 pending HTLCs on the channel; required before close because
/// `PaymentSuccessful` fires one round-trip before the HTLC is removed from peer commitment.
pub(crate) async fn wait_for_htlcs_settled(
	peer: &(impl ExternalNode + ?Sized), ext_channel_id: &str,
) {
	for _ in 0..30 {
		let channels = tokio::time::timeout(Duration::from_secs(5), peer.list_channels())
			.await
			.ok()
			.and_then(|r| r.ok());
		if let Some(channels) = channels {
			if let Some(ch) = channels.iter().find(|c| c.channel_id == ext_channel_id) {
				if ch.pending_htlcs_count == 0 {
					return;
				}
			}
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
	panic!("HTLCs did not settle on {} channel {} within 15s", peer.name(), ext_channel_id);
}

/// Build a fresh LDK node configured for interop tests. Uses electrum at the
/// docker-compose default port and bumps sync timeouts for combo stress.
pub(crate) fn setup_ldk_node() -> Node {
	let config = crate::common::random_config(true);
	let mut builder = ldk_node::Builder::from_config(config.node_config);
	let mut sync_config = ldk_node::config::ElectrumSyncConfig::default();
	sync_config.timeouts_config.onchain_wallet_sync_timeout_secs = 180;
	sync_config.timeouts_config.lightning_wallet_sync_timeout_secs = 120;
	builder.set_chain_source_electrum("tcp://127.0.0.1:50001".to_string(), Some(sync_config));
	let node = builder.build(config.node_entropy).unwrap();
	node.start().unwrap();
	node
}

/// Fund both LDK node and external node, connect them.
pub(crate) async fn setup_interop_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let ldk_address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(50_000_000);
	premine_and_distribute_funds(bitcoind, electrs, vec![ldk_address], premine_amount).await;

	// Fund the peer via the ldk_node_test wallet loaded by premine_and_distribute_funds.
	let ext_funding_addr_str = peer.get_funding_address().await.unwrap();
	let ext_amount = Amount::from_sat(50_000_000);
	let amounts_json = serde_json::json!({&ext_funding_addr_str: ext_amount.to_btc()});
	let empty_account = serde_json::json!("");
	bitcoind
		.call::<serde_json::Value>(
			"sendmany",
			&[empty_account, amounts_json, serde_json::json!(0), serde_json::json!("")],
		)
		.expect("failed to fund external node");
	generate_blocks_and_wait(bitcoind, electrs, 1).await;

	// Block until the peer indexes the funding tx, else channel opens time out.
	let chain_height: u64 = bitcoind.get_blockchain_info().unwrap().blocks.try_into().unwrap();
	peer.wait_for_block_sync(chain_height).await.unwrap();

	sync_wallets_with_retry(node).await;

	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();
	node.connect(ext_node_id, ext_addr, true).unwrap();
}

/// Drive a scenario end-to-end: fund LDK + peer, run the scenario, stop the node.
/// Each `#[tokio::test]` in the integration-test files calls this with the
/// per-impl `setup_clients` future and a scenario fn.
pub(crate) async fn run_interop_scenario<N, E, F>(
	setup_fut: impl Future<Output = (BitcoindClient, E, N)>, scenario: F,
) where
	N: ExternalNode,
	E: ElectrumApi,
	F: AsyncFnOnce(&Node, &N, &BitcoindClient, &E),
{
	let (bitcoind, electrs, ext) = setup_fut.await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &ext, &bitcoind, &electrs).await;
	scenario(&node, &ext, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

/// Open a channel, send a BOLT11 payment in each direction, then cooperatively close.
pub(crate) async fn basic_channel_cycle_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	payment::send_bolt11_to_peer(node, peer, 10_000_000, "basic-send").await;
	payment::receive_bolt11_payment(node, peer, 10_000_000).await;

	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, send keysend in both directions, then cooperatively close.
pub(crate) async fn keysend_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	payment::send_keysend_to_peer(node, peer, 5_000_000).await;
	payment::receive_keysend_payment(node, peer, 5_000_000).await;
	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, send a payment, then force-close from the LDK side.
pub(crate) async fn force_close_after_payment_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	payment::send_bolt11_to_peer(node, peer, 5_000_000, "force-close").await;
	wait_for_htlcs_settled(peer, &ext_ch).await;
	channel::force_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, dispatch a payment with a mid-flight disconnect+reconnect,
/// then cooperatively close.
pub(crate) async fn disconnect_during_payment_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	connectivity::disconnect_during_payment(node, peer, &Side::Ldk).await;
	wait_for_htlcs_settled(peer, &ext_ch).await;
	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, splice-in additional funds, send a post-splice payment, then close.
pub(crate) async fn splice_in_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.splice_in(&user_ch, ext_node_id, 500_000).unwrap();
	expect_splice_pending_event!(node, ext_node_id);
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	sync_wallets_with_retry(node).await;
	expect_channel_ready_event!(node, ext_node_id);

	payment::send_bolt11_to_peer(node, peer, 5_000_000, "post-splice").await;

	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}
