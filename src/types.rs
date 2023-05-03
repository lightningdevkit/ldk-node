use crate::error::Error;
use crate::hex_utils;
use crate::io::fs_store::FilesystemStore;
use crate::logger::FilesystemLogger;
use crate::wallet::{Wallet, WalletKeysManager};
use crate::UniffiCustomTypeConverter;

use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::InMemorySigner;
use lightning::ln::channelmanager::ChannelDetails as LdkChannelDetails;
use lightning::ln::msgs::NetAddress as LdkNetAddress;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::gossip;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::util::ser::{Hostname, Readable, Writeable, Writer};
use lightning_invoice::{Invoice, SignedRawInvoice};
use lightning_net_tokio::SocketDescriptor;
use lightning_transaction_sync::EsploraSyncClient;

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Address, Network, OutPoint, Txid};

use core::convert::TryFrom;
use std::convert::TryInto;
use std::fmt::Display;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

pub(crate) type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	Arc<Wallet<bdk::database::SqliteDatabase>>,
	Arc<Wallet<bdk::database::SqliteDatabase>>,
	Arc<FilesystemLogger>,
	Arc<FilesystemStore>,
>;

pub(crate) type PeerManager = lightning::ln::peer_handler::PeerManager<
	SocketDescriptor,
	Arc<ChannelManager>,
	Arc<dyn RoutingMessageHandler + Send + Sync>,
	Arc<OnionMessenger>,
	Arc<FilesystemLogger>,
	IgnoringMessageHandler,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase>>,
>;

pub(crate) type ChannelManager = lightning::ln::channelmanager::ChannelManager<
	Arc<ChainMonitor>,
	Arc<Wallet<bdk::database::SqliteDatabase>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase>>,
	Arc<Wallet<bdk::database::SqliteDatabase>>,
	Arc<Router>,
	Arc<FilesystemLogger>,
>;

pub(crate) type KeysManager = WalletKeysManager<bdk::database::SqliteDatabase>;

pub(crate) type Router =
	DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>, Arc<Mutex<Scorer>>>;
pub(crate) type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

pub(crate) type UtxoLookup = dyn lightning::routing::utxo::UtxoLookup + Send + Sync;

pub(crate) type P2PGossipSync = lightning::routing::gossip::P2PGossipSync<
	Arc<NetworkGraph>,
	Arc<UtxoLookup>,
	Arc<FilesystemLogger>,
>;
pub(crate) type RapidGossipSync =
	lightning_rapid_gossip_sync::RapidGossipSync<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

pub(crate) type GossipSync = lightning_background_processor::GossipSync<
	Arc<P2PGossipSync>,
	Arc<RapidGossipSync>,
	Arc<NetworkGraph>,
	Arc<UtxoLookup>,
	Arc<FilesystemLogger>,
>;

pub(crate) type OnionMessenger = lightning::onion_message::OnionMessenger<
	Arc<WalletKeysManager<bdk::database::SqliteDatabase>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase>>,
	Arc<FilesystemLogger>,
	IgnoringMessageHandler,
>;

impl UniffiCustomTypeConverter for PublicKey {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(key) = PublicKey::from_str(&val) {
			return Ok(key);
		}

		Err(Error::InvalidPublicKey.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Address {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(addr) = Address::from_str(&val) {
			return Ok(addr);
		}

		Err(Error::InvalidAddress.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Invoice {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(signed) = val.parse::<SignedRawInvoice>() {
			if let Ok(invoice) = Invoice::from_signed(signed) {
				return Ok(invoice);
			}
		}

		Err(Error::InvalidInvoice.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for PaymentHash {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(hash) = Sha256::from_str(&val) {
			Ok(PaymentHash(hash.into_inner()))
		} else {
			Err(Error::InvalidPaymentHash.into())
		}
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		Sha256::from_slice(&obj.0).unwrap().to_string()
	}
}

impl UniffiCustomTypeConverter for PaymentPreimage {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			let bytes_res = bytes_vec.try_into();
			if let Ok(bytes) = bytes_res {
				return Ok(PaymentPreimage(bytes));
			}
		}
		Err(Error::InvalidPaymentPreimage.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl UniffiCustomTypeConverter for PaymentSecret {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			let bytes_res = bytes_vec.try_into();
			if let Ok(bytes) = bytes_res {
				return Ok(PaymentSecret(bytes));
			}
		}
		Err(Error::InvalidPaymentSecret.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

/// The global identifier of a channel.
///
/// Note that this will start out to be a temporary ID until channel funding negotiation is
/// finalized, at which point it will change to be a permanent global ID tied to the on-chain
/// funding transaction.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ChannelId(pub [u8; 32]);

impl UniffiCustomTypeConverter for ChannelId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(hex_vec) = hex_utils::to_vec(&val) {
			if hex_vec.len() == 32 {
				let mut channel_id = [0u8; 32];
				channel_id.copy_from_slice(&hex_vec[..]);
				return Ok(Self(channel_id));
			}
		}
		Err(Error::InvalidChannelId.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl Writeable for ChannelId {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		Ok(self.0.write(writer)?)
	}
}

impl Readable for ChannelId {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		Ok(Self(Readable::read(reader)?))
	}
}

/// A local, potentially user-provided, identifier of a channel.
///
/// By default, this will be randomly generated for the user to ensure local uniqueness.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct UserChannelId(pub u128);

impl UniffiCustomTypeConverter for UserChannelId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(UserChannelId(u128::from_str(&val).map_err(|_| Error::InvalidChannelId)?))
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.0.to_string()
	}
}

impl Writeable for UserChannelId {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		Ok(self.0.write(writer)?)
	}
}

impl Readable for UserChannelId {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		Ok(Self(Readable::read(reader)?))
	}
}

impl UniffiCustomTypeConverter for Network {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(Network::from_str(&val).map_err(|_| Error::InvalidNetwork)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Txid {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(Txid::from_str(&val)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

/// Details of a channel as returned by [`Node::list_channels`].
///
/// [`Node::list_channels`]: [`crate::Node::list_channels`]
pub struct ChannelDetails {
	/// The channel's ID (prior to funding transaction generation, this is a random 32 bytes,
	/// thereafter this is the transaction ID of the funding transaction XOR the funding transaction
	/// output).
	///
	/// Note that this means this value is *not* persistent - it can change once during the
	/// lifetime of the channel.
	pub channel_id: ChannelId,
	/// The `node_id` of our channel's counterparty.
	pub counterparty_node_id: PublicKey,
	/// The channel's funding transaction output, if we've negotiated the funding transaction with
	/// our counterparty already.
	pub funding_txo: Option<OutPoint>,
	/// The value, in satoshis, of this channel as appears in the funding output.
	pub channel_value_satoshis: u64,
	/// The value, in satoshis, that must always be held in the channel for us. This value ensures
	/// that if we broadcast a revoked state, our counterparty can punish us by claiming at least
	/// this value on chain.
	///
	/// This value is not included in [`outbound_capacity_msat`] as it can never be spent.
	///
	/// This value will be `None` for outbound channels until the counterparty accepts the channel.
	///
	/// [`outbound_capacity_msat`]: Self::outbound_capacity_msat
	pub unspendable_punishment_reserve: Option<u64>,
	/// The local `user_channel_id` of this channel.
	pub user_channel_id: UserChannelId,
	/// Total balance of the channel. This is the amount that will be returned to the user if the
	/// channel is closed.
	///
	/// The value is not exact, due to potential in-flight and fee-rate changes. Therefore, exactly
	/// this amount is likely irrecoverable on close.
	pub balance_msat: u64,
	/// Available outbound capacity for sending HTLCs to the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved (and, thus, whose
	/// balance is not available for inclusion in new outbound HTLCs). This further does not include
	/// any pending outgoing HTLCs which are awaiting some other resolution to be sent.
	pub outbound_capacity_msat: u64,
	/// Available outbound capacity for sending HTLCs to the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved
	/// (and, thus, whose balance is not available for inclusion in new inbound HTLCs). This further
	/// does not include any pending outgoing HTLCs which are awaiting some other resolution to be
	/// sent.
	pub inbound_capacity_msat: u64,
	/// The number of required confirmations on the funding transactions before the funding is
	/// considered "locked". The amount is selected by the channel fundee.
	///
	/// The value will be `None` for outbound channels until the counterparty accepts the channel.
	pub confirmations_required: Option<u32>,
	/// The current number of confirmations on the funding transaction.
	pub confirmations: Option<u32>,
	/// Returns `true` if the channel was initiated (and therefore funded) by us.
	pub is_outbound: bool,
	/// Returns `true` if the channel is confirmed, both parties have exchanged `channel_ready`
	/// messages, and the channel is not currently being shut down. Both parties exchange
	/// `channel_ready` messages upon independently verifying that the required confirmations count
	/// provided by `confirmations_required` has been reached.
	pub is_channel_ready: bool,
	/// Returns `true` if the channel is (a) confirmed and `channel_ready` has been exchanged,
	/// (b) the peer is connected, and (c) the channel is not currently negotiating shutdown.
	///
	/// This is a strict superset of `is_channel_ready`.
	pub is_usable: bool,
	/// Returns `true` if this channel is (or will be) publicly-announced
	pub is_public: bool,
	/// The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded over
	/// the channel.
	pub cltv_expiry_delta: Option<u16>,
}

impl From<LdkChannelDetails> for ChannelDetails {
	fn from(value: LdkChannelDetails) -> Self {
		ChannelDetails {
			channel_id: ChannelId(value.channel_id),
			counterparty_node_id: value.counterparty.node_id,
			funding_txo: value.funding_txo.and_then(|o| Some(o.into_bitcoin_outpoint())),
			channel_value_satoshis: value.channel_value_satoshis,
			unspendable_punishment_reserve: value.unspendable_punishment_reserve,
			user_channel_id: UserChannelId(value.user_channel_id),
			balance_msat: value.balance_msat,
			outbound_capacity_msat: value.outbound_capacity_msat,
			inbound_capacity_msat: value.inbound_capacity_msat,
			confirmations_required: value.confirmations_required,
			confirmations: value.confirmations,
			is_outbound: value.is_outbound,
			is_channel_ready: value.is_channel_ready,
			is_usable: value.is_usable,
			is_public: value.is_public,
			cltv_expiry_delta: value.config.and_then(|c| Some(c.cltv_expiry_delta)),
		}
	}
}

/// Details of a known Lightning peer as returned by [`Node::list_peers`].
///
/// [`Node::list_peers`]: [`crate::Node::list_peers`]
pub struct PeerDetails {
	/// Our peer's node ID.
	pub node_id: PublicKey,
	/// The IP address and TCP port of the peer.
	pub address: NetAddress,
	/// Indicates whether or not the user is currently has an active connection with the peer.
	pub is_connected: bool,
}

impl UniffiCustomTypeConverter for SocketAddr {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(SocketAddr::from_str(&val).map_err(|_| Error::InvalidNetAddress)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

/// The network address of a Lightning node.
///
/// Currently only IPv4, IPv6, and DNS hostnames are supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAddress(LdkNetAddress);

impl Display for NetAddress {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self.0 {
			LdkNetAddress::IPv4 { addr, port } => {
				let ip_addr = Ipv4Addr::from(addr);
				write!(f, "{}:{}", ip_addr, port)
			}
			LdkNetAddress::IPv6 { addr, port } => {
				let ip_addr = Ipv6Addr::from(addr);
				write!(f, "[{}]:{}", ip_addr, port)
			}
			LdkNetAddress::Hostname { ref hostname, port } => {
				write!(f, "{}:{}", hostname.as_str(), port)
			}
			LdkNetAddress::OnionV2(o) => {
				write!(f, "OnionV2 (unsupported): {:?}", o)
			}
			LdkNetAddress::OnionV3 { ed25519_pubkey, checksum, version, port } => write!(
				f,
				"OnionV3 (unsupported): {:?}/{:?}/{:?}/{:?}",
				ed25519_pubkey, checksum, version, port
			),
		}
	}
}

impl UniffiCustomTypeConverter for NetAddress {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(NetAddress::from_str(&val).map_err(|_| Error::InvalidNetAddress)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl FromStr for NetAddress {
	type Err = ();

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match std::net::SocketAddr::from_str(s) {
			Ok(addr) => {
				let port: u16 = addr.port();
				match addr {
					std::net::SocketAddr::V4(addr) => {
						let addr = addr.ip().octets();
						return Ok(Self(LdkNetAddress::IPv4 { addr, port }));
					}
					std::net::SocketAddr::V6(addr) => {
						let addr = addr.ip().octets();
						return Ok(Self(LdkNetAddress::IPv6 { addr, port }));
					}
				}
			}
			Err(_) => {
				let trimmed_input = match s.rfind(":") {
					Some(pos) => pos,
					None => return Err(()),
				};
				let host = &s[..trimmed_input];
				let port: u16 = match s[trimmed_input + 1..].parse() {
					Ok(port) => port,
					Err(_) => return Err(()),
				};

				Hostname::try_from(host.to_string())
					.map(|hostname| Self(LdkNetAddress::Hostname { hostname, port }))
					.map_err(|_| ())
			}
		}
	}
}

impl From<SocketAddr> for NetAddress {
	fn from(value: SocketAddr) -> Self {
		match value {
			SocketAddr::V4(v4addr) => NetAddress::from(v4addr),
			SocketAddr::V6(v6addr) => NetAddress::from(v6addr),
		}
	}
}

impl From<SocketAddrV4> for NetAddress {
	fn from(value: SocketAddrV4) -> Self {
		Self(LdkNetAddress::IPv4 { addr: value.ip().octets(), port: value.port() })
	}
}

impl From<SocketAddrV6> for NetAddress {
	fn from(value: SocketAddrV6) -> Self {
		Self(LdkNetAddress::IPv6 { addr: value.ip().octets(), port: value.port() })
	}
}

impl ToSocketAddrs for NetAddress {
	type Iter = std::option::IntoIter<SocketAddr>;

	fn to_socket_addrs(&self) -> std::io::Result<Self::Iter> {
		match self.0 {
			LdkNetAddress::IPv4 { addr, port } => {
				let ip_addr = Ipv4Addr::from(addr);
				(ip_addr, port).to_socket_addrs()
			}
			LdkNetAddress::IPv6 { addr, port } => {
				let ip_addr = Ipv6Addr::from(addr);
				(ip_addr, port).to_socket_addrs()
			}
			LdkNetAddress::Hostname { ref hostname, port } => {
				Ok((hostname.as_str(), port).to_socket_addrs()?.next().into_iter())
			}
			LdkNetAddress::OnionV2(..) => {
				Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
			}
			LdkNetAddress::OnionV3 { .. } => {
				Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
			}
		}
	}
}

impl Writeable for NetAddress {
	fn write<W: lightning::util::ser::Writer>(&self, writer: &mut W) -> Result<(), std::io::Error> {
		self.0.write(writer)
	}
}

impl Readable for NetAddress {
	fn read<R: std::io::Read>(reader: &mut R) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let addr: LdkNetAddress = Readable::read(reader)?;
		Ok(Self(addr))
	}
}
