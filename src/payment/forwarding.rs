// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a handler allowing to query forwarded payments and forwarding statistics.

use std::sync::Arc;

use lightning::ln::types::ChannelId;
use lightning::util::persist::PageToken;

use crate::config::{Config, ForwardedPaymentTrackingMode};
use crate::error::Error;
use crate::payment::store::{
	ChannelForwardingStats, ChannelPairForwardingStats, ForwardedPaymentDetails,
};
use crate::runtime::Runtime;
use crate::types::{
	ChannelForwardingStatsStore, ChannelPairForwardingStatsStore, ForwardedPaymentStore,
};

/// A page of forwarded payments returned from a paginated listing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ForwardedPaymentDetailsPage {
	/// Forwarded payments in this page.
	pub payments: Vec<ForwardedPaymentDetails>,
	/// Token to pass to the next call to continue listing, if another page exists.
	pub next_page_token: Option<PageToken>,
}

/// A page of channel forwarding statistics returned from a paginated listing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelForwardingStatsPage {
	/// Channel forwarding statistics in this page.
	pub stats: Vec<ChannelForwardingStats>,
	/// Token to pass to the next call to continue listing, if another page exists.
	pub next_page_token: Option<PageToken>,
}

/// A page of channel-pair forwarding statistics returned from a paginated listing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelPairForwardingStatsPage {
	/// Channel-pair forwarding statistics in this page.
	pub stats: Vec<ChannelPairForwardingStats>,
	/// Token to pass to the next call to continue listing, if another page exists.
	pub next_page_token: Option<PageToken>,
}

/// A handler allowing to query forwarded payments and forwarding statistics.
///
/// Should be retrieved by calling [`Node::forwarding`].
///
/// [`Node::forwarding`]: crate::Node::forwarding
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct Forwarding {
	runtime: Arc<Runtime>,
	forwarded_payment_store: Arc<ForwardedPaymentStore>,
	channel_forwarding_stats_store: Arc<ChannelForwardingStatsStore>,
	channel_pair_forwarding_stats_store: Arc<ChannelPairForwardingStatsStore>,
	config: Arc<Config>,
}

impl Forwarding {
	pub(crate) fn new(
		runtime: Arc<Runtime>, forwarded_payment_store: Arc<ForwardedPaymentStore>,
		channel_forwarding_stats_store: Arc<ChannelForwardingStatsStore>,
		channel_pair_forwarding_stats_store: Arc<ChannelPairForwardingStatsStore>,
		config: Arc<Config>,
	) -> Self {
		Self {
			runtime,
			forwarded_payment_store,
			channel_forwarding_stats_store,
			channel_pair_forwarding_stats_store,
			config,
		}
	}

	/// Retrieves all forwarded payments that match the given predicate.
	pub fn list_payments_with_filter<F: FnMut(&&ForwardedPaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<ForwardedPaymentDetails> {
		self.forwarded_payment_store.list_filter(f)
	}

	/// Retrieves all channel forwarding statistics that match the given predicate.
	pub fn list_channel_stats_with_filter<F: FnMut(&&ChannelForwardingStats) -> bool>(
		&self, f: F,
	) -> Vec<ChannelForwardingStats> {
		self.channel_forwarding_stats_store.list_filter(f)
	}

	/// Retrieves all channel pair forwarding statistics that match the given predicate.
	pub fn list_channel_pair_stats_with_filter<F: FnMut(&&ChannelPairForwardingStats) -> bool>(
		&self, f: F,
	) -> Vec<ChannelPairForwardingStats> {
		self.channel_pair_forwarding_stats_store.list_filter(f)
	}

	/// Retrieves pages of channel-pair forwarding statistics until the given filter produces a
	/// non-empty result page or the underlying store is exhausted.
	fn list_channel_pair_stats_filtered_page<F: FnMut(&ChannelPairForwardingStats) -> bool>(
		&self, mut page_token: Option<PageToken>, mut f: F,
	) -> Result<ChannelPairForwardingStatsPage, Error> {
		loop {
			let (mut stats, next_page_token) = self
				.runtime
				.block_on(self.channel_pair_forwarding_stats_store.list_page(page_token))?;
			stats.retain(|stats| f(stats));
			if !stats.is_empty() || next_page_token.is_none() {
				stats.sort_by_key(|stats| stats.bucket_start_timestamp);
				return Ok(ChannelPairForwardingStatsPage { stats, next_page_token });
			}
			page_token = next_page_token;
		}
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl Forwarding {
	/// Returns the configured forwarded payment tracking mode.
	pub fn tracking_mode(&self) -> ForwardedPaymentTrackingMode {
		self.config.forwarded_payment_tracking_mode
	}

	/// Retrieve the details of a specific forwarded payment using its opaque identifier.
	///
	/// The identifier is returned in [`ForwardedPaymentDetails::id`].
	pub fn payment(&self, forwarded_payment_id: String) -> Option<ForwardedPaymentDetails> {
		self.forwarded_payment_store.get(&forwarded_payment_id)
	}

	/// Retrieves a page of forwarded payments from the underlying paginated store.
	pub fn list_payments(
		&self, page_token: Option<PageToken>,
	) -> Result<ForwardedPaymentDetailsPage, Error> {
		let (payments, next_page_token) =
			self.runtime.block_on(self.forwarded_payment_store.list_page(page_token))?;
		Ok(ForwardedPaymentDetailsPage { payments, next_page_token })
	}

	/// Retrieve the forwarding statistics for a specific channel.
	pub fn channel_stats(&self, channel_id: &ChannelId) -> Option<ChannelForwardingStats> {
		self.channel_forwarding_stats_store.get(channel_id)
	}

	/// Retrieves a page of channel forwarding statistics from the underlying paginated store.
	pub fn list_channel_stats(
		&self, page_token: Option<PageToken>,
	) -> Result<ChannelForwardingStatsPage, Error> {
		let (stats, next_page_token) =
			self.runtime.block_on(self.channel_forwarding_stats_store.list_page(page_token))?;
		Ok(ChannelForwardingStatsPage { stats, next_page_token })
	}

	/// Retrieves a page of channel pair forwarding statistics from the underlying paginated store.
	pub fn list_channel_pair_stats(
		&self, page_token: Option<PageToken>,
	) -> Result<ChannelPairForwardingStatsPage, Error> {
		let (stats, next_page_token) = self
			.runtime
			.block_on(self.channel_pair_forwarding_stats_store.list_page(page_token))?;
		Ok(ChannelPairForwardingStatsPage { stats, next_page_token })
	}

	/// Retrieves a page of channel pair forwarding statistics within a specific time range.
	///
	/// A bucket matches when its start timestamp is greater than or equal to `start_timestamp` and
	/// less than `end_timestamp`.
	///
	/// The listing is complete when `next_page_token` is `None`. Matches within each returned page
	/// are sorted by bucket start time, but ordering is not global across pages.
	pub fn list_channel_pair_stats_in_range(
		&self, start_timestamp: u64, end_timestamp: u64, page_token: Option<PageToken>,
	) -> Result<ChannelPairForwardingStatsPage, Error> {
		self.list_channel_pair_stats_filtered_page(page_token, |stats| {
			stats.bucket_start_timestamp >= start_timestamp
				&& stats.bucket_start_timestamp < end_timestamp
		})
	}

	/// Retrieves a page of forwarding statistics buckets for a specific channel pair.
	///
	/// The listing is complete when `next_page_token` is `None`. Matches within each returned page
	/// are sorted by bucket start time, but ordering is not global across pages.
	pub fn list_channel_pair_stats_for_pair(
		&self, prev_channel_id: ChannelId, next_channel_id: ChannelId,
		page_token: Option<PageToken>,
	) -> Result<ChannelPairForwardingStatsPage, Error> {
		self.list_channel_pair_stats_filtered_page(page_token, |stats| {
			stats.prev_channel_id == prev_channel_id && stats.next_channel_id == next_channel_id
		})
	}
}
