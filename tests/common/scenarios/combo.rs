// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Combo test orchestrator: runs connectivity → payment → close across all
//! parameter combinations. Called by `suite::run_combos`.

use std::str::FromStr;

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};
use lightning_invoice::Bolt11Invoice;

use super::super::external_node::ExternalNode;
use super::channel::{close_channel, open_channel_to_external};
use super::connectivity::{
	disconnect_during_payment, disconnect_reconnect_idle, reconnect_and_wait,
};
use super::{CloseType, PayType, Phase, Side};

/// Run a single combo: open channel → disconnect/reconnect → payment → close.
/// Called by `suite::run_combos` for each of the 16 permutations.
pub(crate) async fn run_combo<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	disconnect_phase: Phase, disconnect_initiator: Side, close_type: CloseType,
	close_initiator: Side, payment_type: PayType,
) {
	// Open channel
	let (user_channel_id, ext_channel_id) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;

	// Phase 1: Disconnect/Reconnect at the specified phase
	match disconnect_phase {
		Phase::Idle => {
			disconnect_reconnect_idle(node, peer, bitcoind, electrs, &disconnect_initiator).await;
		},
		Phase::Payment => {
			disconnect_during_payment(node, peer, bitcoind, electrs, &disconnect_initiator).await;
		},
	}

	// Ensure peer is reconnected after the disconnect scenario.
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();
	reconnect_and_wait(node, ext_node_id, ext_addr, "combo reconnect").await;

	// Phase 2: Make a payment (PaymentSuccessful event confirms delivery)
	let payment_amount_msat = 5_000_000;
	match payment_type {
		PayType::Bolt11 => {
			let invoice_str =
				peer.create_invoice(payment_amount_msat, "combo-bolt11").await.unwrap();
			let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
			node.bolt11_payment().send(&parsed, None).unwrap();
			expect_event!(node, PaymentSuccessful);
		},
		PayType::Keysend => {
			node.spontaneous_payment().send(payment_amount_msat, ext_node_id, None).unwrap();
			expect_event!(node, PaymentSuccessful);
		},
	}

	// Phase 3: Close channel
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
