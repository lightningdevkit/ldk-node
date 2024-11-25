use ldk_node::Node;
use ldk_server_protos::api::{Bolt12ReceiveRequest, Bolt12ReceiveResponse};
use std::sync::Arc;

pub(crate) const BOLT12_RECEIVE_PATH: &str = "Bolt12Receive";

pub(crate) fn handle_bolt12_receive_request(
	node: Arc<Node>, request: Bolt12ReceiveRequest,
) -> Result<Bolt12ReceiveResponse, ldk_node::NodeError> {
	let offer = match request.amount_msat {
		Some(amount_msat) => node.bolt12_payment().receive(
			amount_msat,
			&request.description,
			request.expiry_secs,
			request.quantity,
		)?,
		None => node
			.bolt12_payment()
			.receive_variable_amount(&request.description, request.expiry_secs)?,
	};

	let response = Bolt12ReceiveResponse { offer: offer.to_string() };
	Ok(response)
}
