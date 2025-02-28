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
	//TODO: Should this be string?
	let mut user_channel_id_bytes = [0u8; 16];
	user_channel_id_bytes.copy_from_slice(&request.user_channel_id);
	let user_channel_id = UserChannelId(u128::from_be_bytes(user_channel_id_bytes));
	let counterparty_node_id = PublicKey::from_str(&request.counterparty_node_id).map_err(|e| {
		LdkServerError::new(
			InvalidRequestError,
			format!("Invalid counterparty node ID, error: {}", e),
		)
	})?;

	match request.force_close {
		Some(true) => context.node.force_close_channel(
			&user_channel_id,
			counterparty_node_id,
			request.force_close_reason,
		)?,
		_ => context.node.close_channel(&user_channel_id, counterparty_node_id)?,
	};

	let response = CloseChannelResponse {};
	Ok(response)
}
