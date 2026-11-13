// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use crate::io::{
	PEER_INFO_PERSISTENCE_KEY, PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger};
use crate::types::DynStore;
use crate::{Error, SocketAddress};

pub struct PeerStore<L: Deref>
where
	L::Target: LdkLogger,
{
	peers: RwLock<HashMap<PublicKey, PeerInfo>>,
	mutation_lock: tokio::sync::Mutex<()>,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> PeerStore<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(kv_store: Arc<DynStore>, logger: L) -> Self {
		let peers = RwLock::new(HashMap::new());
		let mutation_lock = tokio::sync::Mutex::new(());
		Self { peers, mutation_lock, kv_store, logger }
	}

	/// Stores the given peer only if no entry for it exists yet.
	///
	/// In particular, an existing peer's stored address is left untouched. Use this for
	/// gossip- or event-derived peers (e.g., inbound channel counterparties), where the
	/// discovered address must not override an explicitly stored one.
	///
	/// The in-memory state is only updated after the new state has been persisted
	/// successfully.
	pub(crate) async fn add_peer_if_missing(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;

		{
			let peers = self.peers.read().expect("lock");
			if peers.contains_key(&peer_info.node_id) {
				return Ok(());
			}
		}

		self.persist_and_apply(peer_info).await
	}

	/// Stores the given peer, updating the stored address if the peer is already known.
	///
	/// Use this for explicitly supplied addresses (e.g., via [`crate::Node::connect`] with
	/// `persist` set), where a re-connect to a new address should replace the one used for
	/// reconnection.
	///
	/// The in-memory state is only updated after the new state has been persisted
	/// successfully.
	pub(crate) async fn upsert_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;

		{
			let peers = self.peers.read().expect("lock");
			if peers.get(&peer_info.node_id) == Some(&peer_info) {
				return Ok(());
			}
		}

		self.persist_and_apply(peer_info).await
	}

	async fn persist_and_apply(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let data = {
			let mut locked_peers = self.peers.write().expect("lock");

			// Temporarily apply the update so it is included in the serialized state, then
			// restore the current state before persisting: readers cannot observe the
			// temporary value as they need the same lock. This avoids cloning the entire
			// peer map while retaining the persistence-first semantics.
			let previous = locked_peers.insert(peer_info.node_id, peer_info.clone());
			let data = PeerStoreSerWrapper(&locked_peers).encode();
			match previous {
				Some(previous) => {
					locked_peers.insert(peer_info.node_id, previous);
				},
				None => {
					locked_peers.remove(&peer_info.node_id);
				},
			}
			data
		};

		self.persist_peers(data).await?;

		// Persistence succeeded, so apply the update permanently.
		self.peers.write().expect("lock").insert(peer_info.node_id, peer_info);
		Ok(())
	}

	pub(crate) async fn remove_peer(&self, node_id: &PublicKey) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let data = {
			let mut locked_peers = self.peers.write().expect("lock");

			// Temporarily apply the removal so it is excluded from the serialized state,
			// then restore the current state before persisting: readers cannot observe
			// the temporary value as they need the same lock. This avoids cloning the
			// entire peer map while retaining the persistence-first semantics.
			let previous = locked_peers.remove(node_id);
			let data = PeerStoreSerWrapper(&locked_peers).encode();
			if let Some(previous) = previous {
				locked_peers.insert(*node_id, previous);
			}
			data
		};
		self.persist_peers(data).await?;

		// Persistence succeeded, so apply the removal permanently.
		self.peers.write().expect("lock").remove(node_id);
		Ok(())
	}

	/// Returns the current in-memory peer set.
	///
	/// The async mutation lock serializes peer-store mutations, but this synchronous
	/// reader cannot wait on it. Until peer-store reads are async, callers may observe peer
	/// changes that are still being persisted.
	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().expect("lock").values().cloned().collect()
	}

	/// Returns the current in-memory peer info for `node_id`.
	///
	/// The async mutation lock serializes peer-store mutations, but this synchronous
	/// reader cannot wait on it. Until peer-store reads are async, callers may observe peer
	/// changes that are still being persisted.
	pub(crate) fn get_peer(&self, node_id: &PublicKey) -> Option<PeerInfo> {
		self.peers.read().expect("lock").get(node_id).cloned()
	}

	async fn persist_peers(&self, data: Vec<u8>) -> Result<(), Error> {
		KVStore::write(
			&*self.kv_store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
			data,
		)
		.await
		.map_err(|e| {
			log_error!(
				self.logger,
				"Write for key {}/{}/{} failed due to: {}",
				PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				PEER_INFO_PERSISTENCE_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
		Ok(())
	}
}

impl<L: Deref> ReadableArgs<(Arc<DynStore>, L)> for PeerStore<L>
where
	L::Target: LdkLogger,
{
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, args: (Arc<DynStore>, L),
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let (kv_store, logger) = args;
		let read_peers: PeerStoreDeserWrapper = Readable::read(reader)?;
		let peers: RwLock<HashMap<PublicKey, PeerInfo>> = RwLock::new(read_peers.0);
		let mutation_lock = tokio::sync::Mutex::new(());
		Ok(Self { peers, mutation_lock, kv_store, logger })
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerStoreDeserWrapper(HashMap<PublicKey, PeerInfo>);

impl Readable for PeerStoreDeserWrapper {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut peers = HashMap::with_capacity(len as usize);
		for _ in 0..len {
			let k: PublicKey = Readable::read(reader)?;
			let v: PeerInfo = Readable::read(reader)?;
			peers.insert(k, v);
		}
		Ok(Self(peers))
	}
}

pub(crate) struct PeerStoreSerWrapper<'a>(&'a HashMap<PublicKey, PeerInfo>);

impl Writeable for PeerStoreSerWrapper<'_> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for (k, v) in self.0.iter() {
			k.write(writer)?;
			v.write(writer)?;
		}
		Ok(())
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerInfo {
	pub node_id: PublicKey,
	pub address: SocketAddress,
}

impl_writeable_tlv_based!(PeerInfo, {
	(0, node_id, required),
	(2, address, required),
});

#[cfg(test)]
mod tests {
	use std::str::FromStr;
	use std::sync::Arc;

	use bitcoin::io;
	use lightning::util::persist::{PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning::util::test_utils::TestLogger;

	use super::*;
	use crate::io::test_utils::InMemoryStore;
	use crate::types::DynStoreWrapper;

	struct FailingStore;

	impl KVStore for FailingStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "read failed")) }
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "write failed")) }
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "remove failed")) }
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "list failed")) }
		}
	}

	impl PaginatedKVStore for FailingStore {
		fn list_paginated(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
			_page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			async { Err(io::Error::new(io::ErrorKind::Other, "list_paginated failed")) }
		}
	}

	#[tokio::test]
	async fn peer_info_persistence() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();
		let expected_peer_info = PeerInfo { node_id, address };
		assert!(KVStore::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.await
		.is_err());
		peer_store.upsert_peer(expected_peer_info.clone()).await.unwrap();

		// Check we can read back what we persisted.
		let persisted_bytes = KVStore::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.await
		.unwrap();
		let deser_peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();

		let peers = deser_peer_store.list_peers();
		assert_eq!(peers.len(), 1);
		assert_eq!(peers[0], expected_peer_info);
		assert_eq!(deser_peer_store.get_peer(&node_id), Some(expected_peer_info));
	}

	#[tokio::test]
	async fn remove_peer_does_not_mutate_memory_if_persist_fails() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let logger = Arc::new(TestLogger::new());
		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9738").unwrap() };
		let mut peers = HashMap::new();
		peers.insert(node_id, peer_info.clone());
		let persisted_bytes = PeerStoreSerWrapper(&peers).encode();
		let peer_store = PeerStore::read(&mut &persisted_bytes[..], (store, logger)).unwrap();

		assert_eq!(Err(Error::PersistenceFailed), peer_store.remove_peer(&node_id).await);
		assert_eq!(Some(peer_info), peer_store.get_peer(&node_id));
	}

	fn test_node_id() -> PublicKey {
		PublicKey::from_str("0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993")
			.unwrap()
	}

	#[tokio::test]
	async fn upsert_peer_updates_stored_address() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = test_node_id();
		let old_address = SocketAddress::from_str("127.0.0.1:9738").unwrap();
		let new_address = SocketAddress::from_str("127.0.0.1:9739").unwrap();

		let old_peer_info = PeerInfo { node_id, address: old_address };
		peer_store.upsert_peer(old_peer_info.clone()).await.unwrap();
		assert_eq!(peer_store.get_peer(&node_id), Some(old_peer_info));

		// Re-adding the same peer with a new address must update the stored entry, also on
		// disk.
		let new_peer_info = PeerInfo { node_id, address: new_address };
		peer_store.upsert_peer(new_peer_info.clone()).await.unwrap();
		assert_eq!(peer_store.get_peer(&node_id), Some(new_peer_info.clone()));

		let persisted_bytes = KVStore::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.await
		.unwrap();
		let deser_peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();
		assert_eq!(deser_peer_store.get_peer(&node_id), Some(new_peer_info));
	}

	#[tokio::test]
	async fn add_peer_if_missing_does_not_overwrite_stored_address() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = test_node_id();
		let stored =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9738").unwrap() };
		peer_store.upsert_peer(stored.clone()).await.unwrap();

		// A gossip-/event-derived address must not overwrite the explicitly stored one.
		let discovered =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9739").unwrap() };
		peer_store.add_peer_if_missing(discovered).await.unwrap();
		assert_eq!(peer_store.get_peer(&node_id), Some(stored));
	}

	#[tokio::test]
	async fn readding_unchanged_peer_skips_persist() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let logger = Arc::new(TestLogger::new());

		let node_id = test_node_id();
		let peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9738").unwrap() };

		// Seed the store's in-memory state with the peer info. Any attempt to persist
		// below would fail, so succeeding proves the no-op paths skip persistence.
		let mut peers = HashMap::new();
		peers.insert(node_id, peer_info.clone());
		let persisted_bytes = PeerStoreSerWrapper(&peers).encode();
		let peer_store = PeerStore::read(&mut &persisted_bytes[..], (store, logger)).unwrap();

		// Re-adding the unchanged peer must be a no-op and not write again.
		peer_store.upsert_peer(peer_info.clone()).await.unwrap();

		// Nor must the add-if-missing path write (or overwrite) for a known peer.
		let other_peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9739").unwrap() };
		peer_store.add_peer_if_missing(other_peer_info).await.unwrap();
		assert_eq!(Some(peer_info), peer_store.get_peer(&node_id));
	}

	#[tokio::test]
	async fn upsert_peer_does_not_mutate_memory_if_persist_fails() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let logger = Arc::new(TestLogger::new());

		let node_id = test_node_id();
		let old_peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9738").unwrap() };
		let new_peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9739").unwrap() };

		// Seed the store's in-memory state with the old peer info.
		let mut peers = HashMap::new();
		peers.insert(node_id, old_peer_info.clone());
		let persisted_bytes = PeerStoreSerWrapper(&peers).encode();
		let peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), Arc::clone(&logger)))
				.unwrap();

		// Updating the stored address fails, so the old peer info must remain untouched.
		assert_eq!(
			Err(Error::PersistenceFailed),
			peer_store.upsert_peer(new_peer_info.clone()).await
		);
		assert_eq!(Some(old_peer_info.clone()), peer_store.get_peer(&node_id));

		// Inserting a new peer fails too, so no entry must be left behind.
		let peer_store = PeerStore::new(store, logger);
		assert_eq!(
			Err(Error::PersistenceFailed),
			peer_store.upsert_peer(old_peer_info.clone()).await
		);
		assert_eq!(None, peer_store.get_peer(&node_id));
	}
}
