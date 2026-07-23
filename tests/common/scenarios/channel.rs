// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::time::Duration;

use bitcoin::Sequence;
use electrsd::corepc_node::Client as BitcoindClient;
use electrsd::electrum_client::ElectrumApi;
use ldk_node::{Event, Node};

use super::super::external_node::ExternalNode;
use super::super::{generate_blocks_and_wait, wait_for_outpoint_spend};
use super::Side;

/// Open a channel from LDK to peer; returns (user_channel_id, external_channel_id).
pub(crate) async fn open_channel_to_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	funding_amount_sat: u64, push_msat: Option<u64>,
) -> (ldk_node::UserChannelId, String) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	node.open_channel(ext_node_id, ext_addr, funding_amount_sat, push_msat, None).unwrap();

	let funding_txo = expect_channel_pending_event!(node, ext_node_id);
	super::super::wait_for_tx(electrs, funding_txo.txid).await;
	generate_blocks_and_wait(bitcoind, electrs, 10).await;
	super::sync_wallets_with_retry(node).await;
	let user_channel_id = expect_channel_ready_event!(node, ext_node_id);

	let ext_channels = peer.list_channels().await.unwrap();
	let funding_txid_str = funding_txo.txid.to_string();
	let ext_channel_id = ext_channels
		.iter()
		.find(|ch| ch.funding_txid.as_deref() == Some(&funding_txid_str))
		// Fallback to active channel by peer_id; avoids picking up closing channels from prior scenarios.
		.or_else(|| ext_channels.iter().find(|ch| ch.peer_id == node.node_id() && ch.is_active))
		.map(|ch| ch.channel_id.clone())
		.unwrap_or_else(|| panic!("Could not find channel on external node {}", peer.name()));

	(user_channel_id, ext_channel_id)
}

/// Cooperative close from the chosen side. Mines 1 block and asserts ChannelClosed.
pub(crate) async fn cooperative_close<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId, ext_channel_id: &str, initiator: Side,
) {
	tokio::time::sleep(Duration::from_secs(2)).await;
	let funding_txo = node
		.list_channels()
		.into_iter()
		.find(|channel| channel.user_channel_id == *user_channel_id)
		.and_then(|channel| channel.funding_txo)
		.expect("channel funding outpoint must be available before cooperative close");
	match initiator {
		Side::Ldk => {
			let ext_node_id = peer.get_node_id().await.unwrap();
			node.close_channel(user_channel_id, ext_node_id).unwrap();
		},
		Side::External => {
			peer.close_channel(ext_channel_id).await.unwrap();
		},
	}
	let closing_tx = wait_for_outpoint_spend(electrs, funding_txo).await;
	let funding_input = closing_tx
		.input
		.iter()
		.find(|input| input.previous_output == funding_txo)
		.expect("closing transaction must spend the channel funding outpoint");
	let expected_sequence = if node.config().enable_v2_channel_close {
		Sequence::ENABLE_RBF_NO_LOCKTIME
	} else {
		Sequence::MAX
	};
	assert_eq!(
		funding_input.sequence, expected_sequence,
		"cooperative close used an unexpected transaction format"
	);
	generate_blocks_and_wait(bitcoind, electrs, 1).await;
	super::sync_wallets_with_retry(node).await;
	expect_event!(node, ChannelClosed);
}

/// Force close from the chosen side. Mines 6 blocks and asserts ChannelClosed.
///
/// External-initiated path additionally polls the mempool because the peer's
/// commitment-broadcast can lag the force-close RPC return.
pub(crate) async fn force_close<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId, ext_channel_id: &str, initiator: Side,
) {
	match initiator {
		Side::Ldk => {
			let ext_node_id = peer.get_node_id().await.unwrap();
			node.force_close_channel(user_channel_id, ext_node_id, None).unwrap();
			expect_event!(node, ChannelClosed);
			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			super::sync_wallets_with_retry(node).await;
		},
		Side::External => {
			peer.force_close_channel(ext_channel_id).await.unwrap();
			// External peer's force-close RPC may return before commitment tx is broadcast.
			let before =
				bitcoind.call::<Vec<String>>("getrawmempool", &[]).unwrap_or_default().len();
			for _ in 0..30 {
				tokio::time::sleep(Duration::from_secs(1)).await;
				let now =
					bitcoind.call::<Vec<String>>("getrawmempool", &[]).unwrap_or_default().len();
				if now > before {
					break;
				}
			}
			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			super::sync_wallets_with_retry(node).await;
			tokio::time::sleep(Duration::from_secs(2)).await;
			super::sync_wallets_with_retry(node).await;
			expect_event!(node, ChannelClosed);
		},
	}
}
