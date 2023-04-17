use crate::hex_utils;
use crate::io::{
	KVStore, TransactionalWrite, PEER_INFO_PERSISTENCE_KEY, PEER_INFO_PERSISTENCE_NAMESPACE,
};
use crate::logger::{log_error, Logger};
use crate::Error;

use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::PublicKey;

use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
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

		locked_peers.insert(peer_info.pubkey, peer_info);
		self.write_peers_and_commit(&*locked_peers)
	}

	pub(crate) fn remove_peer(&self, peer_pubkey: &PublicKey) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.remove(peer_pubkey);
		self.write_peers_and_commit(&*locked_peers)
	}

	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().unwrap().values().cloned().collect()
	}

	pub(crate) fn get_peer(&self, peer_pubkey: &PublicKey) -> Option<PeerInfo> {
		self.peers.read().unwrap().get(peer_pubkey).cloned()
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
	pub pubkey: PublicKey,
	pub address: SocketAddr,
}

impl Readable for PeerInfo {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let pubkey = Readable::read(reader)?;

		let ip_type: u8 = Readable::read(reader)?;

		let ip_addr = if ip_type == 0 {
			let v4bytes: u32 = Readable::read(reader)?;
			let v4addr = Ipv4Addr::from(v4bytes);
			IpAddr::from(v4addr)
		} else {
			let v6bytes: u128 = Readable::read(reader)?;
			let v6addr = Ipv6Addr::from(v6bytes);
			IpAddr::from(v6addr)
		};

		let port: u16 = Readable::read(reader)?;

		let address = SocketAddr::new(ip_addr, port);

		Ok(PeerInfo { pubkey, address })
	}
}

impl Writeable for PeerInfo {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		self.pubkey.write(writer)?;

		match self.address.ip() {
			IpAddr::V4(v4addr) => {
				0u8.write(writer)?;
				u32::from(v4addr).write(writer)?;
			}
			IpAddr::V6(v6addr) => {
				1u8.write(writer)?;
				u128::from(v6addr).write(writer)?;
			}
		}

		self.address.port().write(writer)?;

		Ok(())
	}
}

impl TryFrom<String> for PeerInfo {
	type Error = Error;

	fn try_from(peer_pubkey_and_ip_addr: String) -> Result<Self, Self::Error> {
		if let Some((pubkey_str, peer_str)) = peer_pubkey_and_ip_addr.split_once('@') {
			if let Some(pubkey) = hex_utils::to_compressed_pubkey(pubkey_str) {
				if let Some(peer_addr) =
					peer_str.to_socket_addrs().ok().and_then(|mut r| r.next()).map(|pa| pa)
				{
					return Ok(PeerInfo { pubkey, address: peer_addr });
				}
			}
		}
		Err(Error::PeerInfoParseFailed)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test::utils::{TestLogger, TestStore};
	use proptest::prelude::*;
	use std::str::FromStr;
	use std::sync::Arc;

	#[test]
	fn peer_info_persistence() {
		let store = Arc::new(TestStore::new());
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let pubkey = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address: SocketAddr = "127.0.0.1:9738".parse().unwrap();
		let expected_peer_info = PeerInfo { pubkey, address };
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
		assert_eq!(deser_peer_store.get_peer(&pubkey), Some(expected_peer_info));
		assert!(!store.get_and_clear_did_persist());
	}

	#[test]
	fn peer_info_parsing() {
		let valid_peer_info_str =
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993@127.0.0.1:9738"
				.to_string();

		let pubkey = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address: SocketAddr = "127.0.0.1:9738".parse().unwrap();
		let expected_peer_info = PeerInfo { pubkey, address };

		assert_eq!(Ok(expected_peer_info), PeerInfo::try_from(valid_peer_info_str));

		let invalid_peer_info_str1 =
			"02-76607124-ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993@127.0.0.1:9738"
				.to_string();
		assert_eq!(Err(Error::PeerInfoParseFailed), PeerInfo::try_from(invalid_peer_info_str1));

		let invalid_peer_info_str2 =
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993@333.0.0.1:9738"
				.to_string();
		assert_eq!(Err(Error::PeerInfoParseFailed), PeerInfo::try_from(invalid_peer_info_str2));

		let invalid_peer_info_str3 =
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993@127.0.0.19738"
				.to_string();
		assert_eq!(Err(Error::PeerInfoParseFailed), PeerInfo::try_from(invalid_peer_info_str3));
	}

	proptest! {
		#[test]
		fn peer_info_parsing_doesnt_crash(s in "\\PC*") {
			let _ = PeerInfo::try_from(s.to_string());
		}
	}
}
