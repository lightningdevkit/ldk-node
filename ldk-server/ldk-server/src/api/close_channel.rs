use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::UserChannelId;
use ldk_server_protos::api::{
	CloseChannelRequest, CloseChannelResponse, ForceCloseChannelRequest, ForceCloseChannelResponse,
};
use std::str::FromStr;

pub(crate) fn handle_close_channel_request(
	context: Context, request: CloseChannelRequest,
) -> Result<CloseChannelResponse, LdkServerError> {
	let user_channel_id = parse_user_channel_id(&request.user_channel_id)?;
	let counterparty_node_id = parse_counterparty_node_id(&request.counterparty_node_id)?;

	context.node.close_channel(&user_channel_id, counterparty_node_id)?;

	Ok(CloseChannelResponse {})
}

pub(crate) fn handle_force_close_channel_request(
	context: Context, request: ForceCloseChannelRequest,
) -> Result<ForceCloseChannelResponse, LdkServerError> {
	let user_channel_id = parse_user_channel_id(&request.user_channel_id)?;
	let counterparty_node_id = parse_counterparty_node_id(&request.counterparty_node_id)?;

	context.node.force_close_channel(
		&user_channel_id,
		counterparty_node_id,
		request.force_close_reason,
	)?;

	Ok(ForceCloseChannelResponse {})
}

fn parse_user_channel_id(id: &str) -> Result<UserChannelId, LdkServerError> {
	let parsed = id.parse::<u128>().map_err(|_| {
		LdkServerError::new(InvalidRequestError, "Invalid UserChannelId.".to_string())
	})?;
	Ok(UserChannelId(parsed))
}

fn parse_counterparty_node_id(id: &str) -> Result<PublicKey, LdkServerError> {
	PublicKey::from_str(id).map_err(|e| {
		LdkServerError::new(
			InvalidRequestError,
			format!("Invalid counterparty node ID, error: {}", e),
		)
	})
}
