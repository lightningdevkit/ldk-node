// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::chain::ChainSource;
use crate::config::ChannelConfig;
use crate::data_store::DataStore;
use crate::fee_estimator::OnchainFeeEstimator;
use crate::gossip::RuntimeSpawner;
use crate::logger::Logger;
use crate::message_handler::NodeCustomMessageHandler;
use crate::payment::PaymentDetails;

use lightning::chain::chainmonitor;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channel_state::ChannelDetails as LdkChannelDetails;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::msgs::SocketAddress;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::ln::types::ChannelId;
use lightning::routing::gossip;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::sign::InMemorySigner;
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable, Writer};
use lightning::util::sweep::OutputSweeper;

use lightning_block_sync::gossip::{GossipVerifier, UtxoSource};

use lightning_net_tokio::SocketDescriptor;

use bitcoin::secp256k1::PublicKey;
use bitcoin::OutPoint;

use std::sync::{Arc, Mutex};

pub(crate) type DynStore = dyn KVStore + Sync + Send;

pub(crate) type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<ChainSource>,
	Arc<Broadcaster>,
	Arc<OnchainFeeEstimator>,
	Arc<Logger>,
	Arc<DynStore>,
>;

pub(crate) type PeerManager = lightning::ln::peer_handler::PeerManager<
	SocketDescriptor,
	Arc<ChannelManager>,
	Arc<dyn RoutingMessageHandler + Send + Sync>,
	Arc<OnionMessenger>,
	Arc<Logger>,
	Arc<NodeCustomMessageHandler<Arc<Logger>>>,
	Arc<KeysManager>,
>;

pub(crate) type LiquidityManager =
	lightning_liquidity::LiquidityManager<Arc<KeysManager>, Arc<ChannelManager>, Arc<ChainSource>>;

pub(crate) type ChannelManager = lightning::ln::channelmanager::ChannelManager<
	Arc<ChainMonitor>,
	Arc<Broadcaster>,
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<OnchainFeeEstimator>,
	Arc<Router>,
	Arc<MessageRouter>,
	Arc<Logger>,
>;

pub(crate) type Broadcaster = crate::tx_broadcaster::TransactionBroadcaster<Arc<Logger>>;

pub(crate) type Wallet =
	crate::wallet::Wallet<Arc<Broadcaster>, Arc<OnchainFeeEstimator>, Arc<Logger>>;

pub(crate) type KeysManager =
	crate::wallet::WalletKeysManager<Arc<Broadcaster>, Arc<OnchainFeeEstimator>, Arc<Logger>>;

pub(crate) type Router = DefaultRouter<
	Arc<Graph>,
	Arc<Logger>,
	Arc<KeysManager>,
	Arc<Mutex<Scorer>>,
	ProbabilisticScoringFeeParameters,
	Scorer,
>;
pub(crate) type Scorer = ProbabilisticScorer<Arc<Graph>, Arc<Logger>>;

pub(crate) type Graph = gossip::NetworkGraph<Arc<Logger>>;

pub(crate) type UtxoLookup = GossipVerifier<RuntimeSpawner, Arc<dyn UtxoSource>, Arc<Logger>>;

pub(crate) type P2PGossipSync =
	lightning::routing::gossip::P2PGossipSync<Arc<Graph>, Arc<UtxoLookup>, Arc<Logger>>;
pub(crate) type RapidGossipSync =
	lightning_rapid_gossip_sync::RapidGossipSync<Arc<Graph>, Arc<Logger>>;

pub(crate) type GossipSync = lightning_background_processor::GossipSync<
	Arc<P2PGossipSync>,
	Arc<RapidGossipSync>,
	Arc<Graph>,
	Arc<UtxoLookup>,
	Arc<Logger>,
>;

pub(crate) type OnionMessenger = lightning::onion_message::messenger::OnionMessenger<
	Arc<KeysManager>,
	Arc<KeysManager>,
	Arc<Logger>,
	Arc<ChannelManager>,
	Arc<MessageRouter>,
	Arc<ChannelManager>,
	IgnoringMessageHandler,
	IgnoringMessageHandler,
	IgnoringMessageHandler,
>;

pub(crate) type MessageRouter = lightning::onion_message::messenger::DefaultMessageRouter<
	Arc<Graph>,
	Arc<Logger>,
	Arc<KeysManager>,
>;

pub(crate) type Sweeper = OutputSweeper<
	Arc<Broadcaster>,
	Arc<KeysManager>,
	Arc<OnchainFeeEstimator>,
	Arc<ChainSource>,
	Arc<DynStore>,
	Arc<Logger>,
	Arc<KeysManager>,
>;

pub(crate) type BumpTransactionEventHandler =
	lightning::events::bump_transaction::BumpTransactionEventHandler<
		Arc<Broadcaster>,
		Arc<lightning::events::bump_transaction::Wallet<Arc<Wallet>, Arc<Logger>>>,
		Arc<KeysManager>,
		Arc<Logger>,
	>;

pub(crate) type PaymentStore = DataStore<PaymentDetails, Arc<Logger>>;

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
	/// The position of the funding transaction in the chain. None if the funding transaction has
	/// not yet been confirmed and the channel fully opened.
	///
	/// Note that if [`inbound_scid_alias`] is set, it will be used for invoices and inbound
	/// payments instead of this.
	///
	/// For channels with [`confirmations_required`] set to `Some(0)`, [`outbound_scid_alias`] may
	/// be used in place of this in outbound routes.
	///
	/// [`inbound_scid_alias`]: Self::inbound_scid_alias
	/// [`outbound_scid_alias`]: Self::outbound_scid_alias
	/// [`confirmations_required`]: Self::confirmations_required
	pub short_channel_id: Option<u64>,
	/// An optional [`short_channel_id`] alias for this channel, randomly generated by us and
	/// usable in place of [`short_channel_id`] to reference the channel in outbound routes when
	/// the channel has not yet been confirmed (as long as [`confirmations_required`] is
	/// `Some(0)`).
	///
	/// This will be `None` as long as the channel is not available for routing outbound payments.
	///
	/// [`short_channel_id`]: Self::short_channel_id
	/// [`confirmations_required`]: Self::confirmations_required
	pub outbound_scid_alias: Option<u64>,
	/// An optional [`short_channel_id`] alias for this channel, randomly generated by our
	/// counterparty and usable in place of [`short_channel_id`] in invoice route hints. Our
	/// counterparty will recognize the alias provided here in place of the [`short_channel_id`]
	/// when they see a payment to be routed to us.
	///
	/// Our counterparty may choose to rotate this value at any time, though will always recognize
	/// previous values for inbound payment forwarding.
	///
	/// [`short_channel_id`]: Self::short_channel_id
	pub inbound_scid_alias: Option<u64>,
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
			short_channel_id: value.short_channel_id,
			outbound_scid_alias: value.outbound_scid_alias,
			inbound_scid_alias: value.inbound_scid_alias,
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

/// Custom TLV entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomTlvRecord {
	/// Type number.
	pub type_num: u64,
	/// Serialized value.
	pub value: Vec<u8>,
}

impl_writeable_tlv_based!(CustomTlvRecord, {
	(0, type_num, required),
	(2, value, required),
});

impl From<&(u64, Vec<u8>)> for CustomTlvRecord {
	fn from(tlv: &(u64, Vec<u8>)) -> Self {
		CustomTlvRecord { type_num: tlv.0, value: tlv.1.clone() }
	}
}
