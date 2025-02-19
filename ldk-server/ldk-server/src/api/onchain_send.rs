use crate::service::Context;
use ldk_node::bitcoin::{Address, FeeRate};
use ldk_server_protos::api::{OnchainSendRequest, OnchainSendResponse};
use std::str::FromStr;

pub(crate) const ONCHAIN_SEND_PATH: &str = "OnchainSend";

pub(crate) fn handle_onchain_send_request(
	context: Context, request: OnchainSendRequest,
) -> Result<OnchainSendResponse, ldk_node::NodeError> {
	let address = Address::from_str(&request.address)
		.map_err(|_| ldk_node::NodeError::InvalidAddress)?
		.require_network(context.node.config().network)
		.map_err(|_| ldk_node::NodeError::InvalidAddress)?;

	let fee_rate = request.fee_rate_sat_per_vb.map(FeeRate::from_sat_per_vb).flatten();
	let txid = match (request.amount_sats, request.send_all) {
		(Some(amount_sats), None) => {
			context.node.onchain_payment().send_to_address(&address, amount_sats, fee_rate)?
		},
		// Retain existing api behaviour to not retain reserves on `send_all_to_address`.
		(None, Some(true)) => {
			context.node.onchain_payment().send_all_to_address(&address, false, fee_rate)?
		},
		_ => return Err(ldk_node::NodeError::InvalidAmount),
	};
	let response = OnchainSendResponse { txid: txid.to_string() };
	Ok(response)
}
