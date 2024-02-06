use bitcoin::secp256k1::PublicKey;
use lightning::chain::channelmonitor::Balance as LdkBalance;
use lightning::ln::{ChannelId, PaymentHash, PaymentPreimage};

/// Details of the known available balances returned by [`Node::list_balances`].
///
/// [`Node::list_balances`]: crate::Node::list_balances
#[derive(Debug, Clone)]
pub struct BalanceDetails {
	/// The total balance that we would be able to claim across all our Lightning channels.
	///
	/// Note this excludes balances that we are unsure if we are able to claim (e.g., as we are
	/// waiting for a preimage or for a timeout to expire). These balances will however be included
	/// as [`MaybePreimageClaimableHTLC`] and
	/// [`MaybeTimeoutClaimableHTLC`] in [`lightning_balances`].
	///
	/// [`MaybePreimageClaimableHTLC`]: LightningBalance::MaybePreimageClaimableHTLC
	/// [`MaybeTimeoutClaimableHTLC`]: LightningBalance::MaybeTimeoutClaimableHTLC
	/// [`lightning_balances`]: Self::lightning_balances
	pub total_lightning_balance_sats: u64,
	/// A detailed list of all known Lightning balances that would be claimable on channel closure.
	///
	/// Note that less than the listed amounts are spendable over lightning as further reserve
	/// restrictions apply. Please refer to [`ChannelDetails::outbound_capacity_msat`] and
	/// [`ChannelDetails::next_outbound_htlc_limit_msat`] as returned by [`Node::list_channels`]
	/// for a better approximation of the spendable amounts.
	///
	/// [`ChannelDetails::outbound_capacity_msat`]: crate::ChannelDetails::outbound_capacity_msat
	/// [`ChannelDetails::next_outbound_htlc_limit_msat`]: crate::ChannelDetails::next_outbound_htlc_limit_msat
	/// [`Node::list_channels`]: crate::Node::list_channels
	pub lightning_balances: Vec<LightningBalance>,
}

/// Details about the status of a known Lightning balance.
#[derive(Debug, Clone)]
pub enum LightningBalance {
	/// The channel is not yet closed (or the commitment or closing transaction has not yet
	/// appeared in a block). The given balance is claimable (less on-chain fees) if the channel is
	/// force-closed now.
	ClaimableOnChannelClose {
		/// The identifier of the channel this balance belongs to.
		channel_id: ChannelId,
		/// The identifier of our channel counterparty.
		counterparty_node_id: PublicKey,
		/// The amount available to claim, in satoshis, excluding the on-chain fees which will be
		/// required to do so.
		amount_satoshis: u64,
	},
	/// The channel has been closed, and the given balance is ours but awaiting confirmations until
	/// we consider it spendable.
	ClaimableAwaitingConfirmations {
		/// The identifier of the channel this balance belongs to.
		channel_id: ChannelId,
		/// The identifier of our channel counterparty.
		counterparty_node_id: PublicKey,
		/// The amount available to claim, in satoshis, possibly excluding the on-chain fees which
		/// were spent in broadcasting the transaction.
		amount_satoshis: u64,
		/// The height at which an [`Event::SpendableOutputs`] event will be generated for this
		/// amount.
		///
		/// [`Event::SpendableOutputs`]: lightning::events::Event::SpendableOutputs
		confirmation_height: u32,
	},
	/// The channel has been closed, and the given balance should be ours but awaiting spending
	/// transaction confirmation. If the spending transaction does not confirm in time, it is
	/// possible our counterparty can take the funds by broadcasting an HTLC timeout on-chain.
	///
	/// Once the spending transaction confirms, before it has reached enough confirmations to be
	/// considered safe from chain reorganizations, the balance will instead be provided via
	/// [`LightningBalance::ClaimableAwaitingConfirmations`].
	ContentiousClaimable {
		/// The identifier of the channel this balance belongs to.
		channel_id: ChannelId,
		/// The identifier of our channel counterparty.
		counterparty_node_id: PublicKey,
		/// The amount available to claim, in satoshis, excluding the on-chain fees which will be
		/// required to do so.
		amount_satoshis: u64,
		/// The height at which the counterparty may be able to claim the balance if we have not
		/// done so.
		timeout_height: u32,
		/// The payment hash that locks this HTLC.
		payment_hash: PaymentHash,
		/// The preimage that can be used to claim this HTLC.
		payment_preimage: PaymentPreimage,
	},
	/// HTLCs which we sent to our counterparty which are claimable after a timeout (less on-chain
	/// fees) if the counterparty does not know the preimage for the HTLCs. These are somewhat
	/// likely to be claimed by our counterparty before we do.
	MaybeTimeoutClaimableHTLC {
		/// The identifier of the channel this balance belongs to.
		channel_id: ChannelId,
		/// The identifier of our channel counterparty.
		counterparty_node_id: PublicKey,
		/// The amount potentially available to claim, in satoshis, excluding the on-chain fees
		/// which will be required to do so.
		amount_satoshis: u64,
		/// The height at which we will be able to claim the balance if our counterparty has not
		/// done so.
		claimable_height: u32,
		/// The payment hash whose preimage our counterparty needs to claim this HTLC.
		payment_hash: PaymentHash,
	},
	/// HTLCs which we received from our counterparty which are claimable with a preimage which we
	/// do not currently have. This will only be claimable if we receive the preimage from the node
	/// to which we forwarded this HTLC before the timeout.
	MaybePreimageClaimableHTLC {
		/// The identifier of the channel this balance belongs to.
		channel_id: ChannelId,
		/// The identifier of our channel counterparty.
		counterparty_node_id: PublicKey,
		/// The amount potentially available to claim, in satoshis, excluding the on-chain fees
		/// which will be required to do so.
		amount_satoshis: u64,
		/// The height at which our counterparty will be able to claim the balance if we have not
		/// yet received the preimage and claimed it ourselves.
		expiry_height: u32,
		/// The payment hash whose preimage we need to claim this HTLC.
		payment_hash: PaymentHash,
	},
	/// The channel has been closed, and our counterparty broadcasted a revoked commitment
	/// transaction.
	///
	/// Thus, we're able to claim all outputs in the commitment transaction, one of which has the
	/// following amount.
	CounterpartyRevokedOutputClaimable {
		/// The identifier of the channel this balance belongs to.
		channel_id: ChannelId,
		/// The identifier of our channel counterparty.
		counterparty_node_id: PublicKey,
		/// The amount, in satoshis, of the output which we can claim.
		amount_satoshis: u64,
	},
}

impl LightningBalance {
	pub(crate) fn from_ldk_balance(
		channel_id: ChannelId, counterparty_node_id: PublicKey, balance: LdkBalance,
	) -> Self {
		match balance {
			LdkBalance::ClaimableOnChannelClose { amount_satoshis } => {
				Self::ClaimableOnChannelClose { channel_id, counterparty_node_id, amount_satoshis }
			},
			LdkBalance::ClaimableAwaitingConfirmations { amount_satoshis, confirmation_height } => {
				Self::ClaimableAwaitingConfirmations {
					channel_id,
					counterparty_node_id,
					amount_satoshis,
					confirmation_height,
				}
			},
			LdkBalance::ContentiousClaimable {
				amount_satoshis,
				timeout_height,
				payment_hash,
				payment_preimage,
			} => Self::ContentiousClaimable {
				channel_id,
				counterparty_node_id,
				amount_satoshis,
				timeout_height,
				payment_hash,
				payment_preimage,
			},
			LdkBalance::MaybeTimeoutClaimableHTLC {
				amount_satoshis,
				claimable_height,
				payment_hash,
			} => Self::MaybeTimeoutClaimableHTLC {
				channel_id,
				counterparty_node_id,
				amount_satoshis,
				claimable_height,
				payment_hash,
			},
			LdkBalance::MaybePreimageClaimableHTLC {
				amount_satoshis,
				expiry_height,
				payment_hash,
			} => Self::MaybePreimageClaimableHTLC {
				channel_id,
				counterparty_node_id,
				amount_satoshis,
				expiry_height,
				payment_hash,
			},
			LdkBalance::CounterpartyRevokedOutputClaimable { amount_satoshis } => {
				Self::CounterpartyRevokedOutputClaimable {
					channel_id,
					counterparty_node_id,
					amount_satoshis,
				}
			},
		}
	}
}
