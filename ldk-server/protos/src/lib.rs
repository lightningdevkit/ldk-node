/// Retrieve a new on-chain funding address.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.OnchainPayment.html#method.new_address>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OnchainReceiveRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OnchainReceiveResponse {
	/// A Bitcoin on-chain address.
	#[prost(string, tag = "1")]
	pub address: ::prost::alloc::string::String,
}
/// Send an on-chain payment to the given address.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OnchainSendRequest {
	/// The address to send coins to.
	#[prost(string, tag = "1")]
	pub address: ::prost::alloc::string::String,
	/// The amount in satoshis to send.
	/// While sending the specified amount, we will respect any on-chain reserve we need to keep,
	/// i.e., won't allow to cut into `total_anchor_channels_reserve_sats`.
	/// See more: <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.OnchainPayment.html#method.send_to_address>
	#[prost(uint64, optional, tag = "2")]
	pub amount_sats: ::core::option::Option<u64>,
	/// If set, the amount_sats field should be unset.
	/// It indicates that node will send full balance to the specified address.
	///
	/// Please note that when send_all is used this operation will **not** retain any on-chain reserves,
	/// which might be potentially dangerous if you have open Anchor channels for which you can't trust
	/// the counterparty to spend the Anchor output after channel closure.
	/// See more: <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.OnchainPayment.html#method.send_all_to_address>
	#[prost(bool, optional, tag = "3")]
	pub send_all: ::core::option::Option<bool>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OnchainSendResponse {
	/// The transaction ID of the broadcasted transaction.
	#[prost(string, tag = "1")]
	pub txid: ::prost::alloc::string::String,
}
/// Return a BOLT11 payable invoice that can be used to request and receive a payment
/// for the given amount, if specified.
/// The inbound payment will be automatically claimed upon arrival.
/// See more:
/// - <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt11Payment.html#method.receive>
/// - <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt11Payment.html#method.receive_variable_amount>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11ReceiveRequest {
	/// The amount in millisatoshi to send. If unset, a "zero-amount" or variable-amount invoice is returned.
	#[prost(uint64, optional, tag = "1")]
	pub amount_msat: ::core::option::Option<u64>,
	/// An optional description to attach along with the invoice.
	/// Will be set in the description field of the encoded payment request.
	#[prost(string, tag = "2")]
	pub description: ::prost::alloc::string::String,
	/// Invoice expiry time in seconds.
	#[prost(uint32, tag = "3")]
	pub expiry_secs: u32,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11ReceiveResponse {
	/// An invoice for a payment within the Lightning Network.
	/// With the details of the invoice, the sender has all the data necessary to send a payment
	/// to the recipient.
	#[prost(string, tag = "1")]
	pub invoice: ::prost::alloc::string::String,
}
/// Send a payment for a BOLT11 invoice.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt11Payment.html#method.send>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11SendRequest {
	/// An invoice for a payment within the Lightning Network.
	#[prost(string, tag = "1")]
	pub invoice: ::prost::alloc::string::String,
	/// Set this field when paying a so-called "zero-amount" invoice, i.e., an invoice that leaves the
	/// amount paid to be determined by the user.
	/// This operation will fail if the amount specified is less than the value required by the given invoice.
	#[prost(uint64, optional, tag = "2")]
	pub amount_msat: ::core::option::Option<u64>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11SendResponse {
	/// An identifier used to uniquely identify a payment.
	#[prost(message, optional, tag = "1")]
	pub payment_id: ::core::option::Option<PaymentId>,
}
/// An identifier used to uniquely identify a payment.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaymentId {
	#[prost(bytes = "bytes", tag = "1")]
	pub data: ::prost::bytes::Bytes,
}
/// Returns a BOLT12 offer for the given amount, if specified.
///
/// See more:
/// - <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt12Payment.html#method.receive>
/// - <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt12Payment.html#method.receive_variable_amount>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12ReceiveRequest {
	/// An optional description to attach along with the offer.
	/// Will be set in the description field of the encoded offer.
	#[prost(string, tag = "1")]
	pub description: ::prost::alloc::string::String,
	/// The amount in millisatoshi to send. If unset, a "zero-amount" or variable-amount offer is returned.
	#[prost(uint64, optional, tag = "2")]
	pub amount_msat: ::core::option::Option<u64>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12ReceiveResponse {
	/// An offer for a payment within the Lightning Network.
	/// With the details of the offer, the sender has all the data necessary to send a payment
	/// to the recipient.
	#[prost(string, tag = "1")]
	pub offer: ::prost::alloc::string::String,
}
/// Send a payment for a BOLT12 offer.
/// See more:
/// - <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt12Payment.html#method.send>
/// - <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.Bolt12Payment.html#method.send_using_amount>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12SendRequest {
	/// An offer for a payment within the Lightning Network.
	#[prost(string, tag = "1")]
	pub offer: ::prost::alloc::string::String,
	/// Set this field when paying a so-called "zero-amount" offer, i.e., an offer that leaves the
	/// amount paid to be determined by the user.
	/// This operation will fail if the amount specified is less than the value required by the given offer.
	#[prost(uint64, optional, tag = "2")]
	pub amount_msat: ::core::option::Option<u64>,
	/// If set, it represents the number of items requested.
	#[prost(uint64, optional, tag = "3")]
	pub quantity: ::core::option::Option<u64>,
	/// If set, it will be seen by the recipient and reflected back in the invoice.
	#[prost(string, optional, tag = "4")]
	pub payer_note: ::core::option::Option<::prost::alloc::string::String>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12SendResponse {
	/// An identifier used to uniquely identify a payment.
	#[prost(message, optional, tag = "1")]
	pub payment_id: ::core::option::Option<PaymentId>,
}
/// Creates a new outbound channel to the given remote node.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.connect_open_channel>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OpenChannelRequest {
	/// The hex-encoded public key of the node to open a channel with.
	#[prost(string, tag = "1")]
	pub node_pubkey: ::prost::alloc::string::String,
	/// An address which can be used to connect to a remote peer.
	/// It can be of type IPv4:port, IPv6:port, OnionV3:port or hostname:port
	#[prost(string, tag = "2")]
	pub address: ::prost::alloc::string::String,
	/// The amount of satoshis the caller is willing to commit to the channel.
	#[prost(uint64, tag = "3")]
	pub channel_amount_sats: u64,
	/// The amount of satoshis to push to the remote side as part of the initial commitment state.
	#[prost(uint64, optional, tag = "4")]
	pub push_to_counterparty_msat: ::core::option::Option<u64>,
	/// The channel configuration to be used for opening this channel. If unset, default ChannelConfig is used.
	#[prost(message, optional, tag = "5")]
	pub channel_config: ::core::option::Option<ChannelConfig>,
	/// Whether the channel should be public.
	#[prost(bool, tag = "6")]
	pub announce_channel: bool,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OpenChannelResponse {
	/// The channel id of the created channel that user can use to refer to channel.
	#[prost(bytes = "bytes", tag = "1")]
	pub user_channel_id: ::prost::bytes::Bytes,
}
/// ChannelConfig represents the configuration settings for a channel in a Lightning Network node.
/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ChannelConfig {
	/// Amount (in millionths of a satoshi) charged per satoshi for payments forwarded outbound
	/// over the channel.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.forwarding_fee_proportional_millionths>
	#[prost(uint32, tag = "1")]
	pub forwarding_fee_proportional_millionths: u32,
	/// Amount (in milli-satoshi) charged for payments forwarded outbound over the channel,
	/// in excess of forwarding_fee_proportional_millionths.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.forwarding_fee_base_msat>
	#[prost(uint32, tag = "2")]
	pub forwarding_fee_base_msat: u32,
	/// The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded
	/// over the channel this config applies to.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.cltv_expiry_delta>
	#[prost(uint32, tag = "3")]
	pub cltv_expiry_delta: u32,
	/// The maximum additional fee we’re willing to pay to avoid waiting for the counterparty’s
	/// to_self_delay to reclaim funds.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.force_close_avoidance_max_fee_satoshis>
	#[prost(uint64, tag = "4")]
	pub force_close_avoidance_max_fee_satoshis: u64,
	/// If set, allows this channel’s counterparty to skim an additional fee off this node’s
	/// inbound HTLCs. Useful for liquidity providers to offload on-chain channel costs to end users.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.accept_underpaying_htlcs>
	#[prost(bool, tag = "5")]
	pub accept_underpaying_htlcs: bool,
	/// Limit our total exposure to potential loss to on-chain fees on close, including
	/// in-flight HTLCs which are burned to fees as they are too small to claim on-chain
	/// and fees on commitment transaction(s) broadcasted by our counterparty in excess of
	/// our own fee estimate.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.max_dust_htlc_exposure>
	#[prost(oneof = "channel_config::MaxDustHtlcExposure", tags = "6, 7")]
	pub max_dust_htlc_exposure: ::core::option::Option<channel_config::MaxDustHtlcExposure>,
}
/// Nested message and enum types in `ChannelConfig`.
pub mod channel_config {
	/// Limit our total exposure to potential loss to on-chain fees on close, including
	/// in-flight HTLCs which are burned to fees as they are too small to claim on-chain
	/// and fees on commitment transaction(s) broadcasted by our counterparty in excess of
	/// our own fee estimate.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.max_dust_htlc_exposure>
	#[allow(clippy::derive_partial_eq_without_eq)]
	#[derive(Clone, PartialEq, ::prost::Oneof)]
	pub enum MaxDustHtlcExposure {
		/// This sets a fixed limit on the total dust exposure in millisatoshis.
		/// See more: <https://docs.rs/lightning/latest/lightning/util/config/enum.MaxDustHTLCExposure.html#variant.FixedLimitMsat>
		#[prost(uint64, tag = "6")]
		FixedLimitMsat(u64),
		/// This sets a multiplier on the ConfirmationTarget::OnChainSweep feerate (in sats/KW) to determine the maximum allowed dust exposure.
		/// See more: <https://docs.rs/lightning/latest/lightning/util/config/enum.MaxDustHTLCExposure.html#variant.FeeRateMultiplier>
		#[prost(uint64, tag = "7")]
		FeeRateMultiplier(u64),
	}
}
/// Closes the channel specified by given request.
/// See more:
/// - <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.close_channel>
/// - <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.force_close_channel>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CloseChannelRequest {
	/// The channel id of the created channel that user can use to refer to channel.
	#[prost(bytes = "bytes", tag = "1")]
	pub user_channel_id: ::prost::bytes::Bytes,
	/// The hex-encoded public key of the node to close a channel with.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// Whether to force close the specified channel.
	#[prost(bool, optional, tag = "3")]
	pub force_close: ::core::option::Option<bool>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CloseChannelResponse {}
/// Returns a list of known channels.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.list_channels>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListChannelsRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListChannelsResponse {
	/// List of channels.
	#[prost(message, repeated, tag = "1")]
	pub channels: ::prost::alloc::vec::Vec<Channel>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Channel {
	/// The channel ID (prior to funding transaction generation, this is a random 32-byte
	/// identifier, afterwards this is the transaction ID of the funding transaction XOR the
	/// funding transaction output).
	///
	/// Note that this means this value is *not* persistent - it can change once during the
	/// lifetime of the channel.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The node ID of our the channel's remote counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The channel's funding transaction output, if we've negotiated the funding transaction with
	/// our counterparty already.
	#[prost(message, optional, tag = "3")]
	pub funding_txo: ::core::option::Option<OutPoint>,
	/// The local `user_channel_id` of this channel.
	#[prost(bytes = "bytes", tag = "4")]
	pub user_channel_id: ::prost::bytes::Bytes,
	/// The value, in satoshis, that must always be held as a reserve in the channel for us. This
	/// value ensures that if we broadcast a revoked state, our counterparty can punish us by
	/// claiming at least this value on chain.
	///
	/// This value is not included in \[`outbound_capacity_msat`\] as it can never be spent.
	///
	/// This value will be `None` for outbound channels until the counterparty accepts the channel.
	#[prost(uint64, optional, tag = "5")]
	pub unspendable_punishment_reserve: ::core::option::Option<u64>,
	/// The value, in satoshis, of this channel as it appears in the funding output.
	#[prost(uint64, tag = "6")]
	pub channel_value_sats: u64,
	/// The currently negotiated fee rate denominated in satoshi per 1000 weight units,
	/// which is applied to commitment and HTLC transactions.
	#[prost(uint32, tag = "7")]
	pub feerate_sat_per_1000_weight: u32,
	/// The available outbound capacity for sending HTLCs to the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved (and, thus, whose
	/// balance is not available for inclusion in new outbound HTLCs). This further does not include
	/// any pending outgoing HTLCs which are awaiting some other resolution to be sent.
	#[prost(uint64, tag = "8")]
	pub outbound_capacity_msat: u64,
	/// The available outbound capacity for sending HTLCs to the remote peer.
	///
	/// The amount does not include any pending HTLCs which are not yet resolved
	/// (and, thus, whose balance is not available for inclusion in new inbound HTLCs). This further
	/// does not include any pending outgoing HTLCs which are awaiting some other resolution to be
	/// sent.
	#[prost(uint64, tag = "9")]
	pub inbound_capacity_msat: u64,
	/// The number of required confirmations on the funding transactions before the funding is
	/// considered "locked". The amount is selected by the channel fundee.
	///
	/// The value will be `None` for outbound channels until the counterparty accepts the channel.
	#[prost(uint32, optional, tag = "10")]
	pub confirmations_required: ::core::option::Option<u32>,
	/// The current number of confirmations on the funding transaction.
	#[prost(uint32, optional, tag = "11")]
	pub confirmations: ::core::option::Option<u32>,
	/// Is `true` if the channel was initiated (and therefore funded) by us.
	#[prost(bool, tag = "12")]
	pub is_outbound: bool,
	/// Is `true` if both parties have exchanged `channel_ready` messages, and the channel is
	/// not currently being shut down. Both parties exchange `channel_ready` messages upon
	/// independently verifying that the required confirmations count provided by
	/// `confirmations_required` has been reached.
	#[prost(bool, tag = "13")]
	pub is_channel_ready: bool,
	/// Is `true` if the channel (a) `channel_ready` messages have been exchanged, (b) the
	/// peer is connected, and (c) the channel is not currently negotiating shutdown.
	///
	/// This is a strict superset of `is_channel_ready`.
	#[prost(bool, tag = "14")]
	pub is_usable: bool,
	/// Is `true` if this channel is (or will be) publicly-announced
	#[prost(bool, tag = "15")]
	pub is_public: bool,
	/// Set of configurable parameters set by self that affect channel operation.
	#[prost(message, optional, tag = "16")]
	pub channel_config: ::core::option::Option<ChannelConfig>,
	/// The available outbound capacity for sending a single HTLC to the remote peer. This is
	/// similar to `outbound_capacity_msat` but it may be further restricted by
	/// the current state and per-HTLC limit(s). This is intended for use when routing, allowing us
	/// to use a limit as close as possible to the HTLC limit we can currently send.
	#[prost(uint64, tag = "17")]
	pub next_outbound_htlc_limit_msat: u64,
	/// The minimum value for sending a single HTLC to the remote peer. This is the equivalent of
	/// `next_outbound_htlc_limit_msat` but represents a lower-bound, rather than
	/// an upper-bound. This is intended for use when routing, allowing us to ensure we pick a
	/// route which is valid.
	#[prost(uint64, tag = "18")]
	pub next_outbound_htlc_minimum_msat: u64,
	/// The number of blocks (after our commitment transaction confirms) that we will need to wait
	/// until we can claim our funds after we force-close the channel. During this time our
	/// counterparty is allowed to punish us if we broadcasted a stale state. If our counterparty
	/// force-closes the channel and broadcasts a commitment transaction we do not have to wait any
	/// time to claim our non-HTLC-encumbered funds.
	///
	/// This value will be `None` for outbound channels until the counterparty accepts the channel.
	#[prost(uint32, optional, tag = "19")]
	pub force_close_spend_delay: ::core::option::Option<u32>,
	/// The smallest value HTLC (in msat) the remote peer will accept, for this channel.
	///
	/// This field is only `None` before we have received either the `OpenChannel` or
	/// `AcceptChannel` message from the remote peer.
	#[prost(uint64, optional, tag = "20")]
	pub counterparty_outbound_htlc_minimum_msat: ::core::option::Option<u64>,
	/// The largest value HTLC (in msat) the remote peer currently will accept, for this channel.
	#[prost(uint64, optional, tag = "21")]
	pub counterparty_outbound_htlc_maximum_msat: ::core::option::Option<u64>,
	/// The value, in satoshis, that must always be held in the channel for our counterparty. This
	/// value ensures that if our counterparty broadcasts a revoked state, we can punish them by
	/// claiming at least this value on chain.
	///
	/// This value is not included in `inbound_capacity_msat` as it can never be spent.
	#[prost(uint64, optional, tag = "22")]
	pub counterparty_unspendable_punishment_reserve: ::core::option::Option<u64>,
	/// Base routing fee in millisatoshis.
	#[prost(uint32, optional, tag = "23")]
	pub counterparty_forwarding_info_fee_base_msat: ::core::option::Option<u32>,
	/// Proportional fee, in millionths of a satoshi the channel will charge per transferred satoshi.
	#[prost(uint32, optional, tag = "24")]
	pub counterparty_forwarding_info_fee_proportional_millionths: ::core::option::Option<u32>,
	/// The minimum difference in CLTV expiry between an ingoing HTLC and its outgoing counterpart,
	/// such that the outgoing HTLC is forwardable to this counterparty.
	#[prost(uint32, optional, tag = "25")]
	pub counterparty_forwarding_info_cltv_expiry_delta: ::core::option::Option<u32>,
}
/// Represent a transaction outpoint.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OutPoint {
	/// The referenced transaction's txid.
	#[prost(string, tag = "1")]
	pub txid: ::prost::alloc::string::String,
	/// The index of the referenced output in its transaction's vout.
	#[prost(uint32, tag = "2")]
	pub vout: u32,
}
