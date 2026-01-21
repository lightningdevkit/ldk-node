// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! A wrapper store that redirects network graph persistence to local storage.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lightning::io;
use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
};

use crate::io::utils::{
	read_network_graph_bytes_from_local_cache, write_network_graph_to_local_cache_bytes,
	NETWORK_GRAPH_LOCAL_CACHE_FILENAME,
};
use crate::types::DynStore;

/// A wrapper around a KVStore that redirects network graph operations to local storage.
///
/// This is used to avoid slow VSS reads/writes for the network graph, which can be
/// regenerated from RGS. All other operations are forwarded to the underlying store.
pub struct LocalGraphStore {
	inner: Arc<DynStore>,
	storage_dir_path: String,
}

impl LocalGraphStore {
	/// Create a new LocalGraphStore wrapping the given store.
	pub fn new(inner: Arc<DynStore>, storage_dir_path: String) -> Self {
		Self { inner, storage_dir_path }
	}

	/// Check if the given namespace/key is for the network graph.
	fn is_network_graph(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> bool {
		primary_namespace == NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE
			&& secondary_namespace == NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE
			&& key == NETWORK_GRAPH_PERSISTENCE_KEY
	}
}

impl KVStoreSync for LocalGraphStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Result<Vec<u8>, io::Error> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			// Read from local cache instead of VSS
			read_network_graph_bytes_from_local_cache(&self.storage_dir_path).map_err(|e| {
				io::Error::new(io::ErrorKind::Other, format!("Local cache read failed: {}", e))
			})
		} else {
			KVStoreSync::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Result<(), io::Error> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			// Write to local cache instead of VSS
			write_network_graph_to_local_cache_bytes(&self.storage_dir_path, &buf).map_err(|e| {
				io::Error::new(io::ErrorKind::Other, format!("Local cache write failed: {}", e))
			})
		} else {
			KVStoreSync::write(&*self.inner, primary_namespace, secondary_namespace, key, buf)
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Result<(), io::Error> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			// Delete the local cache file
			let cache_path =
				format!("{}/{}", self.storage_dir_path, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
			match std::fs::remove_file(&cache_path) {
				Ok(()) => Ok(()),
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
				Err(e) => Err(io::Error::new(
					io::ErrorKind::Other,
					format!("Failed to remove local cache: {}", e),
				)),
			}
		} else {
			KVStoreSync::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Result<Vec<String>, io::Error> {
		// For listing, we still use the inner store
		// Network graph has its own namespace so this should be fine
		KVStoreSync::list(&*self.inner, primary_namespace, secondary_namespace)
	}
}

impl KVStore for LocalGraphStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send>> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			// Read from local cache instead of VSS
			let storage_dir = self.storage_dir_path.clone();
			Box::pin(async move {
				read_network_graph_bytes_from_local_cache(&storage_dir).map_err(|e| {
					io::Error::new(io::ErrorKind::Other, format!("Local cache read failed: {}", e))
				})
			})
		} else {
			KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + 'static + Send>> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			// Write to local cache instead of VSS
			let storage_dir = self.storage_dir_path.clone();
			Box::pin(async move {
				write_network_graph_to_local_cache_bytes(&storage_dir, &buf).map_err(|e| {
					io::Error::new(io::ErrorKind::Other, format!("Local cache write failed: {}", e))
				})
			})
		} else {
			KVStore::write(&*self.inner, primary_namespace, secondary_namespace, key, buf)
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + 'static + Send>> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			// Delete the local cache file
			let storage_dir = self.storage_dir_path.clone();
			Box::pin(async move {
				let cache_path = format!("{}/{}", storage_dir, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
				match std::fs::remove_file(&cache_path) {
					Ok(()) => Ok(()),
					Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
					Err(e) => Err(io::Error::new(
						io::ErrorKind::Other,
						format!("Failed to remove local cache: {}", e),
					)),
				}
			})
		} else {
			KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + 'static + Send>> {
		KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
	}
}
