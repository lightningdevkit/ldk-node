// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a handler allowing to query forwarded payments and forwarding statistics.

use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning::ln::types::ChannelId;
use lightning::util::persist::PageToken;

use crate::config::{Config, ForwardedPaymentTrackingMode};
use crate::error::Error;
use crate::runtime::Runtime;
use crate::types::{
	ChannelForwardingStatsStore, ChannelPairForwardingStatsStore, ForwardedPaymentStore,
};
use crate::UserChannelId;

/// Details of a payment that has been forwarded through this node.
///
/// Only forwards consisting of one incoming and one outgoing HTLC with a known outbound amount and
/// total fee are recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ForwardedPaymentDetails {
	/// An opaque, randomly generated identifier for this forwarded payment.
	pub id: String,
	/// The incoming channel id.
	pub prev_channel_id: ChannelId,
	/// The outgoing channel id.
	pub next_channel_id: ChannelId,
	/// The incoming user channel id, if available.
	pub prev_user_channel_id: Option<UserChannelId>,
	/// The outgoing user channel id, if available.
	pub next_user_channel_id: Option<UserChannelId>,
	/// The previous node id, if available.
	pub prev_node_id: Option<PublicKey>,
	/// The next node id, if available.
	pub next_node_id: Option<PublicKey>,
	/// The inbound amount attributed to this channel pair, in millisatoshis.
	pub inbound_amount_forwarded_msat: Option<u64>,
	/// The fee attributed to this channel pair, in millisatoshis.
	pub total_fee_earned_msat: Option<u64>,
	/// The skimmed fee attributed to this channel pair, in millisatoshis.
	pub skimmed_fee_msat: Option<u64>,
	/// Whether the forwarded HTLC was claimed from an on-chain transaction.
	pub claim_from_onchain_tx: bool,
	/// The outbound amount attributed to this channel pair, in millisatoshis.
	pub outbound_amount_forwarded_msat: Option<u64>,
	/// The timestamp when this payment was forwarded.
	pub forwarded_at_timestamp: u64,
}

impl_writeable_tlv_based!(ForwardedPaymentDetails, {
	(0, id, required),
	(2, prev_channel_id, required),
	(4, next_channel_id, required),
	(6, prev_user_channel_id, option),
	(8, next_user_channel_id, option),
	(10, prev_node_id, option),
	(12, next_node_id, option),
	(14, total_fee_earned_msat, option),
	(16, skimmed_fee_msat, option),
	(18, claim_from_onchain_tx, required),
	(20, outbound_amount_forwarded_msat, option),
	(22, forwarded_at_timestamp, required),
	(24, inbound_amount_forwarded_msat, option),
});

/// Aggregate statistics for forwarded payments through a single channel.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelForwardingStats {
	/// The channel id these stats apply to.
	pub channel_id: ChannelId,
	/// The channel counterparty node id, if known.
	pub counterparty_node_id: Option<PublicKey>,
	/// Number of forwarded payments where this was the incoming channel.
	pub inbound_payments_forwarded: u64,
	/// Number of forwarded payments where this was the outgoing channel.
	pub outbound_payments_forwarded: u64,
	/// Total inbound amount forwarded through this channel, in millisatoshis.
	pub total_inbound_amount_msat: u64,
	/// Total outbound amount forwarded through this channel, in millisatoshis.
	pub total_outbound_amount_msat: u64,
	/// Total forwarding fees earned through this channel, in millisatoshis, if known for every
	/// recorded forward.
	///
	/// Fees are attributed to the incoming channel. This is zero for an outgoing-channel entry.
	pub total_fee_earned_msat: Option<u64>,
	/// Total skimmed fees attributed to this incoming channel, in millisatoshis.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs claimed from on-chain transactions on this outgoing channel.
	pub onchain_claims_count: u64,
	/// Timestamp of the first forward recorded for this channel.
	pub first_forwarded_at_timestamp: u64,
	/// Timestamp of the latest forward recorded for this channel.
	pub last_forwarded_at_timestamp: u64,
}

impl_writeable_tlv_based!(ChannelForwardingStats, {
	(0, channel_id, required),
	(2, counterparty_node_id, option),
	(4, inbound_payments_forwarded, required),
	(6, outbound_payments_forwarded, required),
	(8, total_inbound_amount_msat, required),
	(10, total_outbound_amount_msat, required),
	(12, total_fee_earned_msat, option),
	(14, total_skimmed_fee_msat, required),
	(16, onchain_claims_count, required),
	(18, first_forwarded_at_timestamp, required),
	(20, last_forwarded_at_timestamp, required),
});

/// Aggregated statistics for a specific channel pair.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelPairForwardingStats {
	/// An opaque identifier for this channel-pair bucket.
	pub id: String,
	/// The incoming channel id.
	pub prev_channel_id: ChannelId,
	/// The outgoing channel id.
	pub next_channel_id: ChannelId,
	/// Start timestamp of this aggregation bucket.
	pub bucket_start_timestamp: u64,
	/// Width of this aggregation bucket, in seconds.
	pub bucket_size_secs: u64,
	/// The previous node id, if available.
	pub prev_node_id: Option<PublicKey>,
	/// The next node id, if available.
	pub next_node_id: Option<PublicKey>,
	/// Number of payments aggregated in this bucket.
	pub payment_count: u64,
	/// Total inbound amount in this bucket, in millisatoshis.
	pub total_inbound_amount_msat: u64,
	/// Total outbound amount in this bucket, in millisatoshis.
	pub total_outbound_amount_msat: u64,
	/// Total forwarding fees earned in this bucket, in millisatoshis, if known for every payment.
	pub total_fee_earned_msat: Option<u64>,
	/// Total skimmed fees in this bucket, in millisatoshis.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs claimed from on-chain transactions.
	pub onchain_claims_count: u64,
	/// Average forwarding fee per payment, in millisatoshis, if known for every payment.
	pub avg_fee_msat: Option<u64>,
	/// Average inbound amount per payment, in millisatoshis.
	pub avg_inbound_amount_msat: u64,
	/// Timestamp of the first forward in this bucket.
	pub first_forwarded_at_timestamp: u64,
	/// Timestamp of the latest forward in this bucket.
	pub last_forwarded_at_timestamp: u64,
	/// Timestamp when this bucket was aggregated.
	pub aggregated_at_timestamp: u64,
}

impl_writeable_tlv_based!(ChannelPairForwardingStats, {
	(0, id, required),
	(2, prev_channel_id, required),
	(4, next_channel_id, required),
	(6, prev_node_id, option),
	(8, next_node_id, option),
	(10, payment_count, required),
	(12, total_inbound_amount_msat, required),
	(14, total_outbound_amount_msat, required),
	(16, total_fee_earned_msat, option),
	(18, total_skimmed_fee_msat, required),
	(20, onchain_claims_count, required),
	(22, avg_fee_msat, option),
	(24, avg_inbound_amount_msat, required),
	(26, first_forwarded_at_timestamp, required),
	(28, last_forwarded_at_timestamp, required),
	(30, aggregated_at_timestamp, required),
	(32, bucket_start_timestamp, required),
	(34, bucket_size_secs, required),
});

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
	) -> Result<Vec<ForwardedPaymentDetails>, Error> {
		self.runtime.block_on(self.forwarded_payment_store.list_filter(f))
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
	) -> Result<Vec<ChannelPairForwardingStats>, Error> {
		self.runtime.block_on(self.channel_pair_forwarding_stats_store.list_filter(f))
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
	pub fn payment(
		&self, forwarded_payment_id: String,
	) -> Result<Option<ForwardedPaymentDetails>, Error> {
		self.runtime.block_on(self.forwarded_payment_store.get(&forwarded_payment_id))
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
	/// This may scan every channel-pair statistics page because the underlying keys are ordered by
	/// channel pair before time.
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
