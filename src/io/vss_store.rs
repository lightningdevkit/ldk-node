use std::io::Cursor;
use std::io::{Read, Write};
use std::{error::Error, io, marker};

use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;
use std::sync::RwLock;
use std::sync::{Arc, RwLockReadGuard};
use tokio::runtime::Handle;
use tokio::runtime::Runtime;
use vss_client::client::VssClient;

use crate::KVStore;
use vss_client::types::{GetObjectRequest, KeyValue, ListKeyVersionsRequest, PutObjectRequest};

/// pss
pub struct VssKVStore {
	client: VssClient,
	store_id: String,
	runtime: Arc<RwLock<Option<Runtime>>>,
}

impl VssKVStore {
	/// pssss
	pub fn new(
		base_url: &str, store_id: String, runtime: Arc<RwLock<Option<Runtime>>>,
	) -> Result<Self, Box<dyn Error>> {
		let client = VssClient::new(base_url);
		Ok(Self { client, store_id, runtime })
	}
}

impl KVStore for VssKVStore {
	type Reader = Box<dyn Read>;

	fn read(&self, namespace: &str, key: &str) -> io::Result<Self::Reader> {
		let locked_runtime = self.runtime.read().unwrap();
		let request =
			GetObjectRequest { store_id: self.store_id.to_string(), key: key.to_string() };

		let resp = if locked_runtime.as_ref().is_some() {
			tokio::task::block_in_place(move || {
				locked_runtime
					.as_ref()
					.unwrap()
					.block_on(async move { self.client.get_object(&request).await })
			})
			.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?
		} else {
			Runtime::new()
				.unwrap()
				.block_on(self.client.get_object(&request))
				.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?
		};

		let v = resp.value.unwrap().value;
		if v.is_empty() {
			Err(io::Error::new(io::ErrorKind::NotFound, "err"))
		} else {
			Ok(Box::new(Cursor::new(v)))
		}
	}

	fn write(&self, namespace: &str, key: &str, buf: &[u8]) -> io::Result<()> {
		let locked_runtime = self.runtime.read().unwrap();
		let kv = KeyValue { key: String::from(key), version: 0, value: buf.to_vec() };
		let request = PutObjectRequest {
			store_id: self.store_id.to_string(),
			global_version: None,
			transaction_items: vec![kv],
		};

		let resp = if locked_runtime.as_ref().is_some() {
			tokio::task::block_in_place(move || {
				locked_runtime
					.as_ref()
					.unwrap()
					.block_on(async move { self.client.put_object(&request).await })
			})
			.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?
		} else {
			Runtime::new()
				.unwrap()
				.block_on(self.client.put_object(&request))
				.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?
		};

		Ok(())
	}

	fn remove(&self, namespace: &str, key: &str) -> io::Result<bool> {
		Ok(true)
	}

	fn list(&self, namespace: &str) -> io::Result<Vec<String>> {
		let locked_runtime = self.runtime.read().unwrap();
		let request = ListKeyVersionsRequest {
			store_id: namespace.to_string(),
			key_prefix: Some("".to_string()),
			page_size: Option::from(50),
			page_token: None,
		};
		let resp = if locked_runtime.as_ref().is_some() {
			tokio::task::block_in_place(move || {
				locked_runtime
					.as_ref()
					.unwrap()
					.block_on(async move { self.client.list_key_versions(&request).await })
			})
			.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?
		} else {
			Runtime::new()
				.unwrap()
				.block_on(self.client.list_key_versions(&request))
				.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?
		};

		let keys = resp.key_versions.into_iter().map(|kv| kv.key).collect();
		Ok(keys)
	}
}

impl KVStorePersister for VssKVStore {
	fn persist<W: Writeable>(&self, prefixed_key: &str, object: &W) -> lightning::io::Result<()> {
		self.write("", &prefixed_key, &object.encode())?;
		Ok(())
	}
}
