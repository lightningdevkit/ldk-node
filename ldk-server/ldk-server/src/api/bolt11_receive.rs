// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use ldk_server_protos::api::{Bolt11ReceiveRequest, Bolt11ReceiveResponse};

use crate::api::error::LdkServerError;
use crate::service::Context;
use crate::util::proto_adapter::proto_to_bolt11_description;

pub(crate) fn handle_bolt11_receive_request(
	context: Context, request: Bolt11ReceiveRequest,
) -> Result<Bolt11ReceiveResponse, LdkServerError> {
	let description = proto_to_bolt11_description(request.description)?;
	let invoice = match request.amount_msat {
		Some(amount_msat) => {
			context.node.bolt11_payment().receive(amount_msat, &description, request.expiry_secs)?
		},
		None => context
			.node
			.bolt11_payment()
			.receive_variable_amount(&description, request.expiry_secs)?,
	};

	let response = Bolt11ReceiveResponse { invoice: invoice.to_string() };
	Ok(response)
}
