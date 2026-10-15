// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use ldk_server_protos::api::{GetNodeInfoRequest, GetNodeInfoResponse};
use ldk_server_protos::types::BestBlock;

use crate::api::error::LdkServerError;
use crate::service::Context;

pub(crate) fn handle_get_node_info_request(
	context: Context, _request: GetNodeInfoRequest,
) -> Result<GetNodeInfoResponse, LdkServerError> {
	let node_status = context.node.status();

	let best_block = BestBlock {
		block_hash: node_status.current_best_block.block_hash.to_string(),
		height: node_status.current_best_block.height,
	};

	let response = GetNodeInfoResponse {
		node_id: context.node.node_id().to_string(),
		current_best_block: Some(best_block),
		latest_lightning_wallet_sync_timestamp: node_status.latest_lightning_wallet_sync_timestamp,
		latest_onchain_wallet_sync_timestamp: node_status.latest_onchain_wallet_sync_timestamp,
		latest_fee_rate_cache_update_timestamp: node_status.latest_fee_rate_cache_update_timestamp,
		latest_rgs_snapshot_timestamp: node_status.latest_rgs_snapshot_timestamp,
		latest_node_announcement_broadcast_timestamp: node_status
			.latest_node_announcement_broadcast_timestamp,
	};
	Ok(response)
}
