// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::fmt;

use async_trait::async_trait;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

/// Represents a channel opened to or from an external Lightning node.
#[derive(Debug, Clone)]
pub(crate) struct ExternalChannel {
	/// Implementation-specific channel identifier; treat as opaque.
	pub channel_id: String,
	pub peer_id: PublicKey,
	pub capacity_sat: u64,
	pub local_balance_msat: u64,
	pub remote_balance_msat: u64,
	pub funding_txid: Option<String>,
	pub is_active: bool,
	/// In-flight HTLCs on the peer's view of the channel.
	pub pending_htlcs_count: usize,
}

/// Errors that can occur during interop test operations.
#[derive(Debug)]
pub(crate) enum TestFailure {
	ExternalNodeError { node: String, detail: String },
	NotSupported { node: String, operation: String },
}

impl fmt::Display for TestFailure {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			TestFailure::ExternalNodeError { node, detail } => {
				write!(f, "External node '{}' error: {}", node, detail)
			},
			TestFailure::NotSupported { node, operation } => {
				write!(f, "'{}' does not support '{}'", node, operation)
			},
		}
	}
}

impl std::error::Error for TestFailure {}

/// Abstraction over an external Lightning node used in interop tests.
#[async_trait]
pub(crate) trait ExternalNode: Send + Sync {
	/// Human-readable name for this node (e.g. "eclair", "lnd", "cln").
	fn name(&self) -> &str;

	/// Returns the node's public key.
	async fn get_node_id(&self) -> Result<PublicKey, TestFailure>;

	/// Returns an address on which this node is listening.
	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure>;

	/// Connect to a peer by public key and address.
	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure>;

	/// Disconnect from a peer by public key.
	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure>;

	/// Open a channel to a peer.
	///
	/// Returns a channel id string that the implementation may use
	/// to correlate with subsequent close/query calls.
	async fn open_channel(
		&self, peer_id: PublicKey, addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure>;

	/// Cooperatively close a channel by its implementation-defined channel id.
	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure>;

	/// Force-close a channel by its implementation-defined channel id.
	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure>;

	/// Create a BOLT11 invoice for the given amount.
	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure>;

	/// Pay a BOLT11 invoice; returns an implementation-specific payment identifier on success.
	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure>;

	/// Send a keysend payment to a peer.
	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure>;

	/// Get an on-chain address that can be used to fund this node.
	async fn get_funding_address(&self) -> Result<String, TestFailure>;

	/// Returns the current blockchain height as seen by this node.
	async fn get_block_height(&self) -> Result<u64, TestFailure>;

	/// List all channels known to this node.
	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure>;

	/// Construct a `TestFailure::ExternalNodeError` for this node.
	fn make_error(&self, detail: impl Into<String>) -> TestFailure {
		TestFailure::ExternalNodeError { node: self.name().to_string(), detail: detail.into() }
	}

	/// Wait until this node has synced to at least `min_height`. Polls for up to 60 seconds.
	async fn wait_for_block_sync(&self, min_height: u64) -> Result<(), TestFailure> {
		for _ in 0..60 {
			if let Ok(h) = self.get_block_height().await {
				if h >= min_height {
					return Ok(());
				}
			}
			tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		}
		Err(self.make_error(format!("did not reach height {} after 60s", min_height)))
	}

	/// Splice additional funds into an existing channel.
	///
	/// Not all implementations support splicing. The default returns `NotSupported`.
	async fn splice_in(&self, _channel_id: &str, _amount_sat: u64) -> Result<(), TestFailure> {
		Err(TestFailure::NotSupported {
			node: self.name().to_string(),
			operation: "splice_in".to_string(),
		})
	}

	/// Splice funds out of an existing channel.
	///
	/// If `address` is provided, funds are sent to that on-chain address;
	/// otherwise the implementation decides the destination (e.g. own wallet).
	/// Not all implementations support splicing. The default returns `NotSupported`.
	async fn splice_out(
		&self, _channel_id: &str, _amount_sat: u64, _address: Option<&str>,
	) -> Result<(), TestFailure> {
		Err(TestFailure::NotSupported {
			node: self.name().to_string(),
			operation: "splice_out".to_string(),
		})
	}
}
