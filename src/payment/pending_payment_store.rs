// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::Txid;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::PaymentId;

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

/// Represents a pending payment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPaymentDetails {
	/// The full payment details
	pub details: PaymentDetails,
	/// Transaction IDs that have replaced or conflict with this payment.
	pub conflicting_txids: Vec<Txid>,
	/// For interactive funding (splices), this node's per-candidate funding figures across the
	/// RBF history, keyed by each candidate's txid. Empty for non-funding payments and for
	/// records written before per-candidate tracking existed.
	pub(crate) candidates: Vec<FundingTxCandidate>,
	/// The candidates LDK promoted to the channel's funding, as `ChannelReady` reported them. A
	/// zero-conf splice locks before its transaction confirms, and every later splice builds on
	/// it, so such a round can still confirm once the channel's funding has moved on from it and
	/// once the channel has closed, when LDK holds it no longer. Kept apart from the candidates,
	/// which each funding-record write replaces as a whole.
	pub(crate) locked_rounds: Vec<Txid>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, candidates: Vec<FundingTxCandidate>,
	) -> Self {
		Self { details, conflicting_txids, candidates, locked_rounds: Vec::new() }
	}

	/// Returns this node's recorded funding figures for the candidate with the given txid, if any.
	pub(crate) fn candidate(&self, txid: Txid) -> Option<&FundingTxCandidate> {
		self.candidates.iter().find(|candidate| candidate.txid == txid)
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
	(4, candidates, optional_vec),
	(6, locked_rounds, optional_vec),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub candidates: Vec<FundingTxCandidate>,
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

		if let PaymentKind::Onchain { txid, .. } = &self.details.kind {
			let conflicts_len = self.conflicting_txids.len();
			self.conflicting_txids.retain(|conflicting_txid| conflicting_txid != txid);
			updated |= self.conflicting_txids.len() != conflicts_len;
		}

		// Each funding-record write passes the candidate history as of its own round, so a
		// non-empty update replaces the stored list. An empty update (e.g. a non-funding payment)
		// leaves it untouched. Dropping an abandoned round, the only writer that shrinks it, goes
		// through the store's `mutate` instead.
		if !update.candidates.is_empty() && self.candidates != update.candidates {
			self.candidates = update.candidates;
			updated = true;
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
			candidates: value.candidates.clone(),
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
			pending_payment.conflicting_txids,
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
				downgraded.details.kind,
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
		};
		assert!(merged.update(narrow_update));
		assert!(
			matches!(
				merged.details.kind,
				PaymentKind::Onchain { status: ConfirmationStatus::Confirmed { .. }, .. }
			),
			"a narrow classification update must not downgrade a mirrored confirmation",
		);
		assert_eq!(merged.candidates, candidates);
		assert_eq!(merged.details.amount_msat, Some(1_000));
		assert_eq!(merged.details.fee_paid_msat, Some(100));
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
		assert_eq!(decoded.locked_rounds, Vec::<Txid>::new());

		stored.locked_rounds.push(test_txid(2));
		let decoded: PendingPaymentDetails =
			Readable::read(&mut &stored.encode()[..]).expect("encoding must round-trip");
		assert_eq!(decoded, stored);

		let synced = entry(vec![candidate(2, false), candidate(3, false)]);
		assert!(stored.update(synced.to_update()));
		assert_eq!(stored.candidates.len(), 2);
		assert_eq!(stored.locked_rounds, vec![test_txid(2)]);
	}
}
