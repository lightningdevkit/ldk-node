// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(cln_test)]

mod common;

use clightningrpc::lightningrpc::LightningRPC;
use clightningrpc::responses::NetworkAddress;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use std::default::Default;
use std::str::FromStr;

use crate::common::external_node::{
	do_ldk_opens_channel_full_cycle_with_external_node, init_bitcoind_client, ExternalLightningNode,
};

#[test]
fn test_ldk_initiates_channel_close() {
	init_bitcoind_client();
	let mut client = ClnClient::new();
	do_ldk_opens_channel_full_cycle_with_external_node(&mut client, Some(true));
}

#[test]
fn test_cln_initiates_channel_close() {
	init_bitcoind_client();
	let mut client = ClnClient::new();
	do_ldk_opens_channel_full_cycle_with_external_node(&mut client, Some(false));
}

#[test]
fn test_cln_initiates_force_channel_close() {
	init_bitcoind_client();
	let mut client = ClnClient::new();
	do_ldk_opens_channel_full_cycle_with_external_node(&mut client, None);
}

struct ClnClient {
	pub client: LightningRPC,
	pub rng: rand::rngs::ThreadRng,
}

impl ClnClient {
	pub fn new() -> Self {
		let sock = std::env::var("CLN_SOCK").expect("CLN_SOCK must be set");
		let cln_client = LightningRPC::new(&sock);

		use std::time::{Duration, Instant};
		let start = Instant::now();
		loop {
			match cln_client.getinfo() {
				Ok(info) => {
					if info.blockheight > 0 {
						return Self { client: cln_client, rng: thread_rng() };
					} else if start.elapsed() > Duration::from_secs(30) {
						panic!("Timeout waiting for cln to be ready");
					}
				},
				Err(e) => {
					panic!("Not able to connect to cln: {}", e);
				},
			}
		}
	}
}

impl ExternalLightningNode for ClnClient {
	fn get_node_info(&mut self) -> (PublicKey, SocketAddress) {
		let info = self.client.getinfo().unwrap();
		let node_id = PublicKey::from_str(&info.id).unwrap();
		let address: SocketAddress = match info.binding.first().unwrap() {
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
		(node_id, address)
	}

	fn create_invoice(&mut self, amount_msat: u64, _description: Option<String>) -> String {
		let description = _description
			.unwrap_or_else(|| (0..7).map(|_| self.rng.sample(Alphanumeric) as char).collect());
		let rand_label: String = (0..7).map(|_| self.rng.sample(Alphanumeric) as char).collect();
		let amount = if amount_msat == 0 { None } else { Some(amount_msat) };
		let cln_invoice =
			self.client.invoice(amount, &rand_label, &description, None, None, None).unwrap();

		cln_invoice.bolt11
	}

	fn check_receive_payment(&mut self, invoice: lightning_invoice::Bolt11Invoice) {
		let payment_hash = invoice.payment_hash().to_string();
		let invoices =
			self.client.listinvoices(None, None, Some(&payment_hash), None).unwrap().invoices;

		assert_eq!(invoices.len(), 1);
		assert_eq!(invoices[0].status, "paid");
	}

	fn pay_invoice(&mut self, invoice: &str) {
		self.client.pay(invoice, Default::default()).unwrap();
	}

	fn close_channel(&mut self, _out_point: bitcoin::OutPoint, peer_id: PublicKey, force: bool) {
		// Find the channel to close
		let response_channels =
			self.client.listchannels(None, None, Some(&peer_id.to_string())).unwrap();
		if response_channels.channels.is_empty() && response_channels.channels.len() > 1 {
			panic!("No channels to close");
		}
		let short_channel_id = &response_channels.channels[0].short_channel_id;

		let response: clightningrpc::responses::Close;
		if force {
			let unilateral_timeout = Some(1);
			let input = serde_json::json!({
				"id": short_channel_id,
				"unilateraltimeout": unilateral_timeout,
			});
			response = self.client.call("close", input).unwrap();
			assert_eq!(response.type_, "unilateral");
		} else {
			response = self.client.close(short_channel_id, None, None).unwrap();
			assert_eq!(response.type_, "mutual");
		}
	}
}
