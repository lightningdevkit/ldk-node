use crate::service::Context;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::config::{ChannelConfig, MaxDustHTLCExposure};
use ldk_node::UserChannelId;
use ldk_server_protos::api::{UpdateChannelConfigRequest, UpdateChannelConfigResponse};
use ldk_server_protos::types::channel_config::MaxDustHtlcExposure;
use std::str::FromStr;

pub(crate) const UPDATE_CHANNEL_CONFIG_PATH: &str = "UpdateChannelConfig";

pub(crate) fn handle_update_channel_config_request(
	context: Context, request: UpdateChannelConfigRequest,
) -> Result<UpdateChannelConfigResponse, ldk_node::NodeError> {
	let user_channel_id: u128 =
		request.user_channel_id.parse().map_err(|_| ldk_node::NodeError::InvalidChannelId)?;

	//FIXME: Use ldk/ldk-node's partial config update api.
	let current_config = context
		.node
		.list_channels()
		.into_iter()
		.find(|c| c.user_channel_id.0 == user_channel_id)
		.ok_or_else(|| ldk_node::NodeError::InvalidChannelId)?
		.config;

	let updated_channel_config =
		build_updated_channel_config(current_config, request.channel_config.unwrap());

	let counterparty_node_id = PublicKey::from_str(&request.counterparty_node_id)
		.map_err(|_| ldk_node::NodeError::InvalidPublicKey)?;

	context
		.node
		.update_channel_config(
			&UserChannelId(user_channel_id),
			counterparty_node_id,
			updated_channel_config,
		)
		.map_err(ldk_node::NodeError::from)?;

	Ok(UpdateChannelConfigResponse {})
}

fn build_updated_channel_config(
	current_config: ChannelConfig, proto_channel_config: ldk_server_protos::types::ChannelConfig,
) -> ChannelConfig {
	let max_dust_htlc_exposure = proto_channel_config
		.max_dust_htlc_exposure
		.map(|max_dust_htlc_exposure| match max_dust_htlc_exposure {
			MaxDustHtlcExposure::FixedLimitMsat(limit_msat) => {
				MaxDustHTLCExposure::FixedLimit { limit_msat }
			},
			MaxDustHtlcExposure::FeeRateMultiplier(multiplier) => {
				MaxDustHTLCExposure::FeeRateMultiplier { multiplier }
			},
		})
		.unwrap_or(current_config.max_dust_htlc_exposure);

	let cltv_expiry_delta = proto_channel_config
		.cltv_expiry_delta
		.map(|c| u16::try_from(c).unwrap())
		.unwrap_or(current_config.cltv_expiry_delta);

	ChannelConfig {
		forwarding_fee_proportional_millionths: proto_channel_config
			.forwarding_fee_proportional_millionths
			.unwrap_or(current_config.forwarding_fee_proportional_millionths),
		forwarding_fee_base_msat: proto_channel_config
			.forwarding_fee_base_msat
			.unwrap_or(current_config.forwarding_fee_base_msat),
		cltv_expiry_delta,
		max_dust_htlc_exposure,
		force_close_avoidance_max_fee_satoshis: proto_channel_config
			.force_close_avoidance_max_fee_satoshis
			.unwrap_or(current_config.force_close_avoidance_max_fee_satoshis),
		accept_underpaying_htlcs: proto_channel_config
			.accept_underpaying_htlcs
			.unwrap_or(current_config.accept_underpaying_htlcs),
	}
}
