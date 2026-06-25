// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::str::FromStr;

use ldk_node::lightning::routing::router::RouteParametersConfig;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_server_protos::api::{Bolt11SendRequest, Bolt11SendResponse};

use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;

pub(crate) fn handle_bolt11_send_request(
	context: Context, request: Bolt11SendRequest,
) -> Result<Bolt11SendResponse, LdkServerError> {
	let invoice = Bolt11Invoice::from_str(request.invoice.as_str())
		.map_err(|_| ldk_node::NodeError::InvalidInvoice)?;

	let route_parameters = match request.route_parameters {
		Some(params) => {
			let max_path_count: u8 = params.max_path_count.try_into().map_err(|_| {
				LdkServerError::new(
					InvalidRequestError,
					format!("Invalid max_path_count, must be between 0 and {}", u8::MAX),
				)
			})?;
			let max_channel_saturation_power_of_half: u8 =
				params.max_channel_saturation_power_of_half.try_into().map_err(|_| {
					LdkServerError::new(
						InvalidRequestError,
						format!(
							"Invalid max_channel_saturation_power_of_half, must be between 0 and {}",
							u8::MAX
						),
					)
				})?;
			Some(RouteParametersConfig {
				max_total_routing_fee_msat: params.max_total_routing_fee_msat,
				max_total_cltv_expiry_delta: params.max_total_cltv_expiry_delta,
				max_path_count,
				max_channel_saturation_power_of_half,
			})
		},
		None => None,
	};

	let payment_id = match request.amount_msat {
		None => context.node.bolt11_payment().send(&invoice, route_parameters),
		Some(amount_msat) => {
			context.node.bolt11_payment().send_using_amount(&invoice, amount_msat, route_parameters)
		},
	}?;

	let response = Bolt11SendResponse { payment_id: payment_id.to_string() };
	Ok(response)
}
