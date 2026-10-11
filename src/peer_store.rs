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

	pub(crate) async fn add_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let (node_id, data) = {
			let locked_peers = self.peers.read().expect("lock");
			if locked_peers.contains_key(&peer_info.node_id) {
				return Ok(());
			}
			let node_id = peer_info.node_id;
			let mut updated_peers = locked_peers.clone();
			updated_peers.insert(node_id, peer_info.clone());
			(node_id, PeerStoreSerWrapper(&updated_peers).encode())
		};
		self.persist_peers(data).await?;
		self.peers.write().expect("lock").insert(node_id, peer_info);
		Ok(())
	}

	pub(crate) async fn remove_peer(&self, node_id: &PublicKey) -> Result<(), Error> {
		let _guard = self.mutation_lock.lock().await;
		let data = {
			let locked_peers = self.peers.read().expect("lock");
			let mut updated_peers = locked_peers.clone();
			updated_peers.remove(node_id);
			PeerStoreSerWrapper(&updated_peers).encode()
		};
		self.persist_peers(data).await?;
		self.peers.write().expect("lock").remove(node_id);
		Ok(())
	}

	/// Returns the current in-memory peer set.
	///
	/// Changes are only published once they have been persisted, but this synchronous reader
	/// cannot wait on the async mutation lock, so it may not yet reflect a change in flight.
	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().expect("lock").values().cloned().collect()
	}

	/// Returns the current in-memory peer info for `node_id`.
	///
	/// Changes are only published once they have been persisted, but this synchronous reader
	/// cannot wait on the async mutation lock, so it may not yet reflect a change in flight.
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

	struct FailFirstWriteStore {
		inner: InMemoryStore,
		fail_next_write: std::sync::atomic::AtomicBool,
		// Whether the failing write still reaches the underlying store, i.e., commits without
		// being acknowledged.
		commit_failed_write: bool,
	}

	impl FailFirstWriteStore {
		fn new(commit_failed_write: bool) -> Self {
			Self {
				inner: InMemoryStore::new(),
				fail_next_write: std::sync::atomic::AtomicBool::new(true),
				commit_failed_write,
			}
		}
	}

	impl KVStore for FailFirstWriteStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			KVStore::read(&self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			let fail = self.fail_next_write.swap(false, std::sync::atomic::Ordering::SeqCst);
			let fut = (!fail || self.commit_failed_write).then(|| {
				KVStore::write(&self.inner, primary_namespace, secondary_namespace, key, buf)
			});
			async move {
				let res = match fut {
					Some(fut) => fut.await,
					None => Ok(()),
				};
				if fail {
					Err(io::Error::new(io::ErrorKind::Other, "write failed"))
				} else {
					res
				}
			}
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			KVStore::remove(&self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			KVStore::list(&self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl PaginatedKVStore for FailFirstWriteStore {
		fn list_paginated(
			&self, primary_namespace: &str, secondary_namespace: &str,
			page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			PaginatedKVStore::list_paginated(
				&self.inner,
				primary_namespace,
				secondary_namespace,
				page_token,
			)
		}
	}

	#[tokio::test]
	async fn add_peer_is_retried_after_persist_failure() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailFirstWriteStore::new(false)));
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9738").unwrap() };

		// A failed write must not leave the peer visible as stored.
		assert_eq!(Err(Error::PersistenceFailed), peer_store.add_peer(peer_info.clone()).await);
		assert_eq!(None, peer_store.get_peer(&node_id));

		// The retry has to persist it, so it survives a restart.
		assert_eq!(Ok(()), peer_store.add_peer(peer_info.clone()).await);
		assert_eq!(Some(peer_info.clone()), peer_store.get_peer(&node_id));

		let persisted_bytes = KVStore::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.await
		.unwrap();
		let restored = PeerStore::read(&mut &persisted_bytes[..], (store, logger)).unwrap();
		assert_eq!(Some(peer_info), restored.get_peer(&node_id));
	}

	#[tokio::test]
	async fn failed_add_peer_keeps_other_peers_and_retry_persists_all() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailFirstWriteStore::new(false)));
		let logger = Arc::new(TestLogger::new());

		let existing_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let existing = PeerInfo {
			node_id: existing_id,
			address: SocketAddress::from_str("127.0.0.1:9738").unwrap(),
		};
		let mut peers = HashMap::new();
		peers.insert(existing_id, existing.clone());
		let persisted_bytes = PeerStoreSerWrapper(&peers).encode();
		let peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), Arc::clone(&logger)))
				.unwrap();

		let new_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let new_peer = PeerInfo {
			node_id: new_id,
			address: SocketAddress::from_str("127.0.0.1:9739").unwrap(),
		};

		assert_eq!(Err(Error::PersistenceFailed), peer_store.add_peer(new_peer.clone()).await);
		assert_eq!(vec![existing.clone()], peer_store.list_peers());

		assert_eq!(Ok(()), peer_store.add_peer(new_peer.clone()).await);
		let persisted_bytes = KVStore::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.await
		.unwrap();
		let restored = PeerStore::read(&mut &persisted_bytes[..], (store, logger)).unwrap();
		assert_eq!(Some(existing), restored.get_peer(&existing_id));
		assert_eq!(Some(new_peer), restored.get_peer(&new_id));
	}

	#[tokio::test]
	async fn add_peer_retry_is_idempotent_after_unacknowledged_commit() {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailFirstWriteStore::new(true)));
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let peer_info =
			PeerInfo { node_id, address: SocketAddress::from_str("127.0.0.1:9738").unwrap() };

		// The write reached the store, but we weren't told so we must not publish the peer.
		assert_eq!(Err(Error::PersistenceFailed), peer_store.add_peer(peer_info.clone()).await);
		assert_eq!(None, peer_store.get_peer(&node_id));

		assert_eq!(Ok(()), peer_store.add_peer(peer_info.clone()).await);
		assert_eq!(Some(peer_info.clone()), peer_store.get_peer(&node_id));
		assert_eq!(1, peer_store.list_peers().len());
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
}
