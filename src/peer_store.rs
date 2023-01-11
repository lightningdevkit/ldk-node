use crate::hex_utils;
use crate::Error;

use lightning::util::persist::KVStorePersister;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::PublicKey;

use std::collections::HashMap;
use std::convert::TryFrom;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock};

/// The peer information will be persisted under this key.
pub(crate) const PEER_INFO_PERSISTENCE_KEY: &str = "peers";

pub(crate) struct PeerInfoStorage<K: KVStorePersister> {
	peers: RwLock<HashMap<PublicKey, PeerInfo>>,
	persister: Arc<K>,
}

impl<K: KVStorePersister> PeerInfoStorage<K> {
	pub(crate) fn new(persister: Arc<K>) -> Self {
		let peers = RwLock::new(HashMap::new());
		Self { peers, persister }
	}

	pub(crate) fn add_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.insert(peer_info.pubkey, peer_info);

		self.persister
			.persist(PEER_INFO_PERSISTENCE_KEY, &PeerInfoStorageSerWrapper(&*locked_peers))
			.map_err(|_| Error::PersistenceFailed)?;

		Ok(())
	}

	pub(crate) fn remove_peer(&self, peer_pubkey: &PublicKey) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.remove(peer_pubkey);

		self.persister
			.persist(PEER_INFO_PERSISTENCE_KEY, &PeerInfoStorageSerWrapper(&*locked_peers))
			.map_err(|_| Error::PersistenceFailed)?;

		Ok(())
	}

	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().unwrap().values().cloned().collect()
	}

	pub(crate) fn get_peer(&self, peer_pubkey: &PublicKey) -> Option<PeerInfo> {
		self.peers.read().unwrap().get(peer_pubkey).cloned()
	}
}

impl<K: KVStorePersister> ReadableArgs<Arc<K>> for PeerInfoStorage<K> {
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, persister: Arc<K>,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let read_peers: PeerInfoStorageDeserWrapper = Readable::read(reader)?;
		let peers: RwLock<HashMap<PublicKey, PeerInfo>> = RwLock::new(read_peers.0);
		Ok(Self { peers, persister })
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerInfoStorageDeserWrapper(HashMap<PublicKey, PeerInfo>);

impl Readable for PeerInfoStorageDeserWrapper {
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

pub(crate) struct PeerInfoStorageSerWrapper<'a>(&'a HashMap<PublicKey, PeerInfo>);

impl Writeable for PeerInfoStorageSerWrapper<'_> {
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
	use crate::tests::test_utils::TestPersister;
	use proptest::prelude::*;
	use std::str::FromStr;

	#[test]
	fn peer_info_persistence() {
		let persister = Arc::new(TestPersister::new());
		let peer_store = PeerInfoStorage::new(Arc::clone(&persister));

		let pubkey = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address: SocketAddr = "127.0.0.1:9738".parse().unwrap();
		let expected_peer_info = PeerInfo { pubkey, address };
		peer_store.add_peer(expected_peer_info.clone()).unwrap();
		assert!(persister.get_and_clear_did_persist());

		// Check we can read back what we persisted.
		let persisted_bytes = persister.get_persisted_bytes(PEER_INFO_PERSISTENCE_KEY).unwrap();
		let deser_peer_store =
			PeerInfoStorage::read(&mut &persisted_bytes[..], Arc::clone(&persister)).unwrap();

		let peers = deser_peer_store.list_peers();
		assert_eq!(peers.len(), 1);
		assert_eq!(peers[0], expected_peer_info);
		assert_eq!(deser_peer_store.get_peer(&pubkey), Some(expected_peer_info));
		assert!(!persister.get_and_clear_did_persist());
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
