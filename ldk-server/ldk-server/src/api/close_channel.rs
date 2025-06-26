use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::UserChannelId;
use ldk_server_protos::api::{CloseChannelRequest, CloseChannelResponse};
use std::str::FromStr;

pub(crate) const CLOSE_CHANNEL_PATH: &str = "CloseChannel";

pub(crate) fn handle_close_channel_request(
	context: Context, request: CloseChannelRequest,
) -> Result<CloseChannelResponse, LdkServerError> {
	let user_channel_id =
		UserChannelId((&request.user_channel_id).parse::<u128>().map_err(|_| {
			LdkServerError::new(InvalidRequestError, "Invalid UserChannelId.".to_string())
		})?);
	let counterparty_node_id = PublicKey::from_str(&request.counterparty_node_id).map_err(|e| {
		LdkServerError::new(
			InvalidRequestError,
			format!("Invalid counterparty node ID, error: {}", e),
		)
	})?;
	context.node.close_channel(&user_channel_id, counterparty_node_id)?;

	let response = CloseChannelResponse {};
	Ok(response)
}
