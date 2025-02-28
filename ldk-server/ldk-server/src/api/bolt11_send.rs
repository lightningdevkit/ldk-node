use crate::api::error::LdkServerError;
use crate::service::Context;
use bytes::Bytes;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_server_protos::api::{Bolt11SendRequest, Bolt11SendResponse};
use std::str::FromStr;

pub(crate) const BOLT11_SEND_PATH: &str = "Bolt11Send";

pub(crate) fn handle_bolt11_send_request(
	context: Context, request: Bolt11SendRequest,
) -> Result<Bolt11SendResponse, LdkServerError> {
	let invoice = Bolt11Invoice::from_str(&request.invoice.as_str())
		.map_err(|_| ldk_node::NodeError::InvalidInvoice)?;

	let payment_id = match request.amount_msat {
		None => context.node.bolt11_payment().send(&invoice, None),
		Some(amount_msat) => {
			context.node.bolt11_payment().send_using_amount(&invoice, amount_msat, None)
		},
	}?;

	let response = Bolt11SendResponse { payment_id: Bytes::from(payment_id.0.to_vec()) };
	Ok(response)
}
