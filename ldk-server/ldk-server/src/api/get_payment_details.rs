use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::service::Context;
use crate::util::proto_adapter::payment_to_proto;
use hex::FromHex;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_server_protos::api::{GetPaymentDetailsRequest, GetPaymentDetailsResponse};

pub(crate) const GET_PAYMENT_DETAILS_PATH: &str = "GetPaymentDetails";

pub(crate) fn handle_get_payment_details_request(
	context: Context, request: GetPaymentDetailsRequest,
) -> Result<GetPaymentDetailsResponse, LdkServerError> {
	let payment_id_bytes =
		<[u8; PaymentId::LENGTH]>::from_hex(&request.payment_id).map_err(|_| {
			LdkServerError::new(
				InvalidRequestError,
				format!("Invalid payment_id, must be a {}-byte hex-string.", PaymentId::LENGTH),
			)
		})?;

	let payment_details = context.node.payment(&PaymentId(payment_id_bytes));

	let response = GetPaymentDetailsResponse {
		payment: payment_details.map(|payment| payment_to_proto(payment)),
	};

	Ok(response)
}
