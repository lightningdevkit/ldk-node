use crate::logger::FilesystemLogger;
use crate::sweep::OutputSweeper;

use lightning::blinded_path::BlindedPath;
use lightning::chain::chainmonitor;
use lightning::ln::channelmanager::ChannelDetails as LdkChannelDetails;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::msgs::SocketAddress;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::ln::ChannelId;
use lightning::routing::gossip;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::sign::{EntropySource, InMemorySigner};
use lightning::util::config::ChannelConfig as LdkChannelConfig;
use lightning::util::config::MaxDustHTLCExposure as LdkMaxDustHTLCExposure;
use lightning::util::ser::{Readable, Writeable, Writer};
use lightning_net_tokio::SocketDescriptor;
use lightning_transaction_sync::EsploraSyncClient;

use bitcoin::secp256k1::{self, PublicKey, Secp256k1};
use bitcoin::OutPoint;

use std::sync::{Arc, Mutex, RwLock};

pub(crate) type ChainMonitor<K> = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	Arc<Broadcaster>,
	Arc<FeeEstimator>,
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
	Arc<KeysManager>,
>;

pub(crate) type ChannelManager<K> = lightning::ln::channelmanager::ChannelManager<
	Arc<ChainMonitor<K>>,
	Arc<Broadcaster>,
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<FeeEstimator>,
	Arc<Router>,
	Arc<FilesystemLogger>,
>;

pub(crate) type Broadcaster = crate::tx_broadcaster::TransactionBroadcaster<Arc<FilesystemLogger>>;

pub(crate) type FeeEstimator = crate::fee_estimator::OnchainFeeEstimator<Arc<FilesystemLogger>>;

pub(crate) type Wallet = crate::wallet::Wallet<
	bdk::database::SqliteDatabase,
	Arc<Broadcaster>,
	Arc<FeeEstimator>,
	Arc<FilesystemLogger>,
>;

pub(crate) type KeysManager = crate::wallet::WalletKeysManager<
	bdk::database::SqliteDatabase,
	Arc<Broadcaster>,
	Arc<FeeEstimator>,
	Arc<FilesystemLogger>,
>;

pub(crate) type Router = DefaultRouter<
	Arc<NetworkGraph>,
	Arc<FilesystemLogger>,
	Arc<Mutex<Scorer>>,
	ProbabilisticScoringFeeParameters,
	Scorer,
>;
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

pub(crate) type OnionMessenger = lightning::onion_message::messenger::OnionMessenger<
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<FilesystemLogger>,
	Arc<FakeMessageRouter>,
	IgnoringMessageHandler,
	IgnoringMessageHandler,
>;

pub(crate) struct FakeMessageRouter {}

impl lightning::onion_message::messenger::MessageRouter for FakeMessageRouter {
	fn find_path(
		&self, _sender: PublicKey, _peers: Vec<PublicKey>,
		_destination: lightning::onion_message::messenger::Destination,
	) -> Result<lightning::onion_message::messenger::OnionMessagePath, ()> {
		unimplemented!()
	}
	fn create_blinded_paths<
		ES: EntropySource + ?Sized,
		T: secp256k1::Signing + secp256k1::Verification,
	>(
		&self, _recipient: PublicKey, _peers: Vec<PublicKey>, _entropy_source: &ES,
		_secp_ctx: &Secp256k1<T>,
	) -> Result<Vec<BlindedPath>, ()> {
		unreachable!()
	}
}

pub(crate) type Sweeper<K> = OutputSweeper<
	Arc<Broadcaster>,
	Arc<FeeEstimator>,
	Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	Arc<K>,
	Arc<FilesystemLogger>,
>;

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
#[derive(Debug, Clone)]
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
	/// The value, in satoshis, that must always be held in the channel for our counterparty. This
	/// value ensures that if our counterparty broadcasts a revoked state, we can punish them by
	/// claiming at least this value on chain.
	///
	/// This value is not included in [`inbound_capacity_msat`] as it can never be spent.
	///
	/// [`inbound_capacity_msat`]: ChannelDetails::inbound_capacity_msat
	pub counterparty_unspendable_punishment_reserve: u64,
	/// The smallest value HTLC (in msat) the remote peer will accept, for this channel.
	///
	/// This field is only `None` before we have received either the `OpenChannel` or
	/// `AcceptChannel` message from the remote peer.
	pub counterparty_outbound_htlc_minimum_msat: Option<u64>,
	/// The largest value HTLC (in msat) the remote peer currently will accept, for this channel.
	pub counterparty_outbound_htlc_maximum_msat: Option<u64>,
	/// Base routing fee in millisatoshis.
	pub counterparty_forwarding_info_fee_base_msat: Option<u32>,
	/// Proportional fee, in millionths of a satoshi the channel will charge per transferred satoshi.
	pub counterparty_forwarding_info_fee_proportional_millionths: Option<u32>,
	/// The minimum difference in CLTV expiry between an ingoing HTLC and its outgoing counterpart,
	/// such that the outgoing HTLC is forwardable to this counterparty.
	pub counterparty_forwarding_info_cltv_expiry_delta: Option<u16>,
	/// The available outbound capacity for sending a single HTLC to the remote peer. This is
	/// similar to [`ChannelDetails::outbound_capacity_msat`] but it may be further restricted by
	/// the current state and per-HTLC limit(s). This is intended for use when routing, allowing us
	/// to use a limit as close as possible to the HTLC limit we can currently send.
	///
	/// See also [`ChannelDetails::next_outbound_htlc_minimum_msat`],
	/// [`ChannelDetails::balance_msat`], and [`ChannelDetails::outbound_capacity_msat`].
	pub next_outbound_htlc_limit_msat: u64,
	/// The minimum value for sending a single HTLC to the remote peer. This is the equivalent of
	/// [`ChannelDetails::next_outbound_htlc_limit_msat`] but represents a lower-bound, rather than
	/// an upper-bound. This is intended for use when routing, allowing us to ensure we pick a
	/// route which is valid.
	pub next_outbound_htlc_minimum_msat: u64,
	/// The number of blocks (after our commitment transaction confirms) that we will need to wait
	/// until we can claim our funds after we force-close the channel. During this time our
	/// counterparty is allowed to punish us if we broadcasted a stale state. If our counterparty
	/// force-closes the channel and broadcasts a commitment transaction we do not have to wait any
	/// time to claim our non-HTLC-encumbered funds.
	///
	/// This value will be `None` for outbound channels until the counterparty accepts the channel.
	pub force_close_spend_delay: Option<u16>,
	/// The smallest value HTLC (in msat) we will accept, for this channel.
	pub inbound_htlc_minimum_msat: u64,
	/// The largest value HTLC (in msat) we currently will accept, for this channel.
	pub inbound_htlc_maximum_msat: Option<u64>,
	/// Set of configurable parameters that affect channel operation.
	pub config: Arc<ChannelConfig>,
}

impl From<LdkChannelDetails> for ChannelDetails {
	fn from(value: LdkChannelDetails) -> Self {
		ChannelDetails {
			channel_id: value.channel_id,
			counterparty_node_id: value.counterparty.node_id,
			funding_txo: value.funding_txo.and_then(|o| Some(o.into_bitcoin_outpoint())),
			channel_value_sats: value.channel_value_satoshis,
			unspendable_punishment_reserve: value.unspendable_punishment_reserve,
			user_channel_id: UserChannelId(value.user_channel_id),
			// unwrap safety: This value will be `None` for objects serialized with LDK versions
			// prior to 0.0.115.
			feerate_sat_per_1000_weight: value.feerate_sat_per_1000_weight.unwrap(),
			balance_msat: value.balance_msat,
			outbound_capacity_msat: value.outbound_capacity_msat,
			inbound_capacity_msat: value.inbound_capacity_msat,
			confirmations_required: value.confirmations_required,
			confirmations: value.confirmations,
			is_outbound: value.is_outbound,
			is_channel_ready: value.is_channel_ready,
			is_usable: value.is_usable,
			is_public: value.is_public,
			cltv_expiry_delta: value.config.map(|c| c.cltv_expiry_delta),
			counterparty_unspendable_punishment_reserve: value
				.counterparty
				.unspendable_punishment_reserve,
			counterparty_outbound_htlc_minimum_msat: value.counterparty.outbound_htlc_minimum_msat,
			counterparty_outbound_htlc_maximum_msat: value.counterparty.outbound_htlc_maximum_msat,
			counterparty_forwarding_info_fee_base_msat: value
				.counterparty
				.forwarding_info
				.as_ref()
				.map(|f| f.fee_base_msat),
			counterparty_forwarding_info_fee_proportional_millionths: value
				.counterparty
				.forwarding_info
				.as_ref()
				.map(|f| f.fee_proportional_millionths),
			counterparty_forwarding_info_cltv_expiry_delta: value
				.counterparty
				.forwarding_info
				.as_ref()
				.map(|f| f.cltv_expiry_delta),
			next_outbound_htlc_limit_msat: value.next_outbound_htlc_limit_msat,
			next_outbound_htlc_minimum_msat: value.next_outbound_htlc_minimum_msat,
			force_close_spend_delay: value.force_close_spend_delay,
			// unwrap safety: This field is only `None` for objects serialized prior to LDK 0.0.107
			inbound_htlc_minimum_msat: value.inbound_htlc_minimum_msat.unwrap_or(0),
			inbound_htlc_maximum_msat: value.inbound_htlc_maximum_msat,
			// unwrap safety: `config` is only `None` for LDK objects serialized prior to 0.0.109.
			config: value.config.map(|c| Arc::new(c.into())).unwrap(),
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
	pub address: SocketAddress,
	/// Indicates whether we'll try to reconnect to this peer after restarts.
	pub is_persisted: bool,
	/// Indicates whether we currently have an active connection with the peer.
	pub is_connected: bool,
}

/// Options which apply on a per-channel basis.
///
/// See documentation of [`LdkChannelConfig`] for details.
#[derive(Debug)]
pub struct ChannelConfig {
	inner: RwLock<LdkChannelConfig>,
}

impl Clone for ChannelConfig {
	fn clone(&self) -> Self {
		self.inner.read().unwrap().clone().into()
	}
}

impl ChannelConfig {
	/// Constructs a new `ChannelConfig`.
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns the set `forwarding_fee_proportional_millionths`.
	pub fn forwarding_fee_proportional_millionths(&self) -> u32 {
		self.inner.read().unwrap().forwarding_fee_proportional_millionths
	}

	/// Sets the `forwarding_fee_proportional_millionths`.
	pub fn set_forwarding_fee_proportional_millionths(&self, value: u32) {
		self.inner.write().unwrap().forwarding_fee_proportional_millionths = value;
	}

	/// Returns the set `forwarding_fee_base_msat`.
	pub fn forwarding_fee_base_msat(&self) -> u32 {
		self.inner.read().unwrap().forwarding_fee_base_msat
	}

	/// Sets the `forwarding_fee_base_msat`.
	pub fn set_forwarding_fee_base_msat(&self, fee_msat: u32) {
		self.inner.write().unwrap().forwarding_fee_base_msat = fee_msat;
	}

	/// Returns the set `cltv_expiry_delta`.
	pub fn cltv_expiry_delta(&self) -> u16 {
		self.inner.read().unwrap().cltv_expiry_delta
	}

	/// Sets the `cltv_expiry_delta`.
	pub fn set_cltv_expiry_delta(&self, value: u16) {
		self.inner.write().unwrap().cltv_expiry_delta = value;
	}

	/// Returns the set `force_close_avoidance_max_fee_satoshis`.
	pub fn force_close_avoidance_max_fee_satoshis(&self) -> u64 {
		self.inner.read().unwrap().force_close_avoidance_max_fee_satoshis
	}

	/// Sets the `force_close_avoidance_max_fee_satoshis`.
	pub fn set_force_close_avoidance_max_fee_satoshis(&self, value_sat: u64) {
		self.inner.write().unwrap().force_close_avoidance_max_fee_satoshis = value_sat;
	}

	/// Returns the set `accept_underpaying_htlcs`.
	pub fn accept_underpaying_htlcs(&self) -> bool {
		self.inner.read().unwrap().accept_underpaying_htlcs
	}

	/// Sets the `accept_underpaying_htlcs`.
	pub fn set_accept_underpaying_htlcs(&self, value: bool) {
		self.inner.write().unwrap().accept_underpaying_htlcs = value;
	}

	/// Sets the `max_dust_htlc_exposure` from a fixed limit.
	pub fn set_max_dust_htlc_exposure_from_fixed_limit(&self, limit_msat: u64) {
		self.inner.write().unwrap().max_dust_htlc_exposure =
			LdkMaxDustHTLCExposure::FixedLimitMsat(limit_msat);
	}

	/// Sets the `max_dust_htlc_exposure` from a fee rate multiplier.
	pub fn set_max_dust_htlc_exposure_from_fee_rate_multiplier(&self, multiplier: u64) {
		self.inner.write().unwrap().max_dust_htlc_exposure =
			LdkMaxDustHTLCExposure::FeeRateMultiplier(multiplier);
	}
}

impl From<LdkChannelConfig> for ChannelConfig {
	fn from(value: LdkChannelConfig) -> Self {
		Self { inner: RwLock::new(value) }
	}
}

impl From<ChannelConfig> for LdkChannelConfig {
	fn from(value: ChannelConfig) -> Self {
		*value.inner.read().unwrap()
	}
}

impl Default for ChannelConfig {
	fn default() -> Self {
		LdkChannelConfig::default().into()
	}
}
