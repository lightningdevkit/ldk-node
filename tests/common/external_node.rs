// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(any(cln_test, lnd_test))]
#![allow(dead_code)]

use electrsd::corepc_client::client_sync::Auth;
use lightning::offers::offer;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::{Builder, Event, Node, NodeError, UserChannelId};

use lightning::ln::msgs::SocketAddress;

use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};

use bitcoin::{Amount, OutPoint};

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;
use electrum_client::ElectrumApi;

use std::str::FromStr;
use std::sync::OnceLock;

use crate::common::{
	distribute_funds_unconfirmed, expect_channel_pending_event, expect_channel_ready_event,
	expect_event, generate_blocks_and_wait, premine_blocks, random_config, wait_for_tx,
};

pub trait ExternalLightningNode {
	fn get_node_info(&mut self) -> (PublicKey, SocketAddress);

	fn create_invoice(&mut self, amount_msat: u64, description: Option<String>) -> String;

	fn pay_invoice(&mut self, invoice: &str);

	fn check_receive_payment(&mut self, invoice: Bolt11Invoice);

	fn close_channel(&mut self, channel_id: OutPoint, node_id: PublicKey, force: bool);
}

static BITCOIND_CLIENT: OnceLock<BitcoindClient> = OnceLock::new();

pub(crate) fn init_bitcoind_client() -> &'static BitcoindClient {
	BITCOIND_CLIENT.get_or_init(|| {
		let bitcoind_client = BitcoindClient::new_with_auth(
			"http://127.0.0.1:18443",
			Auth::UserPass("user".to_string(), "pass".to_string()),
		)
		.unwrap();

		let electrs_client = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();
		premine_blocks(&bitcoind_client, &electrs_client);

		bitcoind_client
	})
}

pub(crate) fn setup_test_node(
	anchor_channels: bool,
) -> (Node, &'static BitcoindClient, ElectrumClient) {
	let bitcoind_client = init_bitcoind_client();

	let electrs_client = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	let config = random_config(anchor_channels);
	let mut builder = Builder::from_config(config.node_config);
	builder.set_chain_source_esplora("http://127.0.0.1:3002".to_string(), None);

	let node = builder.build().unwrap();
	node.start().unwrap();

	(node, bitcoind_client, electrs_client)
}

fn add_onchain_funds(bitcoind: &BitcoindClient, electrs: &ElectrumClient, node: &Node) {
	let addr = node.onchain_payment().new_address().unwrap();
	let amount = Amount::from_sat(5_000_000);
	distribute_funds_unconfirmed(bitcoind, electrs, vec![addr], amount);
	generate_blocks_and_wait(bitcoind, electrs, 1);
}

fn open_channel<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, node: &Node, external_node_id: PublicKey,
	external_node_address: SocketAddress, funding_amount_sat: u64, push_msat: Option<u64>,
) -> (UserChannelId, OutPoint) {
	node.sync_wallets().unwrap();

	// Open the channel
	node.open_announced_channel(
		external_node_id,
		external_node_address,
		funding_amount_sat,
		push_msat,
		None,
	)
	.unwrap();

	let funding_txo = expect_channel_pending_event!(node, external_node_id);
	wait_for_tx(electrs, funding_txo.txid);
	generate_blocks_and_wait(bitcoind, electrs, 6);
	node.sync_wallets().unwrap();
	let user_channel_id = expect_channel_ready_event!(node, external_node_id);

	(user_channel_id, funding_txo)
}

fn send_payment_to_external_node<E: ExternalLightningNode>(
	node: &Node, external_node: &mut E, amount_msat: u64,
) -> Bolt11Invoice {
	let invoice_string = external_node.create_invoice(amount_msat, None);
	let invoice = Bolt11Invoice::from_str(&invoice_string).unwrap();
	node.bolt11_payment()
		.send(&invoice, None)
		.unwrap_or_else(|e| panic!("Failed to send payment: {:?}", e));
	invoice
}

fn check_send_payment_succeeds<E: ExternalLightningNode>(
	node: &Node, external_node: &mut E, invoice: Bolt11Invoice,
) {
	expect_event!(node, PaymentSuccessful);
	external_node.check_receive_payment(invoice)
}

fn receive_payment_from_external_node<E: ExternalLightningNode>(
	node: &Node, external_node: &mut E, amount_msat: u64,
) {
	let invoice_description = Bolt11InvoiceDescription::Direct(
		Description::new("test external node".to_string()).unwrap(),
	);
	let ldk_invoice =
		node.bolt11_payment().receive(amount_msat, &invoice_description, 3600).unwrap();
	external_node.pay_invoice(&ldk_invoice.to_string());

	expect_event!(node, PaymentReceived);
}

fn close_channel<E: ExternalLightningNode, F: ElectrumApi>(
	bitcoind_client: &BitcoindClient, electrs_client: &F, external_node: &mut E, node: &Node,
	channel_id: OutPoint, node_id: PublicKey, user_channel_id: &UserChannelId,
	external_force_close: Option<bool>,
) {
	match external_force_close {
		Some(force) => {
			if force {
				// Other Lightning implementations only force close if the counterparty is not connected;
				// they always attempt a cooperative close first before forcing.
				node.stop().unwrap();
				external_node.close_channel(channel_id, node.node_id(), force);
				node.start().unwrap();
			} else {
				external_node.close_channel(channel_id, node.node_id(), force);
			}
			generate_blocks_and_wait(bitcoind_client, electrs_client, 1);
		},
		None => {
			node.close_channel(user_channel_id, node_id).unwrap();
		},
	}
	expect_event!(node, ChannelClosed);
}

pub(crate) fn do_ldk_opens_channel_full_cycle_with_external_node<E: ExternalLightningNode>(
	external_node: &mut E, external_force_close: Option<bool>,
) {
	// Initialize LDK node and clients
	let (node, bitcoind_client, electrs_client) = setup_test_node(true);

	// setup external node info
	let (external_node_id, external_node_address) = external_node.get_node_info();
	let external_node_address = external_node_address.clone();

	// Open the channel
	add_onchain_funds(&bitcoind_client, &electrs_client, &node);
	let funding_amount_sat = 2_000_000;
	let push_msat = Some(500_000_000);
	let (user_channel_id, funding_txo) = open_channel(
		&bitcoind_client,
		&electrs_client,
		&node,
		external_node_id,
		external_node_address,
		funding_amount_sat,
		push_msat,
	);

	// Send a payment to the external node
	let invoice_amount_sat = 100_000_000;
	let invoice = send_payment_to_external_node(&node, external_node, invoice_amount_sat);
	check_send_payment_succeeds(&node, external_node, invoice);

	// Send a payment to LDK
	let amount_msat = 9_000_000;
	receive_payment_from_external_node(&node, external_node, amount_msat);

	// Test overpayment: Send more than the invoice amount to the external node
	let overpaid_invoice_amount_msat = 100_000_000;
	let overpaid_amount_msat = overpaid_invoice_amount_msat + 10_000; // Excesso de 10k msat
	let overpaid_invoice = external_node.create_invoice(overpaid_invoice_amount_msat, None);
	let overpaid_bolt11_invoice = Bolt11Invoice::from_str(&overpaid_invoice).unwrap();
	let _overpaid_payment_id = node
		.bolt11_payment()
		.send_using_amount(&overpaid_bolt11_invoice, overpaid_amount_msat, None)
		.unwrap();
	expect_event!(node, PaymentSuccessful);
	external_node.check_receive_payment(overpaid_bolt11_invoice); // Assumindo que o externo aceita overpayment

	// Test underpayment: Attempt to send less than invoice amount (should fail)
	let underpaid_invoice_amount_msat = 100_000_000;
	let underpaid_amount_msat = underpaid_invoice_amount_msat - 1;
	let underpaid_invoice = external_node.create_invoice(underpaid_invoice_amount_msat, None);
	let underpaid_bolt11_invoice = Bolt11Invoice::from_str(&underpaid_invoice).unwrap();
	assert_eq!(
		Err(NodeError::InvalidAmount),
		node.bolt11_payment().send_using_amount(
			&underpaid_bolt11_invoice,
			underpaid_amount_msat,
			None
		)
	);

	// Test variable amount invoice: External node creates zero-amount invoice, LDK pays a determined amount
	let variable_invoice = external_node.create_invoice(0, None);
	let variable_bolt11_invoice = Bolt11Invoice::from_str(&variable_invoice).unwrap();
	let determined_amount_msat = 50_000_000;
	let _variable_payment_id = node
		.bolt11_payment()
		.send_using_amount(&variable_bolt11_invoice, determined_amount_msat, None)
		.unwrap();
	expect_event!(node, PaymentSuccessful);

	// Close the channel
	close_channel(
		&bitcoind_client,
		&electrs_client,
		external_node,
		&node,
		funding_txo,
		external_node_id,
		&user_channel_id,
		external_force_close,
	);

	node.stop().unwrap();
}
