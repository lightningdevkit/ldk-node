#![cfg(lnd_test)]

mod common;

use bdk_chain::miniscript::ToPublicKey;
use bitcoin::hashes::Hash;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

use lnd_grpc_rust::lnrpc::{
	channel_point, invoice::InvoiceState as LndInvoiceState, ChannelPoint, CloseChannelRequest,
	GetInfoRequest as LndGetInfoRequest, Invoice as LndInvoice,
	QueryRoutesRequest as LndQueryRoutesRequest, SendRequest as LndSendRequest,
};

use lnd_grpc_rust::{connect, LndClient as ApiLndClient};

use bitcoin::hex::DisplayHex;

use std::default::Default;
use std::str::FromStr;

use crate::common::external_node::{
	do_ldk_opens_channel_full_cycle_with_external_node, init_bitcoind_client, ExternalLightningNode,
};

#[test]
fn test_ldk_initiates_channel_close() {
	init_bitcoind_client();
	let mut client = LndClient::new();
	do_ldk_opens_channel_full_cycle_with_external_node(&mut client, None);
}

#[test]
fn test_lnd_initiates_channel_close() {
	init_bitcoind_client();
	let mut client = LndClient::new();
	do_ldk_opens_channel_full_cycle_with_external_node(&mut client, Some(false));
}

#[test]
fn test_lnd_initiates_force_channel_close() {
	init_bitcoind_client();
	let mut client = LndClient::new();
	do_ldk_opens_channel_full_cycle_with_external_node(&mut client, Some(true));
}

struct LndClient {
	client: ApiLndClient,
	runtime: tokio::runtime::Runtime,
	lnd_address: SocketAddress,
}

impl LndClient {
	fn new() -> Self {
		let endpoint = String::from("127.0.0.1:8081");
		let cert_path = std::env::var("LND_CERT_PATH").expect("LND_CERT_PATH not set");
		let macaroon_path = std::env::var("LND_MACAROON_PATH").expect("LND_MACAROON_PATH not set");
		// Read the contents of the file into a vector of bytes
		let cert_bytes = std::fs::read(cert_path).expect("Failed to read tls cert file");
		let mac_bytes = std::fs::read(macaroon_path).expect("Failed to read macaroon file");

		// Convert the bytes to a hex string
		let cert = cert_bytes.as_hex().to_string();
		let macaroon = mac_bytes.as_hex().to_string();

		let runtime = tokio::runtime::Runtime::new().unwrap();
		let client = runtime.block_on(connect(cert, macaroon, endpoint)).unwrap();

		LndClient { client, runtime, lnd_address: "127.0.0.1:9735".parse().unwrap() }
	}

	fn can_send_payments(&mut self, invoice: &str) {
		let invoice = lightning_invoice::Bolt11Invoice::from_str(invoice)
			.expect("Failed to parse invoice string into Bolt11Invoice");

		// To ensure LND has updated its routes/balance after receiving payment, we attempt to query routes to ourselves
		let available_to_spend_msat = invoice.amount_milli_satoshis().unwrap() as i64;
		let node_id = if let Some(key) = invoice.payee_pub_key() {
			key.to_string()
		} else {
			invoice.recover_payee_pub_key().to_string()
		};
		// Wait up to 30 seconds for LND to update routes/balance after receiving payment; panic if timeout
		let timeout = std::time::Duration::from_secs(30);
		let start = std::time::Instant::now();
		loop {
			let found = self
				.runtime
				.block_on(self.client.lightning().query_routes(LndQueryRoutesRequest {
					pub_key: node_id.clone(),
					amt_msat: available_to_spend_msat as i64,
					..Default::default()
				}))
				.ok()
				.and_then(|resp| Some(!resp.into_inner().routes.is_empty()))
				.unwrap_or(false);

			if found {
				break;
			}
			if start.elapsed() > timeout {
				panic!(
					"LND did not update balance/routes after receiving payment within {} seconds",
					timeout.as_secs()
				);
			}
		}
	}
}

impl ExternalLightningNode for LndClient {
	fn get_node_info(&mut self) -> (PublicKey, SocketAddress) {
		let response = self
			.runtime
			.block_on(self.client.lightning().get_info(LndGetInfoRequest {}))
			.expect("Failed to fetch node info from LND")
			.into_inner();
		let pubkey = PublicKey::from_str(&response.identity_pubkey).unwrap();
		(pubkey, self.lnd_address.clone())
	}

	fn create_invoice(&mut self, amount_msat: u64, _description: Option<String>) -> String {
		let invoice = LndInvoice { value_msat: amount_msat as i64, ..Default::default() };

		self.runtime
			.block_on(self.client.lightning().add_invoice(invoice))
			.expect("Failed to create invoice on LND")
			.into_inner()
			.payment_request
	}

	fn pay_invoice(&mut self, invoice: &str) {
		self.can_send_payments(invoice);
		let send_req = LndSendRequest {
			payment_request: invoice.to_string(),
			fee_limit: Some(lnd_grpc_rust::lnrpc::FeeLimit {
				limit: Some(lnd_grpc_rust::lnrpc::fee_limit::Limit::Fixed(10_000)),
			}),
			..Default::default()
		};
		let response = self
			.runtime
			.block_on(self.client.lightning().send_payment_sync(send_req))
			.expect("Failed to pay invoice on LND")
			.into_inner();
		if !response.payment_error.is_empty() || response.payment_preimage.is_empty() {
			panic!("Failed to pay invoice on LND: {}", response.payment_error);
		}
	}

	fn check_receive_payment(&mut self, invoice: lightning_invoice::Bolt11Invoice) {
		let payment_hash_bytes = invoice.payment_hash().as_byte_array().to_vec();

		let payment_hash_req =
			lnd_grpc_rust::lnrpc::PaymentHash { r_hash: payment_hash_bytes, ..Default::default() };

		let invoice_response = self
			.runtime
			.block_on(self.client.lightning().lookup_invoice(payment_hash_req))
			.expect("Failed to lookup invoice from LND")
			.into_inner();

		// Check that the invoice is payment settled
		if invoice_response.state != LndInvoiceState::Settled as i32 {
			panic!("Invoice not settled: expected Settled, found {:?}", invoice_response.state);
		}
	}

	fn close_channel(&mut self, funding_txo: bitcoin::OutPoint, _: PublicKey, force: bool) {
		let request = CloseChannelRequest {
			channel_point: Some(ChannelPoint {
				funding_txid: Some(channel_point::FundingTxid::FundingTxidStr(
					funding_txo.txid.to_string(),
				)),
				output_index: funding_txo.vout,
			}),
			force,
			no_wait: true,
			..Default::default()
		};

		self.runtime
			.block_on(self.client.lightning().close_channel(request))
			.expect("Failed to initiate close channel on LND");
	}
}
