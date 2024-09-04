use ldk_node::Node;
use protos::{OnchainReceiveRequest, OnchainReceiveResponse};
use std::sync::Arc;

pub(crate) const ONCHAIN_RECEIVE_PATH: &str = "OnchainReceive";
pub(crate) fn handle_onchain_receive_request(
	node: Arc<Node>, _request: OnchainReceiveRequest,
) -> Result<OnchainReceiveResponse, ldk_node::NodeError> {
	let response =
		OnchainReceiveResponse { address: node.onchain_payment().new_address()?.to_string() };
	Ok(response)
}
