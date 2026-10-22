// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::sync::RwLock;

use bitcoin::FeeRate;
use lightning::chain::chaininterface::{
	ConfirmationTarget as LdkConfirmationTarget, FeeEstimator as LdkFeeEstimator,
	FEERATE_FLOOR_SATS_PER_KW,
};

/// The highest fee rate we are willing to use for any confirmation target, in sats per 1000 weight
/// units (i.e., 2,000 sats/vB).
///
/// This is well above any fee rate seen in practice, but protects us from blindly following
/// absurdly high estimates reported by a faulty or malicious chain source, which could otherwise
/// have us burn (a large part of) our on-chain balance in fees.
pub(crate) const MAX_FEERATE_SATS_PER_KW: u64 = 2_000 * 250;

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

pub(crate) struct OnchainFeeEstimator {
	fee_rate_cache: RwLock<HashMap<ConfirmationTarget, FeeRate>>,
}

impl OnchainFeeEstimator {
	pub(crate) fn new() -> Self {
		let fee_rate_cache = RwLock::new(HashMap::new());
		Self { fee_rate_cache }
	}

	// Updates the fee rate cache and returns if the new values changed.
	pub(crate) fn set_fee_rate_cache(
		&self, fee_rate_cache_update: HashMap<ConfirmationTarget, FeeRate>,
	) -> bool {
		let mut locked_fee_rate_cache = self.fee_rate_cache.write().expect("lock");
		if fee_rate_cache_update != *locked_fee_rate_cache {
			*locked_fee_rate_cache = fee_rate_cache_update;
			true
		} else {
			false
		}
	}
}

impl FeeEstimator for OnchainFeeEstimator {
	fn estimate_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate {
		let locked_fee_rate_cache = self.fee_rate_cache.read().expect("lock");

		let fallback_sats_kwu = get_fallback_rate_for_target(confirmation_target);

		// We'll fall back on this, if we really don't have any other information.
		let fallback_rate = FeeRate::from_sat_per_kwu(fallback_sats_kwu as u64);

		let estimate = *locked_fee_rate_cache.get(&confirmation_target).unwrap_or(&fallback_rate);

		// Currently we assume every transaction needs to at least be relayable, which is why we
		// enforce a lower bound of `FEERATE_FLOOR_SATS_PER_KW`. Similarly, we never trust estimates
		// above `MAX_FEERATE_SATS_PER_KW`, as they'd allow our chain source to make us burn funds.
		FeeRate::from_sat_per_kwu(
			estimate
				.to_sat_per_kwu()
				.clamp(FEERATE_FLOOR_SATS_PER_KW as u64, MAX_FEERATE_SATS_PER_KW),
		)
	}
}

impl LdkFeeEstimator for OnchainFeeEstimator {
	fn get_est_sat_per_1000_weight(&self, confirmation_target: LdkConfirmationTarget) -> u32 {
		self.estimate_fee_rate(confirmation_target.into())
			.to_sat_per_kwu()
			.try_into()
			.unwrap_or_else(|_| get_fallback_rate_for_ldk_target(confirmation_target))
	}
}

pub(crate) fn get_num_block_defaults_for_target(target: ConfirmationTarget) -> usize {
	match target {
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
	}
}

pub(crate) fn get_fallback_rate_for_target(target: ConfirmationTarget) -> u32 {
	match target {
		ConfirmationTarget::OnchainPayment => 5000,
		ConfirmationTarget::ChannelFunding => 1000,
		ConfirmationTarget::Lightning(ldk_target) => get_fallback_rate_for_ldk_target(ldk_target),
	}
}

pub(crate) fn get_fallback_rate_for_ldk_target(target: LdkConfirmationTarget) -> u32 {
	match target {
		LdkConfirmationTarget::MaximumFeeEstimate => 8000,
		LdkConfirmationTarget::UrgentOnChainSweep => 5000,
		LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee => FEERATE_FLOOR_SATS_PER_KW,
		LdkConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => FEERATE_FLOOR_SATS_PER_KW,
		LdkConfirmationTarget::AnchorChannelFee => 500,
		LdkConfirmationTarget::NonAnchorChannelFee => 1000,
		LdkConfirmationTarget::ChannelCloseMinimum => 500,
		LdkConfirmationTarget::OutputSpendingFee => 1000,
	}
}

pub(crate) fn get_all_conf_targets() -> [ConfirmationTarget; 10] {
	[
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
	]
}

pub(crate) fn apply_post_estimation_adjustments(
	target: ConfirmationTarget, estimated_rate: FeeRate,
) -> FeeRate {
	match target {
		ConfirmationTarget::Lightning(
			LdkConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
		) => {
			let slightly_less_than_background = estimated_rate
				.to_sat_per_kwu()
				.saturating_sub(250)
				.max(FEERATE_FLOOR_SATS_PER_KW as u64);
			FeeRate::from_sat_per_kwu(slightly_less_than_background)
		},
		ConfirmationTarget::Lightning(LdkConfirmationTarget::MaximumFeeEstimate) => {
			// MaximumFeeEstimate is mostly used for protection against fee-inflation attacks. As
			// users were previously impacted by this limit being too restrictive (read: too low),
			// we bump it here a bit to give them some leeway.
			let slightly_bump = estimated_rate
				.to_sat_per_kwu()
				.saturating_mul(11)
				.saturating_div(10)
				.saturating_add(2500);
			FeeRate::from_sat_per_kwu(slightly_bump)
		},
		_ => estimated_rate,
	}
}

/// The most we are willing to pay for a channel funding transaction: `1.5x` our funding feerate
/// estimate. Used as the `max_feerate` ceiling for splices and their RBF fee bumps.
pub(crate) fn max_funding_feerate(estimate: FeeRate) -> FeeRate {
	FeeRate::from_sat_per_kwu(estimate.to_sat_per_kwu() * 3 / 2)
}

/// Picks the `(target, max)` feerates for replacing a pending splice's in-flight funding
/// transaction via RBF, or `None` if the RBF can't be done within our fee ceiling.
///
/// `max` is the most we are willing to pay (see [`max_funding_feerate`]), which tracks our current
/// estimate and so may have risen or fallen since the original splice; it is never inflated to meet
/// the RBF minimum. `target` is what we actually pay — our current estimate, or the template's RBF
/// minimum if that is higher (required to replace the transaction). If that minimum exceeds `max`,
/// we can't RBF.
pub(crate) fn rbf_splice_feerates(
	estimate: FeeRate, min_rbf_feerate: FeeRate,
) -> Option<(FeeRate, FeeRate)> {
	let max = max_funding_feerate(estimate);
	let target = estimate.max(min_rbf_feerate);
	(target <= max).then_some((target, max))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn estimates_are_bounded() {
		let estimator = OnchainFeeEstimator::new();
		let kwu = FeeRate::from_sat_per_kwu;
		let targets = get_all_conf_targets();

		// Absurdly high estimates (as might be reported by a malicious chain source) are capped.
		for absurd_rate in [MAX_FEERATE_SATS_PER_KW + 1, 50_000 * 250, u64::MAX] {
			let cache = targets.iter().map(|t| (*t, kwu(absurd_rate))).collect();
			estimator.set_fee_rate_cache(cache);
			for target in targets {
				assert_eq!(
					estimator.estimate_fee_rate(target),
					kwu(MAX_FEERATE_SATS_PER_KW),
					"{:?} not capped for {}",
					target,
					absurd_rate
				);
			}
			for ldk_target in [
				LdkConfirmationTarget::UrgentOnChainSweep,
				LdkConfirmationTarget::MaximumFeeEstimate,
			] {
				assert_eq!(
					estimator.get_est_sat_per_1000_weight(ldk_target),
					MAX_FEERATE_SATS_PER_KW as u32
				);
			}
		}

		// Reasonable estimates are passed through, and too low estimates are floored.
		let cache = targets
			.iter()
			.enumerate()
			.map(|(i, t)| (*t, if i % 2 == 0 { kwu(MAX_FEERATE_SATS_PER_KW) } else { kwu(1) }))
			.collect();
		estimator.set_fee_rate_cache(cache);
		for (i, target) in targets.iter().enumerate() {
			let expected = if i % 2 == 0 {
				kwu(MAX_FEERATE_SATS_PER_KW)
			} else {
				kwu(FEERATE_FLOOR_SATS_PER_KW as u64)
			};
			assert_eq!(estimator.estimate_fee_rate(*target), expected);
		}
		let cache = targets.iter().map(|t| (*t, kwu(5_000))).collect();
		estimator.set_fee_rate_cache(cache);
		assert_eq!(estimator.estimate_fee_rate(ConfirmationTarget::OnchainPayment), kwu(5_000));
	}

	#[test]
	fn rbf_splice_feerates_target_and_max() {
		let kwu = FeeRate::from_sat_per_kwu;
		// Estimate below the RBF minimum but within our ceiling: pay the minimum to replace the
		// transaction; the max stays 1.5x the estimate (never inflated) and already clears it.
		assert_eq!(rbf_splice_feerates(kwu(253), kwu(278)), Some((kwu(278), kwu(253 * 3 / 2))));
		// Estimate risen above the RBF minimum: pay the higher estimate, not the stale minimum.
		assert_eq!(rbf_splice_feerates(kwu(500), kwu(278)), Some((kwu(500), kwu(500 * 3 / 2))));
		// RBF minimum above our max (1.5x a fallen estimate): we can't RBF within our ceiling.
		assert_eq!(rbf_splice_feerates(kwu(100), kwu(278)), None);
	}
}
