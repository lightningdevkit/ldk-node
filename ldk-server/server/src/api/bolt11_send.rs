use bytes::Bytes;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::Node;
use protos::{Bolt11SendRequest, Bolt11SendResponse};
use std::str::FromStr;
use std::sync::Arc;

pub(crate) const BOLT11_SEND_PATH: &str = "Bolt11Send";

pub(crate) fn handle_bolt11_send_request(
	node: Arc<Node>, request: Bolt11SendRequest,
) -> Result<Bolt11SendResponse, ldk_node::NodeError> {
	let invoice = Bolt11Invoice::from_str(&request.invoice.as_str())
		.map_err(|_| ldk_node::NodeError::InvalidInvoice)?;

	let payment_id = match request.amount_msat {
		None => node.bolt11_payment().send(&invoice),
		Some(amount_msat) => node.bolt11_payment().send_using_amount(&invoice, amount_msat),
	}?;

	let response = Bolt11SendResponse { payment_id: Bytes::from(payment_id.0.to_vec()) };
	Ok(response)
}
