// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::time::Duration;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Event, Node};
use lightning_invoice::Bolt11Invoice;

use super::super::external_node::ExternalNode;
use super::Side;

/// Disconnect a peer from the chosen side. Returns the dispatch result so callers
/// can tolerate races (mid-payment) by ignoring the error.
pub(crate) async fn disconnect_by_side(
	node: &Node, peer: &(impl ExternalNode + ?Sized), side: &Side,
) -> Result<(), String> {
	let ext_node_id = peer.get_node_id().await.unwrap();
	match side {
		Side::Ldk => node.disconnect(ext_node_id).map_err(|e| format!("{:?}", e)),
		Side::External => {
			peer.disconnect_peer(node.node_id()).await.map_err(|e| format!("{:?}", e))
		},
	}
}

/// Reconnect to a peer and wait until the connection is established.
pub(crate) async fn reconnect_and_wait(
	node: &Node, peer_id: PublicKey, addr: SocketAddress, context: &str,
) {
	node.connect(peer_id, addr, true).unwrap();
	let max_attempts = super::super::INTEROP_TIMEOUT_SECS;
	for i in 0..max_attempts {
		if node.list_peers().iter().any(|p| p.node_id == peer_id && p.is_connected) {
			tokio::time::sleep(Duration::from_secs(2)).await;
			return;
		}
		if i + 1 == max_attempts {
			panic!("Peer did not reconnect within {}s ({})", max_attempts, context);
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
}

/// Disconnect during payment, reconnect, verify payment resolves.
pub(crate) async fn disconnect_during_payment(
	node: &Node, peer: &(impl ExternalNode + ?Sized), disconnect_side: &Side,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	let invoice_str = peer.create_invoice(10_000_000, "disconnect-payment-test").await.unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&invoice_str).unwrap();

	// If send() fails immediately, no event will arrive — skip event wait below.
	let send_ok = node.bolt11_payment().send(&parsed_invoice, None).is_ok();

	// Disconnect may race with payment delivery; tolerate failure.
	let _ = disconnect_by_side(node, peer, disconnect_side).await;

	tokio::time::sleep(Duration::from_secs(2)).await;
	reconnect_and_wait(node, ext_node_id, ext_addr, "disconnect during payment").await;

	if send_ok {
		let event = tokio::time::timeout(
			Duration::from_secs(super::super::INTEROP_TIMEOUT_SECS),
			node.next_event_async(),
		)
		.await
		.expect("Timed out waiting for payment to resolve after reconnect");
		match event {
			Event::PaymentSuccessful { .. } | Event::PaymentFailed { .. } => {
				node.event_handled().unwrap();
			},
			other => {
				panic!("Expected payment outcome event, got: {:?}", other);
			},
		}
	}
}
