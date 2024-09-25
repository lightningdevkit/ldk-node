use bytes::Bytes;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::Node;
use protos::{OpenChannelRequest, OpenChannelResponse};
use std::str::FromStr;
use std::sync::Arc;

pub(crate) const OPEN_CHANNEL_PATH: &str = "OpenChannel";

pub(crate) fn handle_open_channel(
	node: Arc<Node>, request: OpenChannelRequest,
) -> Result<OpenChannelResponse, ldk_node::NodeError> {
	let node_id = PublicKey::from_str(&request.node_pubkey)
		.map_err(|_| ldk_node::NodeError::InvalidPublicKey)?;
	let address = SocketAddress::from_str(&request.address)
		.map_err(|_| ldk_node::NodeError::InvalidSocketAddress)?;
	let user_channel_id = node.connect_open_channel(
		node_id,
		address,
		request.channel_amount_sats,
		request.push_to_counterparty_msat,
		// TODO: Allow setting ChannelConfig in open-channel.
		None,
		request.announce_channel,
	)?;
	let response = OpenChannelResponse {
		user_channel_id: Bytes::from(user_channel_id.0.to_be_bytes().to_vec()),
	};
	Ok(response)
}
