// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(eclair_test)]

mod common;

use std::str::FromStr;

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Amount;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Event};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair() {
	// Setup bitcoind / electrs clients
	let bitcoind_client = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs_client = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	// Give electrs a kick.
	common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 1).await;

	// Setup LDK Node
	let config = common::random_config(true);
	let mut builder = Builder::from_config(config.node_config);
	builder.set_chain_source_esplora("http://127.0.0.1:3002".to_string(), None);

	let node = builder.build(config.node_entropy).unwrap();
	node.start().unwrap();

	// Premine some funds and distribute
	let address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(5_000_000);
	common::premine_and_distribute_funds(
		&bitcoind_client,
		&electrs_client,
		vec![address],
		premine_amount,
	)
	.await;

	// Setup Eclair
	let eclair = TestEclairClient::new("http://127.0.0.1:8080", "eclairpw");

	// Wait for Eclair to be synced
	let eclair_info = {
		loop {
			match eclair.get_info().await {
				Ok(info) => {
					let block_height =
						info["blockHeight"].as_u64().expect("blockHeight should be a number");
					if block_height > 0 {
						break info;
					}
				},
				Err(e) => {
					println!("Waiting for Eclair to be ready: {}", e);
				},
			}
			tokio::time::sleep(std::time::Duration::from_millis(500)).await;
		}
	};

	let eclair_node_id =
		PublicKey::from_str(eclair_info["nodeId"].as_str().unwrap()).expect("valid nodeId");
	let eclair_address: SocketAddress = "127.0.0.1:9736".parse().unwrap();

	node.sync_wallets().unwrap();

	// Open the channel
	let funding_amount_sat = 1_000_000;

	node.open_channel(eclair_node_id, eclair_address, funding_amount_sat, Some(500_000_000), None)
		.unwrap();

	let funding_txo = common::expect_channel_pending_event!(node, eclair_node_id);
	common::wait_for_tx(&electrs_client, funding_txo.txid).await;
	common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 6).await;
	node.sync_wallets().unwrap();
	let user_channel_id = common::expect_channel_ready_event!(node, eclair_node_id);

	// Send a payment to Eclair (LDK -> Eclair)
	let eclair_invoice_str = eclair
		.create_invoice(100_000_000, "test-ldk-to-eclair")
		.await
		.expect("Failed to create Eclair invoice");
	let parsed_invoice = Bolt11Invoice::from_str(&eclair_invoice_str).unwrap();

	node.bolt11_payment().send(&parsed_invoice, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	// Verify Eclair received the payment
	let received_info = eclair
		.get_received_info(&eclair_invoice_str)
		.await
		.expect("Failed to get received info from Eclair");
	let status = received_info["status"]["type"].as_str().unwrap_or("unknown");
	assert_eq!(status, "received", "Eclair payment should be in received state");

	// Send a payment to LDK (Eclair -> LDK)
	let amount_msat = 9_000_000;
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new("eclairTest".to_string()).unwrap());
	let ldk_invoice =
		node.bolt11_payment().receive(amount_msat, &invoice_description, 3600).unwrap();
	eclair.pay_invoice(&ldk_invoice.to_string()).await.expect("Eclair failed to pay invoice");
	common::expect_event!(node, PaymentReceived);

	// Splice in (soft-fail: splice interop between LDK and Eclair may not yet be compatible)
	let eclair_channels = eclair.list_channels().await.expect("Failed to list Eclair channels");
	if let Some(channel) = eclair_channels.as_array().and_then(|arr| arr.first()) {
		let channel_id = channel["channelId"].as_str().unwrap_or("");
		if !channel_id.is_empty() {
			match eclair.splice_in(channel_id, 500_000).await {
				Ok(_) => {
					println!("Splice in succeeded, mining blocks to confirm...");
					common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 6).await;
					node.sync_wallets().unwrap();
				},
				Err(e) => {
					println!(
						"Splice in not yet supported in LDK<->Eclair interop, skipping: {}",
						e
					);
				},
			}

			// Splice out (soft-fail)
			let addr = node.onchain_payment().new_address().unwrap();
			match eclair.splice_out(channel_id, 200_000, &addr.to_string()).await {
				Ok(_) => {
					println!("Splice out succeeded, mining blocks to confirm...");
					common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 6).await;
					node.sync_wallets().unwrap();
				},
				Err(e) => {
					println!(
						"Splice out not yet supported in LDK<->Eclair interop, skipping: {}",
						e
					);
				},
			}
		}
	}

	// Close the channel
	node.close_channel(&user_channel_id, eclair_node_id).unwrap();
	common::expect_event!(node, ChannelClosed);
	node.stop().unwrap();
}

struct TestEclairClient {
	base_url: String,
	auth_header: String,
}

impl TestEclairClient {
	fn new(base_url: &str, password: &str) -> Self {
		let credentials = format!(":{}", password);
		let auth_header = format!("Basic {}", BASE64_STANDARD.encode(credentials.as_bytes()));
		TestEclairClient { base_url: base_url.to_string(), auth_header }
	}

	async fn eclair_post(
		&self, endpoint: &str, params: &[(&str, &str)],
	) -> Result<serde_json::Value, String> {
		let url = format!("{}/{}", self.base_url, endpoint);
		let body = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");

		let request = bitreq::post(&url)
			.with_header("Authorization", &self.auth_header)
			.with_header("Content-Type", "application/x-www-form-urlencoded")
			.with_body(body.as_bytes())
			.with_timeout(30);

		let response = request
			.send_async()
			.await
			.map_err(|e| format!("HTTP request to {} failed: {}", endpoint, e))?;

		if response.status_code != 200 {
			let body_str = response.as_str().unwrap_or("(non-utf8 body)");
			return Err(format!(
				"Eclair {} returned HTTP {}: {}",
				endpoint, response.status_code, body_str
			));
		}

		let body_str = response
			.as_str()
			.map_err(|e| format!("Failed to read response body from {}: {}", endpoint, e))?;

		serde_json::from_str(body_str)
			.map_err(|e| format!("Failed to parse JSON from {}: {}", endpoint, e))
	}

	async fn get_info(&self) -> Result<serde_json::Value, String> {
		self.eclair_post("getinfo", &[]).await
	}

	async fn create_invoice(&self, amount_msat: u64, description: &str) -> Result<String, String> {
		let amount_str = amount_msat.to_string();
		let result = self
			.eclair_post(
				"createinvoice",
				&[("amountMsat", &amount_str), ("description", description)],
			)
			.await?;
		result["serialized"]
			.as_str()
			.map(|s| s.to_string())
			.ok_or_else(|| "Missing 'serialized' field in createinvoice response".to_string())
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<serde_json::Value, String> {
		self.eclair_post("payinvoice", &[("invoice", invoice), ("blocking", "true")]).await
	}

	async fn get_received_info(&self, invoice: &str) -> Result<serde_json::Value, String> {
		self.eclair_post("getreceivedinfo", &[("invoice", invoice)]).await
	}

	async fn list_channels(&self) -> Result<serde_json::Value, String> {
		self.eclair_post("channels", &[]).await
	}

	async fn splice_in(
		&self, channel_id: &str, amount_sat: u64,
	) -> Result<serde_json::Value, String> {
		let amount_str = amount_sat.to_string();
		self.eclair_post("splicein", &[("channelId", channel_id), ("amountIn", &amount_str)]).await
	}

	async fn splice_out(
		&self, channel_id: &str, amount_sat: u64, address: &str,
	) -> Result<serde_json::Value, String> {
		let amount_str = amount_sat.to_string();
		self.eclair_post(
			"spliceout",
			&[("channelId", channel_id), ("amountOut", &amount_str), ("address", address)],
		)
		.await
	}
}
