// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::str::FromStr;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::UserChannelId;
use ldk_server_protos::api::{UpdateChannelConfigRequest, UpdateChannelConfigResponse};

use crate::api::build_channel_config_from_proto;
use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::{InvalidRequestError, LightningError};
use crate::service::Context;

pub(crate) fn handle_update_channel_config_request(
	context: Context, request: UpdateChannelConfigRequest,
) -> Result<UpdateChannelConfigResponse, LdkServerError> {
	let user_channel_id: u128 = request
		.user_channel_id
		.parse::<u128>()
		.map_err(|_| LdkServerError::new(InvalidRequestError, "Invalid UserChannelId."))?;

	// FIXME: Use ldk/ldk-node's partial config update api.
	let current_config = context
		.node
		.list_channels()
		.into_iter()
		.find(|c| c.user_channel_id.0 == user_channel_id)
		.ok_or_else(|| {
			LdkServerError::new(InvalidRequestError, "Channel not found for given user_channel_id.")
		})?
		.config;

	let updated_channel_config = build_channel_config_from_proto(
		current_config,
		request.channel_config.ok_or_else(|| {
			LdkServerError::new(InvalidRequestError, "Channel config must be provided.")
		})?,
	)?;

	let counterparty_node_id = PublicKey::from_str(&request.counterparty_node_id).map_err(|e| {
		LdkServerError::new(
			InvalidRequestError,
			format!("Invalid counterparty node id, error {}", e),
		)
	})?;

	context
		.node
		.update_channel_config(
			&UserChannelId(user_channel_id),
			counterparty_node_id,
			updated_channel_config,
		)
		.map_err(|e| {
			LdkServerError::new(LightningError, format!("Failed to update channel config: {}", e))
		})?;

	Ok(UpdateChannelConfigResponse {})
}
