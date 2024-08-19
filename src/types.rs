// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::logger::FilesystemLogger;
use crate::message_handler::NodeCustomMessageHandler;

use lightning::chain::chainmonitor;
use lightning::ln::channel_state::ChannelDetails as LdkChannelDetails;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::msgs::SocketAddress;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::ln::types::ChannelId;
use lightning::routing::gossip;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::sign::InMemorySigner;
use lightning::util::config::ChannelConfig as LdkChannelConfig;
use lightning::util::config::MaxDustHTLCExposure as LdkMaxDustHTLCExposure;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable, Writer};
use lightning::util::sweep::OutputSweeper;
use lightning_net_tokio::SocketDescriptor;
use lightning_transaction_sync::EsploraSyncClient;

use bitcoin::secp256k1::PublicKey;
use bitcoin::OutPoint;

use std::sync::{Arc, Mutex};

pub(crate) type DynStore = dyn KVStore + Sync + Send;

pub(crate) type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<ChainSource>,
	Arc<Broadcaster>,
	Arc<FeeEstimator>,
	Arc<FilesystemLogger>,
	Arc<DynStore>,
>;

pub(crate) type PeerManager = lightning::ln::peer_handler::PeerManager<
	SocketDescriptor,
	Arc<ChannelManager>,
	Arc<dyn RoutingMessageHandler + Send + Sync>,
	Arc<OnionMessenger>,
	Arc<FilesystemLogger>,
	Arc<NodeCustomMessageHandler<Arc<FilesystemLogger>>>,
	Arc<KeysManager>,
>;

pub(crate) type ChainSource = EsploraSyncClient<Arc<FilesystemLogger>>;

pub(crate) type LiquidityManager =
	lightning_liquidity::LiquidityManager<Arc<KeysManager>, Arc<ChannelManager>, Arc<ChainSource>>;

pub(crate) type ChannelManager = lightning::ln::channelmanager::ChannelManager<
	Arc<ChainMonitor>,
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

pub(crate) type Wallet =
	crate::wallet::Wallet<Arc<Broadcaster>, Arc<FeeEstimator>, Arc<FilesystemLogger>>;

pub(crate) type KeysManager =
	crate::wallet::WalletKeysManager<Arc<Broadcaster>, Arc<FeeEstimator>, Arc<FilesystemLogger>>;

pub(crate) type Router = DefaultRouter<
	Arc<Graph>,
	Arc<FilesystemLogger>,
	Arc<KeysManager>,
	Arc<Mutex<Scorer>>,
	ProbabilisticScoringFeeParameters,
	Scorer,
>;
pub(crate) type Scorer = ProbabilisticScorer<Arc<Graph>, Arc<FilesystemLogger>>;

pub(crate) type Graph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

pub(crate) type UtxoLookup = dyn lightning::routing::utxo::UtxoLookup + Send + Sync;

pub(crate) type P2PGossipSync =
	lightning::routing::gossip::P2PGossipSync<Arc<Graph>, Arc<UtxoLookup>, Arc<FilesystemLogger>>;
pub(crate) type RapidGossipSync =
	lightning_rapid_gossip_sync::RapidGossipSync<Arc<Graph>, Arc<FilesystemLogger>>;

pub(crate) type GossipSync = lightning_background_processor::GossipSync<
	Arc<P2PGossipSync>,
	Arc<RapidGossipSync>,
	Arc<Graph>,
	Arc<UtxoLookup>,
	Arc<FilesystemLogger>,
>;

pub(crate) type OnionMessenger = lightning::onion_message::messenger::OnionMessenger<
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<FilesystemLogger>,
	Arc<ChannelManager>,
	Arc<MessageRouter>,
	Arc<ChannelManager>,
	IgnoringMessageHandler,
	IgnoringMessageHandler,
>;

pub(crate) type MessageRouter = lightning::onion_message::messenger::DefaultMessageRouter<
	Arc<Graph>,
	Arc<FilesystemLogger>,
	Arc<KeysManager>,
>;

pub(crate) type Sweeper = OutputSweeper<
	Arc<Broadcaster>,
	Arc<KeysManager>,
	Arc<FeeEstimator>,
	Arc<ChainSource>,
	Arc<DynStore>,
	Arc<FilesystemLogger>,
	Arc<KeysManager>,
>;

pub(crate) type BumpTransactionEventHandler =
	lightning::events::bump_transaction::BumpTransactionEventHandler<
		Arc<Broadcaster>,
		Arc<lightning::events::bump_transaction::Wallet<Arc<Wallet>, Arc<FilesystemLogger>>>,
		Arc<KeysManager>,
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
	/// The available outbound capacity for sending HTLCs to the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved (and, thus, whose
	/// balance is not available for inclusion in new outbound HTLCs). This further does not include
	/// any pending outgoing HTLCs which are awaiting some other resolution to be sent.
	pub outbound_capacity_msat: u64,
	/// The available inbound capacity for receiving HTLCs from the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved
	/// (and, thus, whose balance is not available for inclusion in new inbound HTLCs). This further
	/// does not include any pending incoming HTLCs which are awaiting some other resolution to be
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
	pub is_announced: bool,
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
	/// See also [`ChannelDetails::next_outbound_htlc_minimum_msat`] and
	/// [`ChannelDetails::outbound_capacity_msat`].
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
	pub config: ChannelConfig,
}

impl From<LdkChannelDetails> for ChannelDetails {
	fn from(value: LdkChannelDetails) -> Self {
		ChannelDetails {
			channel_id: value.channel_id,
			counterparty_node_id: value.counterparty.node_id,
			funding_txo: value.funding_txo.map(|o| o.into_bitcoin_outpoint()),
			channel_value_sats: value.channel_value_satoshis,
			unspendable_punishment_reserve: value.unspendable_punishment_reserve,
			user_channel_id: UserChannelId(value.user_channel_id),
			// unwrap safety: This value will be `None` for objects serialized with LDK versions
			// prior to 0.0.115.
			feerate_sat_per_1000_weight: value.feerate_sat_per_1000_weight.unwrap(),
			outbound_capacity_msat: value.outbound_capacity_msat,
			inbound_capacity_msat: value.inbound_capacity_msat,
			confirmations_required: value.confirmations_required,
			confirmations: value.confirmations,
			is_outbound: value.is_outbound,
			is_channel_ready: value.is_channel_ready,
			is_usable: value.is_usable,
			is_announced: value.is_announced,
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
			config: value.config.map(|c| c.into()).unwrap(),
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

/// Options which apply on a per-channel basis and may change at runtime or based on negotiation
/// with our counterparty.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
	/// Amount (in millionths of a satoshi) charged per satoshi for payments forwarded outbound
	/// over the channel.
	/// This may be allowed to change at runtime in a later update, however doing so must result in
	/// update messages sent to notify all nodes of our updated relay fee.
	///
	/// Please refer to [`LdkChannelConfig`] for further details.
	pub forwarding_fee_proportional_millionths: u32,
	/// Amount (in milli-satoshi) charged for payments forwarded outbound over the channel, in
	/// excess of [`ChannelConfig::forwarding_fee_proportional_millionths`].
	/// This may be allowed to change at runtime in a later update, however doing so must result in
	/// update messages sent to notify all nodes of our updated relay fee.
	///
	/// Please refer to [`LdkChannelConfig`] for further details.
	pub forwarding_fee_base_msat: u32,
	/// The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded over
	/// the channel this config applies to.
	///
	/// Please refer to [`LdkChannelConfig`] for further details.
	pub cltv_expiry_delta: u16,
	/// Limit our total exposure to potential loss to on-chain fees on close, including in-flight
	/// HTLCs which are burned to fees as they are too small to claim on-chain and fees on
	/// commitment transaction(s) broadcasted by our counterparty in excess of our own fee estimate.
	///
	/// Please refer to [`LdkChannelConfig`] for further details.
	pub max_dust_htlc_exposure: MaxDustHTLCExposure,
	/// The additional fee we're willing to pay to avoid waiting for the counterparty's
	/// `to_self_delay` to reclaim funds.
	///
	/// Please refer to [`LdkChannelConfig`] for further details.
	pub force_close_avoidance_max_fee_satoshis: u64,
	/// If set, allows this channel's counterparty to skim an additional fee off this node's inbound
	/// HTLCs. Useful for liquidity providers to offload on-chain channel costs to end users.
	///
	/// Please refer to [`LdkChannelConfig`] for further details.
	pub accept_underpaying_htlcs: bool,
}

impl From<LdkChannelConfig> for ChannelConfig {
	fn from(value: LdkChannelConfig) -> Self {
		Self {
			forwarding_fee_proportional_millionths: value.forwarding_fee_proportional_millionths,
			forwarding_fee_base_msat: value.forwarding_fee_base_msat,
			cltv_expiry_delta: value.cltv_expiry_delta,
			max_dust_htlc_exposure: value.max_dust_htlc_exposure.into(),
			force_close_avoidance_max_fee_satoshis: value.force_close_avoidance_max_fee_satoshis,
			accept_underpaying_htlcs: value.accept_underpaying_htlcs,
		}
	}
}

impl From<ChannelConfig> for LdkChannelConfig {
	fn from(value: ChannelConfig) -> Self {
		Self {
			forwarding_fee_proportional_millionths: value.forwarding_fee_proportional_millionths,
			forwarding_fee_base_msat: value.forwarding_fee_base_msat,
			cltv_expiry_delta: value.cltv_expiry_delta,
			max_dust_htlc_exposure: value.max_dust_htlc_exposure.into(),
			force_close_avoidance_max_fee_satoshis: value.force_close_avoidance_max_fee_satoshis,
			accept_underpaying_htlcs: value.accept_underpaying_htlcs,
		}
	}
}

impl Default for ChannelConfig {
	fn default() -> Self {
		LdkChannelConfig::default().into()
	}
}

/// Options for how to set the max dust exposure allowed on a channel.
///
/// See [`LdkChannelConfig::max_dust_htlc_exposure`] for details.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MaxDustHTLCExposure {
	/// This sets a fixed limit on the total dust exposure in millisatoshis.
	///
	/// Please refer to [`LdkMaxDustHTLCExposure`] for further details.
	FixedLimit {
		/// The fixed limit, in millisatoshis.
		limit_msat: u64,
	},
	/// This sets a multiplier on the feerate to determine the maximum allowed dust exposure.
	///
	/// Please refer to [`LdkMaxDustHTLCExposure`] for further details.
	FeeRateMultiplier {
		/// The applied fee rate multiplier.
		multiplier: u64,
	},
}

impl From<LdkMaxDustHTLCExposure> for MaxDustHTLCExposure {
	fn from(value: LdkMaxDustHTLCExposure) -> Self {
		match value {
			LdkMaxDustHTLCExposure::FixedLimitMsat(limit_msat) => Self::FixedLimit { limit_msat },
			LdkMaxDustHTLCExposure::FeeRateMultiplier(multiplier) => {
				Self::FeeRateMultiplier { multiplier }
			},
		}
	}
}

impl From<MaxDustHTLCExposure> for LdkMaxDustHTLCExposure {
	fn from(value: MaxDustHTLCExposure) -> Self {
		match value {
			MaxDustHTLCExposure::FixedLimit { limit_msat } => Self::FixedLimitMsat(limit_msat),
			MaxDustHTLCExposure::FeeRateMultiplier { multiplier } => {
				Self::FeeRateMultiplier(multiplier)
			},
		}
	}
}
