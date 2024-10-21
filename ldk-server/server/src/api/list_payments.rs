use crate::util::proto_adapter::payment_to_proto;
use ldk_node::Node;
use protos::{ListPaymentsRequest, ListPaymentsResponse};
use std::sync::Arc;

pub(crate) const LIST_PAYMENTS_PATH: &str = "ListPayments";

pub(crate) fn handle_list_payments_request(
	node: Arc<Node>, _request: ListPaymentsRequest,
) -> Result<ListPaymentsResponse, ldk_node::NodeError> {
	let payments = node.list_payments().into_iter().map(|p| payment_to_proto(p)).collect();

	let response = ListPaymentsResponse { payments };
	Ok(response)
}
