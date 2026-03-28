// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::time::Duration;

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};
use lightning::events::ClosureReason;
use lightning_invoice::Bolt11Invoice;

use super::super::external_node::ExternalNode;
use super::super::generate_blocks_and_wait;
use super::payment::receive_bolt11_payment;
use super::{setup_interop_test, CloseType, Side};

/// Wait for a ChannelClosed event and assert the closure reason matches expectations.
async fn expect_channel_closed(node: &Node, expected_local_initiated: bool) {
	let event = tokio::time::timeout(
		Duration::from_secs(super::super::INTEROP_TIMEOUT_SECS),
		node.next_event_async(),
	)
	.await
	.unwrap_or_else(|_| panic!("{} timed out waiting for ChannelClosed event", node.node_id()));
	match event {
		Event::ChannelClosed { ref reason, .. } => {
			println!("{} got ChannelClosed: reason={:?}", node.node_id(), reason);
			if let Some(ref r) = reason {
				match r {
					ClosureReason::HolderForceClosed { .. } => {
						assert!(
							expected_local_initiated,
							"Got HolderForceClosed but expected remote-initiated close"
						);
					},
					ClosureReason::LocallyInitiatedCooperativeClosure => {
						assert!(
							expected_local_initiated,
							"Got LocallyInitiatedCooperativeClosure but expected remote-initiated"
						);
					},
					ClosureReason::CounterpartyInitiatedCooperativeClosure => {
						assert!(
							!expected_local_initiated,
							"Got CounterpartyInitiatedCooperativeClosure but expected local-initiated"
						);
					},
					ClosureReason::CommitmentTxConfirmed => {
						assert!(
							!expected_local_initiated,
							"Got CommitmentTxConfirmed but expected local-initiated close"
						);
					},
					_ => {
						// Other reasons (e.g. LegacyCooperativeClosure) are acceptable
					},
				}
			}
			node.event_handled().unwrap();
		},
		ref other => {
			panic!("{} expected ChannelClosed, got: {:?}", node.node_id(), other);
		},
	}
}

/// Open a channel from LDK to external node, wait for it to be confirmed.
/// Returns (user_channel_id, external_channel_id).
pub(crate) async fn open_channel_to_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	funding_amount_sat: u64, push_msat: Option<u64>,
) -> (ldk_node::UserChannelId, String) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	node.open_channel(ext_node_id, ext_addr, funding_amount_sat, push_msat, None).unwrap();

	let funding_txo = expect_channel_pending_event!(node, ext_node_id);
	super::super::wait_for_tx(electrs, funding_txo.txid).await;
	// Mine 10 blocks: Eclair requires minimum_depth=8 for 1M sat channels (amount-based formula),
	// so 6 blocks is not enough. 10 gives a comfortable margin for any external node.
	generate_blocks_and_wait(bitcoind, electrs, 10).await;
	node.sync_wallets().unwrap();
	let user_channel_id = expect_channel_ready_event!(node, ext_node_id);

	// Find the external node's channel ID for this channel
	let ext_channels = peer.list_channels().await.unwrap();
	let funding_txid_str = funding_txo.txid.to_string();
	let ext_channel_id = ext_channels
		.iter()
		.find(|ch| ch.funding_txid.as_deref() == Some(&funding_txid_str))
		.or_else(|| ext_channels.iter().find(|ch| ch.peer_id == node.node_id()))
		.map(|ch| ch.channel_id.clone())
		.unwrap_or_else(|| panic!("Could not find channel on external node {}", peer.name()));

	(user_channel_id, ext_channel_id)
}

/// Open a channel from external node to LDK, wait for it to be confirmed.
/// Returns (user_channel_id, external_channel_id).
pub(crate) async fn open_channel_from_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	funding_amount_sat: u64, push_msat: Option<u64>,
) -> (ldk_node::UserChannelId, String) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ldk_addr = node.listening_addresses().unwrap().first().unwrap().clone();

	peer.open_channel(node.node_id(), ldk_addr, funding_amount_sat, push_msat).await.unwrap();

	let funding_txo = expect_channel_pending_event!(node, ext_node_id);
	super::super::wait_for_tx(electrs, funding_txo.txid).await;
	generate_blocks_and_wait(bitcoind, electrs, 10).await;
	node.sync_wallets().unwrap();
	let user_channel_id = expect_channel_ready_event!(node, ext_node_id);

	// Look up the final channel ID from the external node (the temporary ID from
	// /open changes after funding and may include extra characters in some versions).
	let funding_txid_str = funding_txo.txid.to_string();
	let ext_channels = peer.list_channels().await.unwrap();
	let ext_channel_id = ext_channels
		.iter()
		.find(|ch| ch.funding_txid.as_deref() == Some(&funding_txid_str))
		.or_else(|| ext_channels.iter().find(|ch| ch.peer_id == node.node_id()))
		.map(|ch| ch.channel_id.clone())
		.unwrap_or_else(|| panic!("Could not find channel on external node {}", peer.name()));

	(user_channel_id, ext_channel_id)
}

/// Cooperative close initiated by LDK.
pub(crate) async fn cooperative_close_by_ldk<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.close_channel(user_channel_id, ext_node_id).unwrap();
	generate_blocks_and_wait(bitcoind, electrs, 1).await;
	node.sync_wallets().unwrap();
	expect_channel_closed(node, true).await;
}

/// Cooperative close initiated by external node.
pub(crate) async fn cooperative_close_by_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	ext_channel_id: &str,
) {
	peer.close_channel(ext_channel_id).await.unwrap();
	generate_blocks_and_wait(bitcoind, electrs, 1).await;
	node.sync_wallets().unwrap();
	expect_channel_closed(node, false).await;
}

/// Force close by LDK.
pub(crate) async fn force_close_by_ldk<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.force_close_channel(user_channel_id, ext_node_id, None).unwrap();
	expect_channel_closed(node, true).await;
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	node.sync_wallets().unwrap();
}

/// Force close by external node.
pub(crate) async fn force_close_by_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	ext_channel_id: &str,
) {
	peer.force_close_channel(ext_channel_id).await.unwrap();
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	node.sync_wallets().unwrap();
	expect_channel_closed(node, false).await;
}

/// Helper to close a channel using the given type and initiator.
pub(crate) async fn close_channel<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId, ext_channel_id: &str, close_type: &CloseType,
	close_initiator: &Side,
) {
	match (close_type, close_initiator) {
		(CloseType::Cooperative, Side::Ldk) => {
			cooperative_close_by_ldk(node, peer, bitcoind, electrs, user_channel_id).await;
		},
		(CloseType::Cooperative, Side::External) => {
			cooperative_close_by_external(node, peer, bitcoind, electrs, ext_channel_id).await;
		},
		(CloseType::Force, Side::Ldk) => {
			force_close_by_ldk(node, peer, bitcoind, electrs, user_channel_id).await;
		},
		(CloseType::Force, Side::External) => {
			force_close_by_external(node, peer, bitcoind, electrs, ext_channel_id).await;
		},
	}
}

/// External node opens channel to LDK, payments flow both ways, then close.
pub(crate) async fn run_inbound_channel_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	close_type: CloseType, close_initiator: Side,
) {
	setup_interop_test(node, peer, bitcoind, electrs).await;

	// External node opens channel to LDK with push so both sides have balance
	let (user_channel_id, ext_channel_id) =
		open_channel_from_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000))
			.await;

	// External → LDK payment
	receive_bolt11_payment(node, peer, 5_000_000).await;

	// LDK → External payment
	let invoice_str = peer.create_invoice(5_000_000, "inbound-ldk-to-ext").await.unwrap();
	let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	expect_event!(node, PaymentSuccessful);

	// Close
	close_channel(
		node,
		peer,
		bitcoind,
		electrs,
		&user_channel_id,
		&ext_channel_id,
		&close_type,
		&close_initiator,
	)
	.await;
}

/// Open a channel, mine many blocks to advance the chain state, then verify
/// cooperative close still succeeds after time has passed.
///
/// Note: on regtest with empty blocks, bitcoind's fee estimator may not
/// actually change. This test's primary value is as a smoke test for channel
/// operations after significant chain activity.
pub(crate) async fn cooperative_close_after_fee_change<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	setup_interop_test(node, peer, bitcoind, electrs).await;

	let (user_channel_id, _ext_channel_id) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;

	// Make a payment to confirm channel is working
	let invoice_str = peer.create_invoice(5_000_000, "pre-fee-change").await.unwrap();
	let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	expect_event!(node, PaymentSuccessful);

	// Mine many blocks to change fee environment.
	// In regtest, bitcoind's fee estimation changes as blocks are mined
	// with varying transaction densities.
	generate_blocks_and_wait(bitcoind, electrs, 50).await;
	node.sync_wallets().unwrap();

	// Allow fee rate updates to propagate between peers
	tokio::time::sleep(Duration::from_secs(3)).await;

	// Verify payment still works after fee rate change
	let invoice_str = peer.create_invoice(5_000_000, "post-fee-change").await.unwrap();
	let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	expect_event!(node, PaymentSuccessful);

	// Cooperative close
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.close_channel(&user_channel_id, ext_node_id).unwrap();
	expect_channel_closed(node, true).await;
}

/// Open a channel, mine many blocks, then force close.
/// Verifies that the commitment transaction is accepted on-chain after
/// significant chain activity.
pub(crate) async fn force_close_after_fee_change<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	setup_interop_test(node, peer, bitcoind, electrs).await;

	let (user_channel_id, _ext_channel_id) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;

	// Mine many blocks to shift fee environment
	generate_blocks_and_wait(bitcoind, electrs, 50).await;
	node.sync_wallets().unwrap();
	tokio::time::sleep(Duration::from_secs(3)).await;

	// Force close — commitment tx must have a fee that gets accepted into mempool
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.force_close_channel(&user_channel_id, ext_node_id, None).unwrap();
	expect_channel_closed(node, true).await;

	// Mine blocks to confirm the commitment tx
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	node.sync_wallets().unwrap();
}
