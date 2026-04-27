// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;

use async_trait::async_trait;
use base64::prelude::{Engine as _, BASE64_STANDARD};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use serde_json::Value;

use super::external_node::{ExternalChannel, ExternalNode, TestFailure};

/// Percent-encode a string for `application/x-www-form-urlencoded` form values.
fn form_encode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
				out.push(b as char);
			},
			b' ' => out.push('+'),
			_ => out.push_str(&format!("%{:02X}", b)),
		}
	}
	out
}

pub(crate) struct TestEclairNode {
	base_url: String,
	auth_header: String,
	listen_addr: SocketAddress,
}

impl TestEclairNode {
	pub(crate) fn new(base_url: &str, password: &str, listen_addr: SocketAddress) -> Self {
		let credentials = BASE64_STANDARD.encode(format!(":{}", password));
		Self {
			base_url: base_url.to_string(),
			auth_header: format!("Basic {}", credentials),
			listen_addr,
		}
	}

	pub(crate) fn from_env() -> Self {
		let base_url =
			std::env::var("ECLAIR_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
		let password =
			std::env::var("ECLAIR_API_PASSWORD").unwrap_or_else(|_| "eclairpassword".to_string());
		let listen_addr: SocketAddress = std::env::var("ECLAIR_P2P_ADDR")
			.unwrap_or_else(|_| "127.0.0.1:9736".to_string())
			.parse()
			.unwrap();
		Self::new(&base_url, &password, listen_addr)
	}

	async fn post(&self, endpoint: &str, params: &[(&str, &str)]) -> Result<Value, TestFailure> {
		let url = format!("{}{}", self.base_url, endpoint);
		let body = params
			.iter()
			.map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
			.collect::<Vec<_>>()
			.join("&");

		let request = bitreq::post(&url)
			.with_header("Authorization", &self.auth_header)
			.with_header("Content-Type", "application/x-www-form-urlencoded")
			.with_body(body)
			.with_timeout(30);

		let response = request
			.send_async()
			.await
			.map_err(|e| self.make_error(format!("request to {} failed: {}", endpoint, e)))?;

		if response.status_code < 200 || response.status_code >= 300 {
			let body_str = response.as_str().unwrap_or("(non-utf8 body)");
			return Err(self.make_error(format!(
				"{} returned {}: {}",
				endpoint, response.status_code, body_str
			)));
		}

		let body_str = response
			.as_str()
			.map_err(|e| self.make_error(format!("reading response from {}: {}", endpoint, e)))?;

		serde_json::from_str(body_str).map_err(|e| {
			self.make_error(format!(
				"parsing response from {}: {} (body: {})",
				endpoint, e, body_str
			))
		})
	}

	/// Poll /getsentinfo until the payment settles or fails. Surfaces Eclair-side
	/// failure reasons rather than waiting for an opaque LDK event timeout.
	async fn poll_payment_settlement(
		&self, payment_id: &str, label: &str,
	) -> Result<String, TestFailure> {
		let timeout_secs = super::INTEROP_TIMEOUT_SECS;
		let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
		loop {
			if tokio::time::Instant::now() >= deadline {
				return Err(self.make_error(format!(
					"{} {} did not settle within {}s",
					label, payment_id, timeout_secs
				)));
			}
			tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
			let info = self.post("/getsentinfo", &[("id", payment_id)]).await?;
			if let Some(attempts) = info.as_array() {
				if let Some(last) = attempts.last() {
					let status = last["status"]["type"].as_str().unwrap_or("");
					if status == "sent" {
						return Ok(payment_id.to_string());
					} else if status == "failed" {
						let failure = last["status"]["failures"]
							.as_array()
							.and_then(|f| f.last())
							.and_then(|f| f["failureMessage"].as_str())
							.unwrap_or("unknown");
						return Err(self
							.make_error(format!("{} {} failed: {}", label, payment_id, failure)));
					}
				}
			}
		}
	}
}

#[async_trait]
impl ExternalNode for TestEclairNode {
	fn name(&self) -> &str {
		"Eclair"
	}

	async fn get_node_id(&self) -> Result<PublicKey, TestFailure> {
		let info = self.post("/getinfo", &[]).await?;
		let node_id_str = info["nodeId"]
			.as_str()
			.ok_or_else(|| self.make_error("missing nodeId in getinfo response"))?;
		PublicKey::from_str(node_id_str)
			.map_err(|e| self.make_error(format!("parse nodeId: {}", e)))
	}

	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure> {
		Ok(self.listen_addr.clone())
	}

	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure> {
		let uri = format!("{}@{}", peer_id, addr);
		self.post("/connect", &[("uri", &uri)]).await?;
		Ok(())
	}

	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure> {
		self.post("/disconnect", &[("nodeId", &peer_id.to_string())]).await?;
		Ok(())
	}

	async fn open_channel(
		&self, peer_id: PublicKey, _addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure> {
		let node_id = peer_id.to_string();
		let capacity = capacity_sat.to_string();
		let push_str = push_msat.map(|m| m.to_string());

		let mut params = vec![("nodeId", node_id.as_str()), ("fundingSatoshis", capacity.as_str())];
		if let Some(ref push) = push_str {
			params.push(("pushMsat", push.as_str()));
		}

		let result = self.post("/open", &params).await?;
		let channel_id = result
			.as_str()
			.map(String::from)
			.or_else(|| result["channelId"].as_str().map(String::from))
			.ok_or_else(|| {
				self.make_error(format!("open did not return channel id: {}", result))
			})?;
		Ok(channel_id)
	}

	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		self.post("/close", &[("channelId", channel_id)]).await?;
		Ok(())
	}

	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		self.post("/forceclose", &[("channelId", channel_id)]).await?;
		Ok(())
	}

	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure> {
		let amount_str = amount_msat.to_string();
		let result = self
			.post("/createinvoice", &[("amountMsat", &amount_str), ("description", description)])
			.await?;
		let invoice = result["serialized"]
			.as_str()
			.ok_or_else(|| self.make_error("missing serialized in invoice response"))?;
		Ok(invoice.to_string())
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure> {
		let result = self.post("/payinvoice", &[("invoice", invoice)]).await?;
		let payment_id = result
			.as_str()
			.ok_or_else(|| self.make_error("payinvoice did not return payment id"))?
			.to_string();
		self.poll_payment_settlement(&payment_id, "payment").await
	}

	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure> {
		let amount_str = amount_msat.to_string();
		let node_id_str = peer_id.to_string();
		let result = self
			.post("/sendtonode", &[("nodeId", &node_id_str), ("amountMsat", &amount_str)])
			.await?;
		let payment_id = result
			.as_str()
			.ok_or_else(|| self.make_error("sendtonode did not return payment id"))?
			.to_string();
		self.poll_payment_settlement(&payment_id, "keysend").await
	}

	async fn get_funding_address(&self) -> Result<String, TestFailure> {
		let result = self.post("/getnewaddress", &[]).await?;
		result
			.as_str()
			.map(String::from)
			.ok_or_else(|| self.make_error("getnewaddress did not return string"))
	}

	async fn get_block_height(&self) -> Result<u64, TestFailure> {
		let info = self.post("/getinfo", &[]).await?;
		info["blockHeight"]
			.as_u64()
			.ok_or_else(|| self.make_error("missing blockHeight in getinfo response"))
	}

	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure> {
		let result = self.post("/channels", &[]).await?;
		let channels_arr =
			result.as_array().ok_or_else(|| self.make_error("/channels did not return array"))?;

		let mut channels = Vec::new();
		for ch in channels_arr {
			let channel_id = ch["channelId"]
				.as_str()
				.ok_or_else(|| self.make_error("list_channels: missing channelId"))?
				.to_string();
			let node_id_str = ch["nodeId"]
				.as_str()
				.ok_or_else(|| self.make_error("list_channels: missing nodeId"))?;
			let peer_id = PublicKey::from_str(node_id_str).map_err(|e| {
				self.make_error(format!("list_channels: invalid nodeId '{}': {}", node_id_str, e))
			})?;
			let state_str = ch["state"].as_str().unwrap_or("");
			let commitments = &ch["data"]["commitments"];

			// Closed/closing channels may lack active commitments -- skip them.
			let active_commitment = match commitments["active"].as_array().and_then(|a| a.first()) {
				Some(c) => c,
				None => continue,
			};

			let capacity_sat = active_commitment["fundingAmount"]
				.as_u64()
				.ok_or_else(|| self.make_error("list_channels: missing fundingAmount"))?;
			let funding_txid = active_commitment["fundingInput"]
				.as_str()
				.and_then(|s| s.split(':').next())
				.map(String::from);
			let local_balance_msat =
				active_commitment["localCommit"]["spec"]["toLocal"].as_u64().ok_or_else(|| {
					self.make_error("list_channels: missing localCommit.spec.toLocal")
				})?;
			let remote_balance_msat =
				active_commitment["localCommit"]["spec"]["toRemote"].as_u64().ok_or_else(|| {
					self.make_error("list_channels: missing localCommit.spec.toRemote")
				})?;

			let pending_htlcs_count = active_commitment["localCommit"]["spec"]["htlcs"]
				.as_array()
				.map(|a| a.len())
				.unwrap_or(0);
			channels.push(ExternalChannel {
				channel_id,
				peer_id,
				capacity_sat,
				local_balance_msat,
				remote_balance_msat,
				funding_txid,
				is_active: state_str == "NORMAL",
				pending_htlcs_count,
			});
		}
		Ok(channels)
	}

	async fn splice_in(&self, channel_id: &str, amount_sat: u64) -> Result<(), TestFailure> {
		let amount_str = amount_sat.to_string();
		self.post("/splicein", &[("channelId", channel_id), ("amountIn", &amount_str)]).await?;
		Ok(())
	}

	async fn splice_out(
		&self, channel_id: &str, amount_sat: u64, address: Option<&str>,
	) -> Result<(), TestFailure> {
		// Eclair's /spliceout requires an address; if caller passes None, generate one
		// from Eclair's own wallet so the trait contract is symmetric with CLN.
		let owned_addr;
		let addr = match address {
			Some(a) => a,
			None => {
				owned_addr = self.get_funding_address().await?;
				owned_addr.as_str()
			},
		};
		let amount_str = amount_sat.to_string();
		self.post(
			"/spliceout",
			&[("channelId", channel_id), ("amountOut", &amount_str), ("address", addr)],
		)
		.await?;
		Ok(())
	}
}
