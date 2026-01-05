// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;
use ldk_node::bitcoin::{Address, FeeRate};
use ldk_server_protos::api::{OnchainSendRequest, OnchainSendResponse};
use std::str::FromStr;

pub(crate) fn handle_onchain_send_request(
	context: Context, request: OnchainSendRequest,
) -> Result<OnchainSendResponse, LdkServerError> {
	let address = Address::from_str(&request.address)
		.map_err(|_| ldk_node::NodeError::InvalidAddress)?
		.require_network(context.node.config().network)
		.map_err(|_| {
			LdkServerError::new(
				InvalidRequestError,
				"Address is not valid for the configured network.".to_string(),
			)
		})?;

	let fee_rate = request.fee_rate_sat_per_vb.and_then(FeeRate::from_sat_per_vb);
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
