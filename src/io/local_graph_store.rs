// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Local caching for the network graph to avoid slow VSS reads/writes.

use std::fs;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use lightning::io;
use lightning::io::Cursor;
use lightning::routing::gossip::NetworkGraph;
use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::ReadableArgs;

use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;

const LOCAL_GRAPH_CACHE_MAGIC: &[u8; 4] = b"LGNC";
const LOCAL_GRAPH_CACHE_VERSION: u8 = 1;
pub const NETWORK_GRAPH_LOCAL_CACHE_FILENAME: &str = "network_graph_cache";

/// Result of reading the local graph cache.
pub struct LocalGraphCacheData<L: Deref>
where
	L::Target: LdkLogger,
{
	pub graph: NetworkGraph<L>,
	pub rgs_timestamp: u32,
}

/// Header: 4 bytes magic + 1 byte version + 4 bytes timestamp
const LOCAL_GRAPH_CACHE_HEADER_SIZE: usize = 9;

/// Reads the network graph and RGS timestamp from the local cache.
pub fn read_local_graph_cache<L: Deref + Clone>(
	storage_dir_path: &str, logger: L,
) -> Result<LocalGraphCacheData<L>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let cache_path = format!("{}/{}", storage_dir_path, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
	let data = fs::read(&cache_path)?;

	if data.len() < LOCAL_GRAPH_CACHE_HEADER_SIZE {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Local graph cache too small",
		));
	}

	if &data[0..4] != LOCAL_GRAPH_CACHE_MAGIC {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Invalid local graph cache magic bytes",
		));
	}

	if data[4] != LOCAL_GRAPH_CACHE_VERSION {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			format!("Unsupported local graph cache version: {}", data[4]),
		));
	}

	let rgs_timestamp = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
	let graph_data = &data[LOCAL_GRAPH_CACHE_HEADER_SIZE..];

	let mut graph_cursor = Cursor::new(graph_data);
	let graph = NetworkGraph::read(&mut graph_cursor, logger.clone()).map_err(|e| {
		log_error!(logger, "Failed to deserialize NetworkGraph from local cache: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NetworkGraph")
	})?;

	Ok(LocalGraphCacheData { graph, rgs_timestamp })
}

fn write_local_graph_cache_bytes(
	storage_dir_path: &str, graph_bytes: &[u8], rgs_timestamp: u32,
) -> Result<(), std::io::Error> {
	fs::create_dir_all(storage_dir_path)?;

	let cache_path = format!("{}/{}", storage_dir_path, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);

	let mut cache_data = Vec::with_capacity(LOCAL_GRAPH_CACHE_HEADER_SIZE + graph_bytes.len());
	cache_data.extend_from_slice(LOCAL_GRAPH_CACHE_MAGIC);
	cache_data.push(LOCAL_GRAPH_CACHE_VERSION);
	cache_data.extend_from_slice(&rgs_timestamp.to_be_bytes());
	cache_data.extend_from_slice(graph_bytes);

	fs::write(&cache_path, cache_data)
}

fn read_local_graph_cache_bytes(storage_dir_path: &str) -> Result<Vec<u8>, std::io::Error> {
	let cache_path = format!("{}/{}", storage_dir_path, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
	let data = fs::read(&cache_path)?;

	if data.len() < LOCAL_GRAPH_CACHE_HEADER_SIZE {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Local graph cache too small",
		));
	}

	if &data[0..4] != LOCAL_GRAPH_CACHE_MAGIC {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Invalid local graph cache magic bytes",
		));
	}

	if data[4] != LOCAL_GRAPH_CACHE_VERSION {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			format!("Unsupported local graph cache version: {}", data[4]),
		));
	}

	Ok(data[LOCAL_GRAPH_CACHE_HEADER_SIZE..].to_vec())
}

/// A KVStore wrapper that redirects network graph operations to local storage.
pub struct LocalGraphStore {
	inner: Arc<DynStore>,
	storage_dir_path: String,
	rgs_timestamp: Arc<AtomicU32>,
}

impl LocalGraphStore {
	pub fn new(
		inner: Arc<DynStore>, storage_dir_path: String, rgs_timestamp: Arc<AtomicU32>,
	) -> Self {
		Self { inner, storage_dir_path, rgs_timestamp }
	}

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
			read_local_graph_cache_bytes(&self.storage_dir_path).map_err(|e| {
				if e.kind() == std::io::ErrorKind::NotFound {
					io::Error::new(io::ErrorKind::NotFound, e)
				} else {
					io::Error::new(io::ErrorKind::Other, format!("Local cache read failed: {}", e))
				}
			})
		} else {
			KVStoreSync::read(&*self.inner, primary_namespace, secondary_namespace, key)
		}
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Result<(), io::Error> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			let timestamp = self.rgs_timestamp.load(Ordering::Acquire);
			write_local_graph_cache_bytes(&self.storage_dir_path, &buf, timestamp).map_err(|e| {
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
		KVStoreSync::list(&*self.inner, primary_namespace, secondary_namespace)
	}
}

impl KVStore for LocalGraphStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send>> {
		if self.is_network_graph(primary_namespace, secondary_namespace, key) {
			let storage_dir = self.storage_dir_path.clone();
			Box::pin(async move {
				read_local_graph_cache_bytes(&storage_dir).map_err(|e| {
					if e.kind() == std::io::ErrorKind::NotFound {
						io::Error::new(io::ErrorKind::NotFound, e)
					} else {
						io::Error::new(
							io::ErrorKind::Other,
							format!("Local cache read failed: {}", e),
						)
					}
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
			let storage_dir = self.storage_dir_path.clone();
			let timestamp = self.rgs_timestamp.load(Ordering::Acquire);
			Box::pin(async move {
				write_local_graph_cache_bytes(&storage_dir, &buf, timestamp).map_err(|e| {
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
