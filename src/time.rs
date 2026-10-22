// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Wall-clock and monotonic time sources.
//!
//! All of LDK Node's clock reads go through this module. Native targets default to the system
//! clocks. Hosts without them, such as `wasm32-unknown-unknown`, must supply a [`TimeProvider`]
//! via [`Builder::set_time_provider`].
//!
//! [`Builder::set_time_provider`]: crate::Builder::set_time_provider
//!
//! The provider does not control clocks read inside dependencies, such as Tokio timers.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use lightning_liquidity::utils::time::TimeProvider as LiquidityTimeProvider;

/// Supplies wall and monotonic time for LDK-Node and its explicit-time dependency calls.
///
/// Implementations must be safe to call concurrently and must not call back into LDK-Node's
/// clock-dependent operations, including logging. Wall time should track the actual Unix time
/// used by peers and services for timestamp and expiry validation. Returning a time before the
/// epoch causes some operations to fail or panic, just as with the native clock.
pub trait TimeProvider: Send + Sync {
	/// Returns time since the Unix epoch, or `None` if the clock is set before it.
	fn duration_since_epoch(&self) -> Option<Duration>;

	/// Returns elapsed time from a fixed, arbitrary origin.
	///
	/// Values must never decrease across calls, including calls from different threads. This
	/// clock must advance independently of wall-clock adjustments and must not wrap around.
	fn monotonic_time(&self) -> Duration;
}

impl std::fmt::Debug for dyn TimeProvider {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("TimeProvider")
	}
}

/// A different clock is already in use by this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeProviderAlreadyInitialized;

static TIME_PROVIDER: OnceLock<Arc<dyn TimeProvider>> = OnceLock::new();

/// Installs the clock shared by all nodes in this process.
///
/// The first clock read selects the native provider if none has been installed on a supported
/// native target. Once selected, the provider cannot be replaced, even after all nodes have stopped,
/// so monotonic measurements always use the same origin. Installing the provider that is already in
/// use succeeds, so several nodes can share one.
pub(crate) fn set_time_provider(
	provider: Arc<dyn TimeProvider>,
) -> Result<(), TimeProviderAlreadyInitialized> {
	let installed = TIME_PROVIDER.get_or_init(|| Arc::clone(&provider));
	if Arc::ptr_eq(installed, &provider) {
		Ok(())
	} else {
		Err(TimeProviderAlreadyInitialized)
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
struct NativeTimeProvider(std::time::Instant);

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[allow(clippy::disallowed_methods)] // Native platform clock boundary.
impl TimeProvider for NativeTimeProvider {
	fn duration_since_epoch(&self) -> Option<Duration> {
		std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()
	}

	fn monotonic_time(&self) -> Duration {
		self.0.elapsed()
	}
}

#[allow(clippy::disallowed_methods)] // Select the native platform clock once.
fn time_provider() -> &'static dyn TimeProvider {
	TIME_PROVIDER
		.get_or_init(|| {
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				Arc::new(NativeTimeProvider(std::time::Instant::now()))
			}
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			panic!("install a time provider before reading the clock on this target")
		})
		.as_ref()
}

/// Returns the time elapsed since the Unix epoch, or `None` if the clock is set before it.
pub(crate) fn duration_since_epoch() -> Option<Duration> {
	time_provider().duration_since_epoch()
}

/// Returns the seconds elapsed since the Unix epoch, or `None` if the clock is set before it.
pub(crate) fn unix_time_secs() -> Option<u64> {
	duration_since_epoch().map(|d| d.as_secs())
}

/// Returns the current wall-clock time as a UTC [`DateTime`].
///
/// Falls back to the Unix epoch if the clock is set before it or outside the supported range.
pub(crate) fn now_utc() -> DateTime<Utc> {
	to_utc(duration_since_epoch())
}

fn to_utc(since_epoch: Option<Duration>) -> DateTime<Utc> {
	since_epoch
		.and_then(|d| DateTime::from_timestamp(i64::try_from(d.as_secs()).ok()?, d.subsec_nanos()))
		.unwrap_or_default()
}

/// A measurement of a monotonically nondecreasing clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Instant(Duration);

impl Instant {
	pub(crate) fn now() -> Self {
		Self(time_provider().monotonic_time())
	}

	/// Returns the time elapsed since `earlier`, or zero if `earlier` is later than `self`.
	pub(crate) fn duration_since(&self, earlier: Self) -> Duration {
		self.0.saturating_sub(earlier.0)
	}

	pub(crate) fn elapsed(&self) -> Duration {
		Self::now().duration_since(*self)
	}
}

/// Supplies our clock to LDK components that take a [`LiquidityTimeProvider`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct LdkTimeProvider;

impl LiquidityTimeProvider for LdkTimeProvider {
	fn duration_since_epoch(&self) -> Duration {
		duration_since_epoch().expect("current time should not be earlier than the Unix epoch")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn native_clock() {
		let start = Instant::now();
		assert!(Instant::now() >= start);
		assert!(unix_time_secs().unwrap() > 0);
		// The first read selects the native provider, which can't be replaced afterwards.
		assert_eq!(
			set_time_provider(Arc::new(NativeTimeProvider(std::time::Instant::now()))),
			Err(TimeProviderAlreadyInitialized)
		);
		// Installing the provider already in use succeeds.
		let installed = Arc::clone(TIME_PROVIDER.get().unwrap());
		assert_eq!(set_time_provider(installed), Ok(()));
	}

	#[test]
	fn instant_saturates() {
		let earlier = Instant(Duration::from_secs(10));
		let later = Instant(Duration::from_secs(20));
		assert_eq!(later.duration_since(earlier), Duration::from_secs(10));
		assert_eq!(earlier.duration_since(later), Duration::ZERO);
	}

	#[test]
	fn utc_conversion() {
		let timestamp = Duration::new(1_700_000_000, 123_456_789);
		let utc = to_utc(Some(timestamp));
		assert_eq!(utc.timestamp(), timestamp.as_secs() as i64);
		assert_eq!(utc.timestamp_subsec_nanos(), timestamp.subsec_nanos());
		assert_eq!(to_utc(None), DateTime::<Utc>::default());
		assert_eq!(to_utc(Some(Duration::MAX)), DateTime::<Utc>::default());
	}
}
