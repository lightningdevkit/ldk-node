// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

/// Returns a random `u64` uniformly distributed in `[min, max]` (inclusive).
pub(crate) fn random_range(min: u64, max: u64) -> u64 {
	debug_assert!(min <= max);
	if min == max {
		return min;
	}
	let range = max - min + 1;
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
