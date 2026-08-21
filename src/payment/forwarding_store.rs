// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use lightning::events::HTLCLocator;
use lightning::impl_writeable_tlv_based;
use lightning::ln::types::ChannelId;
use lightning::util::logger::Logger as _;
use lightning::util::persist::PageToken;

use super::forwarding::{
	ChannelForwardingStats, ChannelPairForwardingStats, ChannelPairForwardingStatsId,
	ForwardedPaymentDetails, ForwardedPaymentId,
};
use crate::config::ForwardedPaymentTrackingMode;
use crate::data_store::{
	DataStore, DataStorePage, KeepAllEntries, KeepNoEntries, StorableObject, StorableObjectId,
	StorableObjectUpdate, UpdatableObject,
};
use crate::hex_utils;
use crate::io::{
	CHANNEL_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE,
	CHANNEL_PAIR_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE,
	FORWARDED_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	FORWARDED_PAYMENT_PERSISTENCE_PRIMARY_NAMESPACE,
	FORWARDED_PAYMENT_REPLAY_MARKER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_debug, log_error, Logger};
use crate::types::{
	ChannelForwardingStatsStore, ChannelPairForwardingStatsStore, DynStore, ForwardedPaymentStore,
};
use crate::{Error, UserChannelId};

pub(crate) const FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS: u64 = 60 * 60;

impl StorableObjectId for ForwardedPaymentId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		s.parse().ok()
	}
}

impl StorableObjectId for ChannelPairForwardingStatsId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		s.parse().ok()
	}
}

pub(crate) struct ForwardRecord<'a> {
	pub prev_htlcs: &'a [HTLCLocator],
	pub next_htlcs: &'a [HTLCLocator],
	pub total_fee_earned_msat: Option<u64>,
	pub skimmed_fee_msat: Option<u64>,
	pub claim_from_onchain_tx: bool,
	pub outbound_amount_forwarded_msat: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ForwardedPaymentReplayMarker {
	id: ForwardedPaymentId,
}

impl_writeable_tlv_based!(ForwardedPaymentReplayMarker, { (0, id, required) });

impl StorableObject for ForwardedPaymentReplayMarker {
	type Id = ForwardedPaymentId;

	fn id(&self) -> Self::Id {
		self.id
	}
}

type ForwardedPaymentReplayMarkerStore =
	DataStore<ForwardedPaymentReplayMarker, Arc<Logger>, KeepNoEntries>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredChannelForwardingStats {
	channel_id: ChannelId,
	counterparty_node_id: Option<PublicKey>,
	inbound_payments_forwarded: u64,
	outbound_payments_forwarded: u64,
	total_inbound_amount_msat: u64,
	total_outbound_amount_msat: u64,
	total_fee_earned_msat: Option<u64>,
	total_skimmed_fee_msat: u64,
	onchain_claims_count: u64,
	first_forwarded_at_timestamp: u64,
	last_forwarded_at_timestamp: u64,
	last_inbound_forward_id: Option<ForwardedPaymentId>,
	last_outbound_forward_id: Option<ForwardedPaymentId>,
}

impl_writeable_tlv_based!(StoredChannelForwardingStats, {
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
	(22, last_inbound_forward_id, option),
	(24, last_outbound_forward_id, option),
});

impl From<StoredChannelForwardingStats> for ChannelForwardingStats {
	fn from(stats: StoredChannelForwardingStats) -> Self {
		Self {
			channel_id: stats.channel_id,
			counterparty_node_id: stats.counterparty_node_id,
			inbound_payments_forwarded: stats.inbound_payments_forwarded,
			outbound_payments_forwarded: stats.outbound_payments_forwarded,
			total_inbound_amount_msat: stats.total_inbound_amount_msat,
			total_outbound_amount_msat: stats.total_outbound_amount_msat,
			total_fee_earned_msat: stats.total_fee_earned_msat,
			total_skimmed_fee_msat: stats.total_skimmed_fee_msat,
			onchain_claims_count: stats.onchain_claims_count,
			first_forwarded_at_timestamp: stats.first_forwarded_at_timestamp,
			last_forwarded_at_timestamp: stats.last_forwarded_at_timestamp,
		}
	}
}

impl From<ChannelForwardingStats> for StoredChannelForwardingStats {
	fn from(stats: ChannelForwardingStats) -> Self {
		Self {
			channel_id: stats.channel_id,
			counterparty_node_id: stats.counterparty_node_id,
			inbound_payments_forwarded: stats.inbound_payments_forwarded,
			outbound_payments_forwarded: stats.outbound_payments_forwarded,
			total_inbound_amount_msat: stats.total_inbound_amount_msat,
			total_outbound_amount_msat: stats.total_outbound_amount_msat,
			total_fee_earned_msat: stats.total_fee_earned_msat,
			total_skimmed_fee_msat: stats.total_skimmed_fee_msat,
			onchain_claims_count: stats.onchain_claims_count,
			first_forwarded_at_timestamp: stats.first_forwarded_at_timestamp,
			last_forwarded_at_timestamp: stats.last_forwarded_at_timestamp,
			last_inbound_forward_id: None,
			last_outbound_forward_id: None,
		}
	}
}

/// Owns all forwarding data stores and the operations that must keep them consistent.
pub(crate) struct ForwardingStore {
	details: ForwardedPaymentStore,
	replay_markers: ForwardedPaymentReplayMarkerStore,
	channel_stats: ChannelForwardingStatsStore,
	channel_pair_stats: ChannelPairForwardingStatsStore,
	tracking_mode: ForwardedPaymentTrackingMode,
	logger: Arc<Logger>,
}

impl ForwardingStore {
	pub(crate) fn new(
		channel_stats: Vec<StoredChannelForwardingStats>,
		tracking_mode: ForwardedPaymentTrackingMode, kv_store: Arc<DynStore>, logger: Arc<Logger>,
	) -> Self {
		Self {
			details: ForwardedPaymentStore::new(
				Vec::new(),
				KeepNoEntries,
				FORWARDED_PAYMENT_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				FORWARDED_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			),
			replay_markers: DataStore::new(
				Vec::new(),
				KeepNoEntries,
				FORWARDED_PAYMENT_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				FORWARDED_PAYMENT_REPLAY_MARKER_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			),
			channel_stats: ChannelForwardingStatsStore::new(
				channel_stats,
				KeepAllEntries,
				FORWARDED_PAYMENT_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				CHANNEL_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			),
			channel_pair_stats: ChannelPairForwardingStatsStore::new(
				Vec::new(),
				KeepNoEntries,
				FORWARDED_PAYMENT_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				CHANNEL_PAIR_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				kv_store,
				Arc::clone(&logger),
			),
			tracking_mode,
			logger,
		}
	}

	pub(crate) async fn record_forward(&self, forward: ForwardRecord<'_>) -> Result<(), Error> {
		let ([prev_htlc], [next_htlc], Some(fee_earned_msat), Some(prev_htlc_id)) = (
			forward.prev_htlcs,
			forward.next_htlcs,
			forward.total_fee_earned_msat,
			forward.prev_htlcs.first().and_then(|htlc| htlc.htlc_id),
		) else {
			log_debug!(
				self.logger,
				"Skipping forwarding payment tracking for forward with {} inbound and {} outbound HTLC(s), because tracking requires a single HTLC in each direction, a known fee, and an incoming HTLC ID",
				forward.prev_htlcs.len(),
				forward.next_htlcs.len()
			);
			return Ok(());
		};

		let forward_id = forwarded_payment_id(&prev_htlc.channel_id, prev_htlc_id);
		if self.replay_markers.contains_key(&forward_id).await.map_err(|e| {
			log_error!(self.logger, "Failed to check forwarded payment replay marker: {e}");
			e
		})? {
			return Ok(());
		}
		let details_id = matches!(self.tracking_mode, ForwardedPaymentTrackingMode::Detailed)
			.then_some(forward_id);
		let forwarded_at_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("current time should not be earlier than the Unix epoch")
			.as_secs();
		let inbound_amount_msat =
			forward.outbound_amount_forwarded_msat.saturating_add(fee_earned_msat);

		// Store the stable forward ID with each directional update. If a later write fails, a replay
		// can finish the remaining updates without applying a successful update twice.
		let mut outbound_stats = StoredChannelForwardingStats::from(ChannelForwardingStats {
			channel_id: next_htlc.channel_id,
			counterparty_node_id: next_htlc.node_id,
			inbound_payments_forwarded: 0,
			outbound_payments_forwarded: 1,
			total_inbound_amount_msat: 0,
			total_outbound_amount_msat: forward.outbound_amount_forwarded_msat,
			total_fee_earned_msat: Some(0),
			total_skimmed_fee_msat: 0,
			onchain_claims_count: u64::from(forward.claim_from_onchain_tx),
			first_forwarded_at_timestamp: forwarded_at_timestamp,
			last_forwarded_at_timestamp: forwarded_at_timestamp,
		});
		outbound_stats.last_outbound_forward_id = Some(forward_id);
		self.channel_stats.insert_or_update(outbound_stats).await.map_err(|e| {
			log_error!(self.logger, "Failed to update outbound channel forwarding stats: {e}");
			e
		})?;

		let mut inbound_stats = StoredChannelForwardingStats::from(ChannelForwardingStats {
			channel_id: prev_htlc.channel_id,
			counterparty_node_id: prev_htlc.node_id,
			inbound_payments_forwarded: 1,
			outbound_payments_forwarded: 0,
			total_inbound_amount_msat: inbound_amount_msat,
			total_outbound_amount_msat: 0,
			total_fee_earned_msat: Some(fee_earned_msat),
			total_skimmed_fee_msat: forward.skimmed_fee_msat.unwrap_or(0),
			onchain_claims_count: 0,
			first_forwarded_at_timestamp: forwarded_at_timestamp,
			last_forwarded_at_timestamp: forwarded_at_timestamp,
		});
		inbound_stats.last_inbound_forward_id = Some(forward_id);
		self.channel_stats.insert_or_update(inbound_stats).await.map_err(|e| {
			log_error!(self.logger, "Failed to update inbound channel forwarding stats: {e}");
			e
		})?;

		if let Some(id) = details_id {
			self.details
				.insert(ForwardedPaymentDetails {
					id,
					prev_channel_id: prev_htlc.channel_id,
					next_channel_id: next_htlc.channel_id,
					prev_user_channel_id: prev_htlc.user_channel_id.map(UserChannelId),
					next_user_channel_id: next_htlc.user_channel_id.map(UserChannelId),
					prev_node_id: prev_htlc.node_id,
					next_node_id: next_htlc.node_id,
					inbound_amount_forwarded_msat: Some(inbound_amount_msat),
					total_fee_earned_msat: Some(fee_earned_msat),
					skimmed_fee_msat: forward.skimmed_fee_msat,
					claim_from_onchain_tx: forward.claim_from_onchain_tx,
					outbound_amount_forwarded_msat: Some(forward.outbound_amount_forwarded_msat),
					forwarded_at_timestamp,
				})
				.await
				.map_err(|e| {
					log_error!(self.logger, "Failed to store forwarded payment: {e}");
					e
				})?;
		}

		// Keep this marker after the event is handled. LDK can replay an older event after later
		// events have replaced the directional retry tokens, and it provides no callback after its
		// handled-event state is durable.
		self.replay_markers.insert(ForwardedPaymentReplayMarker { id: forward_id }).await.map_err(
			|e| {
				log_error!(self.logger, "Failed to store forwarded payment replay marker: {e}");
				e
			},
		)?;

		Ok(())
	}

	pub(crate) async fn payment(
		&self, id: &ForwardedPaymentId,
	) -> Result<Option<ForwardedPaymentDetails>, Error> {
		self.details.get(id).await
	}

	pub(crate) async fn list_payments(
		&self, page_token: Option<PageToken>,
	) -> Result<DataStorePage<ForwardedPaymentDetails>, Error> {
		self.details.list_page(page_token).await
	}

	pub(crate) async fn channel_stats(
		&self, id: &ChannelId,
	) -> Result<Option<ChannelForwardingStats>, Error> {
		Ok(self.channel_stats.get(id).await?.map(Into::into))
	}

	pub(crate) async fn list_channel_stats(
		&self, page_token: Option<PageToken>,
	) -> Result<DataStorePage<ChannelForwardingStats>, Error> {
		let page = self.channel_stats.list_page(page_token).await?;
		Ok(DataStorePage {
			objects: page.objects.into_iter().map(Into::into).collect(),
			next_page_token: page.next_page_token,
		})
	}

	pub(crate) async fn list_channel_stats_with_filter<
		F: FnMut(&&ChannelForwardingStats) -> bool,
	>(
		&self, f: F,
	) -> Vec<ChannelForwardingStats> {
		let stats: Vec<_> =
			self.channel_stats.list_filter(|_| true).await.into_iter().map(Into::into).collect();
		let mut f = f;
		stats.into_iter().filter(|stats| f(&stats)).collect()
	}

	pub(crate) async fn list_channel_pair_stats(
		&self, page_token: Option<PageToken>,
	) -> Result<DataStorePage<ChannelPairForwardingStats>, Error> {
		self.channel_pair_stats.list_page(page_token).await
	}

	pub(crate) async fn aggregate_expired(&self, retention_secs: u64) -> Result<(u64, u64), Error> {
		aggregate_expired_forwarded_payments(
			&self.details,
			&self.replay_markers,
			&self.channel_pair_stats,
			retention_secs,
			&self.logger,
		)
		.await
	}
}

fn forwarded_payment_id(channel_id: &ChannelId, htlc_id: u64) -> ForwardedPaymentId {
	let mut bytes = [0u8; 40];
	bytes[..32].copy_from_slice(&channel_id.0);
	bytes[32..].copy_from_slice(&htlc_id.to_be_bytes());
	ForwardedPaymentId(sha256::Hash::hash(&bytes).to_byte_array())
}

impl StorableObject for ForwardedPaymentDetails {
	type Id = ForwardedPaymentId;

	fn id(&self) -> Self::Id {
		self.id
	}
}

fn channel_pair_stats_id(
	prev: &ChannelId, next: &ChannelId, bucket_start_timestamp: u64,
) -> ChannelPairForwardingStatsId {
	// Hash the ordered channel pair and bucket start. The bucket size is fixed and therefore does
	// not need to be part of the ID. Keeping the channels ordered preserves forwarding direction,
	// unlike combining them with XOR.
	let mut bytes = [0u8; 72];
	bytes[0..32].copy_from_slice(&prev.0);
	bytes[32..64].copy_from_slice(&next.0);
	bytes[64..72].copy_from_slice(&bucket_start_timestamp.to_be_bytes());
	ChannelPairForwardingStatsId(sha256::Hash::hash(&bytes).to_byte_array())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelForwardingStatsUpdate {
	pub channel_id: ChannelId,
	pub counterparty_node_id: Option<PublicKey>,
	pub inbound_payments_increment: u64,
	pub outbound_payments_increment: u64,
	pub inbound_amount_increment_msat: u64,
	pub outbound_amount_increment_msat: u64,
	pub fee_earned_increment_msat: Option<u64>,
	pub skimmed_fee_increment_msat: u64,
	pub onchain_claims_increment: u64,
	pub timestamp: u64,
	pub last_inbound_forward_id: Option<ForwardedPaymentId>,
	pub last_outbound_forward_id: Option<ForwardedPaymentId>,
}

impl StorableObjectUpdate<StoredChannelForwardingStats> for ChannelForwardingStatsUpdate {
	fn id(&self) -> ChannelId {
		self.channel_id
	}
}

impl StorableObjectId for ChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		let bytes = hex_utils::to_vec(s)?;
		Some(ChannelId(bytes.try_into().ok()?))
	}
}

impl StorableObject for StoredChannelForwardingStats {
	type Id = ChannelId;

	fn id(&self) -> Self::Id {
		self.channel_id
	}
}

impl UpdatableObject for StoredChannelForwardingStats {
	type Update = ChannelForwardingStatsUpdate;

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(self.channel_id, update.channel_id);
		let mut updated = false;
		if self.counterparty_node_id.is_none() && update.counterparty_node_id.is_some() {
			self.counterparty_node_id = update.counterparty_node_id;
			updated = true;
		}
		let apply_inbound = update.inbound_payments_increment > 0
			&& (update.last_inbound_forward_id.is_none()
				|| update.last_inbound_forward_id != self.last_inbound_forward_id);
		let apply_outbound = update.outbound_payments_increment > 0
			&& (update.last_outbound_forward_id.is_none()
				|| update.last_outbound_forward_id != self.last_outbound_forward_id);
		if apply_inbound {
			self.inbound_payments_forwarded += update.inbound_payments_increment;
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			match (self.total_fee_earned_msat.as_mut(), update.fee_earned_increment_msat) {
				(Some(total), Some(increment)) => *total += increment,
				(Some(_), None) => self.total_fee_earned_msat = None,
				_ => {},
			}
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			self.last_inbound_forward_id = update.last_inbound_forward_id;
			updated = true;
		}
		if apply_outbound {
			self.outbound_payments_forwarded += update.outbound_payments_increment;
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			self.onchain_claims_count += update.onchain_claims_increment;
			self.last_outbound_forward_id = update.last_outbound_forward_id;
			updated = true;
		}
		if updated {
			self.first_forwarded_at_timestamp =
				self.first_forwarded_at_timestamp.min(update.timestamp);
			self.last_forwarded_at_timestamp =
				self.last_forwarded_at_timestamp.max(update.timestamp);
		}
		updated
	}

	fn to_update(&self) -> Self::Update {
		ChannelForwardingStatsUpdate {
			channel_id: self.channel_id,
			counterparty_node_id: self.counterparty_node_id,
			inbound_payments_increment: self.inbound_payments_forwarded,
			outbound_payments_increment: self.outbound_payments_forwarded,
			inbound_amount_increment_msat: self.total_inbound_amount_msat,
			outbound_amount_increment_msat: self.total_outbound_amount_msat,
			fee_earned_increment_msat: self.total_fee_earned_msat,
			skimmed_fee_increment_msat: self.total_skimmed_fee_msat,
			onchain_claims_increment: self.onchain_claims_count,
			timestamp: self.last_forwarded_at_timestamp,
			last_inbound_forward_id: self.last_inbound_forward_id,
			last_outbound_forward_id: self.last_outbound_forward_id,
		}
	}
}

impl StorableObject for ChannelPairForwardingStats {
	type Id = ChannelPairForwardingStatsId;

	fn id(&self) -> Self::Id {
		self.id
	}
}

fn seconds_until_next_forwarding_aggregation(now_timestamp: u64, bucket_size_secs: u64) -> u64 {
	debug_assert!(bucket_size_secs > 0);
	bucket_size_secs - (now_timestamp % bucket_size_secs)
}

async fn aggregate_forwarded_payments_and_log(
	forwarding_store: &ForwardingStore, retention_secs: u64,
) {
	match forwarding_store.aggregate_expired(retention_secs).await {
		Ok((pair_count, payment_count)) if pair_count > 0 => {
			log_debug!(
				forwarding_store.logger,
				"Aggregated {} forwarded payments into {} channel pair buckets",
				payment_count,
				pair_count
			);
		},
		Ok((0, payment_count)) if payment_count > 0 => {
			log_debug!(
				forwarding_store.logger,
				"Removed {} forwarded payment details from previously aggregated buckets",
				payment_count
			);
		},
		Err(e) => {
			log_error!(forwarding_store.logger, "Forwarded payment aggregation failed: {}", e)
		},
		_ => {},
	}
}

pub(crate) async fn run_forwarded_payment_aggregation(
	mut stop_receiver: tokio::sync::watch::Receiver<()>, forwarding_store: Arc<ForwardingStore>,
	retention_secs: u64,
) {
	if retention_secs == 0 {
		match forwarding_store.details.is_empty().await {
			Ok(true) => return,
			Ok(false) => {},
			Err(e) => log_error!(
				forwarding_store.logger,
				"Failed to check forwarded payment store: {}",
				e
			),
		}
	}

	aggregate_forwarded_payments_and_log(&forwarding_store, retention_secs).await;

	if retention_secs == 0 {
		match forwarding_store.details.is_empty().await {
			Ok(true) => return,
			Ok(false) => {},
			Err(e) => log_error!(
				forwarding_store.logger,
				"Failed to check forwarded payment store: {}",
				e
			),
		}
	}

	let period = Duration::from_secs(FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS);
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	let secs_until_next_bucket = seconds_until_next_forwarding_aggregation(
		now,
		FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS,
	);
	let first_tick = tokio::time::Instant::now() + Duration::from_secs(secs_until_next_bucket);
	let mut interval = tokio::time::interval_at(first_tick, period);
	interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	loop {
		tokio::select! {
			_ = stop_receiver.changed() => break,
			_ = interval.tick() => {
				aggregate_forwarded_payments_and_log(&forwarding_store, retention_secs).await;
				if retention_secs == 0 {
					match forwarding_store.details.is_empty().await {
						Ok(true) => break,
						Ok(false) => {},
						Err(e) => log_error!(forwarding_store.logger, "Failed to check forwarded payment store: {}", e),
					}
				}
			}
		}
	}
}

/// Aggregate forwarded payments older than the configured retention period into fixed-width
/// channel-pair statistics buckets.
async fn aggregate_expired_forwarded_payments(
	forwarded_payment_store: &ForwardedPaymentStore,
	replay_marker_store: &ForwardedPaymentReplayMarkerStore,
	channel_pair_stats_store: &ChannelPairForwardingStatsStore, retention_secs: u64,
	logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	aggregate_expired_forwarded_payments_at(
		forwarded_payment_store,
		replay_marker_store,
		channel_pair_stats_store,
		FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS,
		retention_secs,
		now,
		logger,
	)
	.await
}

async fn aggregate_expired_forwarded_payments_at(
	forwarded_payment_store: &ForwardedPaymentStore,
	replay_marker_store: &ForwardedPaymentReplayMarkerStore,
	channel_pair_stats_store: &ChannelPairForwardingStatsStore, bucket_size_secs: u64,
	retention_secs: u64, now: u64, logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	if bucket_size_secs == 0 {
		return Ok((0, 0));
	}
	let retention_cutoff = now.saturating_sub(retention_secs);
	let oldest_retained_bucket_start =
		(retention_cutoff / bucket_size_secs).saturating_mul(bucket_size_secs);

	// Retain details for at least the configured period. We only aggregate complete buckets, so
	// details may remain for up to one additional bucket width. Once an older bucket is persisted,
	// it is a durable commit marker: retries can skip updating its totals and finish deleting detail
	// records left behind by an interrupted cleanup.
	//
	// The scan below runs unlocked, so that a pass over a large store never stalls event handling.
	// What keeps a record out of a bucket we have already closed is that width of slack, not any
	// locking: a record would have to be stamped and then take longer than a whole bucket to be
	// written. Event handling is serialized, so long before that the node has stopped forwarding
	// anything at all. Insertions interleaving between pages are therefore fine, and a straggler we
	// miss is found by the next pass, whose bucket marker exists by then, so it is deleted rather
	// than counted twice.
	let mut bucket_groups: HashMap<(ChannelId, ChannelId, u64), Vec<ForwardedPaymentDetails>> =
		HashMap::new();
	let mut incomplete_buckets = HashSet::new();
	let mut page_token = None;
	loop {
		let page = forwarded_payment_store.list_page(page_token).await?;
		for payment in page.objects {
			if payment.forwarded_at_timestamp >= oldest_retained_bucket_start {
				continue;
			}
			let bucket_start =
				(payment.forwarded_at_timestamp / bucket_size_secs) * bucket_size_secs;
			let bucket_key = (payment.prev_channel_id, payment.next_channel_id, bucket_start);
			// The marker confirms that every analytics write for this forward succeeded. Wait for
			// the whole bucket if any marker is absent, so a later replay cannot add a detail after
			// the bucket totals are committed.
			if !replay_marker_store.contains_key(&payment.id).await? {
				incomplete_buckets.insert(bucket_key);
				bucket_groups.remove(&bucket_key);
				continue;
			}
			if incomplete_buckets.contains(&bucket_key) {
				continue;
			}
			bucket_groups.entry(bucket_key).or_default().push(payment);
		}
		let Some(next_page_token) = page.next_page_token else {
			break;
		};
		page_token = Some(next_page_token);
	}
	if bucket_groups.is_empty() {
		log_debug!(logger, "No forwarded payments in closed aggregation buckets found");
		return Ok((0, 0));
	}

	let mut aggregated_bucket_count = 0u64;
	let mut payment_ids_to_remove = Vec::new();
	for ((prev_channel_id, next_channel_id, bucket_start), payments) in bucket_groups {
		let pair_id = channel_pair_stats_id(&prev_channel_id, &next_channel_id, bucket_start);

		if channel_pair_stats_store.get(&pair_id).await?.is_none() {
			let mut total_inbound_amount_msat = 0u64;
			let mut total_outbound_amount_msat = 0u64;
			let mut total_fee_earned_msat = Some(0u64);
			let mut total_skimmed_fee_msat = 0u64;
			let mut onchain_claims_count = 0u64;
			let mut first_timestamp = u64::MAX;
			let mut last_timestamp = 0u64;

			for payment in &payments {
				let outbound = payment.outbound_amount_forwarded_msat.unwrap_or(0);
				let fee = payment.total_fee_earned_msat;
				let skimmed = payment.skimmed_fee_msat.unwrap_or(0);
				let inbound = payment
					.inbound_amount_forwarded_msat
					.unwrap_or_else(|| outbound.saturating_add(fee.unwrap_or(0)));
				total_inbound_amount_msat = total_inbound_amount_msat.saturating_add(inbound);
				total_outbound_amount_msat = total_outbound_amount_msat.saturating_add(outbound);
				total_fee_earned_msat = match (total_fee_earned_msat, fee) {
					(Some(total), Some(fee)) => Some(total.saturating_add(fee)),
					_ => None,
				};
				total_skimmed_fee_msat = total_skimmed_fee_msat.saturating_add(skimmed);
				if payment.claim_from_onchain_tx {
					onchain_claims_count += 1;
				}
				first_timestamp = first_timestamp.min(payment.forwarded_at_timestamp);
				last_timestamp = last_timestamp.max(payment.forwarded_at_timestamp);
			}

			let payment_count = payments.len() as u64;
			let prev_node_id = payments.iter().find_map(|payment| payment.prev_node_id);
			let next_node_id = payments.iter().find_map(|payment| payment.next_node_id);
			let stats = ChannelPairForwardingStats {
				id: pair_id,
				prev_channel_id,
				next_channel_id,
				bucket_start_timestamp: bucket_start,
				bucket_size_secs,
				prev_node_id,
				next_node_id,
				payment_count,
				total_inbound_amount_msat,
				total_outbound_amount_msat,
				total_fee_earned_msat,
				total_skimmed_fee_msat,
				onchain_claims_count,
				avg_fee_msat: total_fee_earned_msat.map(|total| total / payment_count),
				avg_inbound_amount_msat: total_inbound_amount_msat / payment_count,
				first_forwarded_at_timestamp: first_timestamp,
				last_forwarded_at_timestamp: last_timestamp,
				aggregated_at_timestamp: now,
			};

			channel_pair_stats_store.insert(stats).await.map_err(|e| {
				log_error!(
					logger,
					"Failed to insert channel pair stats bucket for {pair_id:?}: {e}"
				);
				e
			})?;
			aggregated_bucket_count += 1;
		}

		payment_ids_to_remove.extend(payments.into_iter().map(|payment| payment.id()));
	}

	// Removals acquire the mutation lock themselves, one at a time.
	let mut removed_payment_count = 0u64;
	for payment_id in payment_ids_to_remove {
		forwarded_payment_store.remove(&payment_id).await.map_err(|e| {
			log_error!(logger, "Failed to remove forwarded payment {:?}: {}", payment_id, e);
			e
		})?;
		removed_payment_count += 1;
	}

	Ok((aggregated_bucket_count, removed_payment_count))
}

/// Aggregates multiple channel-pair statistics buckets into cumulative totals.
///
/// The result is computed here rather than read from the node, so the four fields that describe a
/// stored bucket do not carry their usual meaning:
///
/// - [`bucket_start_timestamp`] and [`bucket_size_secs`] span the earliest input bucket start
///   through the latest input bucket end, gaps included, instead of describing one fixed-width
///   bucket. A gap is a period in which this channel pair forwarded nothing, so no bucket was
///   stored for it.
/// - `id` is the key of the earliest input bucket, because that is the bucket the span starts at.
///   It does not identify this result, and writing the result back under it would overwrite that
///   real bucket with a differently-shaped one.
/// - [`aggregated_at_timestamp`] is when this call ran, not when any input was aggregated.
///
/// Returns `None` if `buckets` is empty or contains statistics for different channel pairs.
///
/// [`bucket_start_timestamp`]: crate::payment::ChannelPairForwardingStats::bucket_start_timestamp
/// [`bucket_size_secs`]: crate::payment::ChannelPairForwardingStats::bucket_size_secs
/// [`aggregated_at_timestamp`]: crate::payment::ChannelPairForwardingStats::aggregated_at_timestamp
pub fn aggregate_channel_pair_stats(
	buckets: &[ChannelPairForwardingStats],
) -> Option<ChannelPairForwardingStats> {
	let first = buckets.first()?;
	for bucket in &buckets[1..] {
		if bucket.prev_channel_id != first.prev_channel_id
			|| bucket.next_channel_id != first.next_channel_id
		{
			return None;
		}
	}

	let mut payment_count = 0u64;
	let mut total_inbound_amount_msat = 0u64;
	let mut total_outbound_amount_msat = 0u64;
	let mut total_fee_earned_msat = Some(0u64);
	let mut total_skimmed_fee_msat = 0u64;
	let mut onchain_claims_count = 0u64;
	let mut first_forwarded_at_timestamp = u64::MAX;
	let mut last_forwarded_at_timestamp = 0u64;
	let mut earliest_bucket_start = u64::MAX;
	let mut latest_bucket_end = 0u64;
	let mut prev_node_id = None;
	let mut next_node_id = None;
	for bucket in buckets {
		payment_count = payment_count.saturating_add(bucket.payment_count);
		total_inbound_amount_msat =
			total_inbound_amount_msat.saturating_add(bucket.total_inbound_amount_msat);
		total_outbound_amount_msat =
			total_outbound_amount_msat.saturating_add(bucket.total_outbound_amount_msat);
		total_fee_earned_msat = match (total_fee_earned_msat, bucket.total_fee_earned_msat) {
			(Some(total), Some(fee)) => Some(total.saturating_add(fee)),
			_ => None,
		};
		total_skimmed_fee_msat =
			total_skimmed_fee_msat.saturating_add(bucket.total_skimmed_fee_msat);
		onchain_claims_count = onchain_claims_count.saturating_add(bucket.onchain_claims_count);
		first_forwarded_at_timestamp =
			first_forwarded_at_timestamp.min(bucket.first_forwarded_at_timestamp);
		last_forwarded_at_timestamp =
			last_forwarded_at_timestamp.max(bucket.last_forwarded_at_timestamp);
		earliest_bucket_start = earliest_bucket_start.min(bucket.bucket_start_timestamp);
		latest_bucket_end = latest_bucket_end
			.max(bucket.bucket_start_timestamp.saturating_add(bucket.bucket_size_secs));
		if prev_node_id.is_none() {
			prev_node_id = bucket.prev_node_id;
		}
		if next_node_id.is_none() {
			next_node_id = bucket.next_node_id;
		}
	}
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	Some(ChannelPairForwardingStats {
		id: channel_pair_stats_id(
			&first.prev_channel_id,
			&first.next_channel_id,
			earliest_bucket_start,
		),
		prev_channel_id: first.prev_channel_id,
		next_channel_id: first.next_channel_id,
		bucket_start_timestamp: earliest_bucket_start,
		bucket_size_secs: latest_bucket_end.saturating_sub(earliest_bucket_start),
		prev_node_id,
		next_node_id,
		payment_count,
		total_inbound_amount_msat,
		total_outbound_amount_msat,
		total_fee_earned_msat,
		total_skimmed_fee_msat,
		onchain_claims_count,
		avg_fee_msat: if payment_count > 0 {
			total_fee_earned_msat.map(|total| total / payment_count)
		} else {
			None
		},
		avg_inbound_amount_msat: if payment_count > 0 {
			total_inbound_amount_msat / payment_count
		} else {
			0
		},
		first_forwarded_at_timestamp,
		last_forwarded_at_timestamp,
		aggregated_at_timestamp: now,
	})
}

#[cfg(test)]
mod forwarding_stats_tests {
	use std::str::FromStr;

	use lightning::util::persist::{
		KVStore, KVSTORE_NAMESPACE_KEY_ALPHABET, KVSTORE_NAMESPACE_KEY_MAX_LEN,
	};
	use lightning::util::ser::{Readable, Writeable};

	use super::*;
	use crate::data_store::KeepNoEntries;
	#[cfg(feature = "storage-sqlite")]
	use crate::io::sqlite_store::SqliteStore;
	#[cfg(feature = "storage-sqlite")]
	use crate::io::test_utils::random_storage_path;
	use crate::io::test_utils::InMemoryStore;
	use crate::types::{DynStore, DynStoreWrapper};

	type TestForwardedPaymentStore = ForwardedPaymentStore;
	type TestReplayMarkerStore = ForwardedPaymentReplayMarkerStore;
	type TestChannelPairStatsStore = ChannelPairForwardingStatsStore;

	fn test_stores(
	) -> (TestForwardedPaymentStore, TestReplayMarkerStore, TestChannelPairStatsStore, Arc<Logger>)
	{
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger, _) =
			test_stores_with_kv();
		(forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger)
	}

	fn test_stores_with_kv() -> (
		TestForwardedPaymentStore,
		TestReplayMarkerStore,
		TestChannelPairStatsStore,
		Arc<Logger>,
		Arc<DynStore>,
	) {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let primary_namespace = "test_forwarded_payments";
		let forwarded_payment_store = ForwardedPaymentStore::new(
			Vec::new(),
			KeepNoEntries,
			primary_namespace.to_string(),
			"details".to_string(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		let replay_marker_store = ForwardedPaymentReplayMarkerStore::new(
			Vec::new(),
			KeepNoEntries,
			primary_namespace.to_string(),
			"replay_markers".to_string(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		let channel_pair_stats_store = ChannelPairForwardingStatsStore::new(
			Vec::new(),
			KeepNoEntries,
			primary_namespace.to_string(),
			"pair_stats".to_string(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		(forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger, kv_store)
	}

	async fn insert_completed_payment(
		forwarded_payment_store: &TestForwardedPaymentStore,
		replay_marker_store: &TestReplayMarkerStore, payment: ForwardedPaymentDetails,
	) {
		let id = payment.id();
		forwarded_payment_store.insert(payment).await.unwrap();
		replay_marker_store.insert(ForwardedPaymentReplayMarker { id }).await.unwrap();
	}

	fn forwarded_payment(
		id: u8, forwarded_at_timestamp: u64, inbound_amount_msat: u64, outbound_amount_msat: u64,
		fee_msat: u64,
	) -> ForwardedPaymentDetails {
		ForwardedPaymentDetails {
			id: ForwardedPaymentId([id; 32]),
			prev_channel_id: ChannelId([1; 32]),
			next_channel_id: ChannelId([2; 32]),
			prev_user_channel_id: None,
			next_user_channel_id: None,
			prev_node_id: None,
			next_node_id: None,
			inbound_amount_forwarded_msat: Some(inbound_amount_msat),
			total_fee_earned_msat: Some(fee_msat),
			skimmed_fee_msat: Some(0),
			claim_from_onchain_tx: false,
			outbound_amount_forwarded_msat: Some(outbound_amount_msat),
			forwarded_at_timestamp,
		}
	}

	fn channel_pair_stats(
		bucket_start_timestamp: u64, bucket_size_secs: u64, aggregated_at_timestamp: u64,
		payment_count: u64, total_inbound_amount_msat: u64, total_outbound_amount_msat: u64,
		total_fee_earned_msat: u64, first_forwarded_at_timestamp: u64,
		last_forwarded_at_timestamp: u64,
	) -> ChannelPairForwardingStats {
		let prev_channel_id = ChannelId([1; 32]);
		let next_channel_id = ChannelId([2; 32]);
		ChannelPairForwardingStats {
			id: channel_pair_stats_id(&prev_channel_id, &next_channel_id, bucket_start_timestamp),
			prev_channel_id,
			next_channel_id,
			bucket_start_timestamp,
			bucket_size_secs,
			prev_node_id: None,
			next_node_id: None,
			payment_count,
			total_inbound_amount_msat,
			total_outbound_amount_msat,
			total_fee_earned_msat: Some(total_fee_earned_msat),
			total_skimmed_fee_msat: 0,
			onchain_claims_count: 0,
			avg_fee_msat: Some(total_fee_earned_msat / payment_count),
			avg_inbound_amount_msat: total_inbound_amount_msat / payment_count,
			first_forwarded_at_timestamp,
			last_forwarded_at_timestamp,
			aggregated_at_timestamp,
		}
	}

	#[test]
	fn channel_pair_persistence_key_fits_kvstore_limit() {
		let id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 42);
		let other_id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 43);
		let reversed_id = channel_pair_stats_id(&ChannelId([2; 32]), &ChannelId([1; 32]), 42);

		let key = id.encode_to_hex_str();
		assert_eq!(key.len(), 64);
		assert!(key.len() <= KVSTORE_NAMESPACE_KEY_MAX_LEN);
		assert!(key.chars().all(|c| KVSTORE_NAMESPACE_KEY_ALPHABET.contains(c)));
		assert_ne!(key, other_id.encode_to_hex_str());
		assert_ne!(key, reversed_id.encode_to_hex_str());
		assert_eq!(ChannelPairForwardingStatsId::from_str(&key), Ok(id));
		assert!(ChannelPairForwardingStatsId::decode_from_hex_str("00").is_none());
		assert_eq!(ChannelPairForwardingStatsId::from_str(&key.to_uppercase()), Ok(id));
	}

	#[tokio::test]
	async fn aggregation_retains_current_and_previous_buckets() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let closed_bucket_payment = forwarded_payment(1, 899, 110, 100, 10);
		let partial_bucket_payment = forwarded_payment(2, 939, 220, 200, 20);
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			closed_bucket_payment.clone(),
		)
		.await;
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			partial_bucket_payment.clone(),
		)
		.await;

		// At timestamp 1,000, the current bucket starts at 960 and the previous bucket starts at
		// 900. Only payments older than the previous bucket are aggregated.
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		assert!(forwarded_payment_store.get(&closed_bucket_payment.id()).await.unwrap().is_none());
		assert_eq!(
			forwarded_payment_store.get(&partial_bucket_payment.id()).await.unwrap(),
			Some(partial_bucket_payment)
		);
		let bucket_id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 840);
		let stats = channel_pair_stats_store.get(&bucket_id).await.unwrap().unwrap();
		assert_eq!(stats.bucket_size_secs, 60);
		assert_eq!(stats.payment_count, 1);
		assert_eq!(stats.total_inbound_amount_msat, 110);
		assert_eq!(stats.total_outbound_amount_msat, 100);
		assert_eq!(stats.total_fee_earned_msat, Some(10));
		assert_eq!(stats.aggregated_at_timestamp, 1_000);
	}

	#[test]
	fn aggregation_schedule_aligns_to_bucket_closure() {
		assert_eq!(seconds_until_next_forwarding_aggregation(120, 60), 60);
		assert_eq!(seconds_until_next_forwarding_aggregation(121, 60), 59);
		assert_eq!(seconds_until_next_forwarding_aggregation(179, 60), 1);
	}

	#[tokio::test]
	async fn aggregation_waits_for_forward_replay_marker() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let payment = forwarded_payment(1, 850, 110, 100, 10);
		let completed_payment = forwarded_payment(2, 851, 220, 200, 20);
		forwarded_payment_store.insert(payment.clone()).await.unwrap();
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			completed_payment.clone(),
		)
		.await;

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((0, 0))
		);
		assert_eq!(
			forwarded_payment_store.get(&payment.id()).await.unwrap(),
			Some(payment.clone())
		);
		assert_eq!(
			forwarded_payment_store.get(&completed_payment.id()).await.unwrap(),
			Some(completed_payment)
		);

		replay_marker_store
			.insert(ForwardedPaymentReplayMarker { id: payment.id() })
			.await
			.unwrap();
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((1, 2))
		);
		assert!(forwarded_payment_store.get(&payment.id()).await.unwrap().is_none());
		let bucket_id =
			channel_pair_stats_id(&payment.prev_channel_id, &payment.next_channel_id, 840);
		assert_eq!(
			channel_pair_stats_store.get(&bucket_id).await.unwrap().unwrap().payment_count,
			2
		);
	}

	#[tokio::test]
	async fn background_aggregation_runs_immediately() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let forwarding_store = Arc::new(ForwardingStore::new(
			Vec::new(),
			ForwardedPaymentTrackingMode::Stats,
			kv_store,
			Arc::clone(&logger),
		));
		let payment = forwarded_payment(1, 1, 110, 100, 10);
		forwarding_store.details.insert(payment.clone()).await.unwrap();
		forwarding_store
			.replay_markers
			.insert(ForwardedPaymentReplayMarker { id: payment.id() })
			.await
			.unwrap();
		let (_stop_sender, stop_receiver) = tokio::sync::watch::channel(());

		tokio::time::timeout(
			Duration::from_secs(1),
			run_forwarded_payment_aggregation(stop_receiver, Arc::clone(&forwarding_store), 0),
		)
		.await
		.unwrap();

		assert!(forwarding_store.details.is_empty().await.unwrap());
		let bucket_id =
			channel_pair_stats_id(&payment.prev_channel_id, &payment.next_channel_id, 0);
		assert_eq!(
			forwarding_store
				.channel_pair_stats
				.get(&bucket_id)
				.await
				.unwrap()
				.unwrap()
				.payment_count,
			1
		);
	}

	#[tokio::test]
	async fn forwarding_store_records_details_and_channel_stats() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let forwarding_store = ForwardingStore::new(
			Vec::new(),
			ForwardedPaymentTrackingMode::Detailed,
			kv_store,
			logger,
		);
		let prev_channel_id = ChannelId([1; 32]);
		let next_channel_id = ChannelId([2; 32]);
		let prev_htlcs = [HTLCLocator {
			channel_id: prev_channel_id,
			htlc_id: Some(7),
			amount_msat: Some(110),
			user_channel_id: Some(3),
			node_id: None,
		}];
		let next_htlcs = [HTLCLocator {
			channel_id: next_channel_id,
			htlc_id: Some(8),
			amount_msat: Some(100),
			user_channel_id: Some(4),
			node_id: None,
		}];
		let details_id = forwarded_payment_id(&prev_channel_id, 7);

		let forward = || ForwardRecord {
			prev_htlcs: &prev_htlcs,
			next_htlcs: &next_htlcs,
			total_fee_earned_msat: Some(10),
			skimmed_fee_msat: Some(2),
			claim_from_onchain_tx: true,
			outbound_amount_forwarded_msat: 100,
		};
		forwarding_store.record_forward(forward()).await.unwrap();
		forwarding_store.record_forward(forward()).await.unwrap();

		let details = forwarding_store.payment(&details_id).await.unwrap().unwrap();
		assert_eq!(details.inbound_amount_forwarded_msat, Some(110));
		assert_eq!(details.outbound_amount_forwarded_msat, Some(100));
		assert_eq!(details.total_fee_earned_msat, Some(10));
		assert_eq!(details.skimmed_fee_msat, Some(2));
		assert_eq!(ForwardedPaymentId::from_str(&details_id.to_string()), Ok(details_id));
		assert_eq!(
			ForwardedPaymentId::from_str("not-an-id"),
			Err(Error::InvalidForwardedPaymentId)
		);
		assert_eq!(ForwardedPaymentId::from_str("00"), Err(Error::InvalidForwardedPaymentId));
		assert_eq!(
			ForwardedPaymentId::from_str(&details_id.to_string().to_uppercase()),
			Ok(details_id)
		);

		let inbound = forwarding_store.channel_stats(&prev_channel_id).await.unwrap().unwrap();
		assert_eq!(inbound.inbound_payments_forwarded, 1);
		assert_eq!(inbound.total_inbound_amount_msat, 110);
		assert_eq!(inbound.total_fee_earned_msat, Some(10));
		assert_eq!(inbound.total_skimmed_fee_msat, 2);

		let outbound = forwarding_store.channel_stats(&next_channel_id).await.unwrap().unwrap();
		assert_eq!(outbound.outbound_payments_forwarded, 1);
		assert_eq!(outbound.total_outbound_amount_msat, 100);
		assert_eq!(outbound.total_fee_earned_msat, Some(0));
		assert_eq!(outbound.onchain_claims_count, 1);
		assert_eq!(forwarding_store.list_payments(None).await.unwrap().objects.len(), 1);
	}

	#[tokio::test]
	async fn forwarding_store_deduplicates_stats_mode_replays() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let forwarding_store =
			ForwardingStore::new(Vec::new(), ForwardedPaymentTrackingMode::Stats, kv_store, logger);
		let prev_channel_id = ChannelId([1; 32]);
		let next_channel_id = ChannelId([2; 32]);
		let prev_htlcs = [HTLCLocator {
			channel_id: prev_channel_id,
			htlc_id: Some(7),
			amount_msat: Some(110),
			user_channel_id: Some(3),
			node_id: None,
		}];
		let next_htlcs = [HTLCLocator {
			channel_id: next_channel_id,
			htlc_id: Some(8),
			amount_msat: Some(100),
			user_channel_id: Some(4),
			node_id: None,
		}];
		let forward = || ForwardRecord {
			prev_htlcs: &prev_htlcs,
			next_htlcs: &next_htlcs,
			total_fee_earned_msat: Some(10),
			skimmed_fee_msat: Some(2),
			claim_from_onchain_tx: false,
			outbound_amount_forwarded_msat: 100,
		};

		forwarding_store.record_forward(forward()).await.unwrap();
		forwarding_store.record_forward(forward()).await.unwrap();

		let inbound = forwarding_store.channel_stats(&prev_channel_id).await.unwrap().unwrap();
		assert_eq!(inbound.inbound_payments_forwarded, 1);
		assert_eq!(inbound.total_inbound_amount_msat, 110);
		assert_eq!(inbound.total_fee_earned_msat, Some(10));
		let outbound = forwarding_store.channel_stats(&next_channel_id).await.unwrap().unwrap();
		assert_eq!(outbound.outbound_payments_forwarded, 1);
		assert_eq!(outbound.total_outbound_amount_msat, 100);
		assert!(forwarding_store.list_payments(None).await.unwrap().objects.is_empty());
	}

	#[tokio::test]
	async fn forwarding_store_skips_ineligible_forwards() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let forwarding_store = ForwardingStore::new(
			Vec::new(),
			ForwardedPaymentTrackingMode::Detailed,
			kv_store,
			logger,
		);
		let prev_htlcs = [
			HTLCLocator {
				channel_id: ChannelId([1; 32]),
				htlc_id: None,
				amount_msat: Some(110),
				user_channel_id: None,
				node_id: None,
			},
			HTLCLocator {
				channel_id: ChannelId([2; 32]),
				htlc_id: None,
				amount_msat: Some(110),
				user_channel_id: None,
				node_id: None,
			},
		];
		let next_htlcs = [HTLCLocator {
			channel_id: ChannelId([3; 32]),
			htlc_id: None,
			amount_msat: Some(100),
			user_channel_id: None,
			node_id: None,
		}];

		forwarding_store
			.record_forward(ForwardRecord {
				prev_htlcs: &prev_htlcs,
				next_htlcs: &next_htlcs,
				total_fee_earned_msat: Some(10),
				skimmed_fee_msat: None,
				claim_from_onchain_tx: false,
				outbound_amount_forwarded_msat: 100,
			})
			.await
			.unwrap();
		forwarding_store
			.record_forward(ForwardRecord {
				prev_htlcs: &prev_htlcs[..1],
				next_htlcs: &next_htlcs,
				total_fee_earned_msat: None,
				skimmed_fee_msat: None,
				claim_from_onchain_tx: true,
				outbound_amount_forwarded_msat: 100,
			})
			.await
			.unwrap();

		assert!(forwarding_store.details.is_empty().await.unwrap());
		assert!(forwarding_store.channel_stats.is_empty().await.unwrap());
	}

	#[tokio::test]
	async fn aggregation_preserves_unknown_fees() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let known_fee_payment = forwarded_payment(1, 850, 110, 100, 10);
		let mut unknown_fee_payment = forwarded_payment(2, 851, 200, 200, 0);
		unknown_fee_payment.total_fee_earned_msat = None;
		insert_completed_payment(&forwarded_payment_store, &replay_marker_store, known_fee_payment)
			.await;
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			unknown_fee_payment,
		)
		.await;

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((1, 2))
		);

		let bucket_id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 840);
		let stats = channel_pair_stats_store.get(&bucket_id).await.unwrap().unwrap();
		assert_eq!(stats.payment_count, 2);
		assert_eq!(stats.total_fee_earned_msat, None);
		assert_eq!(stats.avg_fee_msat, None);
	}

	#[test]
	fn channel_stats_update_preserves_unknown_fees() {
		let mut stats = StoredChannelForwardingStats::from(ChannelForwardingStats {
			channel_id: ChannelId([1; 32]),
			counterparty_node_id: None,
			inbound_payments_forwarded: 1,
			outbound_payments_forwarded: 0,
			total_inbound_amount_msat: 110,
			total_outbound_amount_msat: 0,
			total_fee_earned_msat: Some(10),
			total_skimmed_fee_msat: 0,
			onchain_claims_count: 0,
			first_forwarded_at_timestamp: 850,
			last_forwarded_at_timestamp: 850,
		});

		assert!(stats.update(ChannelForwardingStatsUpdate {
			channel_id: stats.channel_id,
			counterparty_node_id: None,
			inbound_payments_increment: 1,
			outbound_payments_increment: 0,
			inbound_amount_increment_msat: 200,
			outbound_amount_increment_msat: 0,
			fee_earned_increment_msat: None,
			skimmed_fee_increment_msat: 0,
			onchain_claims_increment: 0,
			timestamp: 851,
			last_inbound_forward_id: None,
			last_outbound_forward_id: None,
		}));
		assert_eq!(stats.total_fee_earned_msat, None);
	}

	#[test]
	fn stored_channel_stats_reads_legacy_stats() {
		let legacy = ChannelForwardingStats {
			channel_id: ChannelId([1; 32]),
			counterparty_node_id: None,
			inbound_payments_forwarded: 1,
			outbound_payments_forwarded: 2,
			total_inbound_amount_msat: 110,
			total_outbound_amount_msat: 200,
			total_fee_earned_msat: Some(10),
			total_skimmed_fee_msat: 2,
			onchain_claims_count: 1,
			first_forwarded_at_timestamp: 850,
			last_forwarded_at_timestamp: 851,
		};

		let encoded = legacy.encode();
		let stored = StoredChannelForwardingStats::read(&mut &encoded[..]).unwrap();
		assert_eq!(stored.last_inbound_forward_id, None);
		assert_eq!(stored.last_outbound_forward_id, None);
		assert_eq!(ChannelForwardingStats::from(stored), legacy);
	}

	#[tokio::test]
	async fn aggregation_keeps_both_retained_bucket_boundaries() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let older_bucket_payment = forwarded_payment(1, 839, 110, 100, 10);
		let previous_bucket_payment = forwarded_payment(2, 840, 220, 200, 20);
		let current_bucket_payment = forwarded_payment(3, 900, 330, 300, 30);
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			older_bucket_payment.clone(),
		)
		.await;
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			previous_bucket_payment.clone(),
		)
		.await;
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			current_bucket_payment.clone(),
		)
		.await;

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				900,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		assert!(forwarded_payment_store.get(&older_bucket_payment.id()).await.unwrap().is_none());
		assert_eq!(
			forwarded_payment_store.get(&previous_bucket_payment.id()).await.unwrap(),
			Some(previous_bucket_payment)
		);
		assert_eq!(
			forwarded_payment_store.get(&current_bucket_payment.id()).await.unwrap(),
			Some(current_bucket_payment)
		);
	}

	#[tokio::test]
	async fn zero_retention_cleans_up_after_the_current_bucket_closes() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let payment = forwarded_payment(1, 899, 110, 100, 10);
		insert_completed_payment(&forwarded_payment_store, &replay_marker_store, payment.clone())
			.await;

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				0,
				899,
				&logger,
			)
			.await,
			Ok((0, 0))
		);
		assert_eq!(
			forwarded_payment_store.get(&payment.id()).await.unwrap(),
			Some(payment.clone())
		);

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				0,
				900,
				&logger,
			)
			.await,
			Ok((1, 1))
		);
		assert!(forwarded_payment_store.get(&payment.id()).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn aggregation_retry_only_cleans_up_committed_bucket() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let remaining_payment = forwarded_payment(2, 851, 220, 200, 20);
		let committed_stats = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		channel_pair_stats_store.insert(committed_stats.clone()).await.unwrap();
		insert_completed_payment(
			&forwarded_payment_store,
			&replay_marker_store,
			remaining_payment.clone(),
		)
		.await;

		// This represents a retry after the bucket write and one of two detail deletions
		// succeeded. The existing bucket is the commit marker, so its totals must not change.
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((0, 1))
		);

		assert!(forwarded_payment_store.get(&remaining_payment.id()).await.unwrap().is_none());
		assert_eq!(
			channel_pair_stats_store.get(&committed_stats.id()).await.unwrap(),
			Some(committed_stats)
		);
	}

	#[tokio::test]
	async fn aggregation_preserves_details_if_commit_marker_is_corrupt() {
		let (
			forwarded_payment_store,
			replay_marker_store,
			channel_pair_stats_store,
			logger,
			kv_store,
		) = test_stores_with_kv();
		let payment = forwarded_payment(2, 851, 220, 200, 20);
		let pair_id =
			channel_pair_stats_id(&payment.prev_channel_id, &payment.next_channel_id, 840);
		insert_completed_payment(&forwarded_payment_store, &replay_marker_store, payment.clone())
			.await;
		KVStore::write(
			&*kv_store,
			"test_forwarded_payments",
			"pair_stats",
			&pair_id.encode_to_hex_str(),
			vec![0xff],
		)
		.await
		.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Err(Error::PersistenceFailed)
		);
		assert_eq!(forwarded_payment_store.get(&payment.id()).await.unwrap(), Some(payment));
	}

	#[test]
	fn cumulative_stats_prefer_known_node_ids_and_cover_bucket_span() {
		let unknown_nodes = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		let mut known_nodes = channel_pair_stats(900, 120, 1_000, 1, 110, 100, 10, 902, 902);
		let node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		known_nodes.prev_node_id = Some(node_id);
		known_nodes.next_node_id = Some(node_id);

		let cumulative = aggregate_channel_pair_stats(&[unknown_nodes, known_nodes]).unwrap();
		assert_eq!(cumulative.prev_node_id, Some(node_id));
		assert_eq!(cumulative.next_node_id, Some(node_id));
		assert_eq!(cumulative.payment_count, 3);
		assert_eq!(cumulative.total_fee_earned_msat, Some(40));
		// The span runs from the earliest bucket start to the latest bucket end, gaps included.
		assert_eq!(cumulative.bucket_start_timestamp, 840);
		assert_eq!(cumulative.bucket_size_secs, 180);
	}

	#[test]
	fn cumulative_stats_reject_mismatched_channel_pairs() {
		let mut other_pair = channel_pair_stats(900, 60, 1_000, 1, 110, 100, 10, 902, 902);
		other_pair.next_channel_id = ChannelId([3; 32]);

		assert!(aggregate_channel_pair_stats(&[]).is_none());
		assert!(aggregate_channel_pair_stats(&[
			channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851),
			other_pair,
		])
		.is_none());
	}

	#[tokio::test]
	async fn retention_does_not_change_bucket_geometry() {
		let (forwarded_payment_store, replay_marker_store, channel_pair_stats_store, logger) =
			test_stores();
		let old_stats = channel_pair_stats(0, 3_600, 7_200, 1, 110, 100, 10, 100, 100);
		let payment = forwarded_payment(2, 4_000, 220, 200, 20);
		channel_pair_stats_store.insert(old_stats.clone()).await.unwrap();
		insert_completed_payment(&forwarded_payment_store, &replay_marker_store, payment.clone())
			.await;

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&replay_marker_store,
				&channel_pair_stats_store,
				3_600,
				7_200,
				15_000,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		let new_id =
			channel_pair_stats_id(&payment.prev_channel_id, &payment.next_channel_id, 3_600);
		assert_eq!(channel_pair_stats_store.get(&old_stats.id()).await.unwrap(), Some(old_stats));
		assert_eq!(channel_pair_stats_store.get(&new_id).await.unwrap().unwrap().payment_count, 1);
		assert!(forwarded_payment_store.get(&payment.id()).await.unwrap().is_none());
	}

	#[tokio::test]
	#[cfg(feature = "storage-sqlite")]
	async fn channel_pair_stats_persist_to_sqlite() {
		let mut storage_path = random_storage_path();
		storage_path.push("channel_pair_stats_persist_to_sqlite");
		let sqlite_store =
			SqliteStore::new(storage_path, Some("stats.sqlite".to_string()), None).unwrap();
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(sqlite_store));
		let logger = Arc::new(Logger::new_log_facade());
		let namespace = "sqlite_channel_pair_stats";
		let secondary_namespace = "pair_stats";
		let stats_store = ChannelPairForwardingStatsStore::new(
			Vec::new(),
			KeepNoEntries,
			namespace.to_string(),
			secondary_namespace.to_string(),
			Arc::clone(&kv_store),
			logger,
		);
		let stats = channel_pair_stats(840, 60, 1_000, 2, 330, 300, 30, 850, 851);

		stats_store.insert(stats.clone()).await.unwrap();
		let keys = KVStore::list(&*kv_store, namespace, secondary_namespace).await.unwrap();
		assert_eq!(keys, vec![stats.id.to_string()]);
		let bytes =
			KVStore::read(&*kv_store, namespace, secondary_namespace, &keys[0]).await.unwrap();
		assert_eq!(ChannelPairForwardingStats::read(&mut &bytes[..]).unwrap(), stats);
	}

	#[tokio::test]
	async fn uncached_data_store_reads_existing_objects_across_pages() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let namespace = "paged_forwarded_payments";
		let store = ForwardedPaymentStore::new(
			Vec::new(),
			KeepNoEntries,
			namespace.to_string(),
			"details".to_string(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		for id in 0..=50 {
			store.insert(forwarded_payment(id, id as u64, 110, 100, 10)).await.unwrap();
		}
		assert_eq!(store.cached_len(), 0);

		// Recreate the store to verify it has no in-memory state to preload.
		let reopened_store = ForwardedPaymentStore::new(
			Vec::new(),
			KeepNoEntries,
			namespace.to_string(),
			"details".to_string(),
			kv_store,
			logger,
		);
		let oldest_payment = forwarded_payment(0, 0, 110, 100, 10);
		assert_eq!(
			reopened_store.get(&oldest_payment.id()).await.unwrap(),
			Some(oldest_payment.clone())
		);
		assert_eq!(reopened_store.cached_len(), 0);

		let first_page = reopened_store.list_page(None).await.unwrap();
		assert_eq!(first_page.objects.len(), 50);
		let second_page = reopened_store.list_page(first_page.next_page_token).await.unwrap();
		assert_eq!(second_page.objects, vec![oldest_payment.clone()]);
		assert!(second_page.next_page_token.is_none());
	}
}
