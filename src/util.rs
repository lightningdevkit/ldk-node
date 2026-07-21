// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Miscellaneous pure helper functions.

use bitcoin::constants::SUBSIDY_HALVING_INTERVAL;
use bitcoin::{Amount, Block, FeeRate};

use crate::fee_estimator::{get_num_block_defaults_for_target, ConfirmationTarget};

/// Block subsidy at the given height (approximate on regtest).
pub(crate) fn block_subsidy(height: u32) -> Amount {
	let halvings = height / SUBSIDY_HALVING_INTERVAL;
	if halvings >= 64 {
		return Amount::ZERO;
	}
	Amount::from_sat((Amount::ONE_BTC.to_sat() * 50) >> halvings)
}

/// Average fee rate of a block, derived from its coinbase: `(coinbase output total - subsidy) /
/// weight`. Lets us compute the fee rate of a block we already hold without a re-download.
pub(crate) fn coinbase_fee_rate(block: &Block, height: u32) -> FeeRate {
	let revenue: Amount = block
		.txdata
		.first()
		.map(|coinbase| coinbase.output.iter().map(|txout| txout.value).sum())
		.unwrap_or(Amount::ZERO);
	let block_fees = revenue.checked_sub(block_subsidy(height)).unwrap_or(Amount::ZERO);
	let fee_rate = block_fees.to_sat().checked_div(block.weight().to_kwu_floor()).unwrap_or(0);
	FeeRate::from_sat_per_kwu(fee_rate)
}

/// Maps a confirmation target to the percentile of the recent-block fee-rate window we read for it.
///
/// More urgent targets (shorter confirmation horizon) read a higher percentile; relaxed targets
/// read a lower one. This is a coarse stand-in for the per-horizon estimates a mempool-aware
/// backend would provide.
pub(crate) fn cbf_percentile_for_target(target: ConfirmationTarget) -> f64 {
	match get_num_block_defaults_for_target(target) {
		0..=2 => 90.0,
		3..=6 => 75.0,
		7..=12 => 50.0,
		13..=144 => 25.0,
		_ => 10.0,
	}
}

/// Returns the value at the given percentile of an ascending-sorted slice using nearest-rank.
/// Returns `0` for an empty slice.
pub(crate) fn percentile_of_sorted(sorted: &[u64], percentile: f64) -> u64 {
	if sorted.is_empty() {
		return 0;
	}
	let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
	let idx = rank.saturating_sub(1).min(sorted.len() - 1);
	sorted[idx]
}

/// Returns a random `u64` uniformly distributed in `[min, max]` (inclusive).
pub(crate) fn random_range(min: u64, max: u64) -> u64 {
	debug_assert!(min <= max);
	if min == max {
		return min;
	}
	let range = match (max - min).checked_add(1) {
		Some(r) => r,
		None => {
			// overflowed — full u64::MAX range
			let mut buf = [0u8; 8];
			getrandom::fill(&mut buf).expect("getrandom failed");
			return u64::from_ne_bytes(buf);
		},
	};
	// We remove bias due to the fact that the range does not evenly divide 2⁶⁴.
	// Imagine we had a range from 0 to 2⁶⁴-2 (of length 2⁶⁴-1), then
	// the outcomes of 0 would be twice as frequent as any other, as 0 can be produced
	// as randomly drawn 0 % 2⁶⁴-1 and as well as 2⁶⁴-1 % 2⁶⁴-1
	let limit = u64::MAX - (u64::MAX % range);
	loop {
		let mut buf = [0u8; 8];
		getrandom::fill(&mut buf).expect("getrandom failed");
		let val = u64::from_ne_bytes(buf);
		if val < limit {
			return min + (val % range);
		}
		// loop runs ~1 iteration on average, in worst case it's ~2 iterations on average
	}
}
