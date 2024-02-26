#![cfg(cln_test)]

mod common;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Amount;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, ChannelType, Event};

use clightningrpc::lightningrpc::LightningRPC;
use clightningrpc::responses::NetworkAddress;

use bitcoincore_rpc::Auth;
use bitcoincore_rpc::Client as BitcoindClient;

use electrum_client::Client as ElectrumClient;
use lightning_invoice::Bolt11Invoice;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use std::default::Default;
use std::str::FromStr;

#[test]
fn test_cln() {
	// Setup bitcoind / electrs clients
	let bitcoind_client = BitcoindClient::new(
		"127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs_client = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	// Give electrs a kick.
	common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 1);

	// Setup LDK Node
	let config = common::random_config(true);
	let mut builder = Builder::from_config(config);
	builder.set_esplora_server("http://127.0.0.1:3002".to_string());

	let node = builder.build().unwrap();
	node.start().unwrap();

	// Premine some funds and distribute
	let address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(5_000_000);
	common::premine_and_distribute_funds(
		&bitcoind_client,
		&electrs_client,
		vec![address],
		premine_amount,
	);

	// Setup CLN
	let sock = "/tmp/lightning-rpc";
	let cln_client = LightningRPC::new(&sock);
	let cln_info = cln_client.getinfo().unwrap();
	let cln_node_id = PublicKey::from_str(&cln_info.id).unwrap();
	let cln_address: SocketAddress = match cln_info.binding.first().unwrap() {
		NetworkAddress::Ipv4 { address, port } => {
			std::net::SocketAddrV4::new(*address, *port).into()
		},
		NetworkAddress::Ipv6 { address, port } => {
			std::net::SocketAddrV6::new(*address, *port, 0, 0).into()
		},
		_ => {
			panic!()
		},
	};

	node.sync_wallets().unwrap();

	// Open the channel
	let funding_amount_sat = 1_000_000;

	node.connect_open_channel(
		cln_node_id,
		cln_address,
		funding_amount_sat,
		Some(500_000_000),
		None,
		false,
	)
	.unwrap();

	let funding_txo = common::expect_channel_pending_event!(node, cln_node_id);
	common::wait_for_tx(&electrs_client, funding_txo.txid);
	common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 6);
	let user_channel_id = common::expect_channel_ready_event!(node, cln_node_id);
	assert_eq!(node.list_channels().first().unwrap().channel_type, Some(ChannelType::Anchors));

	// Send a payment to CLN
	let mut rng = thread_rng();
	let rand_label: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	let cln_invoice =
		cln_client.invoice(Some(2_500_000), &rand_label, &rand_label, None, None, None).unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&cln_invoice.bolt11).unwrap();

	node.bolt11_payment().send(&parsed_invoice).unwrap();
	common::expect_event!(node, PaymentSuccessful);
	let cln_listed_invoices =
		cln_client.listinvoices(Some(&rand_label), None, None, None).unwrap().invoices;
	assert_eq!(cln_listed_invoices.len(), 1);
	assert_eq!(cln_listed_invoices.first().unwrap().status, "paid");

	// Send a payment to LDK
	let rand_label: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	let ldk_invoice = node.bolt11_payment().receive(2_500_000, &rand_label, 3600).unwrap();
	cln_client.pay(&ldk_invoice.to_string(), Default::default()).unwrap();
	common::expect_event!(node, PaymentReceived);

	node.close_channel(&user_channel_id, cln_node_id, false).unwrap();
	common::expect_event!(node, ChannelClosed);
	node.stop().unwrap();
}
