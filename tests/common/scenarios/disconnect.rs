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
use lightning_invoice::Bolt11Invoice;

use super::super::external_node::ExternalNode;
use super::Side;

/// Disconnect during idle, reconnect, verify channel still works.
pub(crate) async fn disconnect_reconnect_idle<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), _bitcoind: &BitcoindClient, _electrs: &E,
	disconnect_side: &Side,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	match disconnect_side {
		Side::Ldk => {
			node.disconnect(ext_node_id).unwrap();
		},
		Side::External => {
			peer.disconnect_peer(node.node_id()).await.unwrap();
		},
	}

	tokio::time::sleep(Duration::from_secs(1)).await;

	// Reconnect and wait for channel reestablishment
	node.connect(ext_node_id, ext_addr, true).unwrap();
	for i in 0..30 {
		let connected =
			node.list_peers().iter().any(|p| p.node_id == ext_node_id && p.is_connected);
		if connected {
			break;
		}
		if i == 29 {
			panic!("Peer did not reconnect within 30s after idle disconnect");
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	// Allow channel reestablishment to complete
	tokio::time::sleep(Duration::from_secs(2)).await;

	// Verify channel still works with a payment
	let invoice_str = peer.create_invoice(10_000_000, "disconnect-idle-test").await.unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed_invoice, None).unwrap();
	expect_event!(node, PaymentSuccessful);
}

/// Disconnect during payment, reconnect, verify payment resolves.
pub(crate) async fn disconnect_during_payment<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), _bitcoind: &BitcoindClient, _electrs: &E,
	disconnect_side: &Side,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	let invoice_str = peer.create_invoice(10_000_000, "disconnect-payment-test").await.unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&invoice_str).unwrap();

	// Send payment (may or may not complete before disconnect).
	// If send() fails immediately, no event will arrive — skip to reconnect verification.
	let send_ok = node.bolt11_payment().send(&parsed_invoice, None).is_ok();

	// Disconnect immediately
	match disconnect_side {
		Side::Ldk => {
			let _ = node.disconnect(ext_node_id);
		},
		Side::External => {
			let _ = peer.disconnect_peer(node.node_id()).await;
		},
	}

	tokio::time::sleep(Duration::from_secs(2)).await;

	// Reconnect
	node.connect(ext_node_id, ext_addr, true).unwrap();
	tokio::time::sleep(Duration::from_secs(2)).await;

	// If the payment was initiated, wait for it to resolve.
	if send_ok {
		let event = tokio::time::timeout(
			Duration::from_secs(super::super::INTEROP_TIMEOUT_SECS),
			node.next_event_async(),
		)
		.await
		.expect("Timed out waiting for payment to resolve after reconnect");
		match event {
			ldk_node::Event::PaymentSuccessful { .. } | ldk_node::Event::PaymentFailed { .. } => {
				node.event_handled().unwrap();
			},
			other => {
				panic!("Expected payment outcome event, got: {:?}", other);
			},
		}
	}
}
