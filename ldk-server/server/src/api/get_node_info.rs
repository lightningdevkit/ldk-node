use ldk_node::Node;
use protos::{BestBlock, GetNodeInfoRequest, GetNodeInfoResponse};
use std::sync::Arc;

pub(crate) const GET_NODE_INFO: &str = "GetNodeInfo";

pub(crate) fn handle_get_node_info_request(
	node: Arc<Node>, _request: GetNodeInfoRequest,
) -> Result<GetNodeInfoResponse, ldk_node::NodeError> {
	let node_status = node.status();

	let best_block = BestBlock {
		block_hash: node_status.current_best_block.block_hash.to_string(),
		height: node_status.current_best_block.height,
	};

	let response = GetNodeInfoResponse {
		node_id: node.node_id().to_string(),
		current_best_block: Some(best_block),
		latest_wallet_sync_timestamp: node_status.latest_wallet_sync_timestamp,
		latest_onchain_wallet_sync_timestamp: node_status.latest_onchain_wallet_sync_timestamp,
		latest_fee_rate_cache_update_timestamp: node_status.latest_fee_rate_cache_update_timestamp,
		latest_rgs_snapshot_timestamp: node_status.latest_rgs_snapshot_timestamp,
		latest_node_announcement_broadcast_timestamp: node_status
			.latest_node_announcement_broadcast_timestamp,
	};
	Ok(response)
}
