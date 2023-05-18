//! Objects and traits for data persistence.

pub(crate) mod fs_store;
pub(crate) mod utils;

pub use fs_store::FilesystemStore;

use lightning::util::persist::KVStorePersister;

use std::io::Read;

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
	///
	/// Returns `true` if the `key` was present in the given `namespace`, and `false` otherwise.
	fn remove(&self, namespace: &str, key: &str) -> std::io::Result<bool>;
	/// Returns a list of keys that are stored under the given `namespace`.
	///
	/// Will return an empty list if the `namespace` is unknown.
	fn list(&self, namespace: &str) -> std::io::Result<Vec<String>>;
}
