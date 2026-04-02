// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;

use ldk_node::{Event, Node};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};

use super::super::external_node::ExternalNode;
use super::retry_until_ok;

/// LDK pays the peer via a fresh BOLT11 invoice; asserts `PaymentSuccessful`.
pub(crate) async fn send_bolt11_to_peer(
	node: &Node, peer: &(impl ExternalNode + ?Sized), amount_msat: u64, label: &str,
) {
	let invoice_str = peer.create_invoice(amount_msat, label).await.unwrap();
	let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	expect_event!(node, PaymentSuccessful);
}

/// External node pays LDK via BOLT11 invoice. Retries to absorb gossip-propagation
/// delay (peer may not yet know a route to LDK right after channel confirmation).
pub(crate) async fn receive_bolt11_payment(
	node: &Node, peer: &(impl ExternalNode + ?Sized), amount_msat: u64,
) {
	let invoice = node
		.bolt11_payment()
		.receive(
			amount_msat,
			&Bolt11InvoiceDescription::Direct(
				Description::new("interop-receive-test".to_string()).unwrap(),
			),
			3600,
		)
		.unwrap();
	let invoice_str = invoice.to_string();
	retry_until_ok(10, "receive_bolt11_payment", || peer.pay_invoice(&invoice_str)).await;
	expect_payment_received_event!(node, amount_msat);
}

/// LDK keysends to peer; asserts `PaymentSuccessful`.
pub(crate) async fn send_keysend_to_peer(
	node: &Node, peer: &(impl ExternalNode + ?Sized), amount_msat: u64,
) {
	let peer_id = peer.get_node_id().await.unwrap();
	node.spontaneous_payment().send(amount_msat, peer_id, None).unwrap();
	expect_event!(node, PaymentSuccessful);
}

/// External node sends keysend to LDK. Retries to absorb gossip-propagation delay.
pub(crate) async fn receive_keysend_payment(
	node: &Node, peer: &(impl ExternalNode + ?Sized), amount_msat: u64,
) {
	let node_id = node.node_id();
	retry_until_ok(10, "receive_keysend_payment", || peer.send_keysend(node_id, amount_msat)).await;
	expect_payment_received_event!(node, amount_msat);
}
