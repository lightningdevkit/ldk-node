// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;

use async_trait::async_trait;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use lnd_grpc_rust::lnrpc::{
	payment::PaymentStatus, CloseChannelRequest as LndCloseChannelRequest,
	ConnectPeerRequest as LndConnectPeerRequest, DisconnectPeerRequest as LndDisconnectPeerRequest,
	GetInfoRequest as LndGetInfoRequest, Invoice as LndInvoice,
	LightningAddress as LndLightningAddress, ListChannelsRequest as LndListChannelsRequest,
	OpenChannelRequest as LndOpenChannelRequest,
};
use lnd_grpc_rust::routerrpc::SendPaymentRequest;
use lnd_grpc_rust::{connect, LndClient};
use tokio::fs;
use tokio::sync::Mutex;

use super::external_node::{ExternalChannel, ExternalNode, TestFailure};

pub(crate) struct TestLndNode {
	client: Mutex<LndClient>,
	listen_addr: SocketAddress,
}

impl TestLndNode {
	pub(crate) async fn new(
		cert_path: String, macaroon_path: String, endpoint: String, listen_addr: SocketAddress,
	) -> Self {
		let cert_bytes = fs::read(&cert_path).await.expect("Failed to read TLS cert file");
		let mac_bytes = fs::read(&macaroon_path).await.expect("Failed to read macaroon file");
		let cert = cert_bytes.as_hex().to_string();
		let macaroon = mac_bytes.as_hex().to_string();
		let client = connect(cert, macaroon, endpoint).await.expect("Failed to connect to LND");
		Self { client: Mutex::new(client), listen_addr }
	}

	pub(crate) async fn from_env() -> Self {
		let cert_path = std::env::var("LND_CERT_PATH").expect("LND_CERT_PATH not set");
		let macaroon_path = std::env::var("LND_MACAROON_PATH").expect("LND_MACAROON_PATH not set");
		let endpoint =
			std::env::var("LND_ENDPOINT").unwrap_or_else(|_| "127.0.0.1:8081".to_string());
		let listen_addr: SocketAddress = std::env::var("LND_P2P_ADDR")
			.unwrap_or_else(|_| "127.0.0.1:9735".to_string())
			.parse()
			.unwrap();
		Self::new(cert_path, macaroon_path, endpoint, listen_addr).await
	}
}

#[async_trait]
impl ExternalNode for TestLndNode {
	fn name(&self) -> &str {
		"LND"
	}

	async fn get_node_id(&self) -> Result<PublicKey, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.get_info(LndGetInfoRequest {})
			.await
			.map_err(|e| self.make_error(format!("get_info: {}", e)))?
			.into_inner();
		PublicKey::from_str(&response.identity_pubkey)
			.map_err(|e| self.make_error(format!("parse pubkey: {}", e)))
	}

	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure> {
		Ok(self.listen_addr.clone())
	}

	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let request = LndConnectPeerRequest {
			addr: Some(LndLightningAddress { pubkey: peer_id.to_string(), host: addr.to_string() }),
			..Default::default()
		};
		client
			.lightning()
			.connect_peer(request)
			.await
			.map_err(|e| self.make_error(format!("connect_peer: {}", e)))?;
		Ok(())
	}

	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let request = LndDisconnectPeerRequest { pub_key: peer_id.to_string() };
		client
			.lightning()
			.disconnect_peer(request)
			.await
			.map_err(|e| self.make_error(format!("disconnect_peer: {}", e)))?;
		Ok(())
	}

	async fn open_channel(
		&self, peer_id: PublicKey, _addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let local_funding_amount: i64 = capacity_sat
			.try_into()
			.map_err(|_| self.make_error(format!("capacity_sat overflow: {}", capacity_sat)))?;
		// LND's OpenChannelRequest accepts push amount in sats only.
		// Sub-satoshi precision is truncated (e.g. 1500 msat → 1 sat).
		let push_sat: i64 = push_msat
			.map(|m| (m / 1000).try_into())
			.transpose()
			.map_err(|_| {
				self.make_error(format!("push_msat overflow: {}", push_msat.unwrap_or(0)))
			})?
			.unwrap_or(0);

		let request = LndOpenChannelRequest {
			node_pubkey: peer_id.serialize().to_vec(),
			local_funding_amount,
			push_sat,
			..Default::default()
		};

		let response = client
			.lightning()
			.open_channel_sync(request)
			.await
			.map_err(|e| self.make_error(format!("open_channel: {}", e)))?
			.into_inner();

		let txid_bytes = match response.funding_txid {
			Some(lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidBytes(bytes)) => {
				bytes
			},
			Some(lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidStr(s)) => {
				bitcoin::Txid::from_str(&s)
					.map_err(|e| {
						self.make_error(format!("open_channel: invalid txid string '{}': {}", s, e))
					})?
					.as_byte_array()
					.to_vec()
			},
			None => return Err(self.make_error("No funding txid in response")),
		};

		// LND returns txid bytes in reversed order
		let mut txid_arr: [u8; 32] = txid_bytes.try_into().map_err(|b: Vec<u8>| {
			self.make_error(format!("open_channel: expected 32-byte txid, got {} bytes", b.len()))
		})?;
		txid_arr.reverse();
		let txid_hex = txid_arr.as_hex().to_string();
		Ok(format!("{}:{}", txid_hex, response.output_index))
	}

	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let (txid_bytes, output_index) = parse_channel_point(channel_id)?;
		let request = LndCloseChannelRequest {
			channel_point: Some(lnd_grpc_rust::lnrpc::ChannelPoint {
				funding_txid: Some(
					lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidBytes(txid_bytes),
				),
				output_index,
			}),
			sat_per_vbyte: 1,
			..Default::default()
		};
		// CloseChannel is a server-streaming RPC that blocks until the close tx
		// is confirmed. We spawn the stream in the background so the caller can
		// mine blocks and wait for the ChannelClosed event separately.
		let stream = client
			.lightning()
			.close_channel(request)
			.await
			.map_err(|e| self.make_error(format!("close_channel: {}", e)))?
			.into_inner();
		tokio::spawn(async move {
			let mut s = stream;
			while let Some(msg) = s.message().await.transpose() {
				if let Err(e) = msg {
					eprintln!("close_channel stream error: {}", e);
					break;
				}
			}
		});
		Ok(())
	}

	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let (txid_bytes, output_index) = parse_channel_point(channel_id)?;
		let request = LndCloseChannelRequest {
			channel_point: Some(lnd_grpc_rust::lnrpc::ChannelPoint {
				funding_txid: Some(
					lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidBytes(txid_bytes),
				),
				output_index,
			}),
			force: true,
			..Default::default()
		};
		let stream = client
			.lightning()
			.close_channel(request)
			.await
			.map_err(|e| self.make_error(format!("force_close_channel: {}", e)))?
			.into_inner();
		tokio::spawn(async move {
			let mut s = stream;
			while let Some(msg) = s.message().await.transpose() {
				if let Err(e) = msg {
					eprintln!("force_close_channel stream error: {}", e);
					break;
				}
			}
		});
		Ok(())
	}

	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let value_msat: i64 = amount_msat
			.try_into()
			.map_err(|_| self.make_error(format!("amount_msat overflow: {}", amount_msat)))?;
		let invoice =
			LndInvoice { value_msat, memo: description.to_string(), ..Default::default() };
		let response = client
			.lightning()
			.add_invoice(invoice)
			.await
			.map_err(|e| self.make_error(format!("create_invoice: {}", e)))?
			.into_inner();
		Ok(response.payment_request)
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let request = SendPaymentRequest {
			payment_request: invoice.to_string(),
			timeout_seconds: 60,
			no_inflight_updates: true,
			..Default::default()
		};

		let mut stream = client
			.router()
			.send_payment_v2(request)
			.await
			.map_err(|e| self.make_error(format!("pay_invoice: {}", e)))?
			.into_inner();

		while let Some(payment) = stream
			.message()
			.await
			.map_err(|e| self.make_error(format!("pay_invoice stream: {}", e)))?
		{
			match PaymentStatus::try_from(payment.status) {
				Ok(PaymentStatus::Succeeded) => {
					return Ok(payment.payment_preimage);
				},
				Ok(PaymentStatus::Failed) => {
					return Err(
						self.make_error(format!("payment failed: {:?}", payment.failure_reason))
					);
				},
				_ => continue,
			}
		}

		Err(self.make_error("payment stream ended without terminal status"))
	}

	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;

		let mut preimage = [0u8; 32];
		rand::Rng::fill(&mut rand::rng(), &mut preimage);
		let payment_hash = sha256::Hash::hash(&preimage).to_byte_array().to_vec();

		// Keysend requires inserting the preimage as TLV record 5482373484.
		let mut dest_custom_records = std::collections::HashMap::new();
		dest_custom_records.insert(5482373484, preimage.to_vec());
		let amt_msat: i64 = amount_msat
			.try_into()
			.map_err(|_| self.make_error(format!("amount_msat overflow: {}", amount_msat)))?;

		let request = SendPaymentRequest {
			dest: peer_id.serialize().to_vec(),
			amt_msat,
			payment_hash,
			dest_custom_records,
			timeout_seconds: 60,
			no_inflight_updates: true,
			..Default::default()
		};

		let mut stream = client
			.router()
			.send_payment_v2(request)
			.await
			.map_err(|e| self.make_error(format!("send_keysend: {}", e)))?
			.into_inner();

		while let Some(payment) =
			stream.message().await.map_err(|e| self.make_error(format!("keysend stream: {}", e)))?
		{
			match PaymentStatus::try_from(payment.status) {
				Ok(PaymentStatus::Succeeded) => {
					return Ok(payment.payment_preimage);
				},
				Ok(PaymentStatus::Failed) => {
					return Err(
						self.make_error(format!("keysend failed: {:?}", payment.failure_reason))
					);
				},
				_ => continue,
			}
		}

		Err(self.make_error("keysend stream ended without terminal status"))
	}

	async fn get_funding_address(&self) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.new_address(lnd_grpc_rust::lnrpc::NewAddressRequest {
				r#type: 4, // TAPROOT_PUBKEY
				..Default::default()
			})
			.await
			.map_err(|e| self.make_error(format!("get_funding_address: {}", e)))?
			.into_inner();
		Ok(response.address)
	}

	async fn get_block_height(&self) -> Result<u64, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.get_info(LndGetInfoRequest {})
			.await
			.map_err(|e| self.make_error(format!("get_info: {}", e)))?
			.into_inner();
		Ok(response.block_height as u64)
	}

	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.list_channels(LndListChannelsRequest { ..Default::default() })
			.await
			.map_err(|e| self.make_error(format!("list_channels: {}", e)))?
			.into_inner();

		let channels = response
			.channels
			.into_iter()
			.map(|ch| {
				let peer_id = PublicKey::from_str(&ch.remote_pubkey).map_err(|e| {
					self.make_error(format!(
						"list_channels: invalid remote_pubkey '{}': {}",
						ch.remote_pubkey, e
					))
				})?;
				// LND reports balances in satoshis; convert to msat (sub-sat precision lost).
				Ok(ExternalChannel {
					channel_id: ch.channel_point.clone(),
					peer_id,
					capacity_sat: ch.capacity as u64,
					local_balance_msat: ch.local_balance as u64 * 1000,
					remote_balance_msat: ch.remote_balance as u64 * 1000,
					funding_txid: ch.channel_point.split(':').next().map(String::from),
					is_active: ch.active,
					pending_htlcs_count: ch.pending_htlcs.len(),
				})
			})
			.collect::<Result<Vec<_>, _>>()?;

		Ok(channels)
	}
}

/// Parse a channel point string "txid:output_index" into (txid_bytes, output_index).
fn parse_channel_point(channel_point: &str) -> Result<(Vec<u8>, u32), TestFailure> {
	let err = |msg: String| TestFailure::ExternalNodeError { node: "LND".to_string(), detail: msg };
	let (txid_str, idx_str) = channel_point
		.split_once(':')
		.ok_or_else(|| err(format!("invalid format: {}", channel_point)))?;
	let txid = bitcoin::Txid::from_str(txid_str).map_err(|e| err(format!("bad txid: {}", e)))?;
	let output_index: u32 = idx_str.parse().map_err(|e| err(format!("bad output index: {}", e)))?;
	Ok((txid.as_byte_array().to_vec(), output_index))
}
