use lightning::chain::channelmonitor::Balance as LdkBalance;
use lightning::ln::{ChannelId, PaymentHash, PaymentPreimage};

use bitcoin::secp256k1::PublicKey;
use bitcoin::{BlockHash, Txid};

use crate::sweep::SpendableOutputInfo;

/// Details of the known available balances returned by [`Node::list_balances`].
///
/// [`Node::list_balances`]: crate::Node::list_balances
#[derive(Debug, Clone)]
pub struct BalanceDetails {
	/// The total balance of our on-chain wallet.
	pub total_onchain_balance_sats: u64,
	/// The currently spendable balance of our on-chain wallet.
	///
	/// This includes any sufficiently confirmed funds, minus
	/// [`total_anchor_channels_reserve_sats`].
	///
	/// [`total_anchor_channels_reserve_sats`]: Self::total_anchor_channels_reserve_sats
	pub spendable_onchain_balance_sats: u64,
	/// The share of our total balance which we retain as an emergency reserve to (hopefully) be
	/// able to spend the Anchor outputs when one of our channels is closed.
	pub total_anchor_channels_reserve_sats: u64,
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
	/// A detailed list of balances currently being swept from the Lightning to the on-chain
	/// wallet.
	///
	/// These are balances resulting from channel closures that may have been encumbered by a
	/// delay, but are now being claimed and useable once sufficiently confirmed on-chain.
	///
	/// Note that, depending on the sync status of the wallets, swept balances listed here might or
	/// might not already be accounted for in [`total_onchain_balance_sats`].
	///
	/// [`total_onchain_balance_sats`]: Self::total_onchain_balance_sats
	pub pending_balances_from_channel_closures: Vec<PendingSweepBalance>,
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

/// Details about the status of a known balance currently being swept to our on-chain wallet.
#[derive(Debug, Clone)]
pub enum PendingSweepBalance {
	/// The spendable output is about to be swept, but a spending transaction has yet to be generated and
	/// broadcast.
	PendingBroadcast {
		/// The identifier of the channel this balance belongs to.
		channel_id: Option<ChannelId>,
		/// The amount, in satoshis, of the output being swept.
		amount_satoshis: u64,
	},
	/// A spending transaction has been generated and broadcast and is awaiting confirmation
	/// on-chain.
	BroadcastAwaitingConfirmation {
		/// The identifier of the channel this balance belongs to.
		channel_id: Option<ChannelId>,
		/// The best height when we last broadcast a transaction spending the output being swept.
		latest_broadcast_height: u32,
		/// The identifier of the transaction spending the swept output we last broadcast.
		latest_spending_txid: Txid,
		/// The amount, in satoshis, of the output being swept.
		amount_satoshis: u64,
	},
	/// A spending transaction has been confirmed on-chain and is awaiting threshold confirmations.
	///
	/// It will be considered irrevocably confirmed after reaching [`ANTI_REORG_DELAY`].
	///
	/// [`ANTI_REORG_DELAY`]: lightning::chain::channelmonitor::ANTI_REORG_DELAY
	AwaitingThresholdConfirmations {
		/// The identifier of the channel this balance belongs to.
		channel_id: Option<ChannelId>,
		/// The identifier of the confirmed transaction spending the swept output.
		latest_spending_txid: Txid,
		/// The hash of the block in which the spending transaction was confirmed.
		confirmation_hash: BlockHash,
		/// The height at which the spending transaction was confirmed.
		confirmation_height: u32,
		/// The amount, in satoshis, of the output being swept.
		amount_satoshis: u64,
	},
}

impl PendingSweepBalance {
	pub(crate) fn from_tracked_spendable_output(output_info: SpendableOutputInfo) -> Self {
		if let Some(confirmation_hash) = output_info.confirmation_hash {
			debug_assert!(output_info.confirmation_height.is_some());
			debug_assert!(output_info.latest_spending_tx.is_some());
			let channel_id = output_info.channel_id;
			let confirmation_height = output_info
				.confirmation_height
				.expect("Height must be set if the output is confirmed");
			let latest_spending_txid = output_info
				.latest_spending_tx
				.as_ref()
				.expect("Spending tx must be set if the output is confirmed")
				.txid();
			let amount_satoshis = output_info.value_satoshis();
			Self::AwaitingThresholdConfirmations {
				channel_id,
				latest_spending_txid,
				confirmation_hash,
				confirmation_height,
				amount_satoshis,
			}
		} else if let Some(latest_broadcast_height) = output_info.latest_broadcast_height {
			debug_assert!(output_info.latest_spending_tx.is_some());
			let channel_id = output_info.channel_id;
			let latest_spending_txid = output_info
				.latest_spending_tx
				.as_ref()
				.expect("Spending tx must be set if the spend was broadcast")
				.txid();
			let amount_satoshis = output_info.value_satoshis();
			Self::BroadcastAwaitingConfirmation {
				channel_id,
				latest_broadcast_height,
				latest_spending_txid,
				amount_satoshis,
			}
		} else {
			let channel_id = output_info.channel_id;
			let amount_satoshis = output_info.value_satoshis();
			Self::PendingBroadcast { channel_id, amount_satoshis }
		}
	}
}
