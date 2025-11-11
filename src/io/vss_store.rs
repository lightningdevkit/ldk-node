// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::boxed::Box;
use std::collections::HashMap;
use std::future::Future;
#[cfg(test)]
use std::panic::RefUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bdk_chain::Merge;
use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use lightning::io::{self, Error, ErrorKind};
use lightning::util::persist::{KVStore, KVStoreSync};
use prost::Message;
use rand::RngCore;
use vss_client::client::VssClient;
use vss_client::error::VssError;
use vss_client::headers::VssHeaderProvider;
use vss_client::types::{
	DeleteObjectRequest, GetObjectRequest, KeyValue, ListKeyVersionsRequest, PutObjectRequest,
	Storable,
};
use vss_client::util::key_obfuscator::KeyObfuscator;
use vss_client::util::retry::{
	ExponentialBackoffRetryPolicy, FilteredRetryPolicy, JitteredRetryPolicy,
	MaxAttemptsRetryPolicy, MaxTotalDelayRetryPolicy, RetryPolicy,
};
use vss_client::util::storable_builder::{EntropySource, StorableBuilder};

use crate::io::utils::check_namespace_key_validity;
use crate::runtime::Runtime;

type CustomRetryPolicy = FilteredRetryPolicy<
	JitteredRetryPolicy<
		MaxTotalDelayRetryPolicy<MaxAttemptsRetryPolicy<ExponentialBackoffRetryPolicy<VssError>>>,
	>,
	Box<dyn Fn(&VssError) -> bool + 'static + Send + Sync>,
>;

enum VssSchemaVersion {
	// The initial schema version.
	// This used an empty `aad` and unobfuscated `primary_namespace`/`secondary_namespace`s in the
	// stored key.
	V0,
}

// We set this to a small number of threads that would still allow to make some progress if one
// would hit a blocking case
const INTERNAL_RUNTIME_WORKERS: usize = 2;
const VSS_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// A [`KVStoreSync`] implementation that writes to and reads from a [VSS](https://github.com/lightningdevkit/vss-server/blob/main/README.md) backend.
pub struct VssStore {
	inner: Arc<VssStoreInner>,
	// Version counter to ensure that writes are applied in the correct order. It is assumed that read and list
	// operations aren't sensitive to the order of execution.
	next_version: AtomicU64,
	runtime: Arc<Runtime>,
	// A VSS-internal runtime we use to avoid any deadlocks we could hit when waiting on a spawned
	// blocking task to finish while the blocked thread had acquired the reactor. In particular,
	// this works around a previously-hit case where a concurrent call to
	// `PeerManager::process_pending_events` -> `ChannelManager::get_and_clear_pending_msg_events`
	// would deadlock when trying to acquire sync `Mutex` locks that are held by the thread
	// currently being blocked waiting on the VSS operation to finish.
	internal_runtime: Option<tokio::runtime::Runtime>,
}

impl VssStore {
	pub(crate) fn new(
		base_url: String, store_id: String, vss_seed: [u8; 32],
		header_provider: Arc<dyn VssHeaderProvider>, runtime: Arc<Runtime>,
	) -> Self {
		let inner = Arc::new(VssStoreInner::new(base_url, store_id, vss_seed, header_provider));
		let next_version = AtomicU64::new(1);
		let internal_runtime = Some(
			tokio::runtime::Builder::new_multi_thread()
				.enable_all()
				.thread_name_fn(|| {
					static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
					let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
					format!("ldk-node-vss-runtime-{}", id)
				})
				.worker_threads(INTERNAL_RUNTIME_WORKERS)
				.max_blocking_threads(INTERNAL_RUNTIME_WORKERS)
				.build()
				.unwrap(),
		);

		Self { inner, next_version, runtime, internal_runtime }
	}

	// Same logic as for the obfuscated keys below, but just for locking, using the plaintext keys
	fn build_locking_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		if primary_namespace.is_empty() {
			key.to_owned()
		} else {
			format!("{}#{}#{}", primary_namespace, secondary_namespace, key)
		}
	}

	fn get_new_version_and_lock_ref(
		&self, locking_key: String,
	) -> (Arc<tokio::sync::Mutex<u64>>, u64) {
		let version = self.next_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("VssStore version counter overflowed");
		}

		// Get a reference to the inner lock. We do this early so that the arc can double as an in-flight counter for
		// cleaning up unused locks.
		let inner_lock_ref = self.inner.get_inner_lock_ref(locking_key);

		(inner_lock_ref, version)
	}
}

impl KVStoreSync for VssStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut =
			async move { inner.read_internal(primary_namespace, secondary_namespace, key).await };
		// TODO: We could drop the timeout here once we ensured vss-client's Retry logic always
		// times out.
		let spawned_fut = internal_runtime.spawn(async move {
			tokio::time::timeout(VSS_IO_TIMEOUT, fut).await.map_err(|_| {
				let msg = "VssStore::read timed out";
				Error::new(ErrorKind::Other, msg)
			})
		});
		self.runtime.block_on(spawned_fut).expect("We should always finish")?
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let fut = async move {
			inner
				.write_internal(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					buf,
				)
				.await
		};
		// TODO: We could drop the timeout here once we ensured vss-client's Retry logic always
		// times out.
		let spawned_fut = internal_runtime.spawn(async move {
			tokio::time::timeout(VSS_IO_TIMEOUT, fut).await.map_err(|_| {
				let msg = "VssStore::write timed out";
				Error::new(ErrorKind::Other, msg)
			})
		});
		self.runtime.block_on(spawned_fut).expect("We should always finish")?
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let fut = async move {
			inner
				.remove_internal(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					lazy,
				)
				.await
		};
		// TODO: We could drop the timeout here once we ensured vss-client's Retry logic always
		// times out.
		let spawned_fut = internal_runtime.spawn(async move {
			tokio::time::timeout(VSS_IO_TIMEOUT, fut).await.map_err(|_| {
				let msg = "VssStore::remove timed out";
				Error::new(ErrorKind::Other, msg)
			})
		});
		self.runtime.block_on(spawned_fut).expect("We should always finish")?
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move { inner.list_internal(primary_namespace, secondary_namespace).await };
		// TODO: We could drop the timeout here once we ensured vss-client's Retry logic always
		// times out.
		let spawned_fut = internal_runtime.spawn(async move {
			tokio::time::timeout(VSS_IO_TIMEOUT, fut).await.map_err(|_| {
				let msg = "VssStore::list timed out";
				Error::new(ErrorKind::Other, msg)
			})
		});
		self.runtime.block_on(spawned_fut).expect("We should always finish")?
	}
}

impl KVStore for VssStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + Send>> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		Box::pin(
			async move { inner.read_internal(primary_namespace, secondary_namespace, key).await },
		)
	}
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		Box::pin(async move {
			inner
				.write_internal(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					buf,
				)
				.await
		})
	}
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		Box::pin(async move {
			inner
				.remove_internal(
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					lazy,
				)
				.await
		})
	}
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + Send>> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		Box::pin(async move { inner.list_internal(primary_namespace, secondary_namespace).await })
	}
}

impl Drop for VssStore {
	fn drop(&mut self) {
		let internal_runtime = self.internal_runtime.take();
		tokio::task::block_in_place(move || drop(internal_runtime));
	}
}

struct VssStoreInner {
	client: VssClient<CustomRetryPolicy>,
	store_id: String,
	storable_builder: StorableBuilder<RandEntropySource>,
	key_obfuscator: KeyObfuscator,
	// Per-key locks that ensures that we don't have concurrent writes to the same namespace/key.
	// The lock also encapsulates the latest written version per key.
	locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
	pending_lazy_deletes: Mutex<Vec<KeyValue>>,
}

impl VssStoreInner {
	pub(crate) fn new(
		base_url: String, store_id: String, vss_seed: [u8; 32],
		header_provider: Arc<dyn VssHeaderProvider>,
	) -> Self {
		let (data_encryption_key, obfuscation_master_key) =
			derive_data_encryption_and_obfuscation_keys(&vss_seed);
		let key_obfuscator = KeyObfuscator::new(obfuscation_master_key);
		let storable_builder = StorableBuilder::new(data_encryption_key, RandEntropySource);
		let retry_policy = ExponentialBackoffRetryPolicy::new(Duration::from_millis(10))
			.with_max_attempts(10)
			.with_max_total_delay(Duration::from_secs(15))
			.with_max_jitter(Duration::from_millis(10))
			.skip_retry_on_error(Box::new(|e: &VssError| {
				matches!(
					e,
					VssError::NoSuchKeyError(..)
						| VssError::InvalidRequestError(..)
						| VssError::ConflictError(..)
				)
			}) as _);

		let client = VssClient::new_with_headers(base_url, retry_policy, header_provider);
		let locks = Mutex::new(HashMap::new());
		let pending_lazy_deletes = Mutex::new(Vec::new());
		Self { client, store_id, storable_builder, key_obfuscator, locks, pending_lazy_deletes }
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<tokio::sync::Mutex<u64>> {
		let mut outer_lock = self.locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	fn build_obfuscated_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		let obfuscated_key = self.key_obfuscator.obfuscate(key);
		if primary_namespace.is_empty() {
			obfuscated_key
		} else {
			format!("{}#{}#{}", primary_namespace, secondary_namespace, obfuscated_key)
		}
	}

	fn extract_key(&self, unified_key: &str) -> io::Result<String> {
		let mut parts = unified_key.splitn(3, '#');
		let (_primary_namespace, _secondary_namespace) = (parts.next(), parts.next());
		match parts.next() {
			Some(obfuscated_key) => {
				let actual_key = self.key_obfuscator.deobfuscate(obfuscated_key)?;
				Ok(actual_key)
			},
			None => Err(Error::new(ErrorKind::InvalidData, "Invalid key format")),
		}
	}

	async fn list_all_keys(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		let mut page_token = None;
		let mut keys = vec![];
		let key_prefix = format!("{}#{}", primary_namespace, secondary_namespace);
		while page_token != Some("".to_string()) {
			let request = ListKeyVersionsRequest {
				store_id: self.store_id.clone(),
				key_prefix: Some(key_prefix.clone()),
				page_token,
				page_size: None,
			};

			let response = self.client.list_key_versions(&request).await.map_err(|e| {
				let msg = format!(
					"Failed to list keys in {}/{}: {}",
					primary_namespace, secondary_namespace, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

			for kv in response.key_versions {
				keys.push(self.extract_key(&kv.key)?);
			}
			page_token = response.next_page_token;
		}
		Ok(keys)
	}

	async fn read_internal(
		&self, primary_namespace: String, secondary_namespace: String, key: String,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, Some(&key), "read")?;

		let obfuscated_key =
			self.build_obfuscated_key(&primary_namespace, &secondary_namespace, &key);
		let request = GetObjectRequest { store_id: self.store_id.clone(), key: obfuscated_key };
		let resp = self.client.get_object(&request).await.map_err(|e| {
			let msg = format!(
				"Failed to read from key {}/{}/{}: {}",
				primary_namespace, secondary_namespace, key, e
			);
			match e {
				VssError::NoSuchKeyError(..) => Error::new(ErrorKind::NotFound, msg),
				_ => Error::new(ErrorKind::Other, msg),
			}
		})?;

		// unwrap safety: resp.value must be always present for a non-erroneous VSS response, otherwise
		// it is an API-violation which is converted to [`VssError::InternalServerError`] in [`VssClient`]
		let storable = Storable::decode(&resp.value.unwrap().value[..]).map_err(|e| {
			let msg = format!(
				"Failed to decode data read from key {}/{}/{}: {}",
				primary_namespace, secondary_namespace, key, e
			);
			Error::new(ErrorKind::Other, msg)
		})?;

		Ok(self.storable_builder.deconstruct(storable)?.0)
	}

	async fn write_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"write",
		)?;

		let delete_items = self
			.pending_lazy_deletes
			.try_lock()
			.ok()
			.and_then(|mut guard| guard.take())
			.unwrap_or_default();
		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let obfuscated_key =
				self.build_obfuscated_key(&primary_namespace, &secondary_namespace, &key);
			let vss_version = -1;
			let storable = self.storable_builder.build(buf, vss_version);
			let request = PutObjectRequest {
				store_id: self.store_id.clone(),
				global_version: None,
				transaction_items: vec![KeyValue {
					key: obfuscated_key,
					version: vss_version,
					value: storable.encode_to_vec(),
				}],
				delete_items: delete_items.clone(),
			};

			self.client.put_object(&request).await.map_err(|e| {
				// Restore delete items so they'll be retried on next write.
				if !delete_items.is_empty() {
					self.pending_lazy_deletes.lock().unwrap().extend(delete_items);
				}

				let msg = format!(
					"Failed to write to key {}/{}/{}: {}",
					primary_namespace, secondary_namespace, key, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

			Ok(())
		})
		.await
	}

	async fn remove_internal(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		primary_namespace: String, secondary_namespace: String, key: String, lazy: bool,
	) -> io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"remove",
		)?;

		let obfuscated_key =
			self.build_obfuscated_key(&primary_namespace, &secondary_namespace, &key);

		let key_value = KeyValue { key: obfuscated_key, version: -1, value: vec![] };
		if lazy {
			let mut pending_lazy_deletes = self.pending_lazy_deletes.lock().unwrap();
			pending_lazy_deletes.push(key_value);
			return Ok(());
		}

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let request =
				DeleteObjectRequest { store_id: self.store_id.clone(), key_value: Some(key_value) };

			self.client.delete_object(&request).await.map_err(|e| {
				let msg = format!(
					"Failed to delete key {}/{}/{}: {}",
					primary_namespace, secondary_namespace, key, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

			Ok(())
		})
		.await
	}

	async fn list_internal(
		&self, primary_namespace: String, secondary_namespace: String,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, None, "list")?;

		let keys =
			self.list_all_keys(&primary_namespace, &secondary_namespace).await.map_err(|e| {
				let msg = format!(
					"Failed to retrieve keys in namespace: {}/{} : {}",
					primary_namespace, secondary_namespace, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

		Ok(keys)
	}

	async fn execute_locked_write<
		F: Future<Output = Result<(), lightning::io::Error>>,
		FN: FnOnce() -> F,
	>(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		callback: FN,
	) -> Result<(), lightning::io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().await;

			// Check if we already have a newer version written/removed. This is used in async contexts to realize eventual
			// consistency.
			let is_stale_version = version <= *last_written_version;

			// If the version is not stale, we execute the callback. Otherwise we can and must skip writing.
			if is_stale_version {
				Ok(())
			} else {
				callback().await.map(|_| {
					*last_written_version = version;
				})
			}
		};

		self.clean_locks(&inner_lock_ref, locking_key);

		res
	}

	fn clean_locks(&self, inner_lock_ref: &Arc<tokio::sync::Mutex<u64>>, locking_key: String) {
		// If there no arcs in use elsewhere, this means that there are no in-flight writes. We can remove the map entry
		// to prevent leaking memory. The two arcs that are expected are the one in the map and the one held here in
		// inner_lock_ref. The outer lock is obtained first, to avoid a new arc being cloned after we've already
		// counted.
		let mut outer_lock = self.locks.lock().unwrap();

		let strong_count = Arc::strong_count(&inner_lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected VssStore strong count");

		if strong_count == 2 {
			outer_lock.remove(&locking_key);
		}
	}
}

fn derive_data_encryption_and_obfuscation_keys(vss_seed: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
	let hkdf = |initial_key_material: &[u8], salt: &[u8]| -> [u8; 32] {
		let mut engine = HmacEngine::<sha256::Hash>::new(salt);
		engine.input(initial_key_material);
		Hmac::from_engine(engine).to_byte_array()
	};

	let prk = hkdf(vss_seed, b"pseudo_random_key");
	let k1 = hkdf(&prk, b"data_encryption_key");
	let k2 = hkdf(&prk, &[&k1[..], b"obfuscation_key"].concat());
	(k1, k2)
}

/// A source for generating entropy/randomness using [`rand`].
pub(crate) struct RandEntropySource;

impl EntropySource for RandEntropySource {
	fn fill_bytes(&self, buffer: &mut [u8]) {
		rand::rng().fill_bytes(buffer);
	}
}

#[cfg(test)]
impl RefUnwindSafe for VssStore {}

#[cfg(test)]
#[cfg(vss_test)]
mod tests {
	use std::collections::HashMap;

	use rand::distr::Alphanumeric;
	use rand::{rng, Rng, RngCore};
	use vss_client::headers::FixedHeaders;

	use super::*;
	use crate::io::test_utils::do_read_write_remove_list_persist;
	use crate::logger::Logger;

	#[test]
	fn vss_read_write_remove_list_persist() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(logger).unwrap());
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider, runtime);

		do_read_write_remove_list_persist(&vss_store);
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn vss_read_write_remove_list_persist_in_runtime_context() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(logger).unwrap());
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider, runtime);

		do_read_write_remove_list_persist(&vss_store);
		drop(vss_store)
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn vss_lazy_delete() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(logger).unwrap());
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider, runtime);

		let primary_namespace = "test_namespace";
		let secondary_namespace = "";
		let key_to_delete = "key_to_delete";
		let key_for_trigger = "key_for_trigger";
		let data_to_delete = b"data_to_delete".to_vec();
		let trigger_data = b"trigger_data".to_vec();

		// Write the key that we'll later lazily delete
		KVStore::write(
			&vss_store,
			primary_namespace,
			secondary_namespace,
			key_to_delete,
			data_to_delete.clone(),
		)
		.await
		.unwrap();

		// Verify the key exists
		let read_data =
			KVStore::read(&vss_store, primary_namespace, secondary_namespace, key_to_delete)
				.await
				.unwrap();
		assert_eq!(read_data, data_to_delete);

		// Perform a lazy delete
		KVStore::remove(&vss_store, primary_namespace, secondary_namespace, key_to_delete, true)
			.await
			.unwrap();

		// Verify the key still exists (lazy delete doesn't immediately remove it)
		let read_data =
			KVStore::read(&vss_store, primary_namespace, secondary_namespace, key_to_delete)
				.await
				.unwrap();
		assert_eq!(read_data, data_to_delete);

		// Verify the key is still in the list
		let keys = KVStore::list(&vss_store, primary_namespace, secondary_namespace).await.unwrap();
		assert!(keys.contains(&key_to_delete.to_string()));

		// Trigger the actual deletion by performing a write operation
		KVStore::write(
			&vss_store,
			primary_namespace,
			secondary_namespace,
			key_for_trigger,
			trigger_data.clone(),
		)
		.await
		.unwrap();

		// Now verify the key is actually deleted
		let read_result =
			KVStore::read(&vss_store, primary_namespace, secondary_namespace, key_to_delete).await;
		assert!(read_result.is_err());
		assert_eq!(read_result.unwrap_err().kind(), ErrorKind::NotFound);

		// Verify the key is no longer in the list
		let keys = KVStore::list(&vss_store, primary_namespace, secondary_namespace).await.unwrap();
		assert!(!keys.contains(&key_to_delete.to_string()));

		// Verify the trigger key still exists
		let read_data =
			KVStore::read(&vss_store, primary_namespace, secondary_namespace, key_for_trigger)
				.await
				.unwrap();
		assert_eq!(read_data, trigger_data);
	}
}
