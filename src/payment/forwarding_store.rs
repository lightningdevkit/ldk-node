// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use lightning::ln::types::ChannelId;
use lightning::util::logger::Logger as _;
use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore};

use super::forwarding::{
	ChannelForwardingStats, ChannelPairForwardingStats, ForwardedPaymentDetails,
};
use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::logger::{log_debug, log_error, Logger};
use crate::types::{DynStore, DynStoreRef};
use crate::Error;

pub(crate) const FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS: u64 = 60 * 60;

/// A disk-backed store for forwarding data that is too large to keep in memory.
pub(crate) struct DiskStore<SO: StorableObject> {
	mutation_lock: tokio::sync::Mutex<()>,
	primary_namespace: String,
	secondary_namespace: String,
	kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
	_object: PhantomData<SO>,
}

impl<SO: StorableObject> DiskStore<SO> {
	pub(crate) fn new(
		primary_namespace: String, secondary_namespace: String, kv_store: Arc<DynStore>,
		logger: Arc<Logger>,
	) -> Self {
		Self {
			mutation_lock: tokio::sync::Mutex::new(()),
			primary_namespace,
			secondary_namespace,
			kv_store,
			logger,
			_object: PhantomData,
		}
	}

	pub(crate) async fn insert(&self, object: SO) -> Result<(), Error> {
		self.insert_with(|| object).await
	}

	/// Builds and inserts an object while holding the mutation lock.
	pub(crate) async fn insert_with<F: FnOnce() -> SO>(
		&self, build_object: F,
	) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		self.persist_unlocked(&build_object()).await
	}

	pub(crate) async fn get(&self, id: &SO::Id) -> Result<Option<SO>, Error> {
		let _guard = self.mutation_lock.lock().await;
		self.get_unlocked(id).await
	}

	pub(crate) async fn contains_key(&self, id: &SO::Id) -> Result<bool, Error> {
		Ok(self.get(id).await?.is_some())
	}

	pub(crate) async fn remove(&self, id: &SO::Id) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let store_key = id.encode_to_hex_str();
		KVStore::remove(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
			false,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Removing object data for key {}/{}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				store_key,
				e
			);
			Error::PersistenceFailed
		})
	}

	pub(crate) async fn is_empty(&self) -> Result<bool, Error> {
		let _guard = self.mutation_lock.lock().await;
		let response = self.list_keys_page_unlocked(None).await?;
		Ok(response.keys.is_empty())
	}

	pub(crate) async fn list_filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Result<Vec<SO>, Error> {
		let _guard = self.mutation_lock.lock().await;
		Ok(self.list_all_unlocked().await?.iter().filter(f).cloned().collect())
	}

	pub(crate) async fn list_page(
		&self, page_token: Option<PageToken>,
	) -> Result<(Vec<SO>, Option<PageToken>), Error> {
		let _guard = self.mutation_lock.lock().await;
		self.list_page_unlocked(page_token).await
	}

	/// Prevents mutations to this store until the returned guard is dropped.
	pub(crate) async fn mutation_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
		self.mutation_lock.lock().await
	}

	async fn get_unlocked(&self, id: &SO::Id) -> Result<Option<SO>, Error> {
		let store_key = id.encode_to_hex_str();
		let data = match KVStore::read(
			&DynStoreRef(Arc::clone(&self.kv_store)),
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
		)
		.await
		{
			Ok(data) => data,
			Err(e) if e.kind() == bitcoin::io::ErrorKind::NotFound => return Ok(None),
			Err(e) => {
				log_error!(
					self.logger,
					"Reading object data for key {}/{}/{} failed due to: {}",
					&self.primary_namespace,
					&self.secondary_namespace,
					store_key,
					e
				);
				return Err(Error::PersistenceFailed);
			},
		};

		SO::read(&mut &data[..]).map(Some).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to deserialize object data for key {}/{}/{}: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				store_key,
				e
			);
			Error::PersistenceFailed
		})
	}

	async fn list_page_unlocked(
		&self, page_token: Option<PageToken>,
	) -> Result<(Vec<SO>, Option<PageToken>), Error> {
		let response = self.list_keys_page_unlocked(page_token).await?;
		let mut objects = Vec::with_capacity(response.keys.len());
		for key in response.keys {
			let object = self.get_unlocked_key(&key).await?;
			objects.push(object);
		}
		Ok((objects, response.next_page_token))
	}

	async fn list_all_unlocked(&self) -> Result<Vec<SO>, Error> {
		let mut objects = Vec::new();
		let mut page_token = None;
		loop {
			let (mut page, next_page_token) = self.list_page_unlocked(page_token).await?;
			objects.append(&mut page);
			let Some(next_page_token) = next_page_token else {
				return Ok(objects);
			};
			page_token = Some(next_page_token);
		}
	}

	async fn list_keys_page_unlocked(
		&self, page_token: Option<PageToken>,
	) -> Result<lightning::util::persist::PaginatedListResponse, Error> {
		PaginatedKVStore::list_paginated(
			&DynStoreRef(Arc::clone(&self.kv_store)),
			&self.primary_namespace,
			&self.secondary_namespace,
			page_token,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Listing object data under {}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				e
			);
			Error::PersistenceFailed
		})
	}

	async fn get_unlocked_key(&self, key: &str) -> Result<SO, Error> {
		let data = KVStore::read(
			&DynStoreRef(Arc::clone(&self.kv_store)),
			&self.primary_namespace,
			&self.secondary_namespace,
			key,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Reading object data for key {}/{}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				key,
				e
			);
			Error::PersistenceFailed
		})?;

		SO::read(&mut &data[..]).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to deserialize object data for key {}/{}/{}: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				key,
				e
			);
			Error::PersistenceFailed
		})
	}

	async fn persist_unlocked(&self, object: &SO) -> Result<(), Error> {
		let store_key = object.id().encode_to_hex_str();
		KVStore::write(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
			object.encode(),
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Write for key {}/{}/{} failed due to: {}",
				&self.primary_namespace,
				&self.secondary_namespace,
				store_key,
				e
			);
			Error::PersistenceFailed
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ForwardedPaymentDetailsUpdate {
	id: String,
}

impl StorableObjectUpdate<ForwardedPaymentDetails> for ForwardedPaymentDetailsUpdate {
	fn id(&self) -> String {
		self.id.clone()
	}
}

impl StorableObject for ForwardedPaymentDetails {
	type Id = String;
	type Update = ForwardedPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id.clone()
	}

	fn update(&mut self, _update: Self::Update) -> bool {
		false
	}

	fn to_update(&self) -> Self::Update {
		ForwardedPaymentDetailsUpdate { id: self.id.clone() }
	}
}

fn channel_pair_stats_id(
	prev: &ChannelId, next: &ChannelId, bucket_start_timestamp: u64,
) -> String {
	// Hash the ordered channel pair and bucket start. The bucket size is fixed and therefore does
	// not need to be part of the ID. Keeping the channels ordered preserves forwarding direction,
	// unlike combining them with XOR.
	let mut bytes = [0u8; 72];
	bytes[0..32].copy_from_slice(&prev.0);
	bytes[32..64].copy_from_slice(&next.0);
	bytes[64..72].copy_from_slice(&bucket_start_timestamp.to_be_bytes());
	sha256::Hash::hash(&bytes).to_string()
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
}

impl StorableObjectUpdate<ChannelForwardingStats> for ChannelForwardingStatsUpdate {
	fn id(&self) -> ChannelId {
		self.channel_id
	}
}

impl StorableObjectId for ChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

impl StorableObject for ChannelForwardingStats {
	type Id = ChannelId;
	type Update = ChannelForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.channel_id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(self.channel_id, update.channel_id);
		let mut updated = false;
		if self.counterparty_node_id.is_none() && update.counterparty_node_id.is_some() {
			self.counterparty_node_id = update.counterparty_node_id;
			updated = true;
		}
		if update.inbound_payments_increment > 0 {
			self.inbound_payments_forwarded += update.inbound_payments_increment;
			updated = true;
		}
		if update.outbound_payments_increment > 0 {
			self.outbound_payments_forwarded += update.outbound_payments_increment;
			updated = true;
		}
		if update.inbound_amount_increment_msat > 0 {
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			updated = true;
		}
		if update.outbound_amount_increment_msat > 0 {
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			updated = true;
		}
		match (self.total_fee_earned_msat.as_mut(), update.fee_earned_increment_msat) {
			(Some(total), Some(increment)) if increment > 0 => {
				*total += increment;
				updated = true;
			},
			(Some(_), None) => {
				self.total_fee_earned_msat = None;
				updated = true;
			},
			_ => {},
		}
		if update.skimmed_fee_increment_msat > 0 {
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			updated = true;
		}
		if update.onchain_claims_increment > 0 {
			self.onchain_claims_count += update.onchain_claims_increment;
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
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelPairForwardingStatsUpdate {
	pub id: String,
	pub prev_node_id: Option<PublicKey>,
	pub next_node_id: Option<PublicKey>,
	pub payment_count_increment: u64,
	pub inbound_amount_increment_msat: u64,
	pub outbound_amount_increment_msat: u64,
	pub fee_earned_increment_msat: Option<u64>,
	pub skimmed_fee_increment_msat: u64,
	pub onchain_claims_increment: u64,
	pub first_timestamp: u64,
	pub last_timestamp: u64,
	pub aggregated_at_timestamp: u64,
}

impl StorableObjectUpdate<ChannelPairForwardingStats> for ChannelPairForwardingStatsUpdate {
	fn id(&self) -> String {
		self.id.clone()
	}
}

impl StorableObject for ChannelPairForwardingStats {
	type Id = String;
	type Update = ChannelPairForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.id.clone()
	}

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(self.id, update.id);
		let mut updated = false;
		if self.prev_node_id.is_none() && update.prev_node_id.is_some() {
			self.prev_node_id = update.prev_node_id;
			updated = true;
		}
		if self.next_node_id.is_none() && update.next_node_id.is_some() {
			self.next_node_id = update.next_node_id;
			updated = true;
		}
		if update.payment_count_increment > 0 {
			self.payment_count += update.payment_count_increment;
			updated = true;
		}
		if update.inbound_amount_increment_msat > 0 {
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			updated = true;
		}
		if update.outbound_amount_increment_msat > 0 {
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			updated = true;
		}
		match (self.total_fee_earned_msat.as_mut(), update.fee_earned_increment_msat) {
			(Some(total), Some(increment)) if increment > 0 => {
				*total += increment;
				updated = true;
			},
			(Some(_), None) => {
				self.total_fee_earned_msat = None;
				updated = true;
			},
			_ => {},
		}
		if update.skimmed_fee_increment_msat > 0 {
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			updated = true;
		}
		if update.onchain_claims_increment > 0 {
			self.onchain_claims_count += update.onchain_claims_increment;
			updated = true;
		}
		if updated {
			if self.first_forwarded_at_timestamp == 0 {
				self.first_forwarded_at_timestamp = update.first_timestamp;
			} else {
				self.first_forwarded_at_timestamp =
					self.first_forwarded_at_timestamp.min(update.first_timestamp);
			}
			self.last_forwarded_at_timestamp =
				self.last_forwarded_at_timestamp.max(update.last_timestamp);
			if self.payment_count > 0 {
				self.avg_fee_msat =
					self.total_fee_earned_msat.map(|total| total / self.payment_count);
				self.avg_inbound_amount_msat = self.total_inbound_amount_msat / self.payment_count;
			}
		}
		if update.aggregated_at_timestamp > self.aggregated_at_timestamp {
			self.aggregated_at_timestamp = update.aggregated_at_timestamp;
			updated = true;
		}
		updated
	}

	fn to_update(&self) -> Self::Update {
		ChannelPairForwardingStatsUpdate {
			id: self.id.clone(),
			prev_node_id: self.prev_node_id,
			next_node_id: self.next_node_id,
			payment_count_increment: self.payment_count,
			inbound_amount_increment_msat: self.total_inbound_amount_msat,
			outbound_amount_increment_msat: self.total_outbound_amount_msat,
			fee_earned_increment_msat: self.total_fee_earned_msat,
			skimmed_fee_increment_msat: self.total_skimmed_fee_msat,
			onchain_claims_increment: self.onchain_claims_count,
			first_timestamp: self.first_forwarded_at_timestamp,
			last_timestamp: self.last_forwarded_at_timestamp,
			aggregated_at_timestamp: self.aggregated_at_timestamp,
		}
	}
}

fn seconds_until_next_forwarding_aggregation(now_timestamp: u64, bucket_size_secs: u64) -> u64 {
	debug_assert!(bucket_size_secs > 0);
	bucket_size_secs - (now_timestamp % bucket_size_secs)
}

async fn aggregate_forwarded_payments_and_log(
	forwarded_payment_store: &DiskStore<ForwardedPaymentDetails>,
	channel_pair_stats_store: &DiskStore<ChannelPairForwardingStats>, retention_secs: u64,
	logger: &Arc<Logger>,
) {
	match aggregate_expired_forwarded_payments(
		forwarded_payment_store,
		channel_pair_stats_store,
		retention_secs,
		logger,
	)
	.await
	{
		Ok((pair_count, payment_count)) if pair_count > 0 => {
			log_debug!(
				logger,
				"Aggregated {} forwarded payments into {} channel pair buckets",
				payment_count,
				pair_count
			);
		},
		Ok((0, payment_count)) if payment_count > 0 => {
			log_debug!(
				logger,
				"Removed {} forwarded payment details from previously aggregated buckets",
				payment_count
			);
		},
		Err(e) => log_error!(logger, "Forwarded payment aggregation failed: {}", e),
		_ => {},
	}
}

pub(crate) async fn run_forwarded_payment_aggregation(
	mut stop_receiver: tokio::sync::watch::Receiver<()>,
	forwarded_payment_store: Arc<DiskStore<ForwardedPaymentDetails>>,
	channel_pair_stats_store: Arc<DiskStore<ChannelPairForwardingStats>>, retention_secs: u64,
	logger: Arc<Logger>,
) {
	if retention_secs == 0 {
		match forwarded_payment_store.is_empty().await {
			Ok(true) => return,
			Ok(false) => {},
			Err(e) => log_error!(logger, "Failed to check forwarded payment store: {}", e),
		}
	}

	aggregate_forwarded_payments_and_log(
		&forwarded_payment_store,
		&channel_pair_stats_store,
		retention_secs,
		&logger,
	)
	.await;

	if retention_secs == 0 {
		match forwarded_payment_store.is_empty().await {
			Ok(true) => return,
			Ok(false) => {},
			Err(e) => log_error!(logger, "Failed to check forwarded payment store: {}", e),
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
				aggregate_forwarded_payments_and_log(
					&forwarded_payment_store,
					&channel_pair_stats_store,
					retention_secs,
					&logger,
				)
				.await;
				if retention_secs == 0 {
					match forwarded_payment_store.is_empty().await {
						Ok(true) => break,
						Ok(false) => {},
						Err(e) => log_error!(logger, "Failed to check forwarded payment store: {}", e),
					}
				}
			}
		}
	}
}

/// Aggregate forwarded payments older than the configured retention period into fixed-width
/// channel-pair statistics buckets.
pub(crate) async fn aggregate_expired_forwarded_payments(
	forwarded_payment_store: &DiskStore<ForwardedPaymentDetails>,
	channel_pair_stats_store: &DiskStore<ChannelPairForwardingStats>, retention_secs: u64,
	logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	aggregate_expired_forwarded_payments_at(
		forwarded_payment_store,
		channel_pair_stats_store,
		FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS,
		retention_secs,
		now,
		logger,
	)
	.await
}

async fn aggregate_expired_forwarded_payments_at(
	forwarded_payment_store: &DiskStore<ForwardedPaymentDetails>,
	channel_pair_stats_store: &DiskStore<ChannelPairForwardingStats>, bucket_size_secs: u64,
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
	// Detail insertions assign their timestamps while holding this same lock. Holding it until all
	// closed-bucket markers are persisted ensures a record can either be included in the marker or
	// receive a timestamp after the aggregation pass, but cannot be inserted late into a committed
	// bucket.
	let forwarded_payment_guard = forwarded_payment_store.mutation_guard().await;
	let expired = forwarded_payment_store
		.list_all_unlocked()
		.await?
		.into_iter()
		.filter(|p| p.forwarded_at_timestamp < oldest_retained_bucket_start)
		.collect::<Vec<_>>();
	if expired.is_empty() {
		log_debug!(logger, "No forwarded payments in closed aggregation buckets found");
		return Ok((0, 0));
	}

	let mut bucket_groups: HashMap<(ChannelId, ChannelId, u64), Vec<ForwardedPaymentDetails>> =
		HashMap::new();
	for payment in expired {
		let bucket_start = (payment.forwarded_at_timestamp / bucket_size_secs) * bucket_size_secs;
		bucket_groups
			.entry((payment.prev_channel_id, payment.next_channel_id, bucket_start))
			.or_default()
			.push(payment);
	}

	let mut aggregated_bucket_count = 0u64;
	let mut payment_ids_to_remove = Vec::new();
	for ((prev_channel_id, next_channel_id, bucket_start), payments) in bucket_groups {
		let pair_id = channel_pair_stats_id(&prev_channel_id, &next_channel_id, bucket_start);

		if !channel_pair_stats_store.contains_key(&pair_id).await? {
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
				id: pair_id.clone(),
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

		payment_ids_to_remove.extend(payments.into_iter().map(|payment| payment.id));
	}

	// Removals acquire the mutation lock themselves. Once every bucket marker has been persisted,
	// detail insertions may resume without risking insertion into any bucket closed by this pass.
	drop(forwarded_payment_guard);
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
/// The returned bucket spans from the earliest input bucket start through the latest input bucket
/// end, including any gaps between buckets.
///
/// Returns `None` if `buckets` is empty or contains statistics for different channel pairs.
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
	let bucket_size_secs = latest_bucket_end.saturating_sub(earliest_bucket_start);
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
		bucket_size_secs,
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
	use lightning::util::ser::Readable;

	use super::*;
	use crate::io::sqlite_store::SqliteStore;
	use crate::io::test_utils::{random_storage_path, InMemoryStore};
	use crate::types::{DynStore, DynStoreWrapper};

	type TestForwardedPaymentStore = DiskStore<ForwardedPaymentDetails>;
	type TestChannelPairStatsStore = DiskStore<ChannelPairForwardingStats>;

	fn test_stores() -> (TestForwardedPaymentStore, TestChannelPairStatsStore, Arc<Logger>) {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let forwarded_payment_store = DiskStore::new(
			"test_forwarded_payments".to_string(),
			String::new(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		let channel_pair_stats_store = DiskStore::new(
			"test_channel_pair_stats".to_string(),
			String::new(),
			kv_store,
			Arc::clone(&logger),
		);
		(forwarded_payment_store, channel_pair_stats_store, logger)
	}

	fn forwarded_payment(
		id: u8, forwarded_at_timestamp: u64, inbound_amount_msat: u64, outbound_amount_msat: u64,
		fee_msat: u64,
	) -> ForwardedPaymentDetails {
		ForwardedPaymentDetails {
			id: hex_utils::to_string(&[id; 32]),
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
	}

	#[tokio::test]
	async fn aggregation_retains_current_and_previous_buckets() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let closed_bucket_payment = forwarded_payment(1, 899, 110, 100, 10);
		let partial_bucket_payment = forwarded_payment(2, 939, 220, 200, 20);
		forwarded_payment_store.insert(closed_bucket_payment.clone()).await.unwrap();
		forwarded_payment_store.insert(partial_bucket_payment.clone()).await.unwrap();

		// At timestamp 1,000, the current bucket starts at 960 and the previous bucket starts at
		// 900. Only payments older than the previous bucket are aggregated.
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		assert!(forwarded_payment_store.get(&closed_bucket_payment.id).await.unwrap().is_none());
		assert_eq!(
			forwarded_payment_store.get(&partial_bucket_payment.id).await.unwrap(),
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
	async fn background_aggregation_runs_immediately() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let forwarded_payment_store = Arc::new(forwarded_payment_store);
		let channel_pair_stats_store = Arc::new(channel_pair_stats_store);
		let payment = forwarded_payment(1, 1, 110, 100, 10);
		forwarded_payment_store.insert(payment.clone()).await.unwrap();
		let (_stop_sender, stop_receiver) = tokio::sync::watch::channel(());

		run_forwarded_payment_aggregation(
			stop_receiver,
			Arc::clone(&forwarded_payment_store),
			Arc::clone(&channel_pair_stats_store),
			0,
			logger,
		)
		.await;

		assert!(forwarded_payment_store.is_empty().await.unwrap());
		let bucket_id =
			channel_pair_stats_id(&payment.prev_channel_id, &payment.next_channel_id, 0);
		assert_eq!(
			channel_pair_stats_store.get(&bucket_id).await.unwrap().unwrap().payment_count,
			1
		);
	}

	#[tokio::test]
	async fn aggregation_preserves_unknown_fees() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let known_fee_payment = forwarded_payment(1, 850, 110, 100, 10);
		let mut unknown_fee_payment = forwarded_payment(2, 851, 200, 200, 0);
		unknown_fee_payment.total_fee_earned_msat = None;
		forwarded_payment_store.insert(known_fee_payment).await.unwrap();
		forwarded_payment_store.insert(unknown_fee_payment).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
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
		let mut stats = ChannelForwardingStats {
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
		};

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
		}));
		assert_eq!(stats.total_fee_earned_msat, None);
	}

	#[tokio::test]
	async fn aggregation_keeps_both_retained_bucket_boundaries() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let older_bucket_payment = forwarded_payment(1, 839, 110, 100, 10);
		let previous_bucket_payment = forwarded_payment(2, 840, 220, 200, 20);
		let current_bucket_payment = forwarded_payment(3, 900, 330, 300, 30);
		forwarded_payment_store.insert(older_bucket_payment.clone()).await.unwrap();
		forwarded_payment_store.insert(previous_bucket_payment.clone()).await.unwrap();
		forwarded_payment_store.insert(current_bucket_payment.clone()).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				900,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		assert!(forwarded_payment_store.get(&older_bucket_payment.id).await.unwrap().is_none());
		assert_eq!(
			forwarded_payment_store.get(&previous_bucket_payment.id).await.unwrap(),
			Some(previous_bucket_payment)
		);
		assert_eq!(
			forwarded_payment_store.get(&current_bucket_payment.id).await.unwrap(),
			Some(current_bucket_payment)
		);
	}

	#[tokio::test]
	async fn zero_retention_cleans_up_after_the_current_bucket_closes() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let payment = forwarded_payment(1, 899, 110, 100, 10);
		forwarded_payment_store.insert(payment.clone()).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				0,
				899,
				&logger,
			)
			.await,
			Ok((0, 0))
		);
		assert_eq!(forwarded_payment_store.get(&payment.id).await.unwrap(), Some(payment.clone()));

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				0,
				900,
				&logger,
			)
			.await,
			Ok((1, 1))
		);
		assert!(forwarded_payment_store.get(&payment.id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn aggregation_retry_only_cleans_up_committed_bucket() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let remaining_payment = forwarded_payment(2, 851, 220, 200, 20);
		let committed_stats = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		channel_pair_stats_store.insert(committed_stats.clone()).await.unwrap();
		forwarded_payment_store.insert(remaining_payment.clone()).await.unwrap();

		// This represents a retry after the bucket write and one of two detail deletions
		// succeeded. The existing bucket is the commit marker, so its totals must not change.
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((0, 1))
		);

		assert!(forwarded_payment_store.get(&remaining_payment.id).await.unwrap().is_none());
		assert_eq!(
			channel_pair_stats_store.get(&committed_stats.id).await.unwrap(),
			Some(committed_stats)
		);
	}

	#[test]
	fn channel_pair_update_refreshes_aggregation_timestamp() {
		let mut stats = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		let update = channel_pair_stats(840, 60, 1_000, 1, 110, 100, 10, 852, 852);

		assert!(stats.update(update.to_update()));
		assert_eq!(stats.payment_count, 3);
		assert_eq!(stats.aggregated_at_timestamp, 1_000);
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
		assert_eq!(cumulative.bucket_start_timestamp, 840);
		assert_eq!(cumulative.bucket_size_secs, 180);
	}

	#[tokio::test]
	async fn retention_does_not_change_bucket_geometry() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let old_stats = channel_pair_stats(0, 3_600, 7_200, 1, 110, 100, 10, 100, 100);
		let payment = forwarded_payment(2, 4_000, 220, 200, 20);
		channel_pair_stats_store.insert(old_stats.clone()).await.unwrap();
		forwarded_payment_store.insert(payment.clone()).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
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
		assert_eq!(channel_pair_stats_store.get(&old_stats.id).await.unwrap(), Some(old_stats));
		assert_eq!(channel_pair_stats_store.get(&new_id).await.unwrap().unwrap().payment_count, 1);
		assert!(forwarded_payment_store.get(&payment.id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn channel_pair_stats_persist_to_sqlite() {
		let mut storage_path = random_storage_path();
		storage_path.push("channel_pair_stats_persist_to_sqlite");
		let sqlite_store =
			SqliteStore::new(storage_path, Some("stats.sqlite".to_string()), None).unwrap();
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(sqlite_store));
		let logger = Arc::new(Logger::new_log_facade());
		let namespace = "sqlite_channel_pair_stats";
		let stats_store =
			DiskStore::new(namespace.to_string(), String::new(), Arc::clone(&kv_store), logger);
		let stats = channel_pair_stats(840, 60, 1_000, 2, 330, 300, 30, 850, 851);

		stats_store.insert(stats.clone()).await.unwrap();
		let keys = KVStore::list(&*kv_store, namespace, "").await.unwrap();
		assert_eq!(keys, vec![stats.id.encode_to_hex_str()]);
		let bytes = KVStore::read(&*kv_store, namespace, "", &keys[0]).await.unwrap();
		assert_eq!(ChannelPairForwardingStats::read(&mut &bytes[..]).unwrap(), stats);
	}

	#[tokio::test]
	async fn disk_store_reads_existing_objects_across_pages() {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let namespace = "paged_forwarded_payments";
		let store = DiskStore::new(
			namespace.to_string(),
			String::new(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		for id in 0..=50 {
			store.insert(forwarded_payment(id, id as u64, 110, 100, 10)).await.unwrap();
		}

		// Recreate the store to verify it has no in-memory state to preload.
		let reopened_store = DiskStore::new(namespace.to_string(), String::new(), kv_store, logger);
		let oldest_payment = forwarded_payment(0, 0, 110, 100, 10);
		assert_eq!(
			reopened_store.get(&oldest_payment.id).await.unwrap(),
			Some(oldest_payment.clone())
		);

		let (first_page, next_page_token) = reopened_store.list_page(None).await.unwrap();
		assert_eq!(first_page.len(), 50);
		let (second_page, next_page_token) =
			reopened_store.list_page(next_page_token).await.unwrap();
		assert_eq!(second_page, vec![oldest_payment.clone()]);
		assert!(next_page_token.is_none());

		assert_eq!(
			reopened_store.list_filter(|payment| payment.id == oldest_payment.id).await.unwrap(),
			vec![oldest_payment]
		);
	}
}
