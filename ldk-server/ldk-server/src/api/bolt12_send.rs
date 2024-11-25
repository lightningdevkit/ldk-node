use bytes::Bytes;
use ldk_node::lightning::offers::offer::Offer;
use ldk_node::Node;
use ldk_server_protos::api::{Bolt12SendRequest, Bolt12SendResponse};
use std::str::FromStr;
use std::sync::Arc;

pub(crate) const BOLT12_SEND_PATH: &str = "Bolt12Send";

pub(crate) fn handle_bolt12_send_request(
	node: Arc<Node>, request: Bolt12SendRequest,
) -> Result<Bolt12SendResponse, ldk_node::NodeError> {
	let offer =
		Offer::from_str(&request.offer.as_str()).map_err(|_| ldk_node::NodeError::InvalidOffer)?;

	let payment_id = match request.amount_msat {
		None => node.bolt12_payment().send(&offer, request.quantity, request.payer_note),
		Some(amount_msat) => node.bolt12_payment().send_using_amount(
			&offer,
			amount_msat,
			request.quantity,
			request.payer_note,
		),
	}?;

	let response = Bolt12SendResponse { payment_id: Bytes::from(payment_id.0.to_vec()) };
	Ok(response)
}
