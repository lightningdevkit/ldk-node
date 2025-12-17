// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use crate::api::build_channel_config_from_proto;
use crate::api::error::LdkServerError;
use crate::service::Context;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::config::ChannelConfig;
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

	let channel_config = request
		.channel_config
		.map(|proto_config| build_channel_config_from_proto(ChannelConfig::default(), proto_config))
		.transpose()?;

	let user_channel_id = if request.announce_channel {
		context.node.open_announced_channel(
			node_id,
			address,
			request.channel_amount_sats,
			request.push_to_counterparty_msat,
			channel_config,
		)?
	} else {
		context.node.open_channel(
			node_id,
			address,
			request.channel_amount_sats,
			request.push_to_counterparty_msat,
			channel_config,
		)?
	};

	let response = OpenChannelResponse { user_channel_id: user_channel_id.0.to_string() };
	Ok(response)
}
