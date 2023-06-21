use crate::logger::FilesystemLogger;
use crate::wallet::{Wallet, WalletKeysManager};

use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::InMemorySigner;
use lightning::ln::channelmanager::ChannelDetails as LdkChannelDetails;
use lightning::ln::msgs::NetAddress as LdkNetAddress;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::routing::gossip;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::util::ser::{Hostname, Readable, Writeable, Writer};
use lightning_net_tokio::SocketDescriptor;
use lightning_transaction_sync::EsploraSyncClient;

use bitcoin::secp256k1::PublicKey;
use bitcoin::OutPoint;

use std::convert::TryFrom;
use std::fmt::Display;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

pub(crate) type ChainMonitor<K> = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	Arc<Wallet<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<Wallet<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<FilesystemLogger>,
	Arc<K>,
>;

pub(crate) type PeerManager<K> = lightning::ln::peer_handler::PeerManager<
	SocketDescriptor,
	Arc<ChannelManager<K>>,
	Arc<dyn RoutingMessageHandler + Send + Sync>,
	Arc<OnionMessenger>,
	Arc<FilesystemLogger>,
	IgnoringMessageHandler,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
>;

pub(crate) type ChannelManager<K> = lightning::ln::channelmanager::ChannelManager<
	Arc<ChainMonitor<K>>,
	Arc<Wallet<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<Wallet<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<Router>,
	Arc<FilesystemLogger>,
>;

pub(crate) type KeysManager =
	WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>;

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
	Arc<WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<WalletKeysManager<bdk::database::SqliteDatabase, Arc<FilesystemLogger>>>,
	Arc<FilesystemLogger>,
	IgnoringMessageHandler,
>;

/// The global identifier of a channel.
///
/// Note that this will start out to be a temporary ID until channel funding negotiation is
/// finalized, at which point it will change to be a permanent global ID tied to the on-chain
/// funding transaction.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ChannelId(pub [u8; 32]);

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

/// Details of a channel as returned by [`Node::list_channels`].
///
/// [`Node::list_channels`]: crate::Node::list_channels
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelDetails {
	/// The channel ID (prior to funding transaction generation, this is a random 32-byte
	/// identifier, afterwards this is the transaction ID of the funding transaction XOR the
	/// funding transaction output).
	///
	/// Note that this means this value is *not* persistent - it can change once during the
	/// lifetime of the channel.
	pub channel_id: ChannelId,
	/// The node ID of our the channel's counterparty.
	pub counterparty_node_id: PublicKey,
	/// The channel's funding transaction output, if we've negotiated the funding transaction with
	/// our counterparty already.
	pub funding_txo: Option<OutPoint>,
	/// The value, in satoshis, of this channel as it appears in the funding output.
	pub channel_value_sats: u64,
	/// The value, in satoshis, that must always be held as a reserve in the channel for us. This
	/// value ensures that if we broadcast a revoked state, our counterparty can punish us by
	/// claiming at least this value on chain.
	///
	/// This value is not included in [`outbound_capacity_msat`] as it can never be spent.
	///
	/// This value will be `None` for outbound channels until the counterparty accepts the channel.
	///
	/// [`outbound_capacity_msat`]: Self::outbound_capacity_msat
	pub unspendable_punishment_reserve: Option<u64>,
	/// The local `user_channel_id` of this channel.
	pub user_channel_id: UserChannelId,
	/// The currently negotiated fee rate denominated in satoshi per 1000 weight units,
	/// which is applied to commitment and HTLC transactions.
	pub feerate_sat_per_1000_weight: u32,
	/// The total balance of the channel. This is the amount that will be returned to
	/// the user if the channel is closed.
	///
	/// The value is not exact, due to potential in-flight and fee-rate changes. Therefore, exactly
	/// this amount is likely irrecoverable on close.
	pub balance_msat: u64,
	/// The available outbound capacity for sending HTLCs to the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved (and, thus, whose
	/// balance is not available for inclusion in new outbound HTLCs). This further does not include
	/// any pending outgoing HTLCs which are awaiting some other resolution to be sent.
	pub outbound_capacity_msat: u64,
	/// The available outbound capacity for sending HTLCs to the remote peer.
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
	/// Returns `true` if both parties have exchanged `channel_ready` messages, and the channel is
	/// not currently being shut down. Both parties exchange `channel_ready` messages upon
	/// independently verifying that the required confirmations count provided by
	/// `confirmations_required` has been reached.
	pub is_channel_ready: bool,
	/// Returns `true` if the channel (a) `channel_ready` messages have been exchanged, (b) the
	/// peer is connected, and (c) the channel is not currently negotiating shutdown.
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
			channel_value_sats: value.channel_value_satoshis,
			unspendable_punishment_reserve: value.unspendable_punishment_reserve,
			user_channel_id: UserChannelId(value.user_channel_id),
			balance_msat: value.balance_msat,
			feerate_sat_per_1000_weight: value.feerate_sat_per_1000_weight.unwrap(),
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
/// [`Node::list_peers`]: crate::Node::list_peers
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDetails {
	/// The node ID of the peer.
	pub node_id: PublicKey,
	/// The network address of the peer.
	pub address: NetAddress,
	/// Indicates whether we'll try to reconnect to this peer after restarts.
	pub is_persisted: bool,
	/// Indicates whether we currently have an active connection with the peer.
	pub is_connected: bool,
}

/// The network address of a Lightning node.
///
/// Currently only IPv4, IPv6, and DNS hostnames are supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAddress(pub LdkNetAddress);

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
