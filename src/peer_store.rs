use crate::io::{
	KVStore, TransactionalWrite, PEER_INFO_PERSISTENCE_KEY, PEER_INFO_PERSISTENCE_NAMESPACE,
};
use crate::logger::{log_error, Logger};
use crate::{Error, NetAddress};

use lightning::impl_writeable_tlv_based;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::PublicKey;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::RwLock;

pub struct PeerStore<K: Deref, L: Deref>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	peers: RwLock<HashMap<PublicKey, PeerInfo>>,
	kv_store: K,
	logger: L,
}

impl<K: Deref, L: Deref> PeerStore<K, L>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	pub(crate) fn new(kv_store: K, logger: L) -> Self {
		let peers = RwLock::new(HashMap::new());
		Self { peers, kv_store, logger }
	}

	pub(crate) fn add_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.insert(peer_info.node_id, peer_info);
		self.write_peers_and_commit(&*locked_peers)
	}

	pub(crate) fn remove_peer(&self, node_id: &PublicKey) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.remove(node_id);
		self.write_peers_and_commit(&*locked_peers)
	}

	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().unwrap().values().cloned().collect()
	}

	pub(crate) fn get_peer(&self, node_id: &PublicKey) -> Option<PeerInfo> {
		self.peers.read().unwrap().get(node_id).cloned()
	}

	fn write_peers_and_commit(
		&self, locked_peers: &HashMap<PublicKey, PeerInfo>,
	) -> Result<(), Error> {
		let mut writer = self
			.kv_store
			.write(PEER_INFO_PERSISTENCE_NAMESPACE, PEER_INFO_PERSISTENCE_KEY)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Getting writer for key {}/{} failed due to: {}",
					PEER_INFO_PERSISTENCE_NAMESPACE,
					PEER_INFO_PERSISTENCE_KEY,
					e
				);
				Error::PersistenceFailed
			})?;
		PeerStoreSerWrapper(&*locked_peers).write(&mut writer).map_err(|e| {
			log_error!(
				self.logger,
				"Writing peer data to key {}/{} failed due to: {}",
				PEER_INFO_PERSISTENCE_NAMESPACE,
				PEER_INFO_PERSISTENCE_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
		writer.commit().map_err(|e| {
			log_error!(
				self.logger,
				"Committing peer data to key {}/{} failed due to: {}",
				PEER_INFO_PERSISTENCE_NAMESPACE,
				PEER_INFO_PERSISTENCE_KEY,
				e
			);
			Error::PersistenceFailed
		})
	}
}

impl<K: Deref, L: Deref> ReadableArgs<(K, L)> for PeerStore<K, L>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, args: (K, L),
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let (kv_store, logger) = args;
		let read_peers: PeerStoreDeserWrapper = Readable::read(reader)?;
		let peers: RwLock<HashMap<PublicKey, PeerInfo>> = RwLock::new(read_peers.0);
		Ok(Self { peers, kv_store, logger })
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
	pub address: NetAddress,
}

impl_writeable_tlv_based!(PeerInfo, {
	(0, node_id, required),
	(2, address, required),
});

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test::utils::{TestLogger, TestStore};
	use std::str::FromStr;
	use std::sync::Arc;

	#[test]
	fn peer_info_persistence() {
		let store = Arc::new(TestStore::new());
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address = NetAddress::from_str("127.0.0.1:9738").unwrap();
		let expected_peer_info = PeerInfo { node_id, address };
		peer_store.add_peer(expected_peer_info.clone()).unwrap();
		assert!(store.get_and_clear_did_persist());

		// Check we can read back what we persisted.
		let persisted_bytes = store
			.get_persisted_bytes(PEER_INFO_PERSISTENCE_NAMESPACE, PEER_INFO_PERSISTENCE_KEY)
			.unwrap();
		let deser_peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();

		let peers = deser_peer_store.list_peers();
		assert_eq!(peers.len(), 1);
		assert_eq!(peers[0], expected_peer_info);
		assert_eq!(deser_peer_store.get_peer(&node_id), Some(expected_peer_info));
		assert!(!store.get_and_clear_did_persist());
	}
}
