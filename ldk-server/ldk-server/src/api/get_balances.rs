use crate::util::proto_adapter::{lightning_balance_to_proto, pending_sweep_balance_to_proto};
use ldk_node::Node;
use ldk_server_protos::api::{GetBalancesRequest, GetBalancesResponse};
use std::sync::Arc;

pub(crate) const GET_BALANCES: &str = "GetBalances";

pub(crate) fn handle_get_balances_request(
	node: Arc<Node>, _request: GetBalancesRequest,
) -> Result<GetBalancesResponse, ldk_node::NodeError> {
	let balance_details = node.list_balances();

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
