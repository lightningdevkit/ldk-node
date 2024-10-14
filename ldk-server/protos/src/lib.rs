/// Retrieve the latest node info like `node_id`, `current_best_block` etc.
/// See more:
/// - <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.node_id>
/// - <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.status>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetNodeInfoRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetNodeInfoResponse {
	/// The hex-encoded `node-id` or public key for our own lightning node.
	#[prost(string, tag = "1")]
	pub node_id: ::prost::alloc::string::String,
	/// The best block to which our Lightning wallet is currently synced.
	///
	/// Should be always set, will never be `None`.
	#[prost(message, optional, tag = "3")]
	pub current_best_block: ::core::option::Option<BestBlock>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully synced our Lightning wallet to
	/// the chain tip.
	///
	/// Will be `None` if the wallet hasn't been synced yet.
	#[prost(uint64, optional, tag = "4")]
	pub latest_lightning_wallet_sync_timestamp: ::core::option::Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully synced our on-chain
	/// wallet to the chain tip.
	///
	/// Will be `None` if the wallet hasn’t been synced since the node was initialized.
	#[prost(uint64, optional, tag = "5")]
	pub latest_onchain_wallet_sync_timestamp: ::core::option::Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully update our fee rate cache.
	///
	/// Will be `None` if the cache hasn’t been updated since the node was initialized.
	#[prost(uint64, optional, tag = "6")]
	pub latest_fee_rate_cache_update_timestamp: ::core::option::Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when the last rapid gossip sync (RGS) snapshot we
	/// successfully applied was generated.
	///
	/// Will be `None` if RGS isn’t configured or the snapshot hasn’t been updated since the node was initialized.
	#[prost(uint64, optional, tag = "7")]
	pub latest_rgs_snapshot_timestamp: ::core::option::Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last broadcasted a node announcement.
	///
	/// Will be `None` if we have no public channels or we haven’t broadcasted since the node was initialized.
	#[prost(uint64, optional, tag = "8")]
	pub latest_node_announcement_broadcast_timestamp: ::core::option::Option<u64>,
}
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
	#[prost(bytes = "bytes", tag = "1")]
	pub payment_id: ::prost::bytes::Bytes,
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
	/// Offer expiry time in seconds.
	#[prost(uint32, optional, tag = "3")]
	pub expiry_secs: ::core::option::Option<u32>,
	/// If set, it represents the number of items requested, can only be set for fixed-amount offers.
	#[prost(uint64, optional, tag = "4")]
	pub quantity: ::core::option::Option<u64>,
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
	#[prost(bytes = "bytes", tag = "1")]
	pub payment_id: ::prost::bytes::Bytes,
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
	#[prost(uint32, optional, tag = "1")]
	pub forwarding_fee_proportional_millionths: ::core::option::Option<u32>,
	/// Amount (in milli-satoshi) charged for payments forwarded outbound over the channel,
	/// in excess of forwarding_fee_proportional_millionths.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.forwarding_fee_base_msat>
	#[prost(uint32, optional, tag = "2")]
	pub forwarding_fee_base_msat: ::core::option::Option<u32>,
	/// The difference in the CLTV value between incoming HTLCs and an outbound HTLC forwarded
	/// over the channel this config applies to.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.cltv_expiry_delta>
	#[prost(uint32, optional, tag = "3")]
	pub cltv_expiry_delta: ::core::option::Option<u32>,
	/// The maximum additional fee we’re willing to pay to avoid waiting for the counterparty’s
	/// to_self_delay to reclaim funds.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.force_close_avoidance_max_fee_satoshis>
	#[prost(uint64, optional, tag = "4")]
	pub force_close_avoidance_max_fee_satoshis: ::core::option::Option<u64>,
	/// If set, allows this channel’s counterparty to skim an additional fee off this node’s
	/// inbound HTLCs. Useful for liquidity providers to offload on-chain channel costs to end users.
	/// See more: <https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.accept_underpaying_htlcs>
	#[prost(bool, optional, tag = "5")]
	pub accept_underpaying_htlcs: ::core::option::Option<bool>,
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
	/// The reason for force-closing, can only be set while force closing a channel.
	#[prost(string, optional, tag = "4")]
	pub force_close_reason: ::core::option::Option<::prost::alloc::string::String>,
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
/// Returns payment details for a given payment_id.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.payment>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetPaymentDetailsRequest {
	/// An identifier used to uniquely identify a payment in hex-encoded form.
	#[prost(string, tag = "1")]
	pub payment_id: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetPaymentDetailsResponse {
	/// Represents a payment.
	/// Will be `None` if payment doesn't exist.
	#[prost(message, optional, tag = "1")]
	pub payment: ::core::option::Option<Payment>,
}
/// Retrieves list of all payments.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.list_payments>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListPaymentsRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListPaymentsResponse {
	/// List of payments.
	#[prost(message, repeated, tag = "1")]
	pub payments: ::prost::alloc::vec::Vec<Payment>,
}
/// Represents a payment.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/payment/struct.PaymentDetails.html>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Payment {
	/// An identifier used to uniquely identify a payment in hex-encoded form.
	#[prost(string, tag = "1")]
	pub id: ::prost::alloc::string::String,
	/// The kind of the payment.
	#[prost(message, optional, tag = "2")]
	pub kind: ::core::option::Option<PaymentKind>,
	/// The amount transferred.
	#[prost(uint64, optional, tag = "3")]
	pub amount_msat: ::core::option::Option<u64>,
	/// The direction of the payment.
	#[prost(enumeration = "PaymentDirection", tag = "4")]
	pub direction: i32,
	/// The status of the payment.
	#[prost(enumeration = "PaymentStatus", tag = "5")]
	pub status: i32,
	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	#[prost(uint64, tag = "6")]
	pub latest_update_timestamp: u64,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaymentKind {
	#[prost(oneof = "payment_kind::Kind", tags = "1, 2, 3, 4, 5, 6")]
	pub kind: ::core::option::Option<payment_kind::Kind>,
}
/// Nested message and enum types in `PaymentKind`.
pub mod payment_kind {
	#[allow(clippy::derive_partial_eq_without_eq)]
	#[derive(Clone, PartialEq, ::prost::Oneof)]
	pub enum Kind {
		#[prost(message, tag = "1")]
		Onchain(super::Onchain),
		#[prost(message, tag = "2")]
		Bolt11(super::Bolt11),
		#[prost(message, tag = "3")]
		Bolt11Jit(super::Bolt11Jit),
		#[prost(message, tag = "4")]
		Bolt12Offer(super::Bolt12Offer),
		#[prost(message, tag = "5")]
		Bolt12Refund(super::Bolt12Refund),
		#[prost(message, tag = "6")]
		Spontaneous(super::Spontaneous),
	}
}
/// Represents an on-chain payment.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Onchain {}
/// Represents a BOLT 11 payment.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11 {
	/// The payment hash, i.e., the hash of the preimage.
	#[prost(string, tag = "1")]
	pub hash: ::prost::alloc::string::String,
	/// The pre-image used by the payment.
	#[prost(string, optional, tag = "2")]
	pub preimage: ::core::option::Option<::prost::alloc::string::String>,
	/// The secret used by the payment.
	#[prost(bytes = "bytes", optional, tag = "3")]
	pub secret: ::core::option::Option<::prost::bytes::Bytes>,
}
/// Represents a BOLT 11 payment intended to open an LSPS 2 just-in-time channel.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11Jit {
	/// The payment hash, i.e., the hash of the preimage.
	#[prost(string, tag = "1")]
	pub hash: ::prost::alloc::string::String,
	/// The pre-image used by the payment.
	#[prost(string, optional, tag = "2")]
	pub preimage: ::core::option::Option<::prost::alloc::string::String>,
	/// The secret used by the payment.
	#[prost(bytes = "bytes", optional, tag = "3")]
	pub secret: ::core::option::Option<::prost::bytes::Bytes>,
	/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
	///
	/// Allowing them to deduct this fee from the first inbound payment will pay for the LSP’s channel opening fees.
	///
	/// See \[`LdkChannelConfig::accept_underpaying_htlcs`\](<https://docs.rs/lightning/latest/lightning/util/config/struct.ChannelConfig.html#structfield.accept_underpaying_htlcs>)
	/// for more information.
	#[prost(message, optional, tag = "4")]
	pub lsp_fee_limits: ::core::option::Option<LspFeeLimits>,
}
/// Represents a BOLT 12 ‘offer’ payment, i.e., a payment for an Offer.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12Offer {
	/// The payment hash, i.e., the hash of the preimage.
	#[prost(string, optional, tag = "1")]
	pub hash: ::core::option::Option<::prost::alloc::string::String>,
	/// The pre-image used by the payment.
	#[prost(string, optional, tag = "2")]
	pub preimage: ::core::option::Option<::prost::alloc::string::String>,
	/// The secret used by the payment.
	#[prost(bytes = "bytes", optional, tag = "3")]
	pub secret: ::core::option::Option<::prost::bytes::Bytes>,
	/// The hex-encoded ID of the offer this payment is for.
	#[prost(string, tag = "4")]
	pub offer_id: ::prost::alloc::string::String,
	/// The payer's note for the payment.
	/// Truncated to \[PAYER_NOTE_LIMIT\](<https://docs.rs/lightning/latest/lightning/offers/invoice_request/constant.PAYER_NOTE_LIMIT.html>).
	///
	/// **Caution**: The `payer_note` field may come from an untrusted source. To prevent potential misuse,
	/// all non-printable characters will be sanitized and replaced with safe characters.
	#[prost(string, optional, tag = "5")]
	pub payer_note: ::core::option::Option<::prost::alloc::string::String>,
	/// The quantity of an item requested in the offer.
	#[prost(uint64, optional, tag = "6")]
	pub quantity: ::core::option::Option<u64>,
}
/// Represents a BOLT 12 ‘refund’ payment, i.e., a payment for a Refund.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12Refund {
	/// The payment hash, i.e., the hash of the preimage.
	#[prost(string, optional, tag = "1")]
	pub hash: ::core::option::Option<::prost::alloc::string::String>,
	/// The pre-image used by the payment.
	#[prost(string, optional, tag = "2")]
	pub preimage: ::core::option::Option<::prost::alloc::string::String>,
	/// The secret used by the payment.
	#[prost(bytes = "bytes", optional, tag = "3")]
	pub secret: ::core::option::Option<::prost::bytes::Bytes>,
	/// The payer's note for the payment.
	/// Truncated to \[PAYER_NOTE_LIMIT\](<https://docs.rs/lightning/latest/lightning/offers/invoice_request/constant.PAYER_NOTE_LIMIT.html>).
	///
	/// **Caution**: The `payer_note` field may come from an untrusted source. To prevent potential misuse,
	/// all non-printable characters will be sanitized and replaced with safe characters.
	#[prost(string, optional, tag = "5")]
	pub payer_note: ::core::option::Option<::prost::alloc::string::String>,
	/// The quantity of an item requested in the offer.
	#[prost(uint64, optional, tag = "6")]
	pub quantity: ::core::option::Option<u64>,
}
/// Represents a spontaneous (“keysend”) payment.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Spontaneous {
	/// The payment hash, i.e., the hash of the preimage.
	#[prost(string, tag = "1")]
	pub hash: ::prost::alloc::string::String,
	/// The pre-image used by the payment.
	#[prost(string, optional, tag = "2")]
	pub preimage: ::core::option::Option<::prost::alloc::string::String>,
}
/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
/// See \[`LdkChannelConfig::accept_underpaying_htlcs`\] for more information.
///
/// \[`LdkChannelConfig::accept_underpaying_htlcs`\]: lightning::util::config::ChannelConfig::accept_underpaying_htlcs
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LspFeeLimits {
	/// The maximal total amount we allow any configured LSP withhold from us when forwarding the
	/// payment.
	#[prost(uint64, optional, tag = "1")]
	pub max_total_opening_fee_msat: ::core::option::Option<u64>,
	/// The maximal proportional fee, in parts-per-million millisatoshi, we allow any configured
	/// LSP withhold from us when forwarding the payment.
	#[prost(uint64, optional, tag = "2")]
	pub max_proportional_opening_fee_ppm_msat: ::core::option::Option<u64>,
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
	/// The hex-encoded local `user_channel_id` of this channel.
	#[prost(string, tag = "4")]
	pub user_channel_id: ::prost::alloc::string::String,
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
	pub is_announced: bool,
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
	#[prost(uint64, tag = "22")]
	pub counterparty_unspendable_punishment_reserve: u64,
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
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BestBlock {
	/// The block’s hash
	#[prost(string, tag = "1")]
	pub block_hash: ::prost::alloc::string::String,
	/// The height at which the block was confirmed.
	#[prost(uint32, tag = "2")]
	pub height: u32,
}
/// Represents the direction of a payment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum PaymentDirection {
	/// The payment is inbound.
	Inbound = 0,
	/// The payment is outbound.
	Outbound = 1,
}
impl PaymentDirection {
	/// String value of the enum field names used in the ProtoBuf definition.
	///
	/// The values are not transformed in any way and thus are considered stable
	/// (if the ProtoBuf definition does not change) and safe for programmatic use.
	pub fn as_str_name(&self) -> &'static str {
		match self {
			PaymentDirection::Inbound => "INBOUND",
			PaymentDirection::Outbound => "OUTBOUND",
		}
	}
	/// Creates an enum from field names used in the ProtoBuf definition.
	pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
		match value {
			"INBOUND" => Some(Self::Inbound),
			"OUTBOUND" => Some(Self::Outbound),
			_ => None,
		}
	}
}
/// Represents the current status of a payment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum PaymentStatus {
	/// The payment is still pending.
	Pending = 0,
	/// The payment succeeded.
	Succeeded = 1,
	/// The payment failed.
	Failed = 2,
}
impl PaymentStatus {
	/// String value of the enum field names used in the ProtoBuf definition.
	///
	/// The values are not transformed in any way and thus are considered stable
	/// (if the ProtoBuf definition does not change) and safe for programmatic use.
	pub fn as_str_name(&self) -> &'static str {
		match self {
			PaymentStatus::Pending => "PENDING",
			PaymentStatus::Succeeded => "SUCCEEDED",
			PaymentStatus::Failed => "FAILED",
		}
	}
	/// Creates an enum from field names used in the ProtoBuf definition.
	pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
		match value {
			"PENDING" => Some(Self::Pending),
			"SUCCEEDED" => Some(Self::Succeeded),
			"FAILED" => Some(Self::Failed),
			_ => None,
		}
	}
}
