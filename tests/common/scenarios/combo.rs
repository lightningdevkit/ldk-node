// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Combo test orchestrator: runs disconnect → payment → close across all
//! parameter combinations. Used by the `interop_combo_tests!` macro.

use std::str::FromStr;
use std::time::Duration;

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};
use lightning_invoice::Bolt11Invoice;

use super::super::external_node::ExternalNode;
use super::channel::{close_channel, open_channel_to_external};
use super::disconnect::{disconnect_during_payment, disconnect_reconnect_idle};
use super::{find_ext_channel, setup_interop_test, CloseType, PayType, Phase, Side};

/// Run a combined interop scenario: setup → open channel → disconnect/reconnect
/// → payment → close. Each parameter axis is varied by `interop_combo_tests!`
/// to cover all 16 combinations (2 phases × 2 disconnect sides × 2 close types
/// × 2 close initiators).
pub(crate) async fn run_interop_combo_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	disconnect_phase: Phase, disconnect_initiator: Side, close_type: CloseType,
	close_initiator: Side, payment_type: PayType,
) {
	// Setup: fund + connect
	setup_interop_test(node, peer, bitcoind, electrs).await;

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

	// Wait for channel to be reestablished after disconnect/reconnect.
	// Note: LND may report active=false for a while after reconnect, even though
	// the channel is functional (payments succeed). We wait for peer connectivity
	// instead, since disconnect_reconnect_idle already verified the channel
	// works by completing a payment.
	let ext_node_id = peer.get_node_id().await.unwrap();
	for i in 0..30 {
		let connected =
			node.list_peers().iter().any(|p| p.node_id == ext_node_id && p.is_connected);
		if connected {
			break;
		}
		if i == 29 {
			panic!("Peer did not reconnect within 30s");
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	tokio::time::sleep(Duration::from_secs(2)).await;

	// Phase 2: Make a payment and verify balance changes
	let payment_amount_msat = 5_000_000;
	let before = find_ext_channel(peer, &ext_channel_id).await;
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
	// Wait for external node to reflect the balance update, then verify.
	// LND in particular may not update `local_balance` immediately.
	let mut balance_increase = 0;
	for _ in 0..10 {
		tokio::time::sleep(Duration::from_millis(500)).await;
		let after = find_ext_channel(peer, &ext_channel_id).await;
		balance_increase = after.local_balance_msat.saturating_sub(before.local_balance_msat);
		if balance_increase > 0 {
			break;
		}
	}
	let tolerance_msat = 10_000; // 10 sat tolerance for fees + rounding
	assert!(
		balance_increase + tolerance_msat >= payment_amount_msat,
		"External node balance did not increase enough after payment: \
		 before={}, increase={}, expected>={}",
		before.local_balance_msat,
		balance_increase,
		payment_amount_msat,
	);

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
