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
	FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
	FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::service::Context;
use bytes::Bytes;
use ldk_server_protos::api::{ListForwardedPaymentsRequest, ListForwardedPaymentsResponse};
use ldk_server_protos::types::{ForwardedPayment, PageToken};
use prost::Message;

pub(crate) fn handle_list_forwarded_payments_request(
	context: Context, request: ListForwardedPaymentsRequest,
) -> Result<ListForwardedPaymentsResponse, LdkServerError> {
	let page_token = request.page_token.map(|p| (p.token, p.index));
	let list_response = context
		.paginated_kv_store
		.list(
			FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
			FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
			page_token,
		)
		.map_err(|e| {
			LdkServerError::new(
				InternalServerError,
				format!("Failed to list forwarded payments: {}", e),
			)
		})?;

	let mut forwarded_payments: Vec<ForwardedPayment> =
		Vec::with_capacity(list_response.keys.len());
	for key in list_response.keys {
		let forwarded_payment_bytes = context
			.paginated_kv_store
			.read(
				FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
				FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
			)
			.map_err(|e| {
				LdkServerError::new(
					InternalServerError,
					format!("Failed to read forwarded payment data: {}", e),
				)
			})?;
		let forwarded_payment = ForwardedPayment::decode(Bytes::from(forwarded_payment_bytes))
			.map_err(|e| {
				LdkServerError::new(
					InternalServerError,
					format!("Failed to decode forwarded payment: {}", e),
				)
			})?;
		forwarded_payments.push(forwarded_payment);
	}
	let response = ListForwardedPaymentsResponse {
		forwarded_payments,
		next_page_token: list_response
			.next_page_token
			.map(|(token, index)| PageToken { token, index }),
	};
	Ok(response)
}
