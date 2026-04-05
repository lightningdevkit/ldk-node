// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use clightningrpc::lightningrpc::LightningRPC;
use clightningrpc::lightningrpc::PayOptions;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use serde_json::json;

use super::external_node::{ExternalChannel, ExternalNode, TestFailure};

pub(crate) struct TestClnNode {
	client: Arc<LightningRPC>,
	listen_addr: SocketAddress,
}

impl TestClnNode {
	pub(crate) fn new(socket_path: &str, listen_addr: SocketAddress) -> Self {
		Self { client: Arc::new(LightningRPC::new(socket_path)), listen_addr }
	}

	pub(crate) fn from_env() -> Self {
		let sock =
			std::env::var("CLN_SOCKET_PATH").unwrap_or_else(|_| "/tmp/lightning-rpc".to_string());
		let listen_addr: SocketAddress = std::env::var("CLN_P2P_ADDR")
			.unwrap_or_else(|_| "127.0.0.1:19846".to_string())
			.parse()
			.unwrap();
		Self::new(&sock, listen_addr)
	}

	/// Run a synchronous CLN RPC call on a dedicated blocking thread.
	///
	/// The `clightningrpc` crate performs synchronous I/O over a Unix socket.
	/// Running these calls directly on the tokio runtime would block the worker
	/// thread, preventing LDK's background tasks from making progress and
	/// causing deadlocks (especially with `worker_threads = 1`).
	async fn rpc<F, T>(&self, f: F) -> T
	where
		F: FnOnce(&LightningRPC) -> T + Send + 'static,
		T: Send + 'static,
	{
		let client = Arc::clone(&self.client);
		tokio::task::spawn_blocking(move || f(&*client)).await.expect("CLN RPC task panicked")
	}

	/// Repeatedly call `splice_update` until `commitments_secured` is true.
	/// Returns the final PSBT. Gives up after 10 attempts.
	async fn splice_update_loop(
		&self, channel_id: &str, mut psbt: String,
	) -> Result<String, TestFailure> {
		const MAX_ATTEMPTS: u32 = 10;
		for _ in 0..MAX_ATTEMPTS {
			let ch_id = channel_id.to_string();
			let psbt_arg = psbt.clone();
			let update_result: serde_json::Value = self
				.rpc(move |c| {
					c.call("splice_update", &json!({"channel_id": ch_id, "psbt": psbt_arg}))
				})
				.await
				.map_err(|e| self.make_error(format!("splice_update: {}", e)))?;
			psbt = update_result["psbt"]
				.as_str()
				.ok_or_else(|| self.make_error("splice_update did not return psbt"))?
				.to_string();
			if update_result["commitments_secured"].as_bool() == Some(true) {
				return Ok(psbt);
			}
		}
		Err(self.make_error(format!(
			"splice_update did not reach commitments_secured after {} attempts",
			MAX_ATTEMPTS
		)))
	}
}

/// Parse a CLN msat value which may be either a plain integer or a string like "1000000msat".
fn parse_msat(v: &serde_json::Value) -> u64 {
	v.as_u64().unwrap_or_else(|| {
		v.as_str().and_then(|s| s.strip_suffix("msat")?.parse().ok()).unwrap_or(0)
	})
}

#[async_trait]
impl ExternalNode for TestClnNode {
	fn name(&self) -> &str {
		"CLN"
	}

	// CLN v25 --experimental-splicing uses feature bit 162, but LDK uses
	// BOLT-spec bit 62/63. Splice interop is not possible until CLN migrates
	// from the experimental bit to the standard bit.
	fn supports_splice(&self) -> bool {
		false
	}

	async fn get_node_id(&self) -> Result<PublicKey, TestFailure> {
		let info = self
			.rpc(|c| c.getinfo())
			.await
			.map_err(|e| self.make_error(format!("getinfo: {}", e)))?;
		PublicKey::from_str(&info.id).map_err(|e| self.make_error(format!("parse node id: {}", e)))
	}

	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure> {
		Ok(self.listen_addr.clone())
	}

	async fn get_block_height(&self) -> Result<u64, TestFailure> {
		let info = self
			.rpc(|c| c.getinfo())
			.await
			.map_err(|e| self.make_error(format!("getinfo: {}", e)))?;
		Ok(info.blockheight as u64)
	}

	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure> {
		let uri = format!("{}@{}", peer_id, addr);
		let _: serde_json::Value = self
			.rpc(move |c| c.call("connect", &json!({"id": uri})))
			.await
			.map_err(|e| self.make_error(format!("connect: {}", e)))?;
		Ok(())
	}

	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure> {
		let id = peer_id.to_string();
		let _: serde_json::Value = self
			.rpc(move |c| c.call("disconnect", &json!({"id": id, "force": true})))
			.await
			.map_err(|e| self.make_error(format!("disconnect: {}", e)))?;
		Ok(())
	}

	async fn open_channel(
		&self, peer_id: PublicKey, _addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure> {
		// Use the generic `call` method to include `push_msat`, which the
		// typed `fundchannel` method does not support.
		let mut params = json!({
			"id": peer_id.to_string(),
			"amount": capacity_sat,
		});
		if let Some(push) = push_msat {
			params["push_msat"] = json!(push);
		}

		let result: serde_json::Value = self
			.rpc(move |c| c.call("fundchannel", &params))
			.await
			.map_err(|e| self.make_error(format!("fundchannel: {}", e)))?;

		let channel_id = result["channel_id"]
			.as_str()
			.ok_or_else(|| self.make_error("fundchannel did not return channel_id"))?;
		Ok(channel_id.to_string())
	}

	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		let ch_id = channel_id.to_string();
		self.rpc(move |c| c.close(&ch_id, None, None))
			.await
			.map_err(|e| self.make_error(format!("close: {}", e)))?;
		Ok(())
	}

	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		// CLN v23.08 removed the `force` parameter; use `unilateraltimeout: 1`
		// to trigger an immediate unilateral close.
		let ch_id = channel_id.to_string();
		let _: serde_json::Value = self
			.rpc(move |c| c.call("close", &json!({"id": ch_id, "unilateraltimeout": 1})))
			.await
			.map_err(|e| self.make_error(format!("force close: {}", e)))?;
		Ok(())
	}

	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure> {
		let desc = description.to_string();
		let label = format!(
			"{}-{}",
			desc,
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap_or_default()
				.as_nanos()
		);
		let invoice = self
			.rpc(move |c| c.invoice(Some(amount_msat), &label, &desc, None, None, None))
			.await
			.map_err(|e| self.make_error(format!("invoice: {}", e)))?;
		Ok(invoice.bolt11)
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure> {
		let inv = invoice.to_string();
		let result = self
			.rpc(move |c| c.pay(&inv, PayOptions::default()))
			.await
			.map_err(|e| self.make_error(format!("pay: {}", e)))?;
		Ok(result.payment_preimage)
	}

	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure> {
		let dest = peer_id.to_string();
		let result: serde_json::Value = self
			.rpc(move |c| {
				c.call(
					"keysend",
					// maxdelay=288 (2 days): LDK requires HTLC_FAIL_BACK_BUFFER(39)+3=42 blocks
					// minimum final CLTV, but CLN's default is too low for direct-peer keysend.
					&json!({"destination": dest, "amount_msat": amount_msat, "maxdelay": 288}),
				)
			})
			.await
			.map_err(|e| self.make_error(format!("keysend: {}", e)))?;
		let preimage = result["payment_preimage"]
			.as_str()
			.ok_or_else(|| self.make_error("keysend did not return payment_preimage"))?;
		Ok(preimage.to_string())
	}

	async fn get_funding_address(&self) -> Result<String, TestFailure> {
		let addr = self
			.rpc(|c| c.newaddr(None))
			.await
			.map_err(|e| self.make_error(format!("newaddr: {}", e)))?;
		addr.bech32.ok_or_else(|| self.make_error("no bech32 address returned"))
	}

	async fn splice_in(&self, channel_id: &str, amount_sat: u64) -> Result<(), TestFailure> {
		// Step 1: splice_init with positive relative_amount
		let ch_id = channel_id.to_string();
		let amount: i64 = amount_sat.try_into().map_err(|_| {
			self.make_error(format!("splice_in: amount_sat overflow: {}", amount_sat))
		})?;
		let init_result: serde_json::Value = self
			.rpc(move |c| {
				c.call("splice_init", &json!({"channel_id": ch_id, "relative_amount": amount}))
			})
			.await
			.map_err(|e| self.make_error(format!("splice_init: {}", e)))?;
		let mut psbt = init_result["psbt"]
			.as_str()
			.ok_or_else(|| self.make_error("splice_init did not return psbt"))?
			.to_string();

		// Step 2: splice_update until commitments_secured
		psbt = self.splice_update_loop(channel_id, psbt).await?;

		// Step 3: splice_signed
		let ch_id = channel_id.to_string();
		let _: serde_json::Value = self
			.rpc(move |c| c.call("splice_signed", &json!({"channel_id": ch_id, "psbt": psbt})))
			.await
			.map_err(|e| self.make_error(format!("splice_signed: {}", e)))?;
		Ok(())
	}

	async fn splice_out(
		&self, channel_id: &str, amount_sat: u64, address: Option<&str>,
	) -> Result<(), TestFailure> {
		// CLN splice-out uses negative relative_amount.
		// Funds always go to CLN's own wallet; specifying a custom address
		// would require manual PSBT manipulation which is out of scope.
		if address.is_some() {
			return Err(self.make_error(
				"splice_out with custom address is not supported by CLN adapter".to_string(),
			));
		}
		let ch_id = channel_id.to_string();
		let positive: i64 = amount_sat.try_into().map_err(|_| {
			self.make_error(format!("splice_out: amount_sat overflow: {}", amount_sat))
		})?;
		let amount = -positive;
		let init_result: serde_json::Value = self
			.rpc(move |c| {
				c.call("splice_init", &json!({"channel_id": ch_id, "relative_amount": amount}))
			})
			.await
			.map_err(|e| self.make_error(format!("splice_init: {}", e)))?;
		let mut psbt = init_result["psbt"]
			.as_str()
			.ok_or_else(|| self.make_error("splice_init did not return psbt"))?
			.to_string();

		psbt = self.splice_update_loop(channel_id, psbt).await?;

		let ch_id = channel_id.to_string();
		let _: serde_json::Value = self
			.rpc(move |c| c.call("splice_signed", &json!({"channel_id": ch_id, "psbt": psbt})))
			.await
			.map_err(|e| self.make_error(format!("splice_signed: {}", e)))?;
		Ok(())
	}

	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure> {
		let response: serde_json::Value = self
			.rpc(|c| c.call("listpeerchannels", &serde_json::Map::new()))
			.await
			.map_err(|e| self.make_error(format!("listpeerchannels: {}", e)))?;
		let mut channels = Vec::new();

		for ch in response["channels"].as_array().unwrap_or(&vec![]) {
			let peer_id_str = ch["peer_id"]
				.as_str()
				.ok_or_else(|| self.make_error("list_channels: missing peer_id"))?;
			let peer_id = PublicKey::from_str(peer_id_str).map_err(|e| {
				self.make_error(format!("list_channels: invalid peer_id '{}': {}", peer_id_str, e))
			})?;
			let channel_id = ch["channel_id"]
				.as_str()
				.ok_or_else(|| self.make_error("list_channels: missing channel_id"))?
				.to_string();
			let total_msat = parse_msat(&ch["total_msat"]);
			let to_us_msat = parse_msat(&ch["to_us_msat"]);
			let funding_txid = ch["funding_txid"].as_str().map(String::from);
			let state = ch["state"].as_str().unwrap_or("");
			channels.push(ExternalChannel {
				channel_id,
				peer_id,
				capacity_sat: total_msat / 1000,
				local_balance_msat: to_us_msat,
				remote_balance_msat: total_msat.saturating_sub(to_us_msat),
				funding_txid,
				is_active: state == "CHANNELD_NORMAL",
			});
		}
		Ok(channels)
	}
}
