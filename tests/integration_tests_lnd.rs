#![cfg(lnd_test)]

mod common;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Amount;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Event};

use lnd_grpc_rust::lnrpc::{
	invoice::InvoiceState::Settled as LndInvoiceStateSettled, GetInfoRequest as LndGetInfoRequest,
	GetInfoResponse as LndGetInfoResponse, Invoice as LndInvoice,
	ListInvoiceRequest as LndListInvoiceRequest, QueryRoutesRequest as LndQueryRoutesRequest,
	Route as LndRoute, SendRequest as LndSendRequest,
};
use lnd_grpc_rust::{connect, LndClient};

use bitcoincore_rpc::Auth;
use bitcoincore_rpc::Client as BitcoindClient;

use electrum_client::Client as ElectrumClient;
use lightning_invoice::{Bolt11InvoiceDescription, Description};

use bitcoin::hex::DisplayHex;

use std::default::Default;
use std::str::FromStr;
use tokio::fs;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_lnd() {
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
	let mut builder = Builder::from_config(config.node_config);
	builder.set_chain_source_esplora("http://127.0.0.1:3002".to_string(), None);

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

	// Setup LND
	let endpoint = "127.0.0.1:8081";
	let cert_path = std::env::var("LND_CERT_PATH").expect("LND_CERT_PATH not set");
	let macaroon_path = std::env::var("LND_MACAROON_PATH").expect("LND_MACAROON_PATH not set");
	let mut lnd = TestLndClient::new(cert_path, macaroon_path, endpoint.to_string()).await;

	let lnd_node_info = lnd.get_node_info().await;
	let lnd_node_id = PublicKey::from_str(&lnd_node_info.identity_pubkey).unwrap();
	let lnd_address: SocketAddress = "127.0.0.1:9735".parse().unwrap();

	node.sync_wallets().unwrap();

	// Open the channel
	let funding_amount_sat = 1_000_000;

	node.open_channel(lnd_node_id, lnd_address, funding_amount_sat, Some(500_000_000), None)
		.unwrap();

	let funding_txo = common::expect_channel_pending_event!(node, lnd_node_id);
	common::wait_for_tx(&electrs_client, funding_txo.txid);
	common::generate_blocks_and_wait(&bitcoind_client, &electrs_client, 6);
	node.sync_wallets().unwrap();
	let user_channel_id = common::expect_channel_ready_event!(node, lnd_node_id);

	// Send a payment to LND
	let lnd_invoice = lnd.create_invoice(100_000_000).await;
	let parsed_invoice = lightning_invoice::Bolt11Invoice::from_str(&lnd_invoice).unwrap();

	node.bolt11_payment().send(&parsed_invoice, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);
	let lnd_listed_invoices = lnd.list_invoices().await;
	assert_eq!(lnd_listed_invoices.len(), 1);
	assert_eq!(lnd_listed_invoices.first().unwrap().state, LndInvoiceStateSettled as i32);

	// Check route LND -> LDK
	let amount_msat = 9_000_000;
	let max_retries = 7;
	for attempt in 1..=max_retries {
		match lnd.query_routes(&node.node_id().to_string(), amount_msat).await {
			Ok(routes) => {
				if !routes.is_empty() {
					break;
				}
			},
			Err(err) => {
				if attempt == max_retries {
					panic!("Failed to find route from LND to LDK: {}", err);
				}
			},
		};
		// wait for the payment process
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;
	}

	// Send a payment to LDK
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new("lndTest".to_string()).unwrap());
	let ldk_invoice =
		node.bolt11_payment().receive(amount_msat, &invoice_description, 3600).unwrap();
	lnd.pay_invoice(&ldk_invoice.to_string()).await;
	common::expect_event!(node, PaymentReceived);

	node.close_channel(&user_channel_id, lnd_node_id).unwrap();
	common::expect_event!(node, ChannelClosed);
	node.stop().unwrap();
}

struct TestLndClient {
	client: LndClient,
}

impl TestLndClient {
	async fn new(cert_path: String, macaroon_path: String, socket: String) -> Self {
		// Read the contents of the file into a vector of bytes
		let cert_bytes = fs::read(cert_path).await.expect("Failed to read tls cert file");
		let mac_bytes = fs::read(macaroon_path).await.expect("Failed to read macaroon file");

		// Convert the bytes to a hex string
		let cert = cert_bytes.as_hex().to_string();
		let macaroon = mac_bytes.as_hex().to_string();

		let client = connect(cert, macaroon, socket).await.expect("Failed to connect to Lnd");

		TestLndClient { client }
	}

	async fn get_node_info(&mut self) -> LndGetInfoResponse {
		let response = self
			.client
			.lightning()
			.get_info(LndGetInfoRequest {})
			.await
			.expect("Failed to fetch node info from LND")
			.into_inner();

		response
	}

	async fn create_invoice(&mut self, amount_msat: u64) -> String {
		let invoice = LndInvoice { value_msat: amount_msat as i64, ..Default::default() };

		self.client
			.lightning()
			.add_invoice(invoice)
			.await
			.expect("Failed to create invoice on LND")
			.into_inner()
			.payment_request
	}

	async fn list_invoices(&mut self) -> Vec<LndInvoice> {
		self.client
			.lightning()
			.list_invoices(LndListInvoiceRequest { ..Default::default() })
			.await
			.expect("Failed to list invoices from LND")
			.into_inner()
			.invoices
	}

	async fn query_routes(
		&mut self, pubkey: &str, amount_msat: u64,
	) -> Result<Vec<LndRoute>, String> {
		let request = LndQueryRoutesRequest {
			pub_key: pubkey.to_string(),
			amt_msat: amount_msat as i64,
			..Default::default()
		};

		let response = self
			.client
			.lightning()
			.query_routes(request)
			.await
			.map_err(|err| format!("Failed to query routes from LND: {:?}", err))?
			.into_inner();

		if response.routes.is_empty() {
			return Err(format!("No routes found for pubkey: {}", pubkey));
		}

		Ok(response.routes)
	}

	async fn pay_invoice(&mut self, invoice_str: &str) {
		let send_req =
			LndSendRequest { payment_request: invoice_str.to_string(), ..Default::default() };
		let response = self
			.client
			.lightning()
			.send_payment_sync(send_req)
			.await
			.expect("Failed to pay invoice on LND")
			.into_inner();

		if !response.payment_error.is_empty() || response.payment_preimage.is_empty() {
			panic!(
				"LND payment failed: {}",
				if response.payment_error.is_empty() {
					"No preimage returned"
				} else {
					&response.payment_error
				}
			);
		}
	}
}
