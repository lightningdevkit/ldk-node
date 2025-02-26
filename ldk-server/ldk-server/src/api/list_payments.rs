use crate::api::error::LdkServerError;
use crate::service::Context;
use crate::util::proto_adapter::payment_to_proto;
use ldk_server_protos::api::{ListPaymentsRequest, ListPaymentsResponse};

pub(crate) const LIST_PAYMENTS_PATH: &str = "ListPayments";

pub(crate) fn handle_list_payments_request(
	context: Context, _request: ListPaymentsRequest,
) -> Result<ListPaymentsResponse, LdkServerError> {
	let payments = context.node.list_payments().into_iter().map(|p| payment_to_proto(p)).collect();

	let response = ListPaymentsResponse { payments };
	Ok(response)
}
