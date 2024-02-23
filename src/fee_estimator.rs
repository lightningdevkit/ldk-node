use crate::logger::{log_error, log_trace, Logger};
use crate::{Config, Error};

use lightning::chain::chaininterface::{
	ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW,
};

use bdk::FeeRate;
use esplora_client::AsyncClient as EsploraClient;

use bitcoin::blockdata::weight::Weight;
use bitcoin::Network;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

pub(crate) struct OnchainFeeEstimator<L: Deref>
where
	L::Target: Logger,
{
	fee_rate_cache: RwLock<HashMap<ConfirmationTarget, FeeRate>>,
	esplora_client: EsploraClient,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref> OnchainFeeEstimator<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(esplora_client: EsploraClient, config: Arc<Config>, logger: L) -> Self {
		let fee_rate_cache = RwLock::new(HashMap::new());
		Self { fee_rate_cache, esplora_client, config, logger }
	}

	pub(crate) async fn update_fee_estimates(&self) -> Result<(), Error> {
		let confirmation_targets = vec![
			ConfirmationTarget::OnChainSweep,
			ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
			ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
			ConfirmationTarget::AnchorChannelFee,
			ConfirmationTarget::NonAnchorChannelFee,
			ConfirmationTarget::ChannelCloseMinimum,
		];
		for target in confirmation_targets {
			let num_blocks = match target {
				ConfirmationTarget::OnChainSweep => 6,
				ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => 1008,
				ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => 144,
				ConfirmationTarget::AnchorChannelFee => 1008,
				ConfirmationTarget::NonAnchorChannelFee => 12,
				ConfirmationTarget::ChannelCloseMinimum => 144,
			};

			let estimates = self.esplora_client.get_fee_estimates().await.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to retrieve fee rate estimates for {:?}: {}",
					target,
					e
				);
				Error::FeerateEstimationUpdateFailed
			})?;

			if estimates.is_empty() && self.config.network == Network::Bitcoin {
				// Ensure we fail if we didn't receive any estimates.
				log_error!(
					self.logger,
					"Failed to retrieve fee rate estimates for {:?}: empty fee estimates are dissallowed on Mainnet.",
					target,
				);
				return Err(Error::FeerateEstimationUpdateFailed);
			}

			let converted_estimates = esplora_client::convert_fee_rate(num_blocks, estimates)
				.map_err(|e| {
					log_error!(
						self.logger,
						"Failed to convert fee rate estimates for {:?}: {}",
						target,
						e
					);
					Error::FeerateEstimationUpdateFailed
				})?;

			let fee_rate = FeeRate::from_sat_per_vb(converted_estimates);

			// LDK 0.0.118 introduced changes to the `ConfirmationTarget` semantics that
			// require some post-estimation adjustments to the fee rates, which we do here.
			let adjusted_fee_rate = match target {
				ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => {
					let slightly_less_than_background =
						fee_rate.fee_wu(Weight::from_wu(1000)) - 250;
					FeeRate::from_sat_per_kwu(slightly_less_than_background as f32)
				},
				_ => fee_rate,
			};

			let mut locked_fee_rate_cache = self.fee_rate_cache.write().unwrap();
			locked_fee_rate_cache.insert(target, adjusted_fee_rate);
			log_trace!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted_fee_rate.fee_wu(Weight::from_wu(1000))
			);
		}
		Ok(())
	}

	pub(crate) fn estimate_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate {
		let locked_fee_rate_cache = self.fee_rate_cache.read().unwrap();

		let fallback_sats_kwu = match confirmation_target {
			ConfirmationTarget::OnChainSweep => 5000,
			ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => FEERATE_FLOOR_SATS_PER_KW,
			ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => FEERATE_FLOOR_SATS_PER_KW,
			ConfirmationTarget::AnchorChannelFee => 500,
			ConfirmationTarget::NonAnchorChannelFee => 1000,
			ConfirmationTarget::ChannelCloseMinimum => 500,
		};

		// We'll fall back on this, if we really don't have any other information.
		let fallback_rate = FeeRate::from_sat_per_kwu(fallback_sats_kwu as f32);

		*locked_fee_rate_cache.get(&confirmation_target).unwrap_or(&fallback_rate)
	}
}

impl<L: Deref> FeeEstimator for OnchainFeeEstimator<L>
where
	L::Target: Logger,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		(self.estimate_fee_rate(confirmation_target).fee_wu(Weight::from_wu(1000)) as u32)
			.max(FEERATE_FLOOR_SATS_PER_KW)
	}
}
