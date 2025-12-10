use crate::api::error::LdkServerError;
use crate::service::Context;
use ldk_node::lightning::offers::offer::Offer;
use ldk_server_protos::api::{Bolt12SendRequest, Bolt12SendResponse};
use std::str::FromStr;

pub(crate) fn handle_bolt12_send_request(
	context: Context, request: Bolt12SendRequest,
) -> Result<Bolt12SendResponse, LdkServerError> {
	let offer =
		Offer::from_str(&request.offer.as_str()).map_err(|_| ldk_node::NodeError::InvalidOffer)?;

	let payment_id = match request.amount_msat {
		None => {
			context.node.bolt12_payment().send(&offer, request.quantity, request.payer_note, None)
		},
		Some(amount_msat) => context.node.bolt12_payment().send_using_amount(
			&offer,
			amount_msat,
			request.quantity,
			request.payer_note,
			None,
		),
	}?;

	let response = Bolt12SendResponse { payment_id: payment_id.to_string() };
	Ok(response)
}
