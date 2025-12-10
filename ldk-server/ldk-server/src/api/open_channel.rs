use crate::api::error::LdkServerError;
use crate::service::Context;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_server_protos::api::{OpenChannelRequest, OpenChannelResponse};
use std::str::FromStr;

pub(crate) fn handle_open_channel(
	context: Context, request: OpenChannelRequest,
) -> Result<OpenChannelResponse, LdkServerError> {
	let node_id = PublicKey::from_str(&request.node_pubkey)
		.map_err(|_| ldk_node::NodeError::InvalidPublicKey)?;
	let address = SocketAddress::from_str(&request.address)
		.map_err(|_| ldk_node::NodeError::InvalidSocketAddress)?;

	let user_channel_id = if request.announce_channel {
		context.node.open_announced_channel(
			node_id,
			address,
			request.channel_amount_sats,
			request.push_to_counterparty_msat,
			// TODO: Allow setting ChannelConfig in open-channel.
			None,
		)?
	} else {
		context.node.open_channel(
			node_id,
			address,
			request.channel_amount_sats,
			request.push_to_counterparty_msat,
			None,
		)?
	};

	let response = OpenChannelResponse { user_channel_id: user_channel_id.0.to_string() };
	Ok(response)
}
