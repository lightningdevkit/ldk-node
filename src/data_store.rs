// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable};

use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;
use crate::Error;

pub(crate) trait StorableObject: Clone + Readable + Writeable {
	type Id: StorableObjectId;
	type Update: StorableObjectUpdate<Self>;

	fn id(&self) -> Self::Id;
	fn update(&mut self, update: Self::Update) -> bool;
	fn to_update(&self) -> Self::Update;
}

pub(crate) trait StorableObjectId: std::hash::Hash + PartialEq + Eq {
	fn encode_to_hex_str(&self) -> String;
}

pub(crate) trait StorableObjectUpdate<SO: StorableObject> {
	fn id(&self) -> SO::Id;
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum DataStoreUpdateResult {
	Updated,
	Unchanged,
	NotFound,
}

pub(crate) struct DataStore<SO: StorableObject, L: Deref>
where
	L::Target: LdkLogger,
{
	objects: Mutex<HashMap<SO::Id, SO>>,
	// Serializes mutations against each other and against readers. Writers hold the write guard
	// across both the store write and the subsequent in-memory update, so readers taking the read
	// guard never observe the window in between, in which the store is already ahead of memory.
	//
	// Note the `objects` lock is always taken *inside* this one, and never held across an `.await`.
	mutation_lock: tokio::sync::RwLock<()>,
	primary_namespace: String,
	secondary_namespace: String,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<SO: StorableObject, L: Deref> DataStore<SO, L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		objects: Vec<SO>, primary_namespace: String, secondary_namespace: String,
		kv_store: Arc<DynStore>, logger: L,
	) -> Self {
		let objects =
			Mutex::new(HashMap::from_iter(objects.into_iter().map(|obj| (obj.id(), obj))));
		Self {
			objects,
			mutation_lock: tokio::sync::RwLock::new(()),
			primary_namespace,
			secondary_namespace,
			kv_store,
			logger,
		}
	}

	pub(crate) async fn insert(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.write().await;

		self.persist(&object).await?;
		let mut locked_objects = self.objects.lock().expect("lock");
		let updated = locked_objects.insert(object.id(), object).is_some();
		Ok(updated)
	}

	/// Like [`Self::insert`], but when an entry with the object's id already exists, merges the
	/// object's full update ([`StorableObject::to_update`]) into it instead of replacing it.
	pub(crate) async fn insert_or_update(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.write().await;

		let id = object.id();
		let data_to_persist = {
			let locked_objects = self.objects.lock().expect("lock");
			if let Some(existing_object) = locked_objects.get(&id) {
				let mut updated_object = existing_object.clone();
				let updated = updated_object.update(object.to_update());
				if updated {
					Some(updated_object)
				} else {
					None
				}
			} else {
				Some(object)
			}
		};

		match data_to_persist {
			Some(updated_object) => {
				self.persist(&updated_object).await?;
				let mut locked_objects = self.objects.lock().expect("lock");
				locked_objects.insert(id, updated_object);
				Ok(true)
			},
			None => Ok(false),
		}
	}

	pub(crate) async fn remove(&self, id: &SO::Id) -> Result<(), Error> {
		let _guard = self.mutation_lock.write().await;
		let should_remove = { self.objects.lock().expect("lock").contains_key(id) };
		if should_remove {
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
			})?;
			self.objects.lock().expect("lock").remove(id);
		}
		Ok(())
	}

	/// Returns the object stored under `id`, if any.
	pub(crate) async fn get(&self, id: &SO::Id) -> Result<Option<SO>, Error> {
		let _guard = self.mutation_lock.read().await;
		Ok(self.objects.lock().expect("lock").get(id).cloned())
	}

	pub(crate) async fn update(&self, update: SO::Update) -> Result<DataStoreUpdateResult, Error> {
		let _guard = self.mutation_lock.write().await;
		let id = update.id();
		let updated_object = {
			let locked_objects = self.objects.lock().expect("lock");
			let Some(object) = locked_objects.get(&id) else {
				return Ok(DataStoreUpdateResult::NotFound);
			};
			let mut updated_object = object.clone();
			if !updated_object.update(update) {
				return Ok(DataStoreUpdateResult::Unchanged);
			}
			updated_object
		};

		self.persist(&updated_object).await?;
		let mut locked_objects = self.objects.lock().expect("lock");
		locked_objects.insert(id, updated_object);
		Ok(DataStoreUpdateResult::Updated)
	}

	/// Atomically transforms the entry for `id` through `f` and persists the result.
	///
	/// `f` receives the current entry (`None` when absent) and returns the new state to write;
	/// returning `None` leaves the store untouched. The read, the closure, and the write share
	/// one critical section of the mutation lock, so no concurrent writer can land in between —
	/// unlike a separate [`Self::get`] followed by an insert or update.
	///
	/// The closure runs on a clone of the entry with the in-memory map lock released, so it may
	/// freely read this store or others (reads see the pre-mutation state) without ordering map
	/// locks against each other. Keep it cheap and non-blocking.
	///
	/// Returns the written object, or `None` when the closure declined to write.
	pub(crate) async fn mutate<F: FnOnce(Option<&SO>) -> Option<SO>>(
		&self, id: &SO::Id, f: F,
	) -> Result<Option<SO>, Error> {
		self.mutate_with(id, |current| async move { Ok(f(current.as_ref())) }).await
	}

	async fn mutate_with<F, Fut>(&self, id: &SO::Id, f: F) -> Result<Option<SO>, Error>
	where
		F: FnOnce(Option<SO>) -> Fut,
		Fut: Future<Output = Result<Option<SO>, Error>>,
	{
		let _guard = self.mutation_lock.lock().await;

		let current = self.objects.lock().expect("lock").get(id).cloned();
		let new_object = match f(current).await? {
			Some(new_object) => new_object,
			None => return Ok(None),
		};
		debug_assert!(new_object.id() == *id, "mutate closure must not change the object's id");

		self.persist(&new_object).await?;
		let mut locked_objects = self.objects.lock().expect("lock");
		locked_objects.insert(new_object.id(), new_object.clone());
		Ok(Some(new_object))
	}

	/// Like [`Self::mutate`], but allows the transformation to await fallible reads.
	///
	/// The mutation lock remains held while `f` runs. This is useful when the new state must be
	/// decided from an async read of another store without letting a concurrent writer invalidate
	/// that decision. Callers must keep cross-store lock ordering consistent to avoid deadlocks.
	pub(crate) async fn mutate_async<F, Fut>(&self, id: &SO::Id, f: F) -> Result<Option<SO>, Error>
	where
		F: FnOnce(Option<SO>) -> Fut,
		Fut: Future<Output = Result<Option<SO>, Error>>,
	{
		self.mutate_with(id, f).await
	}

	/// Returns all stored objects matching `f`.
	pub(crate) async fn list_filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Vec<SO> {
		let _guard = self.mutation_lock.read().await;
		self.objects.lock().expect("lock").values().filter(f).cloned().collect::<Vec<SO>>()
	}

	async fn persist(&self, object: &SO) -> Result<(), Error> {
		let (store_key, data) = Self::encode_object(object);
		self.persist_encoded(store_key, data).await
	}

	fn encode_object(object: &SO) -> (String, Vec<u8>) {
		(object.id().encode_to_hex_str(), object.encode())
	}

	async fn persist_encoded(&self, store_key: String, data: Vec<u8>) -> Result<(), Error> {
		KVStore::write(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&store_key,
			data,
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
		})?;
		Ok(())
	}

	/// Returns whether an object is stored under `id`.
	pub(crate) async fn contains_key(&self, id: &SO::Id) -> Result<bool, Error> {
		let _guard = self.mutation_lock.read().await;
		Ok(self.objects.lock().expect("lock").contains_key(id))
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use lightning::util::persist::{PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning::util::test_utils::TestLogger;
	use lightning::{impl_writeable_tlv_based, io};
	use tokio::sync::Notify;

	use super::*;
	use crate::hex_utils;
	use crate::io::test_utils::InMemoryStore;
	use crate::types::DynStoreWrapper;

	#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
	struct TestObjectId {
		id: [u8; 4],
	}

	impl StorableObjectId for TestObjectId {
		fn encode_to_hex_str(&self) -> String {
			hex_utils::to_string(&self.id)
		}
	}
	impl_writeable_tlv_based!(TestObjectId, { (0, id, required) });

	struct TestObjectUpdate {
		id: TestObjectId,
		data: [u8; 3],
	}
	impl StorableObjectUpdate<TestObject> for TestObjectUpdate {
		fn id(&self) -> TestObjectId {
			self.id
		}
	}

	#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
	struct TestObject {
		id: TestObjectId,
		data: [u8; 3],
	}

	impl StorableObject for TestObject {
		type Id = TestObjectId;
		type Update = TestObjectUpdate;

		fn id(&self) -> Self::Id {
			self.id
		}

		fn update(&mut self, update: Self::Update) -> bool {
			if self.data != update.data {
				self.data = update.data;
				true
			} else {
				false
			}
		}

		fn to_update(&self) -> Self::Update {
			Self::Update { id: self.id, data: self.data }
		}
	}

	impl_writeable_tlv_based!(TestObject, {
		(0, id, required),
		(2, data, required),
	});

	struct FailingStore;

	impl KVStore for FailingStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "read failed")) }
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "write failed")) }
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "remove failed")) }
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "list failed")) }
		}
	}

	impl PaginatedKVStore for FailingStore {
		fn list_paginated(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
			_page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			async { Err(io::Error::new(io::ErrorKind::Other, "list_paginated failed")) }
		}
	}

	fn new_failing_data_store(objects: Vec<TestObject>) -> DataStore<TestObject, Arc<TestLogger>> {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let logger = Arc::new(TestLogger::new());
		DataStore::new(
			objects,
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			store,
			logger,
		)
	}

	/// A store that parks every `write` until it is released, so that tests can hold a write in
	/// flight and observe what concurrent readers see in the meantime.
	struct GatedStore {
		inner: InMemoryStore,
		/// Notified by the store once a `write` has parked.
		write_parked: Arc<Notify>,
		/// Awaited by the store; notify to let the parked `write` proceed.
		release_write: Arc<Notify>,
	}

	impl KVStore for GatedStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			self.inner.read(primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			let write_parked = Arc::clone(&self.write_parked);
			let release_write = Arc::clone(&self.release_write);
			let inner_fut = self.inner.write(primary_namespace, secondary_namespace, key, buf);
			async move {
				write_parked.notify_one();
				release_write.notified().await;
				inner_fut.await
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			self.inner.remove(primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			self.inner.list(primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for GatedStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			self.inner.list_paginated(primary_namespace, secondary_namespace, page_token)
		}
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn readers_wait_for_in_flight_writes() {
		let write_parked = Arc::new(Notify::new());
		let release_write = Arc::new(Notify::new());
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(GatedStore {
			inner: InMemoryStore::new(),
			write_parked: Arc::clone(&write_parked),
			release_write: Arc::clone(&release_write),
		}));
		let logger = Arc::new(TestLogger::new());

		let id = TestObjectId { id: [42u8; 4] };
		let old_object = TestObject { id, data: [23u8; 3] };
		let new_object = TestObject { id, data: [24u8; 3] };

		let data_store: Arc<DataStore<TestObject, Arc<TestLogger>>> = Arc::new(DataStore::new(
			vec![old_object],
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			store,
			logger,
		));

		let writer_store = Arc::clone(&data_store);
		let writer = tokio::spawn(async move { writer_store.insert(new_object).await });

		// Wait until the write has been handed to the store and parked there, i.e., until the
		// object has been persisted but the in-memory state has not caught up yet.
		write_parked.notified().await;

		// A reader must not be able to observe that window: it has to wait for the writer rather
		// than hand out the pre-write object.
		let read_res = tokio::time::timeout(Duration::from_millis(200), data_store.get(&id)).await;
		assert!(
			read_res.is_err(),
			"Reader observed {:?} while a write was still in flight",
			read_res.unwrap()
		);

		release_write.notify_one();
		assert_eq!(Ok(true), writer.await.unwrap());
		assert_eq!(Some(new_object), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn data_is_persisted() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let primary_namespace = "datastore_test_primary".to_string();
		let secondary_namespace = "datastore_test_secondary".to_string();
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			Vec::new(),
			primary_namespace.clone(),
			secondary_namespace.clone(),
			Arc::clone(&store),
			logger,
		);

		let id = TestObjectId { id: [42u8; 4] };
		assert!(data_store.get(&id).await.unwrap().is_none());

		let store_key = id.encode_to_hex_str();

		// Check we start empty.
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_err());

		// Check we successfully store an object and return `false`
		let object = TestObject { id, data: [23u8; 3] };
		assert_eq!(Ok(false), data_store.insert(object.clone()).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_ok());

		// Test re-insertion returns `true`
		let mut override_object = object.clone();
		override_object.data = [24u8; 3];
		assert_eq!(Ok(true), data_store.insert(override_object).await);
		assert_eq!(Some(override_object), data_store.get(&id).await.unwrap());

		// Check update returns `Updated`
		let update = TestObjectUpdate { id, data: [25u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::Updated), data_store.update(update).await);
		assert_eq!(data_store.get(&id).await.unwrap().unwrap().data, [25u8; 3]);

		// Check no-op update yields `Unchanged`
		let update = TestObjectUpdate { id, data: [25u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::Unchanged), data_store.update(update).await);

		// Check bogus update yields `NotFound`
		let bogus_id = TestObjectId { id: [84u8; 4] };
		let update = TestObjectUpdate { id: bogus_id, data: [12u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::NotFound), data_store.update(update).await);

		// Check `insert_or_update` inserts unknown objects
		let iou_id = TestObjectId { id: [55u8; 4] };
		let iou_object = TestObject { id: iou_id, data: [34u8; 3] };
		assert_eq!(Ok(true), data_store.insert_or_update(iou_object.clone()).await);

		// Check `insert_or_update` doesn't update the same object
		assert_eq!(Ok(false), data_store.insert_or_update(iou_object.clone()).await);

		// Check `insert_or_update` updates if object changed
		let mut new_iou_object = iou_object;
		new_iou_object.data[0] += 1;
		assert_eq!(Ok(true), data_store.insert_or_update(new_iou_object).await);
	}

	#[tokio::test]
	async fn mutate_inserts_when_absent() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let primary_namespace = "datastore_test_primary".to_string();
		let secondary_namespace = "datastore_test_secondary".to_string();
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			Vec::new(),
			primary_namespace.clone(),
			secondary_namespace.clone(),
			Arc::clone(&store),
			logger,
		);

		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let result = data_store
			.mutate(&id, |existing| {
				assert!(existing.is_none());
				Some(object)
			})
			.await;
		assert_eq!(Ok(Some(object)), result);

		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
		let store_key = id.encode_to_hex_str();
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_ok());
	}

	#[tokio::test]
	async fn mutate_transforms_existing_entry() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject { id, data: [23u8; 3] };
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![existing_object],
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			store,
			logger,
		);

		// The closure sees the current entry and derives the new state from it.
		let result = data_store
			.mutate(&id, |existing| {
				let mut new_object = *existing.unwrap();
				new_object.data[0] += 1;
				Some(new_object)
			})
			.await;
		let expected = TestObject { id, data: [24u8, 23u8, 23u8] };
		assert_eq!(Ok(Some(expected)), result);
		assert_eq!(Some(expected), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn mutate_async_awaits_fallible_reads() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let id = TestObjectId { id: [42u8; 4] };
		let other_id = TestObjectId { id: [43u8; 4] };
		let existing_object = TestObject { id, data: [23u8; 3] };
		let other_object = TestObject { id: other_id, data: [24u8; 3] };
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![existing_object],
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			Arc::clone(&store),
			Arc::clone(&logger),
		);
		let other_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![other_object],
			"other_datastore_test_primary".to_string(),
			"other_datastore_test_secondary".to_string(),
			store,
			logger,
		);

		let result = data_store
			.mutate_async(&id, |existing| async move {
				let mut updated = existing.unwrap();
				updated.data = other_store.get(&other_id).await?.unwrap().data;
				Ok(Some(updated))
			})
			.await;
		let expected = TestObject { id, data: [24u8; 3] };
		assert_eq!(Ok(Some(expected)), result);
		assert_eq!(Some(expected), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn mutate_runs_the_closure_without_the_map_lock() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject { id, data: [23u8; 3] };
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			vec![existing_object],
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			store,
			logger,
		);

		// Closures gate cross-store decisions on reads of other stores, which lock their own
		// in-memory maps. Holding this store's map lock across the closure would order it
		// before theirs and invite lock-order inversions, so the closure must run with the map
		// lock released.
		let result = data_store
			.mutate(&id, |existing| {
				assert_eq!(Some(&existing_object), existing);
				assert!(data_store.objects.try_lock().is_ok());
				None
			})
			.await;
		assert_eq!(Ok(None), result);
	}

	#[tokio::test]
	async fn mutate_persists_nothing_when_closure_declines() {
		let id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![existing_object]);

		// Returning `None` must not attempt a write (the store fails all writes) nor touch memory.
		let result = data_store
			.mutate(&id, |existing| {
				assert_eq!(Some(&existing_object), existing);
				None
			})
			.await;
		assert_eq!(Ok(None), result);
		assert_eq!(Some(existing_object), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn mutate_does_not_mutate_memory_if_persist_fails() {
		let existing_id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject { id: existing_id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![existing_object]);

		let changed = TestObject { id: existing_id, data: [24u8; 3] };
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.mutate(&existing_id, |_| Some(changed)).await
		);
		assert_eq!(Some(existing_object), data_store.get(&existing_id).await.unwrap());

		let new_id = TestObjectId { id: [55u8; 4] };
		let new_object = TestObject { id: new_id, data: [34u8; 3] };
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.mutate(&new_id, |_| Some(new_object)).await
		);
		assert!(data_store.get(&new_id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn insert_or_update_does_not_mutate_memory_if_persist_fails() {
		let existing_id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject { id: existing_id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![existing_object]);

		let updated_object = TestObject { id: existing_id, data: [24u8; 3] };
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.insert_or_update(updated_object).await
		);
		assert_eq!(Some(existing_object), data_store.get(&existing_id).await.unwrap());

		let new_id = TestObjectId { id: [55u8; 4] };
		let new_object = TestObject { id: new_id, data: [34u8; 3] };
		assert_eq!(Err(Error::PersistenceFailed), data_store.insert_or_update(new_object).await);
		assert!(data_store.get(&new_id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn insert_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![]);

		assert_eq!(Err(Error::PersistenceFailed), data_store.insert(object).await);
		assert!(data_store.get(&id).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn update_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![object]);

		let update = TestObjectUpdate { id, data: [24u8; 3] };
		assert_eq!(Err(Error::PersistenceFailed), data_store.update(update).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
	}

	#[tokio::test]
	async fn remove_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![object]);

		assert_eq!(Err(Error::PersistenceFailed), data_store.remove(&id).await);
		assert_eq!(Some(object), data_store.get(&id).await.unwrap());
	}
}
