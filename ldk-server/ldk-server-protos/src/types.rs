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
/// Details about the status of a known Lightning balance.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LightningBalance {
	#[prost(oneof = "lightning_balance::BalanceType", tags = "1, 2, 3, 4, 5, 6")]
	pub balance_type: ::core::option::Option<lightning_balance::BalanceType>,
}
/// Nested message and enum types in `LightningBalance`.
pub mod lightning_balance {
	#[allow(clippy::derive_partial_eq_without_eq)]
	#[derive(Clone, PartialEq, ::prost::Oneof)]
	pub enum BalanceType {
		#[prost(message, tag = "1")]
		ClaimableOnChannelClose(super::ClaimableOnChannelClose),
		#[prost(message, tag = "2")]
		ClaimableAwaitingConfirmations(super::ClaimableAwaitingConfirmations),
		#[prost(message, tag = "3")]
		ContentiousClaimable(super::ContentiousClaimable),
		#[prost(message, tag = "4")]
		MaybeTimeoutClaimableHtlc(super::MaybeTimeoutClaimableHtlc),
		#[prost(message, tag = "5")]
		MaybePreimageClaimableHtlc(super::MaybePreimageClaimableHtlc),
		#[prost(message, tag = "6")]
		CounterpartyRevokedOutputClaimable(super::CounterpartyRevokedOutputClaimable),
	}
}
/// The channel is not yet closed (or the commitment or closing transaction has not yet appeared in a block).
/// The given balance is claimable (less on-chain fees) if the channel is force-closed now.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.LightningBalance.html#variant.ClaimableOnChannelClose>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClaimableOnChannelClose {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The identifier of our channel counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The amount available to claim, in satoshis, excluding the on-chain fees which will be required to do so.
	#[prost(uint64, tag = "3")]
	pub amount_satoshis: u64,
	/// The transaction fee we pay for the closing commitment transaction.
	/// This amount is not included in the `amount_satoshis` value.
	///
	/// Note that if this channel is inbound (and thus our counterparty pays the commitment transaction fee) this value
	/// will be zero.
	#[prost(uint64, tag = "4")]
	pub transaction_fee_satoshis: u64,
	/// The amount of millisatoshis which has been burned to fees from HTLCs which are outbound from us and are related to
	/// a payment which was sent by us. This is the sum of the millisatoshis part of all HTLCs which are otherwise
	/// represented by `LightningBalance::MaybeTimeoutClaimableHTLC` with their
	/// `LightningBalance::MaybeTimeoutClaimableHTLC::outbound_payment` flag set, as well as any dust HTLCs which would
	/// otherwise be represented the same.
	///
	/// This amount (rounded up to a whole satoshi value) will not be included in `amount_satoshis`.
	#[prost(uint64, tag = "5")]
	pub outbound_payment_htlc_rounded_msat: u64,
	/// The amount of millisatoshis which has been burned to fees from HTLCs which are outbound from us and are related to
	/// a forwarded HTLC. This is the sum of the millisatoshis part of all HTLCs which are otherwise represented by
	/// `LightningBalance::MaybeTimeoutClaimableHTLC` with their `LightningBalance::MaybeTimeoutClaimableHTLC::outbound_payment`
	/// flag not set, as well as any dust HTLCs which would otherwise be represented the same.
	///
	/// This amount (rounded up to a whole satoshi value) will not be included in `amount_satoshis`.
	#[prost(uint64, tag = "6")]
	pub outbound_forwarded_htlc_rounded_msat: u64,
	/// The amount of millisatoshis which has been burned to fees from HTLCs which are inbound to us and for which we know
	/// the preimage. This is the sum of the millisatoshis part of all HTLCs which would be represented by
	/// `LightningBalance::ContentiousClaimable` on channel close, but whose current value is included in `amount_satoshis`,
	/// as well as any dust HTLCs which would otherwise be represented the same.
	///
	/// This amount (rounded up to a whole satoshi value) will not be included in `amount_satoshis`.
	#[prost(uint64, tag = "7")]
	pub inbound_claiming_htlc_rounded_msat: u64,
	/// The amount of millisatoshis which has been burned to fees from HTLCs which are inbound to us and for which we do
	/// not know the preimage. This is the sum of the millisatoshis part of all HTLCs which would be represented by
	/// `LightningBalance::MaybePreimageClaimableHTLC` on channel close, as well as any dust HTLCs which would otherwise be
	/// represented the same.
	///
	/// This amount (rounded up to a whole satoshi value) will not be included in the counterparty’s `amount_satoshis`.
	#[prost(uint64, tag = "8")]
	pub inbound_htlc_rounded_msat: u64,
}
/// The channel has been closed, and the given balance is ours but awaiting confirmations until we consider it spendable.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.LightningBalance.html#variant.ClaimableAwaitingConfirmations>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClaimableAwaitingConfirmations {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The identifier of our channel counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The amount available to claim, in satoshis, possibly excluding the on-chain fees which were spent in broadcasting
	/// the transaction.
	#[prost(uint64, tag = "3")]
	pub amount_satoshis: u64,
	/// The height at which we start tracking it as  `SpendableOutput`.
	#[prost(uint32, tag = "4")]
	pub confirmation_height: u32,
}
/// The channel has been closed, and the given balance should be ours but awaiting spending transaction confirmation.
/// If the spending transaction does not confirm in time, it is possible our counterparty can take the funds by
/// broadcasting an HTLC timeout on-chain.
///
/// Once the spending transaction confirms, before it has reached enough confirmations to be considered safe from chain
/// reorganizations, the balance will instead be provided via `LightningBalance::ClaimableAwaitingConfirmations`.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.LightningBalance.html#variant.ContentiousClaimable>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ContentiousClaimable {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The identifier of our channel counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The amount available to claim, in satoshis, excluding the on-chain fees which were spent in broadcasting
	/// the transaction.
	#[prost(uint64, tag = "3")]
	pub amount_satoshis: u64,
	/// The height at which the counterparty may be able to claim the balance if we have not done so.
	#[prost(uint32, tag = "4")]
	pub timeout_height: u32,
	/// The payment hash that locks this HTLC.
	#[prost(string, tag = "5")]
	pub payment_hash: ::prost::alloc::string::String,
	/// The preimage that can be used to claim this HTLC.
	#[prost(string, tag = "6")]
	pub payment_preimage: ::prost::alloc::string::String,
}
/// HTLCs which we sent to our counterparty which are claimable after a timeout (less on-chain fees) if the counterparty
/// does not know the preimage for the HTLCs. These are somewhat likely to be claimed by our counterparty before we do.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.LightningBalance.html#variant.MaybeTimeoutClaimableHTLC>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct MaybeTimeoutClaimableHtlc {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The identifier of our channel counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The amount available to claim, in satoshis, excluding the on-chain fees which were spent in broadcasting
	/// the transaction.
	#[prost(uint64, tag = "3")]
	pub amount_satoshis: u64,
	/// The height at which we will be able to claim the balance if our counterparty has not done so.
	#[prost(uint32, tag = "4")]
	pub claimable_height: u32,
	/// The payment hash whose preimage our counterparty needs to claim this HTLC.
	#[prost(string, tag = "5")]
	pub payment_hash: ::prost::alloc::string::String,
	/// Indicates whether this HTLC represents a payment which was sent outbound from us.
	#[prost(bool, tag = "6")]
	pub outbound_payment: bool,
}
/// HTLCs which we received from our counterparty which are claimable with a preimage which we do not currently have.
/// This will only be claimable if we receive the preimage from the node to which we forwarded this HTLC before the
/// timeout.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.LightningBalance.html#variant.MaybePreimageClaimableHTLC>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct MaybePreimageClaimableHtlc {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The identifier of our channel counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The amount available to claim, in satoshis, excluding the on-chain fees which were spent in broadcasting
	/// the transaction.
	#[prost(uint64, tag = "3")]
	pub amount_satoshis: u64,
	/// The height at which our counterparty will be able to claim the balance if we have not yet received the preimage and
	/// claimed it ourselves.
	#[prost(uint32, tag = "4")]
	pub expiry_height: u32,
	/// The payment hash whose preimage we need to claim this HTLC.
	#[prost(string, tag = "5")]
	pub payment_hash: ::prost::alloc::string::String,
}
/// The channel has been closed, and our counterparty broadcasted a revoked commitment transaction.
///
/// Thus, we’re able to claim all outputs in the commitment transaction, one of which has the following amount.
///
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.LightningBalance.html#variant.CounterpartyRevokedOutputClaimable>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CounterpartyRevokedOutputClaimable {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, tag = "1")]
	pub channel_id: ::prost::alloc::string::String,
	/// The identifier of our channel counterparty.
	#[prost(string, tag = "2")]
	pub counterparty_node_id: ::prost::alloc::string::String,
	/// The amount, in satoshis, of the output which we can claim.
	#[prost(uint64, tag = "3")]
	pub amount_satoshis: u64,
}
/// Details about the status of a known balance currently being swept to our on-chain wallet.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PendingSweepBalance {
	#[prost(oneof = "pending_sweep_balance::BalanceType", tags = "1, 2, 3")]
	pub balance_type: ::core::option::Option<pending_sweep_balance::BalanceType>,
}
/// Nested message and enum types in `PendingSweepBalance`.
pub mod pending_sweep_balance {
	#[allow(clippy::derive_partial_eq_without_eq)]
	#[derive(Clone, PartialEq, ::prost::Oneof)]
	pub enum BalanceType {
		#[prost(message, tag = "1")]
		PendingBroadcast(super::PendingBroadcast),
		#[prost(message, tag = "2")]
		BroadcastAwaitingConfirmation(super::BroadcastAwaitingConfirmation),
		#[prost(message, tag = "3")]
		AwaitingThresholdConfirmations(super::AwaitingThresholdConfirmations),
	}
}
/// The spendable output is about to be swept, but a spending transaction has yet to be generated and broadcast.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.PendingSweepBalance.html#variant.PendingBroadcast>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PendingBroadcast {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, optional, tag = "1")]
	pub channel_id: ::core::option::Option<::prost::alloc::string::String>,
	/// The amount, in satoshis, of the output being swept.
	#[prost(uint64, tag = "2")]
	pub amount_satoshis: u64,
}
/// A spending transaction has been generated and broadcast and is awaiting confirmation on-chain.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.PendingSweepBalance.html#variant.BroadcastAwaitingConfirmation>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BroadcastAwaitingConfirmation {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, optional, tag = "1")]
	pub channel_id: ::core::option::Option<::prost::alloc::string::String>,
	/// The best height when we last broadcast a transaction spending the output being swept.
	#[prost(uint32, tag = "2")]
	pub latest_broadcast_height: u32,
	/// The identifier of the transaction spending the swept output we last broadcast.
	#[prost(string, tag = "3")]
	pub latest_spending_txid: ::prost::alloc::string::String,
	/// The amount, in satoshis, of the output being swept.
	#[prost(uint64, tag = "4")]
	pub amount_satoshis: u64,
}
/// A spending transaction has been confirmed on-chain and is awaiting threshold confirmations.
///
/// It will be considered irrevocably confirmed after reaching `ANTI_REORG_DELAY`.
/// See more: <https://docs.rs/ldk-node/latest/ldk_node/enum.PendingSweepBalance.html#variant.AwaitingThresholdConfirmations>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AwaitingThresholdConfirmations {
	/// The identifier of the channel this balance belongs to.
	#[prost(string, optional, tag = "1")]
	pub channel_id: ::core::option::Option<::prost::alloc::string::String>,
	/// The identifier of the confirmed transaction spending the swept output.
	#[prost(string, tag = "2")]
	pub latest_spending_txid: ::prost::alloc::string::String,
	/// The hash of the block in which the spending transaction was confirmed.
	#[prost(string, tag = "3")]
	pub confirmation_hash: ::prost::alloc::string::String,
	/// The height at which the spending transaction was confirmed.
	#[prost(uint32, tag = "4")]
	pub confirmation_height: u32,
	/// The amount, in satoshis, of the output being swept.
	#[prost(uint64, tag = "5")]
	pub amount_satoshis: u64,
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
