// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};

use super::super::external_node::ExternalNode;
use super::super::generate_blocks_and_wait;
use super::{find_ext_channel, retry_until_ok};

/// Test splice-in from external node, then splice-out.
///
/// Soft-fails if the external node does not support splicing.
pub(crate) async fn splice_from_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	ext_channel_id: &str,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();

	// Capture capacity before splice for proper assertion
	let capacity_before_in = find_ext_channel(peer, ext_channel_id).await.capacity_sat;

	// Splice in: external node adds 500k sats to the channel
	let splice_in_amount_sat = 500_000;
	match peer.splice_in(ext_channel_id, splice_in_amount_sat).await {
		Ok(_) => {
			println!(
				"[splice] {} splice-in of {} sats initiated",
				peer.name(),
				splice_in_amount_sat
			);

			// Wait for LDK to see the splice pending event
			let splice_txo = expect_splice_pending_event!(node, ext_node_id);
			super::super::wait_for_tx(electrs, splice_txo.txid).await;
			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			let expected_height = bitcoind.get_blockchain_info().unwrap().blocks as u64;
			peer.wait_for_block_sync(expected_height).await.unwrap();
			node.sync_wallets().unwrap();

			// After confirmation, channel should be ready with new capacity
			expect_channel_ready_event!(node, ext_node_id);

			let ch = find_ext_channel(peer, ext_channel_id).await;
			// Allow small tolerance for on-chain fees deducted from the splice tx
			let tolerance_sat = 10_000;
			assert!(
				ch.capacity_sat + tolerance_sat >= capacity_before_in + splice_in_amount_sat,
				"[splice] capacity after splice-in ({}) should be >= before ({}) + splice amount ({}) (minus fees)",
				ch.capacity_sat,
				capacity_before_in,
				splice_in_amount_sat,
			);
			println!(
				"[splice] splice-in confirmed successfully (capacity: {} -> {} sat)",
				capacity_before_in, ch.capacity_sat
			);
		},
		Err(e) => {
			println!("[splice] {} does not support splice-in (skipping): {}", peer.name(), e);
			return;
		},
	}

	// Splice out: external node removes 200k sats from channel to LDK on-chain address
	let splice_out_amount_sat = 200_000;
	let capacity_before_out = find_ext_channel(peer, ext_channel_id).await.capacity_sat;
	let ldk_address = node.onchain_payment().new_address().unwrap();
	match peer
		.splice_out(ext_channel_id, splice_out_amount_sat, Some(&ldk_address.to_string()))
		.await
	{
		Ok(_) => {
			println!(
				"[splice] {} splice-out of {} sats initiated",
				peer.name(),
				splice_out_amount_sat
			);

			let splice_txo = expect_splice_pending_event!(node, ext_node_id);
			super::super::wait_for_tx(electrs, splice_txo.txid).await;
			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			node.sync_wallets().unwrap();

			expect_channel_ready_event!(node, ext_node_id);

			let ch = find_ext_channel(peer, ext_channel_id).await;
			assert!(
				ch.capacity_sat < capacity_before_out,
				"[splice] capacity after splice-out ({}) should be less than before ({})",
				ch.capacity_sat,
				capacity_before_out,
			);
			println!(
				"[splice] splice-out confirmed successfully (capacity: {} -> {} sat)",
				capacity_before_out, ch.capacity_sat
			);
		},
		Err(e) => {
			println!("[splice] {} does not support splice-out (skipping): {}", peer.name(), e);
		},
	}
}

/// Splice in additional funds from external, then verify the external node
/// can pay LDK using the newly spliced-in capacity.
///
/// The splice adds funds to the external side of the channel. We verify
/// by having the external node pay LDK an amount that exercises the
/// new capacity.
pub(crate) async fn splice_then_payment<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	ext_channel_id: &str,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let splice_amount_sat = 500_000;

	// Check how much the external node can send before the splice
	let before = find_ext_channel(peer, ext_channel_id).await;
	let ext_balance_before = before.local_balance_msat;

	match peer.splice_in(ext_channel_id, splice_amount_sat).await {
		Ok(_) => {
			let splice_txo = expect_splice_pending_event!(node, ext_node_id);
			super::super::wait_for_tx(electrs, splice_txo.txid).await;
			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			let expected_height = bitcoind.get_blockchain_info().unwrap().blocks as u64;
			peer.wait_for_block_sync(expected_height).await.unwrap();
			node.sync_wallets().unwrap();
			expect_channel_ready_event!(node, ext_node_id);

			// Pay an amount that exceeds the pre-splice external balance,
			// proving the splice-in capacity is actually usable.
			let payment_msat = ext_balance_before + splice_amount_sat as u64 * 1000 / 2;
			let ldk_invoice = node
				.bolt11_payment()
				.receive(
					payment_msat,
					&lightning_invoice::Bolt11InvoiceDescription::Direct(
						lightning_invoice::Description::new("splice-then-pay".to_string()).unwrap(),
					),
					3600,
				)
				.unwrap();
			let invoice_str = ldk_invoice.to_string();
			retry_until_ok(10, "splice_then_payment", || peer.pay_invoice(&invoice_str)).await;
			expect_event!(node, PaymentReceived);
		},
		Err(e) => {
			println!("[splice] {} does not support splice-in (skipping): {}", peer.name(), e);
		},
	}
}
