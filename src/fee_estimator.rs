// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS;
use crate::logger::{log_error, log_trace, Logger};
use crate::{Config, Error};

use lightning::chain::chaininterface::ConfirmationTarget as LdkConfirmationTarget;
use lightning::chain::chaininterface::FeeEstimator as LdkFeeEstimator;
use lightning::chain::chaininterface::FEERATE_FLOOR_SATS_PER_KW;

use bitcoin::FeeRate;
use esplora_client::AsyncClient as EsploraClient;

use bitcoin::Network;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum ConfirmationTarget {
	/// The default target for onchain payments.
	OnchainPayment,
	/// The target used for funding transactions.
	ChannelFunding,
	/// Targets used by LDK.
	Lightning(LdkConfirmationTarget),
}

pub(crate) trait FeeEstimator {
	fn estimate_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate;
}

impl From<LdkConfirmationTarget> for ConfirmationTarget {
	fn from(value: LdkConfirmationTarget) -> Self {
		Self::Lightning(value)
	}
}

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
		let estimates = tokio::time::timeout(
			Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
			self.esplora_client.get_fee_estimates(),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
			Error::FeerateEstimationUpdateTimeout
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
			Error::FeerateEstimationUpdateFailed
		})?;

		if estimates.is_empty() && self.config.network == Network::Bitcoin {
			// Ensure we fail if we didn't receive any estimates.
			log_error!(
				self.logger,
				"Failed to retrieve fee rate estimates: empty fee estimates are dissallowed on Mainnet.",
			);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let confirmation_targets = vec![
			ConfirmationTarget::OnchainPayment,
			ConfirmationTarget::ChannelFunding,
			LdkConfirmationTarget::MaximumFeeEstimate.into(),
			LdkConfirmationTarget::UrgentOnChainSweep.into(),
			LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee.into(),
			LdkConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee.into(),
			LdkConfirmationTarget::AnchorChannelFee.into(),
			LdkConfirmationTarget::NonAnchorChannelFee.into(),
			LdkConfirmationTarget::ChannelCloseMinimum.into(),
			LdkConfirmationTarget::OutputSpendingFee.into(),
		];

		for target in confirmation_targets {
			let num_blocks = match target {
				ConfirmationTarget::OnchainPayment => 6,
				ConfirmationTarget::ChannelFunding => 12,
				ConfirmationTarget::Lightning(ldk_target) => match ldk_target {
					LdkConfirmationTarget::MaximumFeeEstimate => 1,
					LdkConfirmationTarget::UrgentOnChainSweep => 6,
					LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee => 1008,
					LdkConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => 144,
					LdkConfirmationTarget::AnchorChannelFee => 1008,
					LdkConfirmationTarget::NonAnchorChannelFee => 12,
					LdkConfirmationTarget::ChannelCloseMinimum => 144,
					LdkConfirmationTarget::OutputSpendingFee => 12,
				},
			};

			let converted_estimate_sat_vb =
				esplora_client::convert_fee_rate(num_blocks, estimates.clone()).map_err(|e| {
					log_error!(
						self.logger,
						"Failed to convert fee rate estimates for {:?}: {}",
						target,
						e
					);
					Error::FeerateEstimationUpdateFailed
				})?;

			let fee_rate = FeeRate::from_sat_per_kwu((converted_estimate_sat_vb * 250.0) as u64);

			// LDK 0.0.118 introduced changes to the `ConfirmationTarget` semantics that
			// require some post-estimation adjustments to the fee rates, which we do here.
			let adjusted_fee_rate = match target {
				ConfirmationTarget::Lightning(
					LdkConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
				) => {
					let slightly_less_than_background = fee_rate.to_sat_per_kwu() - 250;
					FeeRate::from_sat_per_kwu(slightly_less_than_background)
				},
				_ => fee_rate,
			};

			let mut locked_fee_rate_cache = self.fee_rate_cache.write().unwrap();
			locked_fee_rate_cache.insert(target, adjusted_fee_rate);
			log_trace!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted_fee_rate.to_sat_per_kwu(),
			);
		}
		Ok(())
	}
}

impl<L: Deref> FeeEstimator for OnchainFeeEstimator<L>
where
	L::Target: Logger,
{
	fn estimate_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate {
		let locked_fee_rate_cache = self.fee_rate_cache.read().unwrap();

		let fallback_sats_kwu = match confirmation_target {
			ConfirmationTarget::OnchainPayment => 5000,
			ConfirmationTarget::ChannelFunding => 1000,
			ConfirmationTarget::Lightning(ldk_target) => match ldk_target {
				LdkConfirmationTarget::MaximumFeeEstimate => 8000,
				LdkConfirmationTarget::UrgentOnChainSweep => 5000,
				LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee => {
					FEERATE_FLOOR_SATS_PER_KW
				},
				LdkConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => {
					FEERATE_FLOOR_SATS_PER_KW
				},
				LdkConfirmationTarget::AnchorChannelFee => 500,
				LdkConfirmationTarget::NonAnchorChannelFee => 1000,
				LdkConfirmationTarget::ChannelCloseMinimum => 500,
				LdkConfirmationTarget::OutputSpendingFee => 1000,
			},
		};

		// We'll fall back on this, if we really don't have any other information.
		let fallback_rate = FeeRate::from_sat_per_kwu(fallback_sats_kwu as u64);

		let estimate = *locked_fee_rate_cache.get(&confirmation_target).unwrap_or(&fallback_rate);

		// Currently we assume every transaction needs to at least be relayable, which is why we
		// enforce a lower bound of `FEERATE_FLOOR_SATS_PER_KW`.
		FeeRate::from_sat_per_kwu(estimate.to_sat_per_kwu().max(FEERATE_FLOOR_SATS_PER_KW as u64))
	}
}

impl<L: Deref> LdkFeeEstimator for OnchainFeeEstimator<L>
where
	L::Target: Logger,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: LdkConfirmationTarget) -> u32 {
		self.estimate_fee_rate(confirmation_target.into()).to_sat_per_kwu() as u32
	}
}
