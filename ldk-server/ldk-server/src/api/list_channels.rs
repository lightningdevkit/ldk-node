use crate::util::proto_adapter::channel_to_proto;
use ldk_node::Node;
use ldk_server_protos::api::{ListChannelsRequest, ListChannelsResponse};
use std::sync::Arc;

pub(crate) const LIST_CHANNELS_PATH: &str = "ListChannels";

pub(crate) fn handle_list_channels_request(
	node: Arc<Node>, _request: ListChannelsRequest,
) -> Result<ListChannelsResponse, ldk_node::NodeError> {
	let channels = node.list_channels().into_iter().map(|c| channel_to_proto(c)).collect();

	let response = ListChannelsResponse { channels };
	Ok(response)
}
