use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;
use ldk_node::bitcoin::{Address, FeeRate};
use ldk_server_protos::api::{OnchainSendRequest, OnchainSendResponse};
use std::str::FromStr;

pub(crate) const ONCHAIN_SEND_PATH: &str = "OnchainSend";

pub(crate) fn handle_onchain_send_request(
	context: Context, request: OnchainSendRequest,
) -> Result<OnchainSendResponse, LdkServerError> {
	let address = Address::from_str(&request.address)
		.map_err(|_| ldk_node::NodeError::InvalidAddress)?
		.require_network(context.node.config().network)
		.map_err(|_| {
			LdkServerError::new(
				InvalidRequestError,
				"Address is not valid for LdkServer's configured network.".to_string(),
			)
		})?;

	let fee_rate = request.fee_rate_sat_per_vb.map(FeeRate::from_sat_per_vb).flatten();
	let txid = match (request.amount_sats, request.send_all) {
		(Some(amount_sats), None) => {
			context.node.onchain_payment().send_to_address(&address, amount_sats, fee_rate)?
		},
		// Retain existing api behaviour to not retain reserves on `send_all_to_address`.
		(None, Some(true)) => {
			context.node.onchain_payment().send_all_to_address(&address, false, fee_rate)?
		},
		_ => {
			return Err(LdkServerError::new(
				InvalidRequestError,
				"Must specify either `send_all` or `amount_sats`, but not both or neither",
			))
		},
	};
	let response = OnchainSendResponse { txid: txid.to_string() };
	Ok(response)
}
