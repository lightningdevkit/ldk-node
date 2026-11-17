// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! The wallet's payment stores behind one API, so that every write the wallet makes to them
//! happens under the lock that keeps a payment record and its tracker entry consistent.

use std::ops::Deref;
use std::sync::Arc;

use lightning::ln::channelmanager::PaymentId;

use crate::payment::PaymentDetails;
use crate::types::{OnchainPaymentTracker, PaymentStore};
use crate::wallet::payment_tracker::TrackedOnchainPayment;
use crate::Error;

/// The wallet's payment store and payment tracker, with the lock serializing their writers.
///
/// The writers must observe the payment record and its tracker entry (candidate history
/// included) as one consistent unit: wallet sync's event arms and the funding-record writers each
/// hold the lock from payment-id resolution through their last write. Without the lock, a
/// confirmation landing between a writer's two store writes sees the record but not the candidate
/// history — resolving the wrong payment id or stamping the confirmed candidate with another
/// candidate's figures — and a funding-record write landing inside an arm's decision sequence gets
/// overwritten by the arm's stale generic fallback.
///
/// The writes are methods of [`PaymentStoresGuard`], which only [`Self::lock`] hands out, so a
/// write compiles only for a holder of the lock. The reads are methods of this type and take no
/// lock; a caller whose write depends on what it read takes the lock first and reads through the
/// guard.
pub(super) struct PaymentStores {
	payment_store: Arc<PaymentStore>,
	payment_tracker: Arc<OnchainPaymentTracker>,
	update_lock: tokio::sync::Mutex<()>,
}

/// Exclusive access to the writers of a [`PaymentStores`], held by the holder of its lock and by
/// no one else. It dereferences to the stores, so their reads are available under the lock too.
#[must_use = "dropping the guard releases the lock at once"]
pub(super) struct PaymentStoresGuard<'a> {
	stores: &'a PaymentStores,
	_guard: tokio::sync::MutexGuard<'a, ()>,
}

impl PaymentStores {
	pub(super) fn new(
		payment_store: Arc<PaymentStore>, payment_tracker: Arc<OnchainPaymentTracker>,
	) -> Self {
		Self { payment_store, payment_tracker, update_lock: tokio::sync::Mutex::new(()) }
	}

	/// Takes the lock for as long as the returned guard lives.
	pub(super) async fn lock(&self) -> PaymentStoresGuard<'_> {
		PaymentStoresGuard { stores: self, _guard: self.update_lock.lock().await }
	}

	/// The payment record stored under `id`, if any.
	pub(super) async fn payment(&self, id: &PaymentId) -> Result<Option<PaymentDetails>, Error> {
		self.payment_store.get(id).await
	}

	/// The tracker entry stored under `id`, if any.
	pub(super) async fn tracked_payment(
		&self, id: &PaymentId,
	) -> Result<Option<TrackedOnchainPayment>, Error> {
		self.payment_tracker.get(id).await
	}

	/// The tracker entries matching `f`.
	pub(super) async fn tracked_payments<F: FnMut(&&TrackedOnchainPayment) -> bool>(
		&self, f: F,
	) -> Vec<TrackedOnchainPayment> {
		self.payment_tracker.list_filter(f).await
	}
}

#[cfg(test)]
impl PaymentStores {
	/// The payment store itself, for tests to set up and inspect records around the wallet's API.
	pub(super) fn payment_store(&self) -> &PaymentStore {
		&self.payment_store
	}

	/// The payment tracker itself, for tests to set up and inspect entries around the
	/// wallet's API.
	pub(super) fn payment_tracker(&self) -> &OnchainPaymentTracker {
		&self.payment_tracker
	}
}

impl Deref for PaymentStoresGuard<'_> {
	type Target = PaymentStores;

	fn deref(&self) -> &Self::Target {
		self.stores
	}
}

impl PaymentStoresGuard<'_> {
	/// Stores `details`, merging its update into the record already stored under its id, if any.
	/// Returns whether anything was written.
	pub(super) async fn insert_or_update_payment(
		&self, details: PaymentDetails,
	) -> Result<bool, Error> {
		self.stores.payment_store.insert_or_update(details).await
	}

	/// Removes the payment record stored under `id`, if any.
	pub(super) async fn remove_payment(&self, id: &PaymentId) -> Result<(), Error> {
		self.stores.payment_store.remove(id).await
	}

	/// Transforms the payment record stored under `id` through `f` and persists the result, all
	/// in one critical section of the store; see
	/// [`DataStore::mutate`](crate::data_store::DataStore::mutate).
	pub(super) async fn mutate_payment<F>(
		&self, id: &PaymentId, f: F,
	) -> Result<Option<PaymentDetails>, Error>
	where
		F: FnOnce(Option<&PaymentDetails>) -> Option<PaymentDetails>,
	{
		self.stores.payment_store.mutate(id, f).await
	}

	/// Removes the tracker entry stored under `id`, if any.
	pub(super) async fn remove_tracked_payment(&self, id: &PaymentId) -> Result<(), Error> {
		self.stores.payment_tracker.remove(id).await
	}

	/// Transforms the tracker entry stored under `id` through `f` and persists the result,
	/// all in one critical section of the store; see
	/// [`DataStore::mutate`](crate::data_store::DataStore::mutate).
	pub(super) async fn mutate_tracked_payment<F>(
		&self, id: &PaymentId, f: F,
	) -> Result<Option<TrackedOnchainPayment>, Error>
	where
		F: FnOnce(Option<&TrackedOnchainPayment>) -> Option<TrackedOnchainPayment>,
	{
		self.stores.payment_tracker.mutate(id, f).await
	}
}
