// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{hash_map, HashMap};
use std::future::Future;
use std::panic::RefUnwindSafe;
use std::path::PathBuf;
use std::sync::Mutex;

use lightning::io;
use lightning::util::persist::{KVStore, KVSTORE_NAMESPACE_KEY_MAX_LEN};
use rand::distr::Alphanumeric;
use rand::{rng, Rng};
use tokio::runtime::Runtime;

pub struct InMemoryStore {
	persisted_bytes: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
}

impl InMemoryStore {
	pub fn new() -> Self {
		let persisted_bytes = Mutex::new(HashMap::new());
		Self { persisted_bytes }
	}

	fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		let persisted_lock = self.persisted_bytes.lock().unwrap();
		let prefixed = format!("{primary_namespace}/{secondary_namespace}");

		if let Some(outer_ref) = persisted_lock.get(&prefixed) {
			if let Some(inner_ref) = outer_ref.get(key) {
				let bytes = inner_ref.clone();
				Ok(bytes)
			} else {
				Err(io::Error::new(io::ErrorKind::NotFound, "Key not found"))
			}
		} else {
			Err(io::Error::new(io::ErrorKind::NotFound, "Namespace not found"))
		}
	}

	fn write_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();

		let prefixed = format!("{primary_namespace}/{secondary_namespace}");
		let outer_e = persisted_lock.entry(prefixed).or_insert(HashMap::new());
		outer_e.insert(key.to_string(), buf);
		Ok(())
	}

	fn remove_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> io::Result<()> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();

		let prefixed = format!("{primary_namespace}/{secondary_namespace}");
		if let Some(outer_ref) = persisted_lock.get_mut(&prefixed) {
			outer_ref.remove(&key.to_string());
		}

		Ok(())
	}

	fn list_internal(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();

		let prefixed = format!("{primary_namespace}/{secondary_namespace}");
		match persisted_lock.entry(prefixed) {
			hash_map::Entry::Occupied(e) => Ok(e.get().keys().cloned().collect()),
			hash_map::Entry::Vacant(_) => Ok(Vec::new()),
		}
	}
}

impl KVStore for InMemoryStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let res = self.read_internal(&primary_namespace, &secondary_namespace, &key);
		async move { res }
	}
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let res = self.write_internal(&primary_namespace, &secondary_namespace, &key, buf);
		async move { res }
	}
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let res = self.remove_internal(&primary_namespace, &secondary_namespace, &key, lazy);
		async move { res }
	}
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let res = self.list_internal(primary_namespace, secondary_namespace);
		async move { res }
	}
}

unsafe impl Sync for InMemoryStore {}
unsafe impl Send for InMemoryStore {}

pub(crate) fn random_storage_path() -> PathBuf {
	let mut temp_path = std::env::temp_dir();
	let mut rng = rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

pub(crate) fn do_read_write_remove_list_persist<K: KVStore + RefUnwindSafe>(kv_store: &K) {
	let rt = Runtime::new().unwrap();
	rt.block_on(do_read_write_remove_list_persist_async(kv_store));
}

async fn do_read_write_remove_list_persist_async<K: KVStore + RefUnwindSafe>(kv_store: &K) {
	let data = vec![42u8; 32];

	let primary_namespace = "testspace";
	let secondary_namespace = "testsubspace";
	let key = "testkey";

	KVStore::write(kv_store, primary_namespace, secondary_namespace, key, data.clone())
		.await
		.unwrap();

	KVStore::write(kv_store, "", "", key, data.clone()).await.unwrap();

	let listed_keys =
		KVStore::list(kv_store, primary_namespace, secondary_namespace).await.unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], key);

	let read_data =
		KVStore::read(kv_store, primary_namespace, secondary_namespace, key).await.unwrap();
	assert_eq!(data, &*read_data);

	KVStore::remove(kv_store, primary_namespace, secondary_namespace, key, false).await.unwrap();

	let listed_keys =
		KVStore::list(kv_store, primary_namespace, secondary_namespace).await.unwrap();
	assert_eq!(listed_keys.len(), 0);

	let max_chars: String = std::iter::repeat('A').take(KVSTORE_NAMESPACE_KEY_MAX_LEN).collect();
	KVStore::write(kv_store, &max_chars, &max_chars, &max_chars, data.clone()).await.unwrap();

	let listed_keys = KVStore::list(kv_store, &max_chars, &max_chars).await.unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], max_chars);

	let read_data = KVStore::read(kv_store, &max_chars, &max_chars, &max_chars).await.unwrap();
	assert_eq!(data, &*read_data);

	KVStore::remove(kv_store, &max_chars, &max_chars, &max_chars, false).await.unwrap();

	let listed_keys = KVStore::list(kv_store, &max_chars, &max_chars).await.unwrap();
	assert_eq!(listed_keys.len(), 0);
}
