/// Retrieve the latest node info like `node_id`, `current_best_block` etc.
/// See more:
/// - <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.node_id>
/// - <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.status>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetNodeInfoRequest {}
/// The response `content` for the `GetNodeInfo` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
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
	pub current_best_block: ::core::option::Option<super::types::BestBlock>,
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
/// The response `content` for the `OnchainReceive` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`..
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
	/// If `fee_rate_sat_per_vb` is set it will be used on the resulting transaction. Otherwise we'll retrieve
	/// a reasonable estimate from BitcoinD.
	#[prost(uint64, optional, tag = "4")]
	pub fee_rate_sat_per_vb: ::core::option::Option<u64>,
}
/// The response `content` for the `OnchainSend` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
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
	#[prost(message, optional, tag = "2")]
	pub description: ::core::option::Option<super::types::Bolt11InvoiceDescription>,
	/// Invoice expiry time in seconds.
	#[prost(uint32, tag = "3")]
	pub expiry_secs: u32,
}
/// The response `content` for the `Bolt11Receive` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
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
/// The response `content` for the `Bolt11Send` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt11SendResponse {
	/// An identifier used to uniquely identify a payment in hex-encoded form.
	#[prost(string, tag = "1")]
	pub payment_id: ::prost::alloc::string::String,
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
/// The response `content` for the `Bolt12Receive` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
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
/// The response `content` for the `Bolt12Send` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bolt12SendResponse {
	/// An identifier used to uniquely identify a payment in hex-encoded form.
	#[prost(string, tag = "1")]
	pub payment_id: ::prost::alloc::string::String,
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
	pub channel_config: ::core::option::Option<super::types::ChannelConfig>,
	/// Whether the channel should be public.
	#[prost(bool, tag = "6")]
	pub announce_channel: bool,
}
/// The response `content` for the `OpenChannel` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OpenChannelResponse {
	/// The local channel id of the created channel that user can use to refer to channel.
	#[prost(string, tag = "1")]
	pub user_channel_id: ::prost::alloc::string::String,
}
/// Update the config for a previously opened channel.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.update_channel_config>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UpdateChannelConfigRequest {
	/// The local `user_channel_id` of this channel.
	#[prost(string, tag = "1")]
	pub user_channel_id: ::prost::alloc::string::String,
	/// The hex-encoded public key of the counterparty node to update channel config with.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The updated channel configuration settings for a channel.
	#[prost(message, optional, tag = "3")]
	pub channel_config: ::core::option::Option<super::types::ChannelConfig>,
}
/// The response `content` for the `UpdateChannelConfig` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UpdateChannelConfigResponse {}
/// Closes the channel specified by given request.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.close_channel>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CloseChannelRequest {
	/// The local `user_channel_id` of this channel.
	#[prost(string, tag = "1")]
	pub user_channel_id: ::prost::alloc::string::String,
	/// The hex-encoded public key of the node to close a channel with.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
}
/// The response `content` for the `CloseChannel` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CloseChannelResponse {}
/// Returns a list of known channels.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.list_channels>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListChannelsRequest {}
/// The response `content` for the `ListChannels` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListChannelsResponse {
	/// List of channels.
	#[prost(message, repeated, tag = "1")]
	pub channels: ::prost::alloc::vec::Vec<super::types::Channel>,
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
/// The response `content` for the `GetPaymentDetails` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetPaymentDetailsResponse {
	/// Represents a payment.
	/// Will be `None` if payment doesn't exist.
	#[prost(message, optional, tag = "1")]
	pub payment: ::core::option::Option<super::types::Payment>,
}
/// Retrieves list of all payments.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.list_payments>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListPaymentsRequest {
	/// `page_token` is a pagination token.
	///
	/// To query for the first page, `page_token` must not be specified.
	///
	/// For subsequent pages, use the value that was returned as `next_page_token` in the previous
	/// page's response.
	#[prost(message, optional, tag = "1")]
	pub page_token: ::core::option::Option<super::types::PageToken>,
}
/// The response `content` for the `ListPayments` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListPaymentsResponse {
	/// List of payments.
	#[prost(message, repeated, tag = "1")]
	pub payments: ::prost::alloc::vec::Vec<super::types::Payment>,
	/// `next_page_token` is a pagination token, used to retrieve the next page of results.
	/// Use this value to query for next-page of paginated operation, by specifying
	/// this value as the `page_token` in the next request.
	///
	/// If `next_page_token` is `None`, then the "last page" of results has been processed and
	/// there is no more data to be retrieved.
	///
	/// If `next_page_token` is not `None`, it does not necessarily mean that there is more data in the
	/// result set. The only way to know when you have reached the end of the result set is when
	/// `next_page_token` is `None`.
	///
	/// **Caution**: Clients must not assume a specific number of records to be present in a page for
	/// paginated response.
	#[prost(message, optional, tag = "2")]
	pub next_page_token: ::core::option::Option<super::types::PageToken>,
}
/// Retrieves list of all forwarded payments.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.Event.html#variant.PaymentForwarded>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListForwardedPaymentsRequest {
	/// `page_token` is a pagination token.
	///
	/// To query for the first page, `page_token` must not be specified.
	///
	/// For subsequent pages, use the value that was returned as `next_page_token` in the previous
	/// page's response.
	#[prost(message, optional, tag = "1")]
	pub page_token: ::core::option::Option<super::types::PageToken>,
}
/// The response `content` for the `ListForwardedPayments` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListForwardedPaymentsResponse {
	/// List of forwarded payments.
	#[prost(message, repeated, tag = "1")]
	pub forwarded_payments: ::prost::alloc::vec::Vec<super::types::ForwardedPayment>,
	/// `next_page_token` is a pagination token, used to retrieve the next page of results.
	/// Use this value to query for next-page of paginated operation, by specifying
	/// this value as the `page_token` in the next request.
	///
	/// If `next_page_token` is `None`, then the "last page" of results has been processed and
	/// there is no more data to be retrieved.
	///
	/// If `next_page_token` is not `None`, it does not necessarily mean that there is more data in the
	/// result set. The only way to know when you have reached the end of the result set is when
	/// `next_page_token` is `None`.
	///
	/// **Caution**: Clients must not assume a specific number of records to be present in a page for
	/// paginated response.
	#[prost(message, optional, tag = "2")]
	pub next_page_token: ::core::option::Option<super::types::PageToken>,
}
/// Retrieves an overview of all known balances.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/struct.Node.html#method.list_balances>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetBalancesRequest {}
/// The response `content` for the `GetBalances` API, when HttpStatusCode is OK (200).
/// When HttpStatusCode is not OK (non-200), the response `content` contains a serialized `ErrorResponse`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GetBalancesResponse {
	/// The total balance of our on-chain wallet.
	#[prost(uint64, tag = "1")]
	pub total_onchain_balance_sats: u64,
	/// The currently spendable balance of our on-chain wallet.
	///
	/// This includes any sufficiently confirmed funds, minus `total_anchor_channels_reserve_sats`.
	#[prost(uint64, tag = "2")]
	pub spendable_onchain_balance_sats: u64,
	/// The share of our total balance that we retain as an emergency reserve to (hopefully) be
	/// able to spend the Anchor outputs when one of our channels is closed.
	#[prost(uint64, tag = "3")]
	pub total_anchor_channels_reserve_sats: u64,
	/// The total balance that we would be able to claim across all our Lightning channels.
	///
	/// Note this excludes balances that we are unsure if we are able to claim (e.g., as we are
	/// waiting for a preimage or for a timeout to expire). These balances will however be included
	/// as `MaybePreimageClaimableHTLC` and `MaybeTimeoutClaimableHTLC` in `lightning_balances`.
	#[prost(uint64, tag = "4")]
	pub total_lightning_balance_sats: u64,
	/// A detailed list of all known Lightning balances that would be claimable on channel closure.
	///
	/// Note that less than the listed amounts are spendable over lightning as further reserve
	/// restrictions apply. Please refer to `Channel::outbound_capacity_msat` and
	/// Channel::next_outbound_htlc_limit_msat as returned by `ListChannels`
	/// for a better approximation of the spendable amounts.
	#[prost(message, repeated, tag = "5")]
	pub lightning_balances: ::prost::alloc::vec::Vec<super::types::LightningBalance>,
	/// A detailed list of balances currently being swept from the Lightning to the on-chain
	/// wallet.
	///
	/// These are balances resulting from channel closures that may have been encumbered by a
	/// delay, but are now being claimed and useable once sufficiently confirmed on-chain.
	///
	/// Note that, depending on the sync status of the wallets, swept balances listed here might or
	/// might not already be accounted for in `total_onchain_balance_sats`.
	#[prost(message, repeated, tag = "6")]
	pub pending_balances_from_channel_closures:
		::prost::alloc::vec::Vec<super::types::PendingSweepBalance>,
}
