// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::secp256k1::PublicKey;
use bitcoin::{TxOut, Txid};
use lightning::chain::transaction::OutPoint as LdkOutPoint;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectUpdate};
use crate::payment::store::PaymentDetailsUpdate;
use crate::payment::{PaymentDetails, PaymentKind};

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
	/// Whether this node signed the candidate but the signatures have yet to be exchanged. Set
	/// when the round is recorded at signing time, cleared when LDK reports the splice negotiated
	/// (`SpliceNegotiated`, emitted as it hands the fully signed round to the broadcaster). Only
	/// such a round can be abandoned without a trace — the counterparty aborts, or the channel
	/// closes, before the signatures are exchanged — so only such a round may be dropped from the
	/// history.
	pub awaiting_broadcast: bool,
}

impl_writeable_tlv_based!(FundingTxCandidate, {
	(0, txid, required),
	(2, amount_msat, option),
	(4, fee_paid_msat, option),
	(6, awaiting_broadcast, required),
});

/// The parameters of the API call that initiated a splice, recording what was attempted
/// independently of the contribution built from them.
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
/// node). Until the new funding transaction locks we keep enough state to recognize a splice LDK
/// no longer knows about and to describe events about it in terms of the original request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpliceIntent {
	/// The channel counterparty.
	pub counterparty_node_id: PublicKey,
	/// The channel being spliced.
	pub channel_id: ChannelId,
	/// The channel's funding outpoint when the splice was initiated. It only changes once a splice
	/// locks, so a mismatch with the channel's current funding outpoint means the splice (or a
	/// replacement) completed and the intent is stale.
	pub pre_splice_funding_txo: LdkOutPoint,
	/// The contribution handed to [`ChannelManager::funding_contributed`], kept to match later
	/// events about the splice back to this intent.
	///
	/// [`ChannelManager::funding_contributed`]: lightning::ln::channelmanager::ChannelManager::funding_contributed
	pub contribution: FundingContribution,
	/// The parameters of the originating API call.
	pub kind: SpliceKind,
}

impl_writeable_tlv_based!(SpliceIntent, {
	(0, counterparty_node_id, required),
	(2, channel_id, required),
	(4, pre_splice_funding_txo, required),
	(6, contribution, required),
	(8, kind, required),
});

/// A pending payment tracked by LDK Node, keyed by [`PaymentId`].
///
/// A user-initiated splice is persisted as a [`PendingSplice`] before its contribution is handed
/// to LDK — at which point no funding transaction, and therefore no [`PaymentDetails`], exists yet.
/// Once the splice is recorded as a funding payment it becomes a [`Tracked`] payment carrying the
/// real [`PaymentDetails`], while retaining its [`SpliceIntent`] until the splice locks.
///
/// [`PendingSplice`]: Self::PendingSplice
/// [`Tracked`]: Self::Tracked
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingPaymentDetails {
	/// A user-initiated splice persisted before hand-off to LDK; no funding transaction exists yet.
	/// Keyed by the generated [`PaymentId`]; never mirrored into the payment store.
	PendingSplice { id: PaymentId, intent: SpliceIntent },
	/// A pending payment tracked toward confirmation, optionally still carrying a live splice
	/// intent until the splice locks.
	///
	/// Each field is written by a different subsystem: wallet sync records `conflicting_txids`
	/// for any wallet transaction (splice fundings included), the signing-time recording
	/// records `candidates` for interactive funding, the `ChannelReady` arm records
	/// `locked_rounds`, and `splice_intent` is owned by the splice entry points and the splice
	/// tracker — persisted at splice initiation, carried over from a [`PendingSplice`] record
	/// when the payment is promoted, and cleared once the splice locks or its failure is
	/// surfaced. A splice uses all of them; the fields do not partition by payment type.
	///
	/// [`PendingSplice`]: Self::PendingSplice
	Tracked {
		/// The full payment details.
		details: PaymentDetails,
		/// Transaction IDs wallet sync observed to have replaced or to conflict with this
		/// payment, used to map later events about those txids back to this record. This is
		/// BDK's view, distinct from `candidates`: it can hold conflicts that were never
		/// negotiated candidates, while a candidate replaced between wallet syncs may never
		/// appear here (it gets no `TxReplaced` event of its own).
		conflicting_txids: Vec<Txid>,
		/// For interactive funding (splices), this node's per-candidate funding figures across the
		/// RBF history, keyed by each candidate's txid and recorded as each round is signed.
		/// Empty for non-funding payments.
		candidates: Vec<FundingTxCandidate>,
		/// The live splice intent, or `None` for a non-splice payment or a splice that has
		/// locked. It lives here as well as on
		/// [`PendingSplice`] because a fee bump — a fresh negotiation LDK likewise abandons if the
		/// peer disconnects before signing — would share the broadcast splice's record rather than
		/// get one of its own.
		///
		/// [`PendingSplice`]: Self::PendingSplice
		splice_intent: Option<SpliceIntent>,
		/// The candidates LDK promoted to the channel's funding, as `ChannelReady` reported them.
		/// A zero-conf splice locks before its transaction confirms, and every later splice builds
		/// on it, so such a round can still confirm once the channel's funding has moved on from
		/// it and once the channel has closed, when LDK holds it no longer. Kept apart from the
		/// candidates, which each funding-record write replaces as a whole.
		locked_rounds: Vec<Txid>,
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
		Self::Tracked {
			details,
			conflicting_txids,
			candidates,
			splice_intent,
			locked_rounds: Vec::new(),
		}
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

	/// The rounds LDK promoted to the channel's funding, as `ChannelReady` reported them; empty
	/// for a splice without a funding transaction yet.
	pub(crate) fn locked_rounds(&self) -> &[Txid] {
		match self {
			Self::PendingSplice { .. } => &[],
			Self::Tracked { locked_rounds, .. } => locked_rounds,
		}
	}

	/// Records that LDK promoted the round with the given txid to the channel's funding. Returns
	/// whether the record changed: a round recorded as promoted already, or a splice without a
	/// funding transaction yet, leaves it as it is.
	pub(crate) fn record_locked_round(&mut self, txid: Txid) -> bool {
		match self {
			Self::PendingSplice { .. } => false,
			Self::Tracked { locked_rounds, .. } => {
				if locked_rounds.contains(&txid) {
					return false;
				}
				locked_rounds.push(txid);
				true
			},
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

	/// This node's recorded funding figures across the candidate history, in LDK's order; empty for
	/// a splice without a funding transaction yet and for non-funding payments.
	pub(crate) fn candidates(&self) -> &[FundingTxCandidate] {
		match self {
			Self::PendingSplice { .. } => &[],
			Self::Tracked { candidates, .. } => candidates,
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
		(8, locked_rounds, optional_vec),
	},
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub candidates: Vec<FundingTxCandidate>,
	/// The splice intent to set (`Some(Some(..))`) or clear (`Some(None)`), or `None` to leave it
	/// unchanged. Setting it on a [`PendingPaymentDetails::PendingSplice`] replaces the intent;
	/// clearing a pre-broadcast splice is done by removing the record, not through this field.
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
				// A pre-broadcast record only carries a splice intent; the only meaningful update
				// is replacing that intent. Clearing it is done by removing the record.
				if let Some(Some(new_intent)) = update.splice_intent {
					if *intent != new_intent {
						*intent = new_intent;
						return true;
					}
				}
				false
			},
			Self::Tracked { details, conflicting_txids, candidates, splice_intent, .. } => {
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

				// Each funding-record write passes the candidate history as of its own round, so a
				// non-empty update replaces the stored list. An empty update (e.g. a non-funding
				// payment) leaves it untouched. Dropping an abandoned round, the only writer that
				// shrinks it, goes through the store's `mutate` instead.
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
				..
			} => {
				let conflicting_txids = if conflicting_txids.is_empty() {
					None
				} else {
					Some(conflicting_txids.clone())
				};
				// Leave the splice intent unchanged: it is owned by the splice entry points and the
				// splice tracker, never by a payment-tracking merge. Emitting the current value
				// here would let an `insert_or_update` of a payment record (e.g. from wallet sync,
				// built without an intent) clobber a live intent to `None`.
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

/// Builds a [`FundingContribution`] for tests through its `Readable` impl — the only path open
/// outside `rust-lightning`, which keeps its builder private. The length-prefixed stream holds
/// the required TLV records (the given estimated fee in satoshis, feerate, max feerate, and the
/// is-splice flag) plus the given contributed outputs.
///
/// [`FundingContribution`]: lightning::ln::funding::FundingContribution
#[cfg(test)]
pub(crate) fn test_funding_contribution_with_outputs(
	estimated_fee_sat: u64, feerate: u64, outputs: &[bitcoin::TxOut],
) -> lightning::ln::funding::FundingContribution {
	test_funding_contribution_with_parts(estimated_fee_sat, feerate, &[], outputs, None)
}

/// Builds a [`FundingContribution`] for tests from its parts: the given estimated fee, an input
/// spending output 0 — which must be P2WPKH — of each given previous transaction, the given
/// contributed outputs and change output, and the given input-selection feerate (also used as
/// the maximum), with the is-splice flag set.
///
/// [`FundingContribution`]: lightning::ln::funding::FundingContribution
#[cfg(test)]
pub(crate) fn test_funding_contribution_with_parts(
	estimated_fee_sat: u64, feerate: u64, prevtxs: &[bitcoin::Transaction],
	outputs: &[bitcoin::TxOut], change_output: Option<&bitcoin::TxOut>,
) -> lightning::ln::funding::FundingContribution {
	use lightning::util::ser::{BigSize, Writeable};
	use lightning::util::wallet_utils::ConfirmedUtxo;
	let mut records = vec![1, 8]; // (1, estimated_fee)
	records.extend_from_slice(&estimated_fee_sat.to_be_bytes());
	if !prevtxs.is_empty() {
		let mut input_bytes = Vec::new();
		for prevtx in prevtxs {
			ConfirmedUtxo::new_p2wpkh(prevtx.clone(), 0)
				.expect("test prevtx output 0 must be P2WPKH")
				.write(&mut input_bytes)
				.expect("in-memory write must succeed");
		}
		records.push(3); // (3, inputs)
		BigSize(input_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend_from_slice(&input_bytes);
	}
	if !outputs.is_empty() {
		let mut output_bytes = Vec::new();
		for output in outputs {
			output.write(&mut output_bytes).expect("in-memory write must succeed");
		}
		records.push(5); // (5, outputs)
		BigSize(output_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend_from_slice(&output_bytes);
	}
	if let Some(change_output) = change_output {
		let change_bytes = change_output.encode();
		records.push(7); // (7, change_output)
		BigSize(change_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend_from_slice(&change_bytes);
	}
	records.extend_from_slice(&[9, 8]); // (9, feerate)
	records.extend_from_slice(&feerate.to_be_bytes());
	records.extend_from_slice(&[11, 8]); // (11, max_feerate)
	records.extend_from_slice(&feerate.to_be_bytes());
	records.extend_from_slice(&[13, 1, 1]); // (13, is_splice: true)
	let mut tlv_bytes = Vec::new();
	// BigSize length prefix over the TLV records above.
	BigSize(records.len() as u64).write(&mut tlv_bytes).expect("in-memory write must succeed");
	tlv_bytes.extend(records);
	lightning::util::ser::Readable::read(&mut &tlv_bytes[..])
		.expect("hand-built TLV stream must decode")
}

/// Builds a [`FundingContribution`] for tests carrying just the required TLV records: a zero
/// estimated fee, the default feerate, and no contributed outputs.
///
/// [`FundingContribution`]: lightning::ln::funding::FundingContribution
#[cfg(test)]
pub(crate) fn test_funding_contribution() -> lightning::ln::funding::FundingContribution {
	test_funding_contribution_with_feerate(253)
}

/// Like [`test_funding_contribution`], but with the given input-selection feerate in sat/kwu.
#[cfg(test)]
pub(crate) fn test_funding_contribution_with_feerate(
	feerate: u64,
) -> lightning::ln::funding::FundingContribution {
	test_funding_contribution_with_outputs(0, feerate, &[])
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use lightning::util::ser::{Readable, Writeable};

	use super::*;
	use crate::payment::store::ConfirmationStatus;
	use crate::payment::{PaymentDirection, PaymentKind, PaymentStatus};

	#[test]
	fn pending_payment_candidate_lookup() {
		let payment_id = PaymentId([1u8; 32]);
		let first_txid = Txid::from_byte_array([2u8; 32]);
		let rbf_txid = Txid::from_byte_array([3u8; 32]);

		// A leading counterparty-initiated round we didn't contribute to (no figures), then our own
		// original and RBF candidates.
		let counterparty_txid = Txid::from_byte_array([4u8; 32]);
		let candidates = vec![
			FundingTxCandidate {
				txid: counterparty_txid,
				amount_msat: None,
				fee_paid_msat: None,
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: first_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(1_000),
				awaiting_broadcast: false,
			},
			FundingTxCandidate {
				txid: rbf_txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(5_000),
				awaiting_broadcast: false,
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
	fn funding_classification_pending_update_preserves_mirrored_confirmation() {
		use bitcoin::BlockHash;

		use crate::payment::store::PaymentDetailsUpdate;

		let txid = test_txid(7);
		let payment_id = PaymentId(txid.to_byte_array());

		// A pending entry wallet sync has already mirrored a confirmation into (via
		// `apply_funding_status_update_locked`) before classification ran.
		let confirmed_details = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Confirmed {
					block_hash: BlockHash::from_byte_array([8u8; 32]),
					height: 100,
					timestamp: 1,
				},
				tx_type: None,
			},
			Some(2_000_000),
			Some(999),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		);
		let mirrored = PendingPaymentDetails::new(confirmed_details, Vec::new(), Vec::new());

		// A fresh classification is always Unconfirmed and carries the candidate history; its
		// figures are the active candidate's.
		let fresh = pending_onchain_payment(payment_id, txid);
		let candidates = vec![FundingTxCandidate {
			txid,
			amount_msat: fresh.amount_msat,
			fee_paid_msat: fresh.fee_paid_msat,
			awaiting_broadcast: false,
		}];

		// The old fresh-insert path merged the full fresh record, downgrading the mirrored
		// confirmation.
		let mut downgraded = mirrored.clone();
		let full_update =
			PendingPaymentDetails::new(fresh.clone(), Vec::new(), candidates.clone()).to_update();
		assert!(downgraded.update(full_update));
		assert!(
			matches!(
				downgraded.details().expect("tracked").kind,
				PaymentKind::Onchain { status: ConfirmationStatus::Unconfirmed, .. }
			),
			"a full merge of a fresh classification downgrades a mirrored confirmation",
		);

		// The narrow classification update merges the candidates while preserving the
		// confirmation state wallet sync owns. It names the confirmed txid, so its
		// contribution-derived figures replace the mirrored wallet-view ones.
		let mut merged = mirrored.clone();
		let narrow_update = PendingPaymentDetailsUpdate {
			id: payment_id,
			payment_update: Some(PaymentDetailsUpdate::funding_reclassification(fresh)),
			conflicting_txids: None,
			candidates: candidates.clone(),
			splice_intent: None,
		};
		assert!(merged.update(narrow_update));
		let merged_details = merged.details().expect("tracked");
		assert!(
			matches!(
				merged_details.kind,
				PaymentKind::Onchain { status: ConfirmationStatus::Confirmed { .. }, .. }
			),
			"a narrow classification update must not downgrade a mirrored confirmation",
		);
		assert_eq!(merged.candidate(txid), Some(&candidates[0]));
		assert_eq!(merged_details.amount_msat, Some(1_000));
		assert_eq!(merged_details.fee_paid_msat, Some(100));
	}

	fn test_intent() -> SpliceIntent {
		use std::str::FromStr;

		SpliceIntent {
			counterparty_node_id: PublicKey::from_str(
				"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
			)
			.unwrap(),
			channel_id: ChannelId([11u8; 32]),
			pre_splice_funding_txo: LdkOutPoint { txid: test_txid(12), index: 0 },
			contribution: test_funding_contribution(),
			kind: SpliceKind::In { amount_sats: 500_000 },
		}
	}

	#[test]
	fn payment_tracking_merge_preserves_a_live_splice_intent() {
		let payment_id = PaymentId([7u8; 32]);
		let txid = test_txid(8);
		let intent = test_intent();
		let mut record = PendingPaymentDetails::tracked(
			pending_onchain_payment(payment_id, txid),
			Vec::new(),
			Vec::new(),
			Some(intent.clone()),
		);

		// Wallet sync merges its view of a transaction through `to_update()` of a fresh record,
		// which is built without an intent; the merge must leave the live intent in place.
		let fresh = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, txid),
			vec![test_txid(9)],
			Vec::new(),
		);
		assert!(record.update(fresh.to_update()));
		assert_eq!(record.splice_intent(), Some(&intent));
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
	fn pending_splice_round_trips() {
		use std::str::FromStr;

		let id = PaymentId([10u8; 32]);
		let intent = SpliceIntent {
			counterparty_node_id: PublicKey::from_str(
				"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
			)
			.unwrap(),
			channel_id: ChannelId([11u8; 32]),
			pre_splice_funding_txo: LdkOutPoint { txid: test_txid(12), index: 0 },
			contribution: test_funding_contribution(),
			kind: SpliceKind::In { amount_sats: 500_000 },
		};
		let record = PendingPaymentDetails::PendingSplice { id, intent };

		let encoded = record.encode();
		let decoded = PendingPaymentDetails::read(&mut &encoded[..]).unwrap();
		assert_eq!(record, decoded);
		assert_eq!(decoded.id(), id);
		assert!(decoded.details().is_none());
	}

	#[test]
	fn tracked_payment_round_trips() {
		// The `PendingSplice` variant round-trips in `pending_splice_round_trips`; here we cover
		// the `Tracked` variant and its enum discriminant.
		let payment_id = PaymentId([7u8; 32]);
		let txid = Txid::from_byte_array([8u8; 32]);
		let record = PendingPaymentDetails::new(
			pending_onchain_payment(payment_id, txid),
			vec![Txid::from_byte_array([9u8; 32])],
			vec![FundingTxCandidate {
				txid,
				amount_msat: Some(1_000),
				fee_paid_msat: Some(100),
				awaiting_broadcast: false,
			}],
		);

		let encoded = record.encode();
		let decoded = PendingPaymentDetails::read(&mut &encoded[..]).unwrap();
		assert_eq!(record, decoded);
		assert_eq!(decoded.id(), payment_id);
		assert!(decoded.details().is_some());
	}

	/// A candidate with the given txid byte, with a stake of ours in it if `ours`.
	fn candidate(txid_byte: u8, ours: bool) -> FundingTxCandidate {
		FundingTxCandidate {
			txid: test_txid(txid_byte),
			amount_msat: ours.then_some(1_000),
			fee_paid_msat: ours.then_some(100),
			awaiting_broadcast: false,
		}
	}

	fn entry(candidates: Vec<FundingTxCandidate>) -> PendingPaymentDetails {
		let payment_id = PaymentId([1u8; 32]);
		let txid = candidates.last().expect("at least one candidate").txid;
		PendingPaymentDetails::new(pending_onchain_payment(payment_id, txid), vec![], candidates)
	}

	/// The rounds LDK promoted round-trip with the entry, absent or present, and the merge of a
	/// record's full update, as wallet sync writes it, leaves them.
	#[test]
	fn locked_rounds_round_trip_and_survive_a_merge() {
		let mut stored = entry(vec![candidate(2, false)]);
		let decoded: PendingPaymentDetails =
			Readable::read(&mut &stored.encode()[..]).expect("encoding must round-trip");
		assert!(decoded.locked_rounds().is_empty());

		assert!(stored.record_locked_round(test_txid(2)));
		assert!(!stored.record_locked_round(test_txid(2)));
		let decoded: PendingPaymentDetails =
			Readable::read(&mut &stored.encode()[..]).expect("encoding must round-trip");
		assert_eq!(decoded, stored);

		let synced = entry(vec![candidate(2, false), candidate(3, false)]);
		assert!(stored.update(synced.to_update()));
		assert_eq!(stored.candidates().len(), 2);
		assert_eq!(stored.locked_rounds(), &[test_txid(2)]);
	}
}
