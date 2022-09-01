use crate::hex;
use crate::Error;

use lightning::util::persist::KVStorePersister;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::PublicKey;

use std::convert::TryFrom;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock};

/// The peer information will be persisted under this key.
pub(crate) const PEER_INFO_PERSISTENCE_KEY: &str = "peers";

pub(crate) struct PeerInfoStorage<K: KVStorePersister> {
	peers: RwLock<PeerInfoStorageSerWrapper>,
	persister: Arc<K>,
}

impl<K: KVStorePersister> PeerInfoStorage<K> {
	pub(crate) fn new(persister: Arc<K>) -> Self {
		let peers = RwLock::new(PeerInfoStorageSerWrapper(Vec::new()));
		Self { peers, persister }
	}

	pub(crate) fn add_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		// Check if we have the peer. If so, either update it or do nothing.
		for stored_info in locked_peers.0.iter_mut() {
			if stored_info.pubkey == peer_info.pubkey {
				if stored_info.address != peer_info.address {
					stored_info.address = peer_info.address;
				}
				return Ok(());
			}
		}

		locked_peers.0.push(peer_info);
		self.persister.persist(PEER_INFO_PERSISTENCE_KEY, &*locked_peers)?;

		return Ok(());
	}

	pub(crate) fn remove_peer(&self, peer_pubkey: &PublicKey) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.0.retain(|info| info.pubkey != *peer_pubkey);

		self.persister.persist(PEER_INFO_PERSISTENCE_KEY, &*locked_peers)?;

		return Ok(());
	}

	pub(crate) fn peers(&self) -> Vec<PeerInfo> {
		self.peers.read().unwrap().0.clone()
	}
}

impl<K: KVStorePersister> ReadableArgs<Arc<K>> for PeerInfoStorage<K> {
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, persister: Arc<K>,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let peers: RwLock<PeerInfoStorageSerWrapper> = RwLock::new(Readable::read(reader)?);
		Ok(Self { peers, persister })
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerInfoStorageSerWrapper(Vec<PeerInfo>);

impl Readable for PeerInfoStorageSerWrapper {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut peers = Vec::with_capacity(len as usize);
		for _ in 0..len {
			peers.push(Readable::read(reader)?);
		}
		Ok(PeerInfoStorageSerWrapper(peers))
	}
}

impl Writeable for PeerInfoStorageSerWrapper {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for e in self.0.iter() {
			e.write(writer)?;
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
			let octets: [u8; 4] = Readable::read(reader)?;
			IpAddr::from(octets)
		} else {
			let octets: [u8; 16] = Readable::read(reader)?;
			IpAddr::from(octets)
		};

		let port: u16 = Readable::read(reader)?;

		let address = SocketAddr::new(ip_addr, port);

		Ok(PeerInfo { pubkey, address })
	}
}

impl Writeable for PeerInfo {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		self.pubkey.write(writer)?;

		let ip_type: u8 = if self.address.ip().is_ipv4() { 0 } else { 1 };
		ip_type.write(writer)?;

		let octets = match self.address.ip() {
			IpAddr::V4(ip) => ip.octets().to_vec(),
			IpAddr::V6(ip) => ip.octets().to_vec(),
		};
		octets.write(writer)?;

		self.address.port().write(writer)?;

		Ok(())
	}
}

impl TryFrom<String> for PeerInfo {
	type Error = Error;

	fn try_from(peer_pubkey_and_ip_addr: String) -> Result<Self, Self::Error> {
		let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
		let pubkey = pubkey_and_addr.next();
		let peer_addr_str = pubkey_and_addr.next();
		if pubkey.is_none() || peer_addr_str.is_none() {
			return Err(Error::PeerInfoParse(
				"Incorrect format. Should be formatted as: `pubkey@host:port`.",
			));
		}

		let peer_addr = peer_addr_str.unwrap().to_socket_addrs().map(|mut r| r.next());
		if peer_addr.is_err() || peer_addr.as_ref().unwrap().is_none() {
			return Err(Error::PeerInfoParse(
				"Couldn't parse pubkey@host:port into a socket address.",
			));
		}

		let pubkey = hex::to_compressed_pubkey(pubkey.unwrap());
		if pubkey.is_none() {
			return Err(Error::PeerInfoParse("Unable to parse pubkey for node."));
		}

		Ok(PeerInfo { pubkey: pubkey.unwrap(), address: peer_addr.unwrap().unwrap() })
	}
}
