use crate::api::error::LdkServerError;
use crate::service::Context;
use ldk_server_protos::api::{OnchainReceiveRequest, OnchainReceiveResponse};

pub(crate) const ONCHAIN_RECEIVE_PATH: &str = "OnchainReceive";
pub(crate) fn handle_onchain_receive_request(
	context: Context, _request: OnchainReceiveRequest,
) -> Result<OnchainReceiveResponse, LdkServerError> {
	let response = OnchainReceiveResponse {
		address: context.node.onchain_payment().new_address()?.to_string(),
	};
	Ok(response)
}
