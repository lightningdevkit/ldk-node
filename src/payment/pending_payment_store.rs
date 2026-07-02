// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::secp256k1::PublicKey;
use bitcoin::{TxOut, Txid};
use lightning::chain::transaction::OutPoint;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectUpdate};
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::{PaymentDetails, PaymentKind};
use crate::types::UserChannelId;

/// The number of times a splice intent is resubmitted to LDK before it is abandoned and the
/// failure is surfaced to the user.
pub(crate) const MAX_SPLICE_ATTEMPTS: u8 = 3;

/// One candidate transaction in an interactive-funding (splice) RBF history, holding this node's
/// share of the funding amount and fee for that candidate. Both are `None` for a candidate this
/// node did not contribute to — e.g. a counterparty-initiated round before our `splice_in` joined
/// it via RBF. Recorded per pending payment so that, on confirmation, the payment reports the
/// figures of the candidate that actually confirmed, which need not be the last one broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FundingTxCandidate {
	/// The candidate's broadcast transaction id.
	pub txid: Txid,
	/// This node's share of the funding amount for this candidate, in millisatoshis, or `None` if
	/// this node did not contribute to it.
	pub amount_msat: Option<u64>,
	/// This node's share of the on-chain fee for this candidate, in millisatoshis, or `None` if
	/// this node did not contribute to it.
	pub fee_paid_msat: Option<u64>,
}

impl_writeable_tlv_based!(FundingTxCandidate, {
	(0, txid, required),
	(2, amount_msat, option),
	(4, fee_paid_msat, option),
});

/// The parameters of the API call that initiated a splice, used to rebuild a fresh contribution
/// when the stored one has become stale (e.g. its feerate is no longer sufficient).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpliceKind {
	/// [`Node::splice_in`] with a resolved amount.
	///
	/// [`Node::splice_in`]: crate::Node::splice_in
	In { amount_sats: u64 },
	/// [`Node::splice_out`] to the given outputs.
	///
	/// [`Node::splice_out`]: crate::Node::splice_out
	Out { outputs: Vec<TxOut> },
	/// [`Node::bump_channel_funding_fee`] of a pending splice.
	///
	/// [`Node::bump_channel_funding_fee`]: crate::Node::bump_channel_funding_fee
	Rbf {},
}

impl_writeable_tlv_based_enum!(SpliceKind,
	(0, In) => {
		(0, amount_sats, required),
	},
	(2, Out) => {
		(0, outputs, required_vec),
	},
	(4, Rbf) => {},
);

/// A user-initiated splice that has been handed to LDK but is not yet guaranteed to survive a
/// restart. LDK only persists a splice once its negotiation reaches `AwaitingSignatures`, and it
/// abandons an in-progress negotiation whenever the peer disconnects (which includes stopping the
/// node). Until the new funding transaction locks we keep enough state to resubmit the splice
/// ourselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpliceIntent {
	/// The channel's local identifier, carried so the retrier can address the splice without a
	/// separate store keyed by it.
	pub user_channel_id: UserChannelId,
	/// The channel counterparty.
	pub counterparty_node_id: PublicKey,
	/// The channel being spliced.
	pub channel_id: ChannelId,
	/// The channel's funding outpoint when the splice was initiated. It only changes once a splice
	/// locks, so a mismatch with the channel's current funding outpoint means the splice (or a
	/// replacement) completed and there is nothing left to resubmit.
	pub pre_splice_funding_txo: OutPoint,
	/// The contribution handed to [`ChannelManager::funding_contributed`], resubmitted verbatim.
	///
	/// [`ChannelManager::funding_contributed`]: lightning::ln::channelmanager::ChannelManager::funding_contributed
	pub contribution: FundingContribution,
	/// The parameters of the originating API call, used to rebuild a fresh contribution when the
	/// stored one has become stale.
	pub kind: SpliceKind,
	/// The number of times the contribution has been resubmitted to LDK after the originating API
	/// call handed it off.
	pub attempts: u8,
}

impl_writeable_tlv_based!(SpliceIntent, {
	(0, user_channel_id, required),
	(2, counterparty_node_id, required),
	(4, channel_id, required),
	(6, pre_splice_funding_txo, required),
	(8, contribution, required),
	(10, kind, required),
	(12, attempts, required),
});

/// A pending payment tracked by LDK Node, keyed by [`PaymentId`].
///
/// A user-initiated splice is persisted as a [`PendingSplice`] before its contribution is handed
/// to LDK — at which point no funding transaction, and therefore no [`PaymentDetails`], exists yet.
/// Once the splice is broadcast and classified it becomes a [`Tracked`] payment carrying the real
/// [`PaymentDetails`], while retaining its [`SpliceIntent`] until the splice locks.
///
/// [`PendingSplice`]: Self::PendingSplice
/// [`Tracked`]: Self::Tracked
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingPaymentDetails {
	/// A user-initiated splice persisted before hand-off to LDK; no funding transaction exists yet.
	/// Keyed by the generated [`PaymentId`]; never mirrored into the payment store.
	PendingSplice { id: PaymentId, intent: SpliceIntent },
	/// A pending payment tracked toward confirmation, optionally still carrying a live splice
	/// intent to resubmit until the splice locks.
	Tracked {
		/// The full payment details.
		details: PaymentDetails,
		/// Transaction IDs that have replaced or conflict with this payment.
		conflicting_txids: Vec<Txid>,
		/// For interactive funding (splices), this node's per-candidate funding figures across the
		/// RBF history, keyed by each candidate's txid. Empty for non-funding payments.
		candidates: Vec<FundingTxCandidate>,
		/// The splice intent to resubmit if LDK drops the splice before it locks, or `None` for a
		/// non-splice payment or a splice that has locked.
		splice_intent: Option<SpliceIntent>,
	},
}

impl PendingPaymentDetails {
	pub(crate) fn new(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, candidates: Vec<FundingTxCandidate>,
	) -> Self {
		Self::tracked(details, conflicting_txids, candidates, None)
	}

	pub(crate) fn tracked(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, candidates: Vec<FundingTxCandidate>,
		splice_intent: Option<SpliceIntent>,
	) -> Self {
		Self::Tracked { details, conflicting_txids, candidates, splice_intent }
	}

	pub(crate) fn pending_splice(id: PaymentId, intent: SpliceIntent) -> Self {
		Self::PendingSplice { id, intent }
	}

	/// The full payment details, or `None` for a splice not yet broadcast.
	pub(crate) fn details(&self) -> Option<&PaymentDetails> {
		match self {
			Self::PendingSplice { .. } => None,
			Self::Tracked { details, .. } => Some(details),
		}
	}

	/// Transaction IDs that have replaced or conflict with this payment.
	pub(crate) fn conflicting_txids(&self) -> &[Txid] {
		match self {
			Self::PendingSplice { .. } => &[],
			Self::Tracked { conflicting_txids, .. } => conflicting_txids,
		}
	}

	/// The splice intent this record carries, if it is a splice that has not yet locked.
	pub(crate) fn splice_intent(&self) -> Option<&SpliceIntent> {
		match self {
			Self::PendingSplice { intent, .. } => Some(intent),
			Self::Tracked { splice_intent, .. } => splice_intent.as_ref(),
		}
	}

	/// Returns this node's recorded funding figures for the candidate with the given txid, if any.
	pub(crate) fn candidate(&self, txid: Txid) -> Option<&FundingTxCandidate> {
		match self {
			Self::PendingSplice { .. } => None,
			Self::Tracked { candidates, .. } => {
				candidates.iter().find(|candidate| candidate.txid == txid)
			},
		}
	}
}

impl_writeable_tlv_based_enum!(PendingPaymentDetails,
	(0, PendingSplice) => {
		(0, id, required),
		(2, intent, required),
	},
	(2, Tracked) => {
		(0, details, required),
		(2, conflicting_txids, optional_vec),
		(4, candidates, optional_vec),
		(6, splice_intent, option),
	},
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub candidates: Vec<FundingTxCandidate>,
	/// The splice intent to set (`Some(Some(..))`) or clear (`Some(None)`), or `None` to leave it
	/// unchanged. Setting it on a [`PendingPaymentDetails::PendingSplice`] replaces the intent (e.g.
	/// to bump the retry attempt count); clearing a pre-broadcast splice is done by removing the
	/// record, not through this field.
	pub splice_intent: Option<Option<SpliceIntent>>,
}

impl StorableObject for PendingPaymentDetails {
	type Id = PaymentId;
	type Update = PendingPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		match self {
			Self::PendingSplice { id, .. } => *id,
			Self::Tracked { details, .. } => details.id,
		}
	}

	fn update(&mut self, update: Self::Update) -> bool {
		match self {
			Self::PendingSplice { intent, .. } => {
				// A pre-broadcast record only carries a splice intent; the only meaningful update is
				// replacing that intent. Clearing it is done by removing the record.
				if let Some(Some(new_intent)) = update.splice_intent {
					if *intent != new_intent {
						*intent = new_intent;
						return true;
					}
				}
				false
			},
			Self::Tracked { details, conflicting_txids, candidates, splice_intent } => {
				let mut updated = false;

				// Update the underlying payment details if present
				if let Some(payment_update) = update.payment_update {
					updated |= details.update(payment_update);
				}

				if let Some(new_conflicting_txids) = update.conflicting_txids {
					if *conflicting_txids != new_conflicting_txids {
						*conflicting_txids = new_conflicting_txids;
						updated = true;
					}
				}

				if let PaymentKind::Onchain { txid, .. } = &details.kind {
					let conflicts_len = conflicting_txids.len();
					conflicting_txids.retain(|conflicting_txid| conflicting_txid != txid);
					updated |= conflicting_txids.len() != conflicts_len;
				}

				// Each classify passes the complete candidate history, so a non-empty update
				// replaces the stored list. An empty update (e.g. a non-funding payment) leaves it
				// untouched.
				if !update.candidates.is_empty() && *candidates != update.candidates {
					*candidates = update.candidates;
					updated = true;
				}

				if let Some(new_splice_intent) = update.splice_intent {
					if *splice_intent != new_splice_intent {
						*splice_intent = new_splice_intent;
						updated = true;
					}
				}

				updated
			},
		}
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
		match value {
			PendingPaymentDetails::PendingSplice { id, intent } => Self {
				id: *id,
				payment_update: None,
				conflicting_txids: None,
				candidates: Vec::new(),
				splice_intent: Some(Some(intent.clone())),
			},
			PendingPaymentDetails::Tracked {
				details,
				conflicting_txids,
				candidates,
				splice_intent,
			} => {
				let conflicting_txids = if conflicting_txids.is_empty() {
					None
				} else {
					Some(conflicting_txids.clone())
				};
				// Leave the splice intent unchanged: it is owned by the splice entry points and the
				// retrier, never by a payment-tracking merge. Emitting the current value here would
				// let an `insert_or_update` of a payment record (e.g. from wallet sync, built without
				// an intent) clobber a live intent to `None`.
				let _ = splice_intent;
				Self {
					id: details.id,
					payment_update: Some(details.to_update()),
					conflicting_txids,
					candidates: candidates.clone(),
					splice_intent: None,
				}
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::payment::store::ConfirmationStatus;
	use crate::payment::{PaymentDirection, PaymentKind, PaymentStatus};
	use bitcoin::hashes::Hash;
	use lightning::util::ser::{Readable, Writeable};

	#[test]
	fn pending_payment_candidate_lookup() {
		let payment_id = PaymentId([1u8; 32]);
		let first_txid = Txid::from_byte_array([2u8; 32]);
		let rbf_txid = Txid::from_byte_array([3u8; 32]);

		// A leading counterparty-initiated round we didn't contribute to (no figures), then our own
		// original and RBF candidates.
		let counterparty_txid = Txid::from_byte_array([4u8; 32]);
		let candidates = vec![
			FundingTxCandidate { txid: counterparty_txid, amount_msat: None, fee_paid_msat: None },
			FundingTxCandidate {
				txid: first_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(1_000),
			},
			FundingTxCandidate {
				txid: rbf_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(5_000),
			},
		];

		// The stored details only need to be a valid funding payment; `candidate` resolves figures
		// purely from the recorded candidate list.
		let details = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid: rbf_txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: None,
			},
			Some(1_000_000),
			Some(5_000),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		);
		let pending =
			PendingPaymentDetails::new(details, vec![first_txid, counterparty_txid], candidates);

		// Each candidate resolves to its own figures, so a non-last candidate that confirms reports
		// its own (lower) fee rather than the last-broadcast candidate's.
		assert_eq!(pending.candidate(first_txid).and_then(|c| c.fee_paid_msat), Some(1_000));
		assert_eq!(pending.candidate(rbf_txid).and_then(|c| c.fee_paid_msat), Some(5_000));
		// A candidate we didn't contribute to carries no figures, so the payment reports `None`
		// rather than another candidate's stale figures.
		let counterparty = pending.candidate(counterparty_txid).expect("candidate is recorded");
		assert_eq!(counterparty.amount_msat, None);
		assert_eq!(counterparty.fee_paid_msat, None);
		assert_eq!(pending.candidate(Txid::from_byte_array([9u8; 32])), None);
	}

	fn test_txid(byte: u8) -> Txid {
		Txid::from_byte_array([byte; 32])
	}

	fn pending_onchain_payment(payment_id: PaymentId, txid: Txid) -> PaymentDetails {
		PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain { txid, status: ConfirmationStatus::Unconfirmed, tx_type: None },
			Some(1_000),
			Some(100),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	#[test]
	fn pending_onchain_conflicts_exclude_current_txid_after_txid_rotation() {
		let original_txid = test_txid(1);
		let replacement_txid = test_txid(2);
		let payment_id = PaymentId(original_txid.to_byte_array());

		let mut pending_payment = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, replacement_txid),
			vec![original_txid],
			Vec::new(),
		);
		let update = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, original_txid),
			Vec::new(),
			Vec::new(),
		)
		.to_update();

		assert!(pending_payment.update(update));
		assert_eq!(
			pending_payment.conflicting_txids(),
			Vec::<Txid>::new(),
			"current txid must not remain in its own conflict list"
		);
	}

	#[test]
	fn splice_kind_round_trips() {
		for kind in [
			SpliceKind::In { amount_sats: 500_000 },
			SpliceKind::Out {
				outputs: vec![TxOut {
					value: bitcoin::Amount::from_sat(400_000),
					script_pubkey: bitcoin::ScriptBuf::new(),
				}],
			},
			SpliceKind::Rbf {},
		] {
			let encoded = kind.encode();
			let decoded = SpliceKind::read(&mut &encoded[..]).unwrap();
			assert_eq!(kind, decoded);
		}
	}

	#[test]
	fn tracked_payment_round_trips() {
		// A `PendingSplice` record round-trips through the restart integration tests, which persist a
		// real `FundingContribution`; here we cover the `Tracked` variant and its enum discriminant.
		let payment_id = PaymentId([7u8; 32]);
		let txid = Txid::from_byte_array([8u8; 32]);
		let record = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, txid),
			vec![Txid::from_byte_array([9u8; 32])],
			vec![FundingTxCandidate { txid, amount_msat: Some(1_000), fee_paid_msat: Some(100) }],
		);

		let encoded = record.encode();
		let decoded = PendingPaymentDetails::read(&mut &encoded[..]).unwrap();
		assert_eq!(record, decoded);
		assert_eq!(decoded.id(), payment_id);
		assert!(decoded.details().is_some());
	}
}
