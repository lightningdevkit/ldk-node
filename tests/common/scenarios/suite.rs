// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Standard interop test sequences shared across all external node implementations.

use std::str::FromStr;

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};
use lightning_invoice::Bolt11Invoice;

use super::super::external_node::ExternalNode;
use super::channel::{
	cooperative_close_after_fee_change, cooperative_close_by_ldk, force_close_after_fee_change,
	force_close_by_external, force_close_by_ldk, open_channel_to_external,
	run_inbound_channel_test,
};
use super::combo::run_combo;
use super::connectivity::disconnect_reconnect_idle;
use super::payment::{
	bidirectional_payments, concurrent_payments, pay_expired_invoice, receive_bolt11_payment,
};
use super::{CloseType, PayType, Phase, Side};

/// Run the standard interop test sequence that is identical across all implementations.
///
/// Caller must have called `setup_interop_test` before this function.
/// Does NOT call `node.stop()` — caller handles that.
pub(crate) async fn run_scenarios<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	// --- Basic channel cycle: open, pay both directions, cooperative close ---
	let (user_ch, _ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;

	let invoice = peer.create_invoice(10_000_000, "basic-send").await.unwrap();
	let parsed = Bolt11Invoice::from_str(&invoice).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	expect_event!(node, PaymentSuccessful);

	receive_bolt11_payment(node, peer, 10_000_000).await;

	cooperative_close_by_ldk(node, peer, bitcoind, electrs, &user_ch).await;

	// --- Disconnect/reconnect ---
	let (_user_ch, ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;
	disconnect_reconnect_idle(node, peer, bitcoind, electrs, &Side::Ldk).await;
	disconnect_reconnect_idle(node, peer, bitcoind, electrs, &Side::External).await;
	force_close_by_external(node, peer, bitcoind, electrs, &ext_ch).await;

	// --- Force close by LDK ---
	let (user_ch, _ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;
	force_close_by_ldk(node, peer, bitcoind, electrs, &user_ch).await;

	// --- Force close by external ---
	let (_user_ch, ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;
	force_close_by_external(node, peer, bitcoind, electrs, &ext_ch).await;

	// --- Inbound channel (external opens to LDK) ---
	run_inbound_channel_test(node, peer, bitcoind, electrs, CloseType::Cooperative, Side::External)
		.await;

	// --- Bidirectional payments ---
	let (user_ch, ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;
	bidirectional_payments(node, peer, &ext_ch, 5_000_000).await;
	cooperative_close_by_ldk(node, peer, bitcoind, electrs, &user_ch).await;

	// --- Expired invoice ---
	let (user_ch, _ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;
	pay_expired_invoice(node, peer).await;
	cooperative_close_by_ldk(node, peer, bitcoind, electrs, &user_ch).await;

	// --- Concurrent payments ---
	let (user_ch, _ext_ch) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;
	concurrent_payments(node, peer, 5, 1_000_000).await;
	cooperative_close_by_ldk(node, peer, bitcoind, electrs, &user_ch).await;

	// --- Fee change scenarios ---
	cooperative_close_after_fee_change(node, peer, bitcoind, electrs).await;

	force_close_after_fee_change(node, peer, bitcoind, electrs).await;
}

/// Run all 16 combo permutations (phase × disconnect side × close type × close initiator).
///
/// Caller must have called `setup_interop_test` before this function.
/// Does NOT call `node.stop()` — caller handles that.
pub(crate) async fn run_combos<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let combos = [
		(Phase::Payment, Side::Ldk, CloseType::Cooperative, Side::Ldk),
		(Phase::Payment, Side::Ldk, CloseType::Cooperative, Side::External),
		(Phase::Payment, Side::Ldk, CloseType::Force, Side::Ldk),
		(Phase::Payment, Side::Ldk, CloseType::Force, Side::External),
		(Phase::Payment, Side::External, CloseType::Cooperative, Side::Ldk),
		(Phase::Payment, Side::External, CloseType::Cooperative, Side::External),
		(Phase::Payment, Side::External, CloseType::Force, Side::Ldk),
		(Phase::Payment, Side::External, CloseType::Force, Side::External),
		(Phase::Idle, Side::Ldk, CloseType::Cooperative, Side::Ldk),
		(Phase::Idle, Side::Ldk, CloseType::Cooperative, Side::External),
		(Phase::Idle, Side::Ldk, CloseType::Force, Side::Ldk),
		(Phase::Idle, Side::Ldk, CloseType::Force, Side::External),
		(Phase::Idle, Side::External, CloseType::Cooperative, Side::Ldk),
		(Phase::Idle, Side::External, CloseType::Cooperative, Side::External),
		(Phase::Idle, Side::External, CloseType::Force, Side::Ldk),
		(Phase::Idle, Side::External, CloseType::Force, Side::External),
	];

	for (i, (phase, disc, close, initiator)) in combos.iter().enumerate() {
		println!(
			"=== combo {}/{}: {:?} {:?} {:?} {:?} ===",
			i + 1,
			combos.len(),
			phase,
			disc,
			close,
			initiator
		);
		run_combo(
			node,
			peer,
			bitcoind,
			electrs,
			*phase,
			*disc,
			*close,
			*initiator,
			PayType::Bolt11,
		)
		.await;
	}
}
