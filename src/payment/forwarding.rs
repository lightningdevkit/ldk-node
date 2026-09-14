// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a handler allowing to query forwarded payments and forwarding statistics.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning::ln::types::ChannelId;
use lightning::util::ser::{Readable, Writeable, Writer};

use crate::config::{Config, ForwardedPaymentTrackingMode};
use crate::error::Error;
use crate::ffi::{maybe_deref, maybe_wrap};
use crate::hex_utils;
use crate::runtime::Runtime;
use crate::UserChannelId;

use super::forwarding_store::ForwardingStore;
use super::PageToken;

fn parse_forwarding_id(id: &str) -> Option<[u8; 32]> {
	hex_utils::to_vec(id)?.try_into().ok()
}

/// An identifier for a forwarded payment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ForwardedPaymentId(pub [u8; 32]);

impl FromStr for ForwardedPaymentId {
	type Err = Error;

	fn from_str(id: &str) -> Result<Self, Self::Err> {
		parse_forwarding_id(id).map(Self).ok_or(Error::InvalidForwardedPaymentId)
	}
}

impl fmt::Display for ForwardedPaymentId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&hex_utils::to_string(&self.0))
	}
}

impl Writeable for ForwardedPaymentId {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		Ok(self.0.write(writer)?)
	}
}

impl Readable for ForwardedPaymentId {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		Ok(Self(Readable::read(reader)?))
	}
}

/// An identifier for an aggregated channel-pair forwarding statistics bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelPairForwardingStatsId(pub [u8; 32]);

impl FromStr for ChannelPairForwardingStatsId {
	type Err = Error;

	fn from_str(id: &str) -> Result<Self, Self::Err> {
		parse_forwarding_id(id).map(Self).ok_or(Error::InvalidChannelPairForwardingStatsId)
	}
}

impl fmt::Display for ChannelPairForwardingStatsId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&hex_utils::to_string(&self.0))
	}
}

impl Writeable for ChannelPairForwardingStatsId {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		Ok(self.0.write(writer)?)
	}
}

impl Readable for ChannelPairForwardingStatsId {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		Ok(Self(Readable::read(reader)?))
	}
}

/// Details of a payment that has been forwarded through this node.
///
/// A forward is recorded only when it consisted of exactly one incoming and one outgoing HTLC and
/// LDK reported a total fee. LDK reports no fee when the incoming channel was force-closed and the
/// funds are claimed on chain, because the on-chain fees are not yet known at that point, so those
/// forwards are not recorded at all.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ForwardedPaymentDetails {
	/// An opaque identifier for this forwarded payment.
	pub id: ForwardedPaymentId,
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
	///
	/// This is the share of [`Self::total_fee_earned_msat`] that was withheld in addition to the
	/// forwarding fee, not an amount earned on top of it. Adding the two would double-count.
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
	/// A single record covers this channel in both roles. A forward contributes its fee here when
	/// this was the incoming channel, and contributes nothing when this was the outgoing channel,
	/// because fees are attributed to the incoming side.
	pub total_fee_earned_msat: Option<u64>,
	/// Total skimmed fees earned through this channel, in millisatoshis.
	///
	/// This is the share of [`Self::total_fee_earned_msat`] that was withheld in addition to the
	/// forwarding fee, not an amount earned on top of it. Adding the two would double-count.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs that the next hop claimed from an on-chain transaction.
	///
	/// A forward contributes here when this was the outgoing channel.
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
	pub id: ChannelPairForwardingStatsId,
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
	///
	/// This is the share of [`Self::total_fee_earned_msat`] that was withheld in addition to the
	/// forwarding fee, not an amount earned on top of it. Adding the two would double-count.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs that the next hop claimed from an on-chain transaction.
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
/// Should be retrieved by calling [`Node::forwarding_analytics`].
///
/// [`Node::forwarding_analytics`]: crate::Node::forwarding_analytics
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct ForwardingAnalytics {
	runtime: Arc<Runtime>,
	forwarding_store: Arc<ForwardingStore>,
	config: Arc<Config>,
}

impl ForwardingAnalytics {
	pub(crate) fn new(
		runtime: Arc<Runtime>, forwarding_store: Arc<ForwardingStore>, config: Arc<Config>,
	) -> Self {
		Self { runtime, forwarding_store, config }
	}

	/// Retrieves all channel forwarding statistics that match the given predicate.
	pub fn list_channel_stats_with_filter<F: FnMut(&&ChannelForwardingStats) -> bool>(
		&self, f: F,
	) -> Vec<ChannelForwardingStats> {
		self.runtime.block_on(self.forwarding_store.list_channel_stats_with_filter(f))
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl ForwardingAnalytics {
	/// Returns the configured forwarded payment tracking mode.
	pub fn tracking_mode(&self) -> ForwardedPaymentTrackingMode {
		self.config.forwarded_payment_tracking_mode
	}

	/// Retrieve the details of a specific forwarded payment using its opaque identifier.
	///
	/// The identifier is returned in [`ForwardedPaymentDetails::id`].
	pub fn payment(
		&self, forwarded_payment_id: &ForwardedPaymentId,
	) -> Result<Option<ForwardedPaymentDetails>, Error> {
		self.runtime.block_on(self.forwarding_store.payment(forwarded_payment_id))
	}

	/// Retrieves a page of forwarded payments from the underlying paginated store.
	pub fn list_payments(
		&self, page_token: Option<PageToken>,
	) -> Result<ForwardedPaymentDetailsPage, Error> {
		let ldk_page_token = page_token.as_ref().map(|token| maybe_deref(token).clone());
		let page = self.runtime.block_on(self.forwarding_store.list_payments(ldk_page_token))?;
		Ok(ForwardedPaymentDetailsPage {
			payments: page.objects,
			next_page_token: page.next_page_token.map(maybe_wrap),
		})
	}

	/// Retrieve the forwarding statistics for a specific channel.
	pub fn channel_stats(
		&self, channel_id: &ChannelId,
	) -> Result<Option<ChannelForwardingStats>, Error> {
		self.runtime.block_on(self.forwarding_store.channel_stats(channel_id))
	}

	/// Retrieves a page of channel forwarding statistics from the underlying paginated store.
	pub fn list_channel_stats(
		&self, page_token: Option<PageToken>,
	) -> Result<ChannelForwardingStatsPage, Error> {
		let ldk_page_token = page_token.as_ref().map(|token| maybe_deref(token).clone());
		let page =
			self.runtime.block_on(self.forwarding_store.list_channel_stats(ldk_page_token))?;
		Ok(ChannelForwardingStatsPage {
			stats: page.objects,
			next_page_token: page.next_page_token.map(maybe_wrap),
		})
	}

	/// Retrieves a page of channel pair forwarding statistics from the underlying paginated store.
	pub fn list_channel_pair_stats(
		&self, page_token: Option<PageToken>,
	) -> Result<ChannelPairForwardingStatsPage, Error> {
		let ldk_page_token = page_token.as_ref().map(|token| maybe_deref(token).clone());
		let page =
			self.runtime.block_on(self.forwarding_store.list_channel_pair_stats(ldk_page_token))?;
		Ok(ChannelPairForwardingStatsPage {
			stats: page.objects,
			next_page_token: page.next_page_token.map(maybe_wrap),
		})
	}
}
