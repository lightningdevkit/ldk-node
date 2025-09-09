// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::io::utils::check_namespace_key_validity;
use crate::runtime::Runtime;

use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use lightning::io::{self, Error, ErrorKind};
use lightning::util::persist::KVStore;
use prost::Message;
use rand::RngCore;
#[cfg(test)]
use std::panic::RefUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
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

type CustomRetryPolicy = FilteredRetryPolicy<
	JitteredRetryPolicy<
		MaxTotalDelayRetryPolicy<MaxAttemptsRetryPolicy<ExponentialBackoffRetryPolicy<VssError>>>,
	>,
	Box<dyn Fn(&VssError) -> bool + 'static + Send + Sync>,
>;

const KEY_LENGTH: usize = 32;

/// A [`KVStore`] implementation that writes to and reads from a [VSS](https://github.com/lightningdevkit/vss-server/blob/main/README.md) backend.
pub struct VssStore {
	client: VssClient<CustomRetryPolicy>,
	store_id: String,
	runtime: Arc<Runtime>,
	data_encryption_key: [u8; KEY_LENGTH],
	key_obfuscator: KeyObfuscator,
}

impl VssStore {
	pub(crate) fn new(
		base_url: String, store_id: String, vss_seed: [u8; 32],
		header_provider: Arc<dyn VssHeaderProvider>, runtime: Arc<Runtime>,
	) -> Self {
		let (data_encryption_key, obfuscation_master_key) =
			derive_data_encryption_and_obfuscation_keys(&vss_seed);
		let key_obfuscator = KeyObfuscator::new(obfuscation_master_key);
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
		Self { client, store_id, runtime, data_encryption_key, key_obfuscator }
	}

	fn build_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<String> {
		let obfuscated_key = self.key_obfuscator.obfuscate(key);
		if primary_namespace.is_empty() {
			Ok(obfuscated_key)
		} else {
			Ok(format!("{}#{}#{}", primary_namespace, secondary_namespace, obfuscated_key))
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
}

impl KVStore for VssStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "read")?;

		let store_key = self.build_key(primary_namespace, secondary_namespace, key)?;
		let request = GetObjectRequest { store_id: self.store_id.clone(), key: store_key.clone() };
		let resp = self.runtime.block_on(self.client.get_object(&request)).map_err(|e| {
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

		let storable_builder = StorableBuilder::new(RandEntropySource);
		let decrypted = storable_builder
			.deconstruct(storable, &self.data_encryption_key, store_key.as_bytes())?
			.0;
		Ok(decrypted)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: &[u8],
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "write")?;
		let store_key = self.build_key(primary_namespace, secondary_namespace, key)?;
		let version = -1;
		let storable_builder = StorableBuilder::new(RandEntropySource);
		let storable = storable_builder.build(
			buf.to_vec(),
			version,
			&&self.data_encryption_key,
			store_key.as_bytes(),
		);
		let request = PutObjectRequest {
			store_id: self.store_id.clone(),
			global_version: None,
			transaction_items: vec![KeyValue {
				key: store_key,
				version,
				value: storable.encode_to_vec(),
			}],
			delete_items: vec![],
		};

		self.runtime.block_on(self.client.put_object(&request)).map_err(|e| {
			let msg = format!(
				"Failed to write to key {}/{}/{}: {}",
				primary_namespace, secondary_namespace, key, e
			);
			Error::new(ErrorKind::Other, msg)
		})?;

		Ok(())
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "remove")?;
		let request = DeleteObjectRequest {
			store_id: self.store_id.clone(),
			key_value: Some(KeyValue {
				key: self.build_key(primary_namespace, secondary_namespace, key)?,
				version: -1,
				value: vec![],
			}),
		};

		self.runtime.block_on(self.client.delete_object(&request)).map_err(|e| {
			let msg = format!(
				"Failed to delete key {}/{}/{}: {}",
				primary_namespace, secondary_namespace, key, e
			);
			Error::new(ErrorKind::Other, msg)
		})?;
		Ok(())
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, None, "list")?;

		let keys = self
			.runtime
			.block_on(self.list_all_keys(primary_namespace, secondary_namespace))
			.map_err(|e| {
				let msg = format!(
					"Failed to retrieve keys in namespace: {}/{} : {}",
					primary_namespace, secondary_namespace, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

		Ok(keys)
	}
}

impl Drop for VssStore {
	fn drop(&mut self) {
		// Zeroize the data_encryption_key
		for elem in self.data_encryption_key.iter_mut() {
			unsafe {
				::core::ptr::write_volatile(elem, 0);
			}

		};
		::core::sync::atomic::compiler_fence(::core::sync::atomic::Ordering::SeqCst);
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
		rand::thread_rng().fill_bytes(buffer);
	}
}

#[cfg(test)]
impl RefUnwindSafe for VssStore {}

#[cfg(test)]
#[cfg(vss_test)]
mod tests {
	use super::*;
	use crate::io::test_utils::do_read_write_remove_list_persist;
	use rand::distributions::Alphanumeric;
	use rand::{thread_rng, Rng, RngCore};
	use std::collections::HashMap;
	use tokio::runtime;
	use vss_client::headers::FixedHeaders;

	#[test]
	fn vss_read_write_remove_list_persist() {
		let runtime = Arc::new(Runtime::new().unwrap());
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = thread_rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider, runtime).unwrap();

		do_read_write_remove_list_persist(&vss_store);
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn vss_read_write_remove_list_persist_in_runtime_context() {
		let runtime = Arc::new(Runtime::new().unwrap());
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = thread_rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider, runtime).unwrap();

		do_read_write_remove_list_persist(&vss_store);
		drop(vss_store)
	}
}
