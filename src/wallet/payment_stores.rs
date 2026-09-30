// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! The wallet's payment stores behind one API, so that every write the wallet makes to them
//! happens under the lock that keeps a payment record and its pending-store entry consistent.

use std::ops::Deref;
use std::sync::Arc;

use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::PageToken;

use crate::data_store::DataStorePage;
use crate::payment::{PaymentDetails, PendingPaymentDetails};
use crate::types::{PaymentStore, PendingPaymentStore};
use crate::Error;

/// The wallet's payment store and pending payment store, with the lock serializing their writers.
///
/// The writers must observe the payment record and its pending-store entry (candidate history
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
	pending_payment_store: Arc<PendingPaymentStore>,
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
		payment_store: Arc<PaymentStore>, pending_payment_store: Arc<PendingPaymentStore>,
	) -> Self {
		Self { payment_store, pending_payment_store, update_lock: tokio::sync::Mutex::new(()) }
	}

	/// Takes the lock for as long as the returned guard lives.
	pub(super) async fn lock(&self) -> PaymentStoresGuard<'_> {
		PaymentStoresGuard { stores: self, _guard: self.update_lock.lock().await }
	}

	/// The payment record stored under `id`, if any.
	pub(super) async fn payment(&self, id: &PaymentId) -> Result<Option<PaymentDetails>, Error> {
		self.payment_store.get(id).await
	}

	/// The pending-store entry stored under `id`, if any.
	pub(super) async fn pending_payment(
		&self, id: &PaymentId,
	) -> Result<Option<PendingPaymentDetails>, Error> {
		self.pending_payment_store.get(id).await
	}

	/// A page of payment records, ordered from most recently created to least recently created;
	/// see [`DataStore::list_page`](crate::data_store::DataStore::list_page).
	pub(super) async fn payments_page(
		&self, page_token: Option<PageToken>,
	) -> Result<DataStorePage<PaymentDetails>, Error> {
		self.payment_store.list_page(page_token).await
	}

	/// Whether the pending store has an entry under `id`.
	pub(super) async fn has_pending_payment(&self, id: &PaymentId) -> Result<bool, Error> {
		self.pending_payment_store.contains_key(id).await
	}

	/// The pending-store entries matching `f`.
	pub(super) async fn pending_payments<F: FnMut(&&PendingPaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<PendingPaymentDetails> {
		self.pending_payment_store.list_filter(f).await
	}
}

#[cfg(test)]
impl PaymentStores {
	/// The payment store itself, for tests to set up and inspect records around the wallet's API.
	pub(super) fn payment_store(&self) -> &PaymentStore {
		&self.payment_store
	}

	/// The pending payment store itself, for tests to set up and inspect entries around the
	/// wallet's API.
	pub(super) fn pending_payment_store(&self) -> &PendingPaymentStore {
		&self.pending_payment_store
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

	/// Removes the pending-store entry stored under `id`, if any.
	pub(super) async fn remove_pending_payment(&self, id: &PaymentId) -> Result<(), Error> {
		self.stores.pending_payment_store.remove(id).await
	}

	/// Transforms the pending-store entry stored under `id` through `f` and persists the result,
	/// all in one critical section of the store; see
	/// [`DataStore::mutate`](crate::data_store::DataStore::mutate).
	pub(super) async fn mutate_pending_payment<F>(
		&self, id: &PaymentId, f: F,
	) -> Result<Option<PendingPaymentDetails>, Error>
	where
		F: FnOnce(Option<&PendingPaymentDetails>) -> Option<PendingPaymentDetails>,
	{
		self.stores.pending_payment_store.mutate(id, f).await
	}
}
