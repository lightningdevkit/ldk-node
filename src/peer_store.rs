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
use lightning::util::ser::{CollectionLength, Readable, ReadableArgs, Writeable, Writer};

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

	pub(crate) async fn add_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		self.update_peers(|peers| {
			if peers.contains_key(&peer_info.node_id) {
				return false;
			}
			peers.insert(peer_info.node_id, peer_info);
			true
		})
		.await
	}

	pub(crate) async fn remove_peer(&self, node_id: &PublicKey) -> Result<(), Error> {
		self.update_peers(|peers| peers.remove(node_id).is_some()).await
	}

	/// Applies `update` to a copy of the peer set, persists the result, and only then publishes
	/// it to readers, so that the in-memory peer set never gets ahead of the persisted one.
	///
	/// `update` returns whether it changed anything; if it didn't, nothing is written.
	async fn update_peers<F: FnOnce(&mut HashMap<PublicKey, PeerInfo>) -> bool>(
		&self, update: F,
	) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let mut updated_peers = self.peers.read().expect("lock").clone();
		if !update(&mut updated_peers) {
			return Ok(());
		}
		let data = PeerStoreSerWrapper(&updated_peers).encode();
		self.persist_peers(data).await?;
		*self.peers.write().expect("lock") = updated_peers;
		Ok(())
	}

	/// Returns the current in-memory peer set.
	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().expect("lock").values().cloned().collect()
	}

	/// Returns the current in-memory peer info for `node_id`.
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
		let len: CollectionLength = Readable::read(reader)?;
		// We don't preallocate, as a corrupted count would otherwise have us allocate up to
		// `u64::MAX` entries before failing to read them.
		let mut peers = HashMap::new();
		for _ in 0..len.0 {
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
		CollectionLength(self.0.len() as u64).write(writer)?;
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

	fn test_peer_info() -> PeerInfo {
		PeerInfo {
			node_id: PublicKey::from_str(
				"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
			)
			.unwrap(),
			address: SocketAddress::from_str("127.0.0.1:9738").unwrap(),
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
		peer_store.add_peer(expected_peer_info.clone()).await.unwrap();

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

	#[tokio::test]
	async fn failed_add_peer_is_not_published_and_retried() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let peer_store = PeerStore::new(store, Arc::new(TestLogger::new()));
		let peer_info = test_peer_info();

		assert_eq!(Err(Error::PersistenceFailed), peer_store.add_peer(peer_info.clone()).await);
		assert_eq!(None, peer_store.get_peer(&peer_info.node_id));

		// A retry has to attempt the write again.
		assert_eq!(Err(Error::PersistenceFailed), peer_store.add_peer(peer_info).await);
	}

	#[tokio::test]
	async fn removing_unknown_peer_does_not_write() {
		// Every write to this store fails, so removing only succeeds if nothing is written.
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let peer_store = PeerStore::new(store, Arc::new(TestLogger::new()));

		assert_eq!(Ok(()), peer_store.remove_peer(&test_peer_info().node_id).await);
	}

	#[test]
	fn peer_store_keeps_legacy_encoding_below_u16_max() {
		let peer_info = test_peer_info();
		let mut peers = HashMap::new();
		peers.insert(peer_info.node_id, peer_info.clone());

		// Before `CollectionLength`, the count was written as a plain `u16`.
		let mut legacy_bytes = Vec::new();
		(peers.len() as u16).write(&mut legacy_bytes).unwrap();
		peer_info.node_id.write(&mut legacy_bytes).unwrap();
		peer_info.write(&mut legacy_bytes).unwrap();

		assert_eq!(PeerStoreSerWrapper(&peers).encode(), legacy_bytes);
		let read_peers: PeerStoreDeserWrapper = Readable::read(&mut &legacy_bytes[..]).unwrap();
		assert_eq!(read_peers.0, peers);
	}

	#[test]
	fn peer_store_read_rejects_bogus_peer_count() {
		// A count of 2^60 + 0xffff peers, followed by no peers at all.
		let bytes = [0xff, 0xff, 0x10, 0, 0, 0, 0, 0, 0, 0];
		assert!(<PeerStoreDeserWrapper as Readable>::read(&mut &bytes[..]).is_err());
	}
}
