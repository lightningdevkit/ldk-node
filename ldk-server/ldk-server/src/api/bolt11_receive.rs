use crate::service::Context;
use ldk_server_protos::api::{Bolt11ReceiveRequest, Bolt11ReceiveResponse};

pub(crate) const BOLT11_RECEIVE_PATH: &str = "Bolt11Receive";

pub(crate) fn handle_bolt11_receive_request(
	context: Context, request: Bolt11ReceiveRequest,
) -> Result<Bolt11ReceiveResponse, ldk_node::NodeError> {
	let invoice = match request.amount_msat {
		Some(amount_msat) => context.node.bolt11_payment().receive(
			amount_msat,
			&request.description,
			request.expiry_secs,
		)?,
		None => context
			.node
			.bolt11_payment()
			.receive_variable_amount(&request.description, request.expiry_secs)?,
	};

	let response = Bolt11ReceiveResponse { invoice: invoice.to_string() };
	Ok(response)
}
