use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;
use ldk_node::lightning::offers::offer::Offer;
use ldk_node::lightning::routing::router::RouteParametersConfig;
use ldk_server_protos::api::{Bolt12SendRequest, Bolt12SendResponse};
use std::str::FromStr;

pub(crate) fn handle_bolt12_send_request(
	context: Context, request: Bolt12SendRequest,
) -> Result<Bolt12SendResponse, LdkServerError> {
	let offer =
		Offer::from_str(&request.offer.as_str()).map_err(|_| ldk_node::NodeError::InvalidOffer)?;

	let route_parameters = match request.route_parameters {
		Some(params) => {
			let max_path_count = params.max_path_count.try_into().map_err(|_| {
				LdkServerError::new(
					InvalidRequestError,
					format!("Invalid max_path_count, must be between 0 and {}", u8::MAX),
				)
			})?;
			let max_channel_saturation_power_of_half =
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
		None => context.node.bolt12_payment().send(
			&offer,
			request.quantity,
			request.payer_note,
			route_parameters,
		),
		Some(amount_msat) => context.node.bolt12_payment().send_using_amount(
			&offer,
			amount_msat,
			request.quantity,
			request.payer_note,
			route_parameters,
		),
	}?;

	let response = Bolt12SendResponse { payment_id: payment_id.to_string() };
	Ok(response)
}
