// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use ldk_server_protos::api::{GetBalancesRequest, GetBalancesResponse};

use crate::api::error::LdkServerError;
use crate::service::Context;
use crate::util::proto_adapter::{lightning_balance_to_proto, pending_sweep_balance_to_proto};

pub(crate) fn handle_get_balances_request(
	context: Context, _request: GetBalancesRequest,
) -> Result<GetBalancesResponse, LdkServerError> {
	let balance_details = context.node.list_balances();

	let response = GetBalancesResponse {
		total_onchain_balance_sats: balance_details.total_onchain_balance_sats,
		spendable_onchain_balance_sats: balance_details.spendable_onchain_balance_sats,
		total_anchor_channels_reserve_sats: balance_details.total_anchor_channels_reserve_sats,
		total_lightning_balance_sats: balance_details.total_lightning_balance_sats,
		lightning_balances: balance_details
			.lightning_balances
			.into_iter()
			.map(lightning_balance_to_proto)
			.collect(),
		pending_balances_from_channel_closures: balance_details
			.pending_balances_from_channel_closures
			.into_iter()
			.map(pending_sweep_balance_to_proto)
			.collect(),
	};
	Ok(response)
}
