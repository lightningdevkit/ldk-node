// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;
use crate::Error;

use lightning::util::ser::{Readable, Writeable};

use std::collections::hash_map;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

pub(crate) trait StorableObject: Clone + Readable + Writeable {
	type Id: StorableObjectId;
	type Update: StorableObjectUpdate<Self>;

	fn id(&self) -> Self::Id;
	fn update(&mut self, update: &Self::Update) -> bool;
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
		Self { objects, primary_namespace, secondary_namespace, kv_store, logger }
	}

	pub(crate) fn insert(&self, object: SO) -> Result<bool, Error> {
		let mut locked_objects = self.objects.lock().unwrap();

		self.persist(&object)?;
		let updated = locked_objects.insert(object.id(), object).is_some();
		Ok(updated)
	}

	pub(crate) fn insert_or_update(&self, object: SO) -> Result<bool, Error> {
		let mut locked_objects = self.objects.lock().unwrap();

		let updated;
		match locked_objects.entry(object.id()) {
			hash_map::Entry::Occupied(mut e) => {
				let update = object.to_update();
				updated = e.get_mut().update(&update);
				if updated {
					self.persist(&e.get())?;
				}
			},
			hash_map::Entry::Vacant(e) => {
				e.insert(object.clone());
				self.persist(&object)?;
				updated = true;
			},
		}

		Ok(updated)
	}

	pub(crate) fn remove(&self, id: &SO::Id) -> Result<(), Error> {
		let removed = self.objects.lock().unwrap().remove(id).is_some();
		if removed {
			let store_key = id.encode_to_hex_str();
			self.kv_store
				.remove(&self.primary_namespace, &self.secondary_namespace, &store_key, false)
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
		}
		Ok(())
	}

	pub(crate) fn get(&self, id: &SO::Id) -> Option<SO> {
		self.objects.lock().unwrap().get(id).cloned()
	}

	pub(crate) fn update(&self, update: &SO::Update) -> Result<DataStoreUpdateResult, Error> {
		let mut locked_objects = self.objects.lock().unwrap();

		if let Some(object) = locked_objects.get_mut(&update.id()) {
			let updated = object.update(update);
			if updated {
				self.persist(&object)?;
				Ok(DataStoreUpdateResult::Updated)
			} else {
				Ok(DataStoreUpdateResult::Unchanged)
			}
		} else {
			Ok(DataStoreUpdateResult::NotFound)
		}
	}

	pub(crate) fn list_filter<F: FnMut(&&SO) -> bool>(&self, f: F) -> Vec<SO> {
		self.objects.lock().unwrap().values().filter(f).cloned().collect::<Vec<SO>>()
	}

	fn persist(&self, object: &SO) -> Result<(), Error> {
		let store_key = object.id().encode_to_hex_str();
		let data = object.encode();
		self.kv_store
			.write(&self.primary_namespace, &self.secondary_namespace, &store_key, data)
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
}

#[cfg(test)]
mod tests {
	use lightning::impl_writeable_tlv_based;
	use lightning::util::test_utils::{TestLogger, TestStore};

	use crate::hex_utils;

	use super::*;

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

		fn update(&mut self, update: &Self::Update) -> bool {
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

	#[test]
	fn data_is_persisted() {
		let store: Arc<DynStore> = Arc::new(TestStore::new(false));
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
		assert!(store.read(&primary_namespace, &secondary_namespace, &store_key).is_err());

		// Check we successfully store an object and return `false`
		let object = TestObject { id, data: [23u8; 3] };
		assert_eq!(Ok(false), data_store.insert(object.clone()));
		assert_eq!(Some(object), data_store.get(&id));
		assert!(store.read(&primary_namespace, &secondary_namespace, &store_key).is_ok());

		// Test re-insertion returns `true`
		let mut override_object = object.clone();
		override_object.data = [24u8; 3];
		assert_eq!(Ok(true), data_store.insert(override_object));
		assert_eq!(Some(override_object), data_store.get(&id));

		// Check update returns `Updated`
		let update = TestObjectUpdate { id, data: [25u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::Updated), data_store.update(&update));
		assert_eq!(data_store.get(&id).unwrap().data, [25u8; 3]);

		// Check no-op update yields `Unchanged`
		let update = TestObjectUpdate { id, data: [25u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::Unchanged), data_store.update(&update));

		// Check bogus update yields `NotFound`
		let bogus_id = TestObjectId { id: [84u8; 4] };
		let update = TestObjectUpdate { id: bogus_id, data: [12u8; 3] };
		assert_eq!(Ok(DataStoreUpdateResult::NotFound), data_store.update(&update));

		// Check `insert_or_update` inserts unknown objects
		let iou_id = TestObjectId { id: [55u8; 4] };
		let iou_object = TestObject { id: iou_id, data: [34u8; 3] };
		assert_eq!(Ok(true), data_store.insert_or_update(iou_object.clone()));

		// Check `insert_or_update` doesn't update the same object
		assert_eq!(Ok(false), data_store.insert_or_update(iou_object.clone()));

		// Check `insert_or_update` updates if object changed
		let mut new_iou_object = iou_object;
		new_iou_object.data[0] += 1;
		assert_eq!(Ok(true), data_store.insert_or_update(new_iou_object));
	}
}
