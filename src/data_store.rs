// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use lightning::util::persist::{KVStore, PageToken};
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

pub(crate) trait StorableObjectId: Clone + std::hash::Hash + Eq {
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

struct InMemoryObjects<SO: StorableObject> {
	objects: HashMap<SO::Id, SO>,
	creation_order: BTreeMap<u64, SO::Id>,
	creation_order_by_key: HashMap<String, u64>,
	next_creation_order: u64,
}

pub(crate) struct DataStore<SO: StorableObject, L: Deref>
where
	L::Target: LdkLogger,
{
	objects: Mutex<InMemoryObjects<SO>>,
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
		let object_count = objects.len();
		let mut creation_order = BTreeMap::new();
		let mut creation_order_by_key = HashMap::with_capacity(object_count);
		let mut objects_by_id = HashMap::with_capacity(objects.len());
		for (creation_index, object) in objects.into_iter().rev().enumerate() {
			let id = object.id();
			let order = creation_index as u64;
			creation_order.insert(order, id.clone());
			creation_order_by_key.insert(id.encode_to_hex_str(), order);
			objects_by_id.insert(id, object);
		}
		let objects = Mutex::new(InMemoryObjects {
			objects: objects_by_id,
			creation_order,
			creation_order_by_key,
			next_creation_order: object_count as u64,
		});
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
		let id = object.id();
		let updated = locked_objects.objects.insert(id.clone(), object).is_some();
		if !updated {
			let creation_order = locked_objects.next_creation_order;
			locked_objects.next_creation_order =
				creation_order.checked_add(1).expect("creation order overflow");
			locked_objects.creation_order.insert(creation_order, id.clone());
			locked_objects.creation_order_by_key.insert(id.encode_to_hex_str(), creation_order);
		}
		Ok(updated)
	}

	pub(crate) async fn insert_or_update(&self, object: SO) -> Result<bool, Error> {
		let _guard = self.mutation_lock.lock().await;

		let id = object.id();
		let data_to_persist = {
			let locked_objects = self.objects.lock().expect("lock");
			if let Some(existing_object) = locked_objects.objects.get(&id) {
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
				let is_new = locked_objects.objects.insert(id.clone(), updated_object).is_none();
				if is_new {
					let creation_order = locked_objects.next_creation_order;
					locked_objects.next_creation_order =
						creation_order.checked_add(1).expect("creation order overflow");
					locked_objects.creation_order.insert(creation_order, id.clone());
					locked_objects
						.creation_order_by_key
						.insert(id.encode_to_hex_str(), creation_order);
				}
				Ok(true)
			},
			None => Ok(false),
		}
	}

	pub(crate) async fn remove(&self, id: &SO::Id) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let should_remove = { self.objects.lock().expect("lock").objects.contains_key(id) };
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
			let mut locked_objects = self.objects.lock().expect("lock");
			locked_objects.objects.remove(id);
			if let Some(creation_order) = locked_objects.creation_order_by_key.remove(&store_key) {
				locked_objects.creation_order.remove(&creation_order);
			}
		}
		Ok(())
	}

	/// Returns the current in-memory object for `id`.
	///
	/// The async mutation lock serializes writers, but this synchronous reader cannot wait on it.
	/// Until store reads are async, callers may temporarily see in-memory state that has not yet
	/// caught up to a write in progress.
	pub(crate) fn get(&self, id: &SO::Id) -> Option<SO> {
		self.objects.lock().expect("lock").objects.get(id).cloned()
	}

	pub(crate) async fn update(&self, update: SO::Update) -> Result<DataStoreUpdateResult, Error> {
		let _guard = self.mutation_lock.lock().await;
		let id = update.id();
		let updated_object = {
			let locked_objects = self.objects.lock().expect("lock");
			let Some(object) = locked_objects.objects.get(&id) else {
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
		locked_objects.objects.insert(id, updated_object);
		Ok(DataStoreUpdateResult::Updated)
	}

	/// Returns in-memory objects matching `f`.
	///
	/// The async mutation lock serializes writers, but this synchronous reader cannot wait on it.
	/// Until store reads are async, callers may temporarily see in-memory state that has not yet
	/// caught up to a write in progress.
	pub(crate) fn list_filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Vec<SO> {
		self.objects.lock().expect("lock").objects.values().filter(f).cloned().collect()
	}

	/// Returns a page of objects, ordered from most recently created to least recently created,
	/// together with a token that can be passed to a subsequent call to retrieve the next page.
	pub(crate) fn list_page(
		&self, page_token: Option<PageToken>,
	) -> Result<(Vec<SO>, Option<PageToken>), Error> {
		const PAGE_SIZE: usize = 50;

		let locked_objects = self.objects.lock().expect("lock");
		let start_order = if let Some(token) = page_token {
			// The key resolves the current order across reloads, while the encoded order lets us
			// continue if the key was removed after the token was issued.
			let (encoded_order, key) = token
				.as_str()
				.split_once(':')
				.and_then(|(order, key)| order.parse::<u64>().ok().map(|order| (order, key)))
				.ok_or_else(|| {
					log_error!(self.logger, "Object page token not found: {}", token);
					Error::InvalidPageToken
				})?;
			if let Some(current_order) = locked_objects.creation_order_by_key.get(key) {
				// Reloading after an older removal shifts the current order down, while removing and
				// reinserting the cursor shifts it up. In either case, resume from the older position.
				encoded_order.min(*current_order)
			} else if encoded_order < locked_objects.next_creation_order {
				encoded_order
			} else {
				log_error!(self.logger, "Object page token not found: {}", token);
				return Err(Error::InvalidPageToken);
			}
		} else {
			locked_objects.next_creation_order
		};

		let mut entries = locked_objects
			.creation_order
			.range(..start_order)
			.rev()
			.filter_map(|(order, id)| {
				locked_objects.objects.get(id).cloned().map(|object| (*order, id, object))
			})
			.take(PAGE_SIZE + 1)
			.collect::<Vec<_>>();
		let has_more = entries.len() > PAGE_SIZE;
		entries.truncate(PAGE_SIZE);

		let next_page_token = if has_more {
			entries.last().map(|(order, id, _)| {
				PageToken::new(format!("{}:{}", order, id.encode_to_hex_str()))
			})
		} else {
			None
		};
		let objects = entries.into_iter().map(|(_, _, object)| object).collect();
		Ok((objects, next_page_token))
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
		self.objects.lock().expect("lock").objects.contains_key(id)
	}
}

#[cfg(test)]
mod tests {
	use std::cell::Cell;

	use lightning::util::persist::{PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning::util::test_utils::TestLogger;
	use lightning::{impl_writeable_tlv_based, io};

	use super::*;
	use crate::hex_utils;
	use crate::io::test_utils::InMemoryStore;
	use crate::io::utils::read_all_objects;
	use crate::types::DynStoreWrapper;

	#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
	struct TestObjectId {
		id: [u8; 4],
	}

	thread_local! {
		static ID_ENCODING_COUNT: Cell<usize> = const { Cell::new(0) };
	}

	impl StorableObjectId for TestObjectId {
		fn encode_to_hex_str(&self) -> String {
			ID_ENCODING_COUNT.set(ID_ENCODING_COUNT.get() + 1);
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

	#[tokio::test]
	async fn list_page_paginates_in_reverse_creation_order() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			Vec::new(),
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			Arc::clone(&store),
			logger,
		);

		// Insert more objects than fit in a single page to exercise the pagination loop.
		let num_objects = 120u32;
		for i in 0..num_objects {
			let id = TestObjectId { id: i.to_be_bytes() };
			data_store.insert(TestObject { id, data: [7u8; 3] }).await.unwrap();
		}

		let mut listed = Vec::with_capacity(num_objects as usize);
		let mut page_token = None;
		loop {
			let (page, next_page_token) = data_store.list_page(page_token).unwrap();
			assert!(!page.is_empty());
			listed.extend(page);
			page_token = next_page_token.map(|token| PageToken::new(token.to_string()));
			if page_token.is_none() {
				break;
			}
		}

		let expected: Vec<TestObject> = (0..num_objects)
			.rev()
			.map(|i| TestObject { id: TestObjectId { id: i.to_be_bytes() }, data: [7u8; 3] })
			.collect();
		assert_eq!(listed, expected);
	}

	#[test]
	fn list_page_cursor_lookup_does_not_scan_preceding_entries() {
		let objects = (0..120u32)
			.rev()
			.map(|i| TestObject { id: TestObjectId { id: i.to_be_bytes() }, data: [7u8; 3] })
			.collect();
		let data_store = new_failing_data_store(objects);
		ID_ENCODING_COUNT.set(0);

		let mut page_token = None;
		loop {
			let (_, next_page_token) = data_store.list_page(page_token).unwrap();
			page_token = next_page_token;
			if page_token.is_none() {
				break;
			}
		}

		assert_eq!(ID_ENCODING_COUNT.get(), 2);
	}

	#[tokio::test]
	async fn list_page_resumes_after_cursor_payment_is_removed() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			Vec::new(),
			"datastore_test_primary".to_string(),
			"datastore_test_secondary".to_string(),
			store,
			logger,
		);

		for i in 0..60u32 {
			let id = TestObjectId { id: i.to_be_bytes() };
			data_store.insert(TestObject { id, data: [7u8; 3] }).await.unwrap();
		}

		let (first_page, page_token) = data_store.list_page(None).unwrap();
		assert_eq!(first_page.len(), 50);
		let cursor_id = first_page.last().unwrap().id;
		assert_eq!(cursor_id.id, 10u32.to_be_bytes());
		let page_token = page_token.unwrap();
		data_store.remove(&cursor_id).await.unwrap();

		let (second_page, next_page_token) =
			data_store.list_page(Some(page_token.clone())).unwrap();
		assert_eq!(second_page.len(), 10);
		assert_eq!(second_page.first().unwrap().id.id, 9u32.to_be_bytes());
		assert_eq!(second_page.last().unwrap().id.id, 0u32.to_be_bytes());
		assert!(next_page_token.is_none());

		data_store.insert(TestObject { id: cursor_id, data: [7u8; 3] }).await.unwrap();
		let (page_after_reinsert, next_page_token) =
			data_store.list_page(Some(page_token)).unwrap();
		assert_eq!(page_after_reinsert, second_page);
		assert!(next_page_token.is_none());
	}

	#[tokio::test]
	async fn list_page_token_survives_reload_after_unseen_object_is_removed() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let primary_namespace = "datastore_test_primary".to_string();
		let secondary_namespace = "datastore_test_secondary".to_string();
		let data_store: DataStore<TestObject, Arc<TestLogger>> = DataStore::new(
			Vec::new(),
			primary_namespace.clone(),
			secondary_namespace.clone(),
			Arc::clone(&store),
			Arc::clone(&logger),
		);

		for i in 0..101u32 {
			let id = TestObjectId { id: i.to_be_bytes() };
			data_store.insert(TestObject { id, data: [7u8; 3] }).await.unwrap();
		}

		let (first_page, page_token) = data_store.list_page(None).unwrap();
		assert_eq!(first_page.first().unwrap().id.id, 100u32.to_be_bytes());
		assert_eq!(first_page.last().unwrap().id.id, 51u32.to_be_bytes());
		let page_token = page_token.unwrap();

		let oldest_id = TestObjectId { id: 0u32.to_be_bytes() };
		data_store.remove(&oldest_id).await.unwrap();
		let reloaded_objects = read_all_objects(
			&*store,
			&primary_namespace,
			&secondary_namespace,
			Arc::clone(&logger),
		)
		.await
		.unwrap();
		let reloaded_data_store: DataStore<TestObject, Arc<TestLogger>> =
			DataStore::new(reloaded_objects, primary_namespace, secondary_namespace, store, logger);

		let (second_page, next_page_token) =
			reloaded_data_store.list_page(Some(page_token)).unwrap();
		assert_eq!(second_page.first().unwrap().id.id, 50u32.to_be_bytes());
		assert_eq!(second_page.last().unwrap().id.id, 1u32.to_be_bytes());
		assert_eq!(second_page.len(), 50);
		assert!(next_page_token.is_none());
	}

	#[test]
	fn list_page_rejects_invalid_tokens() {
		let newest = TestObject { id: TestObjectId { id: 2u32.to_be_bytes() }, data: [2u8; 3] };
		let oldest = TestObject { id: TestObjectId { id: 1u32.to_be_bytes() }, data: [1u8; 3] };
		let data_store = new_failing_data_store(vec![newest, oldest]);

		let malformed_error =
			data_store.list_page(Some(PageToken::new("3".to_string()))).unwrap_err();
		assert_eq!(malformed_error, Error::InvalidPageToken);

		let unknown_error =
			data_store.list_page(Some(PageToken::new("ffffffff".to_string()))).unwrap_err();
		assert_eq!(unknown_error, Error::InvalidPageToken);
	}

	#[test]
	fn list_page_only_reads_in_memory() {
		let newest = TestObject { id: TestObjectId { id: 2u32.to_be_bytes() }, data: [2u8; 3] };
		let oldest = TestObject { id: TestObjectId { id: 1u32.to_be_bytes() }, data: [1u8; 3] };
		let data_store = new_failing_data_store(vec![newest, oldest]);

		let (page, next_page_token) = data_store.list_page(None).unwrap();
		assert_eq!(page, vec![newest, oldest]);
		assert!(next_page_token.is_none());
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
