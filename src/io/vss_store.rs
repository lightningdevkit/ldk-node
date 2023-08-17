use io::Error;
use std::io;
use std::io::ErrorKind;
#[cfg(test)]
use std::panic::RefUnwindSafe;

use crate::io::utils::check_namespace_key_validity;
use lightning::util::persist::KVStore;
use tokio::runtime::Runtime;
use vss_client::client::VssClient;
use vss_client::error::VssError;
use vss_client::types::{
	DeleteObjectRequest, GetObjectRequest, KeyValue, ListKeyVersionsRequest, PutObjectRequest,
};

/// A [`KVStore`] implementation that writes to and reads from a [VSS](https://github.com/lightningdevkit/vss-server/blob/main/README.md) backend.
pub struct VssStore {
	client: VssClient,
	store_id: String,
	runtime: Runtime,
}

impl VssStore {
	pub(crate) fn new(base_url: &str, store_id: String) -> Self {
		let client = VssClient::new(base_url);
		let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
		Self { client, store_id, runtime }
	}

	fn build_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<String> {
		if primary_namespace.is_empty() {
			Ok(key.to_string())
		} else {
			Ok(format!("{}#{}#{}", primary_namespace, secondary_namespace, key))
		}
	}

	fn extract_key(&self, unified_key: &str) -> io::Result<String> {
		let mut parts = unified_key.splitn(3, '#');
		let (_primary_namespace, _secondary_namespace) = (parts.next(), parts.next());
		match parts.next() {
			Some(actual_key) => Ok(actual_key.to_string()),
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
		let request = GetObjectRequest {
			store_id: self.store_id.clone(),
			key: self.build_key(primary_namespace, secondary_namespace, key)?,
		};

		let resp =
			tokio::task::block_in_place(|| self.runtime.block_on(self.client.get_object(&request)))
				.map_err(|e| {
					let msg = format!(
						"Failed to read from key {}/{}/{}: {}",
						primary_namespace, secondary_namespace, key, e
					);
					match e {
						VssError::NoSuchKeyError(..) => Error::new(ErrorKind::NotFound, msg),
						_ => Error::new(ErrorKind::Other, msg),
					}
				})?;
		Ok(resp.value.unwrap().value)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: &[u8],
	) -> io::Result<()> {
		check_namespace_key_validity(primary_namespace, secondary_namespace, Some(key), "write")?;
		let request = PutObjectRequest {
			store_id: self.store_id.clone(),
			global_version: None,
			transaction_items: vec![KeyValue {
				key: self.build_key(primary_namespace, secondary_namespace, key)?,
				version: -1,
				value: buf.to_vec(),
			}],
			delete_items: vec![],
		};

		tokio::task::block_in_place(|| self.runtime.block_on(self.client.put_object(&request)))
			.map_err(|e| {
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

		tokio::task::block_in_place(|| self.runtime.block_on(self.client.delete_object(&request)))
			.map_err(|e| {
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

		let keys = tokio::task::block_in_place(|| {
			self.runtime.block_on(self.list_all_keys(primary_namespace, secondary_namespace))
		})
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

#[cfg(test)]
impl RefUnwindSafe for VssStore {}

#[cfg(test)]
#[cfg(vss_test)]
mod tests {
	use super::*;
	use crate::io::test_utils::do_read_write_remove_list_persist;
	use rand::distributions::Alphanumeric;
	use rand::{thread_rng, Rng};

	#[test]
	fn read_write_remove_list_persist() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = thread_rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let vss_store = VssStore::new(&vss_base_url, rand_store_id);

		do_read_write_remove_list_persist(&vss_store);
	}
}
