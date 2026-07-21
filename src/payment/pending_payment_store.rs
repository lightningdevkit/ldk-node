// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use bitcoin::Txid;
use lightning::ln::channelmanager::PaymentId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};
use lightning_types::payment::PaymentHash;

use crate::data_store::{DataStore, StorableObject, StorableObjectUpdate};
use crate::logger::LdkLogger;
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use crate::types::DynStore;
use crate::Error;

/// One candidate transaction in an interactive-funding (splice) RBF history, holding this node's
/// share of the funding amount and fee for that candidate. Both are `None` for a candidate this
/// node did not contribute to — e.g. a counterparty-initiated round before our `splice_in` joined
/// it via RBF. Recorded per pending payment so that, on confirmation, the payment reports the
/// figures of the candidate that actually confirmed, which need not be the last one broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FundingTxCandidate {
	/// The candidate's broadcast transaction id.
	pub txid: Txid,
	/// This node's share of the funding amount for this candidate, in millisatoshis, or `None` if
	/// this node did not contribute to it.
	pub amount_msat: Option<u64>,
	/// This node's share of the on-chain fee for this candidate, in millisatoshis, or `None` if
	/// this node did not contribute to it.
	pub fee_paid_msat: Option<u64>,
}

impl_writeable_tlv_based!(FundingTxCandidate, {
	(0, txid, required),
	(2, amount_msat, option),
	(4, fee_paid_msat, option),
});

/// The condition after which this pending payment can be pruned.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingPaymentExpiry {
	/// The Unix timestamp after which this pending payment can be pruned.
	Time { timestamp: u64 },
	/// The block height after which this pending payment can be pruned.
	Height { height: u32 },
}

impl_writeable_tlv_based_enum!(PendingPaymentExpiry,
	(0, Time) => {
		(0, timestamp, required),
	},
	(2, Height) => {
		(0, height, required),
	},
);

/// Represents a pending payment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPaymentDetails {
	/// The full payment details
	pub details: PaymentDetails,
	/// Transaction IDs that have replaced or conflict with this payment.
	pub conflicting_txids: Vec<Txid>,
	/// For interactive funding (splices), this node's per-candidate funding figures across the
	/// RBF history, keyed by each candidate's txid. Empty for non-funding payments and for
	/// records written before per-candidate tracking existed.
	pub(crate) candidates: Vec<FundingTxCandidate>,
	/// The condition after which this pending payment can be pruned.
	pub(crate) expiry: Option<PendingPaymentExpiry>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, candidates: Vec<FundingTxCandidate>,
	) -> Self {
		Self { details, conflicting_txids, candidates, expiry: None }
	}

	pub(crate) fn new_with_expiry(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, expiry: Option<PendingPaymentExpiry>,
	) -> Self {
		Self { details, conflicting_txids, candidates: Vec::new(), expiry }
	}

	/// Returns this node's recorded funding figures for the candidate with the given txid, if any.
	pub(crate) fn candidate(&self, txid: Txid) -> Option<&FundingTxCandidate> {
		self.candidates.iter().find(|candidate| candidate.txid == txid)
	}

	pub(crate) fn has_expired(&self, now: u64, current_height: u32) -> bool {
		match self.expiry {
			Some(PendingPaymentExpiry::Time { timestamp }) => timestamp <= now,
			Some(PendingPaymentExpiry::Height { height }) => height <= current_height,
			None => false,
		}
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
	(4, candidates, optional_vec),
	(6, expiry, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub candidates: Vec<FundingTxCandidate>,
	pub expiry: Option<Option<PendingPaymentExpiry>>,
}

impl StorableObject for PendingPaymentDetails {
	type Id = PaymentId;
	type Update = PendingPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.details.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		let mut updated = false;

		// Update the underlying payment details if present
		if let Some(payment_update) = update.payment_update {
			updated |= self.details.update(payment_update);
		}

		if let Some(new_conflicting_txids) = update.conflicting_txids {
			if self.conflicting_txids != new_conflicting_txids {
				self.conflicting_txids = new_conflicting_txids;
				updated = true;
			}
		}

		if let PaymentKind::Onchain { txid, .. } = &self.details.kind {
			let conflicts_len = self.conflicting_txids.len();
			self.conflicting_txids.retain(|conflicting_txid| conflicting_txid != txid);
			updated |= self.conflicting_txids.len() != conflicts_len;
		}

		// Each classify passes the complete candidate history, so a non-empty update replaces the
		// stored list. An empty update (e.g. a non-funding payment) leaves it untouched.
		if !update.candidates.is_empty() && self.candidates != update.candidates {
			self.candidates = update.candidates;
			updated = true;
		}

		if let Some(new_expiry) = update.expiry {
			if self.expiry != new_expiry {
				self.expiry = new_expiry;
				updated = true;
			}
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

impl StorableObjectUpdate<PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn id(&self) -> <PendingPaymentDetails as StorableObject>::Id {
		self.id
	}
}

impl From<&PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn from(value: &PendingPaymentDetails) -> Self {
		let conflicting_txids = if value.conflicting_txids.is_empty() {
			None
		} else {
			Some(value.conflicting_txids.clone())
		};
		Self {
			id: value.id(),
			payment_update: Some(value.details.to_update()),
			conflicting_txids,
			candidates: value.candidates.clone(),
			expiry: Some(value.expiry),
		}
	}
}

pub(crate) struct PendingPaymentStore<L: Deref>
where
	L::Target: LdkLogger,
{
	inner: DataStore<PendingPaymentDetails, L>,
	mutation_lock: tokio::sync::Mutex<()>,
	manual_bolt11_payment_hash_index: Mutex<HashMap<PaymentHash, Vec<PaymentId>>>,
}

impl<L: Deref> PendingPaymentStore<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		pending_payments: Vec<PendingPaymentDetails>, primary_namespace: String,
		secondary_namespace: String, kv_store: Arc<DynStore>, logger: L,
	) -> Self {
		// TODO: Revisit this initialization once pending payments are no longer all kept in
		// memory.
		let manual_bolt11_payment_hash_index =
			Mutex::new(Self::build_manual_bolt11_payment_hash_index(&pending_payments));
		let inner = DataStore::new(
			pending_payments,
			primary_namespace,
			secondary_namespace,
			kv_store,
			logger,
		);
		Self { inner, mutation_lock: tokio::sync::Mutex::new(()), manual_bolt11_payment_hash_index }
	}

	pub(crate) async fn insert_or_update(
		&self, pending_payment: PendingPaymentDetails,
	) -> Result<bool, Error> {
		let _guard = self.mutation_lock.lock().await;
		self.insert_or_update_locked(pending_payment).await
	}

	pub(crate) async fn insert_manual_bolt11(
		&self, pending_payment: PendingPaymentDetails,
	) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let Some(payment_hash) = manual_bolt11_payment_hash(&pending_payment.details) else {
			debug_assert!(false, "manual BOLT11 insert requires a pending inbound BOLT11 payment");
			self.insert_or_update_locked(pending_payment).await?;
			return Ok(());
		};
		let duplicate = {
			let index = self.manual_bolt11_payment_hash_index.lock().expect("lock");
			index.get(&payment_hash).map_or(false, |ids| !ids.is_empty())
		};
		if duplicate {
			return Err(Error::DuplicatePayment);
		}

		self.insert_or_update_locked(pending_payment).await?;
		Ok(())
	}

	async fn insert_or_update_locked(
		&self, pending_payment: PendingPaymentDetails,
	) -> Result<bool, Error> {
		let id = pending_payment.id();
		let before = self.inner.get(&id);
		let updated = self.inner.insert_or_update(pending_payment).await?;
		if updated {
			let after = self.inner.get(&id);
			self.replace_in_index(before.as_ref(), after.as_ref());
		}
		Ok(updated)
	}

	pub(crate) async fn remove(&self, id: &PaymentId) -> Result<(), Error> {
		self.remove_batch(std::slice::from_ref(id)).await
	}

	pub(crate) async fn remove_batch(&self, ids: &[PaymentId]) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let (removed_payments, result) = self.inner.remove_batch_with_partial_result(ids).await;
		for payment in removed_payments {
			self.remove_from_index(&payment);
		}
		result
	}

	pub(crate) fn get(&self, id: &PaymentId) -> Option<PendingPaymentDetails> {
		self.inner.get(id)
	}

	pub(crate) fn contains_key(&self, id: &PaymentId) -> bool {
		self.inner.contains_key(id)
	}

	pub(crate) fn list_filter<F: FnMut(&&PendingPaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<PendingPaymentDetails> {
		self.inner.list_filter(f)
	}

	pub(crate) fn get_pending_manual_bolt11_by_payment_hash(
		&self, payment_hash: &PaymentHash,
	) -> Option<PendingPaymentDetails> {
		let index = self.manual_bolt11_payment_hash_index.lock().expect("lock");
		let ids = index.get(payment_hash)?;
		debug_assert!(
			ids.len() <= 1,
			"manual BOLT11 payment hash maps to multiple pending payment IDs"
		);
		ids.iter().find_map(|id| self.inner.get(id))
	}

	fn build_manual_bolt11_payment_hash_index(
		pending_payments: &[PendingPaymentDetails],
	) -> HashMap<PaymentHash, Vec<PaymentId>> {
		let mut index = HashMap::new();
		for payment in pending_payments {
			Self::insert_into_manual_bolt11_hash_index(&mut index, payment);
		}
		index
	}

	fn replace_in_index(
		&self, before: Option<&PendingPaymentDetails>, after: Option<&PendingPaymentDetails>,
	) {
		let mut index = self.manual_bolt11_payment_hash_index.lock().expect("lock");
		if let Some(payment) = before {
			Self::remove_from_manual_bolt11_hash_index(&mut index, payment);
		}
		if let Some(payment) = after {
			Self::insert_into_manual_bolt11_hash_index(&mut index, payment);
		}
	}

	fn remove_from_index(&self, payment: &PendingPaymentDetails) {
		let mut index = self.manual_bolt11_payment_hash_index.lock().expect("lock");
		Self::remove_from_manual_bolt11_hash_index(&mut index, payment);
	}

	fn insert_into_manual_bolt11_hash_index(
		index: &mut HashMap<PaymentHash, Vec<PaymentId>>, payment: &PendingPaymentDetails,
	) {
		if let Some(payment_hash) = manual_bolt11_payment_hash(&payment.details) {
			index.entry(payment_hash).or_default().push(payment.details.id);
		}
	}

	fn remove_from_manual_bolt11_hash_index(
		index: &mut HashMap<PaymentHash, Vec<PaymentId>>, payment: &PendingPaymentDetails,
	) {
		if let Some(payment_hash) = manual_bolt11_payment_hash(&payment.details) {
			if let Some(ids) = index.get_mut(&payment_hash) {
				ids.retain(|id| *id != payment.details.id);
				if ids.is_empty() {
					index.remove(&payment_hash);
				}
			}
		}
	}
}

fn manual_bolt11_payment_hash(payment: &PaymentDetails) -> Option<PaymentHash> {
	match payment.kind {
		PaymentKind::Bolt11 { hash, preimage: None, .. }
			if payment.direction == PaymentDirection::Inbound
				&& payment.status == PaymentStatus::Pending =>
		{
			Some(hash)
		},
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::pin::Pin;
	use std::sync::atomic::{AtomicUsize, Ordering};

	use bitcoin::hashes::Hash;
	use lightning::io;
	use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning::util::test_utils::TestLogger;
	use lightning_types::payment::PaymentSecret;

	use super::*;
	use crate::io::test_utils::InMemoryStore;
	use crate::payment::store::ConfirmationStatus;
	use crate::payment::{PaymentDirection, PaymentKind, PaymentStatus};
	use crate::types::{DynStore, DynStoreWrapper};

	#[test]
	fn pending_payment_candidate_lookup() {
		let payment_id = PaymentId([1u8; 32]);
		let first_txid = Txid::from_byte_array([2u8; 32]);
		let rbf_txid = Txid::from_byte_array([3u8; 32]);

		// A leading counterparty-initiated round we didn't contribute to (no figures), then our own
		// original and RBF candidates.
		let counterparty_txid = Txid::from_byte_array([4u8; 32]);
		let candidates = vec![
			FundingTxCandidate { txid: counterparty_txid, amount_msat: None, fee_paid_msat: None },
			FundingTxCandidate {
				txid: first_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(1_000),
			},
			FundingTxCandidate {
				txid: rbf_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(5_000),
			},
		];

		// The stored details only need to be a valid funding payment; `candidate` resolves figures
		// purely from the recorded candidate list.
		let details = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid: rbf_txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: None,
			},
			Some(1_000_000),
			Some(5_000),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		);
		let pending =
			PendingPaymentDetails::new(details, vec![first_txid, counterparty_txid], candidates);

		// Each candidate resolves to its own figures, so a non-last candidate that confirms reports
		// its own (lower) fee rather than the last-broadcast candidate's.
		assert_eq!(pending.candidate(first_txid).and_then(|c| c.fee_paid_msat), Some(1_000));
		assert_eq!(pending.candidate(rbf_txid).and_then(|c| c.fee_paid_msat), Some(5_000));
		// A candidate we didn't contribute to carries no figures, so the payment reports `None`
		// rather than another candidate's stale figures.
		let counterparty = pending.candidate(counterparty_txid).expect("candidate is recorded");
		assert_eq!(counterparty.amount_msat, None);
		assert_eq!(counterparty.fee_paid_msat, None);
		assert_eq!(pending.candidate(Txid::from_byte_array([9u8; 32])), None);
	}

	fn test_txid(byte: u8) -> Txid {
		Txid::from_byte_array([byte; 32])
	}

	fn pending_onchain_payment(payment_id: PaymentId, txid: Txid) -> PaymentDetails {
		PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain { txid, status: ConfirmationStatus::Unconfirmed, tx_type: None },
			Some(1_000),
			Some(100),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	fn pending_store() -> PendingPaymentStore<Arc<TestLogger>> {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		PendingPaymentStore::new(
			Vec::new(),
			"pending_payment_store_test_primary".to_string(),
			"pending_payment_store_test_secondary".to_string(),
			store,
			logger,
		)
	}

	struct FailSecondRemoveStore {
		inner: InMemoryStore,
		remove_calls: AtomicUsize,
	}

	impl FailSecondRemoveStore {
		fn new() -> Self {
			Self { inner: InMemoryStore::new(), remove_calls: AtomicUsize::new(0) }
		}
	}

	impl KVStore for FailSecondRemoveStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::write(&self.inner, primary_namespace, secondary_namespace, key, buf)
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			let remove_call = self.remove_calls.fetch_add(1, Ordering::Relaxed) + 1;
			let fut: Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> =
				if remove_call == 2 {
					Box::pin(async { Err(io::Error::new(io::ErrorKind::Other, "remove failed")) })
				} else {
					Box::pin(KVStore::remove(
						&self.inner,
						primary_namespace,
						secondary_namespace,
						key,
						false,
					))
				};
			fut
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for FailSecondRemoveStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			PaginatedKVStore::list_paginated(
				&self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	fn pending_store_with_fail_second_remove() -> PendingPaymentStore<Arc<TestLogger>> {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailSecondRemoveStore::new()));
		let logger = Arc::new(TestLogger::new());
		PendingPaymentStore::new(
			Vec::new(),
			"pending_payment_store_test_primary".to_string(),
			"pending_payment_store_test_secondary".to_string(),
			store,
			logger,
		)
	}

	fn pending_manual_bolt11_payment(
		payment_hash: PaymentHash, payment_secret: Option<PaymentSecret>,
	) -> PendingPaymentDetails {
		let payment_id = PaymentId(payment_hash.0);
		let details = PaymentDetails::new(
			payment_id,
			PaymentKind::Bolt11 {
				hash: payment_hash,
				preimage: None,
				secret: payment_secret,
				counterparty_skimmed_fee_msat: None,
			},
			Some(1_000),
			None,
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);
		PendingPaymentDetails::new_with_expiry(
			details,
			Vec::new(),
			Some(PendingPaymentExpiry::Time { timestamp: 1_000_000 }),
		)
	}

	async fn prune_expired(
		store: &PendingPaymentStore<Arc<TestLogger>>, now: u64,
	) -> Result<(), Error> {
		let expired_payment_ids = store
			.list_filter(|payment| payment.has_expired(now, 0))
			.into_iter()
			.map(|payment| payment.details.id)
			.collect::<Vec<_>>();
		store.remove_batch(&expired_payment_ids).await
	}

	#[tokio::test]
	async fn manual_bolt11_insert_rejects_duplicate_hash() {
		let store = pending_store();
		let payment_hash = PaymentHash([42; 32]);
		let pending_payment = pending_manual_bolt11_payment(payment_hash, None);
		assert_eq!(store.insert_manual_bolt11(pending_payment.clone()).await, Ok(()));

		let duplicate = pending_manual_bolt11_payment(payment_hash, None);
		assert_eq!(store.insert_manual_bolt11(duplicate).await, Err(Error::DuplicatePayment));
		assert_eq!(
			store.get_pending_manual_bolt11_by_payment_hash(&payment_hash),
			Some(pending_payment)
		);

		let updated = pending_manual_bolt11_payment(payment_hash, Some(PaymentSecret([43; 32])));
		assert_eq!(store.insert_or_update(updated.clone()).await, Ok(true));
		assert_eq!(store.get_pending_manual_bolt11_by_payment_hash(&payment_hash), Some(updated));
	}

	#[tokio::test]
	async fn expired_manual_bolt11_entries_can_be_retried_after_partial_prune_failure() {
		let store = pending_store_with_fail_second_remove();
		let first_hash = PaymentHash([41; 32]);
		let second_hash = PaymentHash([42; 32]);
		assert_eq!(
			store.insert_manual_bolt11(pending_manual_bolt11_payment(first_hash, None)).await,
			Ok(())
		);
		assert_eq!(
			store.insert_manual_bolt11(pending_manual_bolt11_payment(second_hash, None)).await,
			Ok(())
		);

		assert_eq!(prune_expired(&store, 1_000_001).await, Err(Error::PersistenceFailed));
		assert_eq!(store.list_filter(|_| true).len(), 1);

		assert_eq!(prune_expired(&store, 1_000_001).await, Ok(()));
		assert!(store.list_filter(|_| true).is_empty());

		assert_eq!(
			store.insert_manual_bolt11(pending_manual_bolt11_payment(first_hash, None)).await,
			Ok(())
		);
		assert_eq!(
			store.insert_manual_bolt11(pending_manual_bolt11_payment(second_hash, None)).await,
			Ok(())
		);
	}

	#[test]
	fn pending_onchain_conflicts_exclude_current_txid_after_txid_rotation() {
		let original_txid = test_txid(1);
		let replacement_txid = test_txid(2);
		let payment_id = PaymentId(original_txid.to_byte_array());

		let mut pending_payment = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, replacement_txid),
			vec![original_txid],
			Vec::new(),
		);
		let update = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, original_txid),
			Vec::new(),
			Vec::new(),
		)
		.to_update();

		assert!(pending_payment.update(update));
		assert_eq!(
			pending_payment.conflicting_txids,
			Vec::<Txid>::new(),
			"current txid must not remain in its own conflict list"
		);
	}
}
