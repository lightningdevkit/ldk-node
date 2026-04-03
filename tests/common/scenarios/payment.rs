// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::time::Duration;

use ldk_node::{Event, Node};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};

use super::super::external_node::ExternalNode;
use super::{find_ext_channel, retry_until_ok};

/// External node pays LDK via BOLT11 invoice.
///
/// Retries the payment to handle gossip propagation delay — the external node
/// may not yet know a route to LDK right after channel confirmation.
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

/// External node sends keysend to LDK.
///
/// Retries to handle gossip propagation delay.
pub(crate) async fn receive_keysend_payment(
	node: &Node, peer: &(impl ExternalNode + ?Sized), amount_msat: u64,
) {
	let node_id = node.node_id();
	retry_until_ok(10, "receive_keysend_payment", || peer.send_keysend(node_id, amount_msat)).await;
	expect_payment_received_event!(node, amount_msat);
}

/// Bidirectional payments: LDK → external, then external → LDK.
pub(crate) async fn bidirectional_payments(
	node: &Node, peer: &(impl ExternalNode + ?Sized), ext_channel_id: &str, amount_msat: u64,
) {
	let before = find_ext_channel(peer, ext_channel_id).await;

	// LDK → external
	let invoice_str = peer.create_invoice(amount_msat, "bidir-forward").await.unwrap();
	let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	expect_event!(node, PaymentSuccessful);

	// external → LDK
	let ldk_invoice = node
		.bolt11_payment()
		.receive(
			amount_msat,
			&Bolt11InvoiceDescription::Direct(
				Description::new("bidir-reverse".to_string()).unwrap(),
			),
			3600,
		)
		.unwrap();
	let invoice_str = ldk_invoice.to_string();
	retry_until_ok(10, "bidirectional_reverse_payment", || peer.pay_invoice(&invoice_str)).await;
	expect_event!(node, PaymentReceived);

	// Net balance on external node should be roughly unchanged (± fees + rounding).
	// Poll because some implementations (LND) are slow to reflect balance updates.
	let mut diff: i64 = 0;
	for _ in 0..10 {
		tokio::time::sleep(Duration::from_millis(500)).await;
		let after = find_ext_channel(peer, ext_channel_id).await;
		diff = (after.local_balance_msat as i64) - (before.local_balance_msat as i64);
		if diff.abs() <= 10_000 {
			break;
		}
	}
	let tolerance_msat: i64 = 10_000; // 10 sat tolerance for fees + rounding
	assert!(
		diff.abs() <= tolerance_msat,
		"Bidirectional payments should roughly cancel out, but balance diff was {} msat",
		diff,
	);
}

/// Pay an expired invoice — should fail gracefully.
///
/// Note: invoice expiry is checked by the *payer*, so exact behavior is
/// implementation-dependent. Most implementations reject expired invoices
/// before attempting to route, but the timing window is inherently racy.
pub(crate) async fn pay_expired_invoice(node: &Node, peer: &(impl ExternalNode + ?Sized)) {
	// Create invoice with short expiry
	let invoice = node
		.bolt11_payment()
		.receive(
			1_000_000,
			&Bolt11InvoiceDescription::Direct(Description::new("expire-test".to_string()).unwrap()),
			3,
		)
		.unwrap();

	// Wait for it to expire (generous margin for slow CI)
	tokio::time::sleep(Duration::from_secs(5)).await;

	// External node attempts to pay — should fail
	let result = peer.pay_invoice(&invoice.to_string()).await;
	assert!(result.is_err(), "Paying an expired invoice should fail");
}

/// Send multiple payments in rapid succession to test concurrent HTLC handling.
pub(crate) async fn concurrent_payments(
	node: &Node, peer: &(impl ExternalNode + ?Sized), num_payments: usize, amount_msat_each: u64,
) {
	for i in 0..num_payments {
		let invoice_str =
			peer.create_invoice(amount_msat_each, &format!("concurrent-{}", i)).await.unwrap();
		let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
		node.bolt11_payment().send(&parsed, None).unwrap();
	}

	// Collect all payment outcomes
	for _ in 0..num_payments {
		let event = tokio::time::timeout(
			Duration::from_secs(super::super::INTEROP_TIMEOUT_SECS),
			node.next_event_async(),
		)
		.await
		.expect("Timed out waiting for concurrent payment to resolve");
		match event {
			Event::PaymentSuccessful { .. } => {
				node.event_handled().unwrap();
			},
			other => {
				panic!("Expected PaymentSuccessful, got: {:?}", other);
			},
		}
	}
}
