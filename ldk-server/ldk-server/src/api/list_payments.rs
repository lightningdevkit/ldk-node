// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InternalServerError;
use crate::io::persist::{
	PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::service::Context;
use bytes::Bytes;
use ldk_server_protos::api::{ListPaymentsRequest, ListPaymentsResponse};
use ldk_server_protos::types::{PageToken, Payment};
use prost::Message;

pub(crate) fn handle_list_payments_request(
	context: Context, request: ListPaymentsRequest,
) -> Result<ListPaymentsResponse, LdkServerError> {
	let page_token = request.page_token.map(|p| (p.token, p.index));
	let list_response = context
		.paginated_kv_store
		.list(
			PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
			page_token,
		)
		.map_err(|e| {
			LdkServerError::new(InternalServerError, format!("Failed to list payments: {}", e))
		})?;

	let mut payments: Vec<Payment> = Vec::with_capacity(list_response.keys.len());
	for key in list_response.keys {
		let payment_bytes = context
			.paginated_kv_store
			.read(
				PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
			)
			.map_err(|e| {
				LdkServerError::new(
					InternalServerError,
					format!("Failed to read payment data: {}", e),
				)
			})?;
		let payment = Payment::decode(Bytes::from(payment_bytes)).map_err(|e| {
			LdkServerError::new(InternalServerError, format!("Failed to decode payment: {}", e))
		})?;
		payments.push(payment);
	}
	let response = ListPaymentsResponse {
		payments,
		next_page_token: list_response
			.next_page_token
			.map(|(token, index)| PageToken { token, index }),
	};
	Ok(response)
}
