//! Objects and traits for data persistence.

pub(crate) mod fs_store;
pub(crate) mod sqlite_store;
pub(crate) mod utils;

pub use fs_store::FilesystemStore;
pub use sqlite_store::SqliteStore;

use lightning::util::persist::KVStorePersister;

use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;

// The namespacs and keys LDK uses for persisting
pub(crate) const CHANNEL_MANAGER_PERSISTENCE_NAMESPACE: &str = "";
pub(crate) const CHANNEL_MANAGER_PERSISTENCE_KEY: &str = "manager";

pub(crate) const CHANNEL_MONITOR_PERSISTENCE_NAMESPACE: &str = "monitors";

pub(crate) const NETWORK_GRAPH_PERSISTENCE_NAMESPACE: &str = "";
pub(crate) const NETWORK_GRAPH_PERSISTENCE_KEY: &str = "network_graph";

pub(crate) const SCORER_PERSISTENCE_NAMESPACE: &str = "";
pub(crate) const SCORER_PERSISTENCE_KEY: &str = "scorer";

/// The event queue will be persisted under this key.
pub(crate) const EVENT_QUEUE_PERSISTENCE_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_KEY: &str = "events";

/// The peer information will be persisted under this key.
pub(crate) const PEER_INFO_PERSISTENCE_NAMESPACE: &str = "";
pub(crate) const PEER_INFO_PERSISTENCE_KEY: &str = "peers";

/// The payment information will be persisted under this prefix.
pub(crate) const PAYMENT_INFO_PERSISTENCE_NAMESPACE: &str = "payments";

/// RapidGossipSync's `latest_sync_timestamp` will be persisted under this key.
pub(crate) const LATEST_RGS_SYNC_TIMESTAMP_NAMESPACE: &str = "";
pub(crate) const LATEST_RGS_SYNC_TIMESTAMP_KEY: &str = "latest_rgs_sync_timestamp";

/// The last time we broadcast a node announcement will be persisted under this key.
pub(crate) const LATEST_NODE_ANN_BCAST_TIMESTAMP_NAMESPACE: &str = "";
pub(crate) const LATEST_NODE_ANN_BCAST_TIMESTAMP_KEY: &str = "latest_node_ann_bcast_timestamp";

/// Provides an interface that allows to store and retrieve persisted values that are associated
/// with given keys.
///
/// In order to avoid collisions the key space is segmented based on the given `namespace`s.
/// Implementations of this trait are free to handle them in different ways, as long as
/// per-namespace key uniqueness is asserted.
///
/// Keys and namespaces are required to be valid ASCII strings and the empty namespace (`""`) is
/// assumed to be valid namespace.
pub trait KVStore: KVStorePersister {
	/// A reader as returned by [`Self::read`].
	type Reader: Read;
	/// Returns a [`Read`] for the given `namespace` and `key` from which [`Readable`]s may be
	/// read.
	///
	/// Returns an [`ErrorKind::NotFound`] if the given `key` could not be found in the given `namespace`.
	///
	/// [`Readable`]: lightning::util::ser::Readable
	/// [`ErrorKind::NotFound`]: std::io::ErrorKind::NotFound
	fn read(&self, namespace: &str, key: &str) -> std::io::Result<Self::Reader>;
	/// Persists the given data under the given `key`.
	///
	/// Will create the given `namespace` if not already present in the store.
	fn write(&self, namespace: &str, key: &str, buf: &[u8]) -> std::io::Result<()>;
	/// Removes any data that had previously been persisted under the given `key`.
	fn remove(&self, namespace: &str, key: &str) -> std::io::Result<()>;
	/// Returns a list of keys that are stored under the given `namespace`.
	///
	/// Will return an empty list if the `namespace` is unknown.
	fn list(&self, namespace: &str) -> std::io::Result<Vec<String>>;
}

fn get_namespace_and_key_from_prefixed(
	prefixed_key: &str,
) -> lightning::io::Result<(String, String)> {
	let dest_file_path = PathBuf::from_str(prefixed_key).map_err(|_| {
		let msg = format!("Could not persist file for key {}.", prefixed_key);
		lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg)
	})?;

	let parent_directory = dest_file_path.parent().ok_or_else(|| {
		let msg = format!("Could not persist file for key {}.", prefixed_key);
		lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg)
	})?;
	let namespace = parent_directory.display().to_string();

	let dest_without_namespace = dest_file_path.strip_prefix(&namespace).map_err(|_| {
		let msg = format!("Could not persist file for key {}.", prefixed_key);
		lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg)
	})?;
	let key = dest_without_namespace.display().to_string();

	Ok((namespace, key))
}

#[cfg(test)]
fn do_read_write_remove_list_persist<K: KVStore>(data: &[u8; 32], kv_store: &K) {
	use lightning::util::ser::Readable;

	let namespace = "testspace";
	let key = "testkey";

	// Test the basic KVStore operations.
	kv_store.write(namespace, key, data).unwrap();

	// Test empty namespace is allowed, but not empty key.
	kv_store.write("", key, data).unwrap();
	assert!(kv_store.write(namespace, "", data).is_err());

	let listed_keys = kv_store.list(namespace).unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], key);

	let mut reader = kv_store.read(namespace, key).unwrap();
	let read_data: [u8; 32] = Readable::read(&mut reader).unwrap();
	assert_eq!(*data, read_data);

	kv_store.remove(namespace, key).unwrap();

	let listed_keys = kv_store.list(namespace).unwrap();
	assert_eq!(listed_keys.len(), 0);

	// Test KVStorePersister
	let prefixed_key = format!("{}/{}", namespace, key);
	kv_store.persist(&prefixed_key, &data).unwrap();
	let mut reader = kv_store.read(namespace, key).unwrap();
	let read_data: [u8; 32] = Readable::read(&mut reader).unwrap();
	assert_eq!(*data, read_data);
}
