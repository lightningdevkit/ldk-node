use ldk_node::bitcoin::Address;
use ldk_node::Node;
use ldk_server_protos::api::{OnchainSendRequest, OnchainSendResponse};
use std::str::FromStr;
use std::sync::Arc;

pub(crate) const ONCHAIN_SEND_PATH: &str = "OnchainSend";

pub(crate) fn handle_onchain_send_request(
	node: Arc<Node>, request: OnchainSendRequest,
) -> Result<OnchainSendResponse, ldk_node::NodeError> {
	let address = Address::from_str(&request.address)
		.map_err(|_| ldk_node::NodeError::InvalidAddress)?
		.require_network(node.config().network)
		.map_err(|_| ldk_node::NodeError::InvalidAddress)?;
	let txid = match (request.amount_sats, request.send_all) {
		(Some(amount_sats), None) => {
			node.onchain_payment().send_to_address(&address, amount_sats)?
		},
		(None, Some(true)) => node.onchain_payment().send_all_to_address(&address)?,
		_ => return Err(ldk_node::NodeError::InvalidAmount),
	};
	let response = OnchainSendResponse { txid: txid.to_string() };
	Ok(response)
}
