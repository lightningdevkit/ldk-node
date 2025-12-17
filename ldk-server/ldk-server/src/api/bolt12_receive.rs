// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use crate::api::error::LdkServerError;
use crate::service::Context;
use ldk_server_protos::api::{Bolt12ReceiveRequest, Bolt12ReceiveResponse};

pub(crate) fn handle_bolt12_receive_request(
	context: Context, request: Bolt12ReceiveRequest,
) -> Result<Bolt12ReceiveResponse, LdkServerError> {
	let offer = match request.amount_msat {
		Some(amount_msat) => context.node.bolt12_payment().receive(
			amount_msat,
			&request.description,
			request.expiry_secs,
			request.quantity,
		)?,
		None => context
			.node
			.bolt12_payment()
			.receive_variable_amount(&request.description, request.expiry_secs)?,
	};

	let response = Bolt12ReceiveResponse { offer: offer.to_string() };
	Ok(response)
}
