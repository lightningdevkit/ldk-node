// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::secp256k1::PublicKey;
use bitcoin::Txid;
use lightning::impl_writeable_tlv_based;
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;

use crate::data_store::{StorableObject, StorableObjectUpdate};
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::PaymentDetails;

/// Identifies which channel lifecycle event a [`FundingDetails`] record tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FundingPurpose {
	/// The funding transaction opens a channel.
	Establishment,
	/// The funding transaction splices an already-open channel.
	Splice,
}

impl_writeable_tlv_based_enum!(FundingPurpose,
	(0, Establishment) => {},
	(2, Splice) => {}
);

/// One broadcast of a funding transaction (channel open or splice) in which this node
/// had a stake.
///
/// When an RBF produces multiple candidates for the same [`FundingDetails`], each
/// broadcast is recorded as its own [`FundingCandidate`] so that whichever candidate
/// actually confirms can be identified and its contribution values restored onto the
/// outer [`PaymentDetails`].
///
/// `contribution` is set for dual-funded opens and splices, where the local node submits
/// a [`FundingContribution`] describing its inputs, outputs, and fee share. It is `None`
/// for single-funded opens, which have exactly one candidate and no alternative to swap
/// to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundingCandidate {
	/// Transaction ID of this broadcast.
	pub txid: Txid,
	/// The contribution used to build this candidate, if any.
	pub contribution: Option<FundingContribution>,
}

impl_writeable_tlv_based!(FundingCandidate, {
	(0, txid, required),
	(2, contribution, option),
});

/// Marks an on-chain payment as belonging to a channel's funding lifecycle (open or
/// splice), and carries the per-candidate state needed to react to RBF replacements.
///
/// The candidate whose `txid` matches the outer [`PaymentDetails`]`::kind.txid` is the
/// one currently reflected by the payment's `amount_msat` and `fee_paid_msat`. On RBF, a
/// new candidate is appended and becomes active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundingDetails {
	/// The channel whose funding is being tracked.
	pub channel_id: ChannelId,
	/// The channel's counterparty.
	pub counterparty_node_id: PublicKey,
	/// Whether this funding is for a channel open or a splice.
	pub purpose: FundingPurpose,
	/// Broadcast candidates, in the order they were observed.
	pub candidates: Vec<FundingCandidate>,
}

impl_writeable_tlv_based!(FundingDetails, {
	(0, channel_id, required),
	(2, counterparty_node_id, required),
	(4, purpose, required),
	(6, candidates, optional_vec),
});

/// Represents a pending payment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPaymentDetails {
	/// The full payment details
	pub details: PaymentDetails,
	/// Transaction IDs that have replaced or conflict with this payment.
	pub conflicting_txids: Vec<Txid>,
	/// Set when the payment's transaction is a channel funding (open or splice). The
	/// record transitions to [`PaymentStatus::Succeeded`] on `ChannelReady` instead of
	/// after [`ANTI_REORG_DELAY`] confirmations.
	///
	/// [`PaymentStatus::Succeeded`]: crate::payment::store::PaymentStatus::Succeeded
	/// [`ANTI_REORG_DELAY`]: lightning::chain::channelmonitor::ANTI_REORG_DELAY
	pub funding_details: Option<FundingDetails>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(details: PaymentDetails, conflicting_txids: Vec<Txid>) -> Self {
		Self { details, conflicting_txids, funding_details: None }
	}

	pub(crate) fn with_funding_details(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, funding_details: FundingDetails,
	) -> Self {
		Self { details, conflicting_txids, funding_details: Some(funding_details) }
	}

	/// Convert to finalized payment for the main payment store
	pub fn into_payment_details(self) -> PaymentDetails {
		self.details
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
	(4, funding_details, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub funding_details: Option<Option<FundingDetails>>,
}

impl StorableObject for PendingPaymentDetails {
	type Id = PaymentId;
	type Update = PendingPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.details.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		let mut updated = false;

		// Update the underlying payment details if present
		if let Some(payment_update) = update.payment_update {
			updated |= self.details.update(payment_update);
		}

		if let Some(new_conflicting_txids) = update.conflicting_txids {
			if self.conflicting_txids != new_conflicting_txids {
				self.conflicting_txids = new_conflicting_txids;
				updated = true;
			}
		}

		if let Some(new_funding_details) = update.funding_details {
			if self.funding_details != new_funding_details {
				self.funding_details = new_funding_details;
				updated = true;
			}
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

impl StorableObjectUpdate<PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn id(&self) -> <PendingPaymentDetails as StorableObject>::Id {
		self.id
	}
}

impl From<&PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn from(value: &PendingPaymentDetails) -> Self {
		let conflicting_txids = if value.conflicting_txids.is_empty() {
			None
		} else {
			Some(value.conflicting_txids.clone())
		};
		Self {
			id: value.id(),
			payment_update: Some(value.details.to_update()),
			conflicting_txids,
			funding_details: Some(value.funding_details.clone()),
		}
	}
}
