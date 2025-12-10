use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::{InvalidRequestError, LightningError};
use crate::service::Context;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::config::{ChannelConfig, MaxDustHTLCExposure};
use ldk_node::UserChannelId;
use ldk_server_protos::api::{UpdateChannelConfigRequest, UpdateChannelConfigResponse};
use ldk_server_protos::types::channel_config::MaxDustHtlcExposure;
use std::str::FromStr;

pub(crate) fn handle_update_channel_config_request(
	context: Context, request: UpdateChannelConfigRequest,
) -> Result<UpdateChannelConfigResponse, LdkServerError> {
	let user_channel_id: u128 = request
		.user_channel_id
		.parse::<u128>()
		.map_err(|_| LdkServerError::new(InvalidRequestError, "Invalid UserChannelId."))?;

	//FIXME: Use ldk/ldk-node's partial config update api.
	let current_config = context
		.node
		.list_channels()
		.into_iter()
		.find(|c| c.user_channel_id.0 == user_channel_id)
		.ok_or_else(|| {
			LdkServerError::new(InvalidRequestError, "Channel not found for given user_channel_id.")
		})?
		.config;

	let updated_channel_config = build_updated_channel_config(
		current_config,
		request.channel_config.ok_or_else(|| {
			LdkServerError::new(InvalidRequestError, "Channel config must be provided.")
		})?,
	)?;

	let counterparty_node_id = PublicKey::from_str(&request.counterparty_node_id).map_err(|e| {
		LdkServerError::new(
			InvalidRequestError,
			format!("Invalid counterparty node id, error {}", e),
		)
	})?;

	context
		.node
		.update_channel_config(
			&UserChannelId(user_channel_id),
			counterparty_node_id,
			updated_channel_config,
		)
		.map_err(|e| {
			LdkServerError::new(LightningError, format!("Failed to update channel config: {}", e))
		})?;

	Ok(UpdateChannelConfigResponse {})
}

fn build_updated_channel_config(
	current_config: ChannelConfig, proto_channel_config: ldk_server_protos::types::ChannelConfig,
) -> Result<ChannelConfig, LdkServerError> {
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

	let cltv_expiry_delta = match proto_channel_config.cltv_expiry_delta {
		Some(c) => Some(u16::try_from(c).map_err(|_| {
			LdkServerError::new(
				InvalidRequestError,
				format!("Invalid cltv_expiry_delta, must be between 0 and {}", u16::MAX),
			)
		})?),
		None => None,
	}
	.unwrap_or_else(|| current_config.cltv_expiry_delta);

	Ok(ChannelConfig {
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
	})
}
