// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
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
	mutation_lock: tokio::sync::Mutex<()>,
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
			mutation_lock: tokio::sync::Mutex::new(()),
			primary_namespace,
			secondary_namespace,
			kv_store,
			logger,
		}
	}

	pub(crate) async fn insert(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.lock().await;

		self.persist(&object).await?;
		let mut locked_objects = self.objects.lock().expect("lock");
		let updated = locked_objects.insert(object.id(), object).is_some();
		Ok(updated)
	}

	pub(crate) async fn insert_or_update(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.lock().await;

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
		let _guard = self.mutation_lock.lock().await;
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

	/// Returns the current in-memory object for `id`.
	///
	/// The async mutation lock serializes writers, but this synchronous reader cannot wait on it.
	/// Until store reads are async, callers may temporarily see in-memory state that has not yet
	/// caught up to a write in progress.
	pub(crate) fn get(&self, id: &SO::Id) -> Option<SO> {
		self.objects.lock().expect("lock").get(id).cloned()
	}

	pub(crate) async fn update(&self, update: SO::Update) -> Result<DataStoreUpdateResult, Error> {
		let _guard = self.mutation_lock.lock().await;
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

	/// Returns in-memory objects matching `f`.
	///
	/// The async mutation lock serializes writers, but this synchronous reader cannot wait on it.
	/// Until store reads are async, callers may temporarily see in-memory state that has not yet
	/// caught up to a write in progress.
	pub(crate) fn list_filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Vec<SO> {
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

	/// Returns whether the in-memory store contains `id`.
	///
	/// The async mutation lock serializes writers, but this synchronous reader cannot wait on it.
	/// Until store reads are async, callers may temporarily see in-memory state that has not yet
	/// caught up to a write in progress.
	pub(crate) fn contains_key(&self, id: &SO::Id) -> bool {
		self.objects.lock().expect("lock").contains_key(id)
	}
}

#[cfg(test)]
mod tests {
	use lightning::impl_writeable_tlv_based;
	use lightning::io;
	use lightning::util::test_utils::TestLogger;

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
		assert!(data_store.get(&id).is_none());

		let store_key = id.encode_to_hex_str();

		// Check we start empty.
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_err());

		// Check we successfully store an object and return `false`
		let object = TestObject { id, data: [23u8; 3] };
		assert_eq!(Ok(false), data_store.insert(object.clone()).await);
		assert_eq!(Some(object), data_store.get(&id));
		assert!(KVStore::read(&*store, &primary_namespace, &secondary_namespace, &store_key)
			.await
			.is_ok());

		// Test re-insertion returns `true`
		let mut override_object = object.clone();
		override_object.data = [24u8; 3];
		assert_eq!(Ok(true), data_store.insert(override_object).await);
		assert_eq!(Some(override_object), data_store.get(&id));

		// Check update returns `Updated`
		let update = TestObjectUpdate { id, data: [25u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::Updated), data_store.update(update).await);
		assert_eq!(data_store.get(&id).unwrap().data, [25u8; 3]);

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
	async fn insert_or_update_does_not_mutate_memory_if_persist_fails() {
		let existing_id = TestObjectId { id: [42u8; 4] };
		let existing_object = TestObject { id: existing_id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![existing_object]);

		let updated_object = TestObject { id: existing_id, data: [24u8; 3] };
		assert_eq!(
			Err(Error::PersistenceFailed),
			data_store.insert_or_update(updated_object).await
		);
		assert_eq!(Some(existing_object), data_store.get(&existing_id));

		let new_id = TestObjectId { id: [55u8; 4] };
		let new_object = TestObject { id: new_id, data: [34u8; 3] };
		assert_eq!(Err(Error::PersistenceFailed), data_store.insert_or_update(new_object).await);
		assert!(data_store.get(&new_id).is_none());
	}

	#[tokio::test]
	async fn insert_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![]);

		assert_eq!(Err(Error::PersistenceFailed), data_store.insert(object).await);
		assert!(data_store.get(&id).is_none());
	}

	#[tokio::test]
	async fn update_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![object]);

		let update = TestObjectUpdate { id, data: [24u8; 3] };
		assert_eq!(Err(Error::PersistenceFailed), data_store.update(update).await);
		assert_eq!(Some(object), data_store.get(&id));
	}

	#[tokio::test]
	async fn remove_does_not_mutate_memory_if_persist_fails() {
		let id = TestObjectId { id: [42u8; 4] };
		let object = TestObject { id, data: [23u8; 3] };
		let data_store = new_failing_data_store(vec![object]);

		assert_eq!(Err(Error::PersistenceFailed), data_store.remove(&id).await);
		assert_eq!(Some(object), data_store.get(&id));
	}
}
