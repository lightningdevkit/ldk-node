// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! [`RateLimiter`] to control the rate of requests from users.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Implements a leaky-bucket style rate limiter parameterized by the max capacity of the bucket, the refill interval,
/// and the max idle duration.
///
/// For every passing of the refill interval, one token is added to the bucket, up to the maximum capacity. When the
/// bucket has remained at the maximum capacity for longer than the max idle duration, it is removed to prevent memory
/// leakage.
pub(crate) struct RateLimiter {
	users: HashMap<Vec<u8>, Bucket>,
	capacity: u32,
	refill_interval: Duration,
	max_idle: Duration,
}

struct Bucket {
	tokens: u32,
	last_refill: Instant,
}

impl RateLimiter {
	pub(crate) fn new(capacity: u32, refill_interval: Duration, max_idle: Duration) -> Self {
		Self { users: HashMap::new(), capacity, refill_interval, max_idle }
	}

	pub(crate) fn allow(&mut self, user_id: &[u8]) -> bool {
		let now = Instant::now();

		let entry = self.users.entry(user_id.to_vec());
		let is_new_user = matches!(entry, std::collections::hash_map::Entry::Vacant(_));

		let bucket = entry.or_insert(Bucket { tokens: self.capacity, last_refill: now });

		let elapsed = now.duration_since(bucket.last_refill);
		let tokens_to_add = (elapsed.as_secs_f64() / self.refill_interval.as_secs_f64()) as u32;

		if tokens_to_add > 0 {
			bucket.tokens = (bucket.tokens + tokens_to_add).min(self.capacity);
			bucket.last_refill = now;
		}

		let allow = if bucket.tokens > 0 {
			bucket.tokens -= 1;
			true
		} else {
			false
		};

		// Each time a new user is added, we take the opportunity to clean up old rate limits.
		if is_new_user {
			self.garbage_collect(self.max_idle);
		}

		allow
	}

	fn garbage_collect(&mut self, max_idle: Duration) {
		let now = Instant::now();
		self.users.retain(|_, bucket| now.duration_since(bucket.last_refill) < max_idle);
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use crate::payment::asynchronous::rate_limiter::RateLimiter;

	#[test]
	fn rate_limiter_test() {
		// Test
		let mut rate_limiter =
			RateLimiter::new(3, Duration::from_millis(100), Duration::from_secs(1));

		assert!(rate_limiter.allow(b"user1"));
		assert!(rate_limiter.allow(b"user1"));
		assert!(rate_limiter.allow(b"user1"));
		assert!(!rate_limiter.allow(b"user1"));
		assert!(rate_limiter.allow(b"user2"));

		std::thread::sleep(Duration::from_millis(150));

		assert!(rate_limiter.allow(b"user1"));
		assert!(rate_limiter.allow(b"user2"));
	}
}
