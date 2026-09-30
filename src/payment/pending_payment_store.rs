// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::Txid;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::PaymentId;

use crate::data_store::{StorableObject, StorableObjectUpdate, UpdatableObject};
use crate::payment::store::{Channel, PaymentDetailsUpdate, TransactionType};
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
	/// Whether this node signed the candidate but LDK has yet to report the round negotiated. Set
	/// when the round is recorded at signing time, cleared when LDK reports the splice negotiated
	/// (`SpliceNegotiated`, emitted only once our `tx_signatures` for the round are ready to send).
	/// Such a round may be abandoned without a trace — the counterparty aborts, or the channel
	/// closes, before the signatures are exchanged — so only such a round may be dropped from the
	/// history, and only once LDK no longer holds it.
	pub awaiting_broadcast: bool,
}

impl_writeable_tlv_based!(FundingTxCandidate, {
	(0, txid, required),
	(2, amount_msat, option),
	(4, fee_paid_msat, option),
	(6, awaiting_broadcast, required),
});

/// A pending payment tracked by LDK Node, keyed by [`PaymentId`].
///
/// Each part of an entry is written by a different subsystem and is present on its own schedule,
/// so all of them are optional. Signing a round of an interactive funding adds the round to
/// `candidates` and names the `funding_channels` it belongs to, still without a transaction anyone
/// has seen; wallet sync adds `details` once it observes the transaction, and records
/// `conflicting_txids` for any wallet transaction. A splice uses all of them; the fields do not
/// partition by payment type. An entry holding none of them tracks nothing and is removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetails {
	/// The payment this entry tracks.
	pub id: PaymentId,
	/// The full payment details, or `None` for a splice whose transaction wallet sync has yet to
	/// observe — including one this node has signed but nothing has broadcast.
	pub details: Option<PaymentDetails>,
	/// Transaction IDs wallet sync observed to have replaced or to conflict with this
	/// payment, used to map later events about those txids back to this record. This is
	/// BDK's view, distinct from `candidates`: it can hold conflicts that were never
	/// negotiated candidates, while a candidate replaced between wallet syncs may never
	/// appear here (it gets no `TxReplaced` event of its own).
	pub conflicting_txids: Vec<Txid>,
	/// The channels whose interactive funding `candidates` are rounds of, as the signing of a
	/// round named them. Empty for a non-funding payment and for a record wallet sync created
	/// on its own, whose channels its classification names instead.
	pub funding_channels: Vec<Channel>,
	/// For interactive funding (splices), this node's per-candidate funding figures across the
	/// RBF history, keyed by each candidate's txid and recorded as each round is signed.
	/// Empty for non-funding payments.
	pub candidates: Vec<FundingTxCandidate>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, candidates: Vec<FundingTxCandidate>,
	) -> Self {
		Self {
			id: details.id,
			details: Some(details),
			conflicting_txids,
			funding_channels: Vec::new(),
			candidates,
		}
	}

	/// An entry for the rounds of an interactive funding of `funding_channels` this node has
	/// signed, before any transaction of it has been observed and therefore before a payment
	/// record for it exists.
	pub(crate) fn signed_rounds(
		id: PaymentId, funding_channels: Vec<Channel>, candidates: Vec<FundingTxCandidate>,
	) -> Self {
		Self { id, details: None, conflicting_txids: Vec::new(), funding_channels, candidates }
	}

	/// The full payment details, or `None` for a splice whose transaction has not been observed.
	pub(crate) fn details(&self) -> Option<&PaymentDetails> {
		self.details.as_ref()
	}

	/// Transaction IDs that have replaced or conflict with this payment.
	pub(crate) fn conflicting_txids(&self) -> &[Txid] {
		&self.conflicting_txids
	}

	/// Returns this node's recorded funding figures for the candidate with the given txid, if any.
	pub(crate) fn candidate(&self, txid: Txid) -> Option<&FundingTxCandidate> {
		self.candidates.iter().find(|candidate| candidate.txid == txid)
	}

	/// This node's recorded funding figures across the candidate history, in LDK's order; empty
	/// for a splice without a signed round yet and for non-funding payments.
	pub(crate) fn candidates(&self) -> &[FundingTxCandidate] {
		&self.candidates
	}

	/// The channels of the interactive funding this entry tracks: those the signing of a round
	/// named, else those its classification names.
	pub(crate) fn funding_channels(&self) -> &[Channel] {
		if !self.funding_channels.is_empty() {
			return &self.funding_channels;
		}
		match self.details.as_ref().map(|details| &details.kind) {
			Some(PaymentKind::Onchain {
				tx_type: Some(TransactionType::InteractiveFunding { channels }),
				..
			}) => channels,
			_ => &[],
		}
	}

	/// Whether this entry tracks nothing anymore and can be dropped.
	pub(crate) fn is_empty(&self) -> bool {
		self.details.is_none() && self.candidates.is_empty()
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, id, required),
	(2, details, option),
	(4, conflicting_txids, optional_vec),
	(6, funding_channels, optional_vec),
	(8, candidates, optional_vec),
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

	fn id(&self) -> Self::Id {
		self.id
	}
}

impl UpdatableObject for PendingPaymentDetails {
	type Update = PendingPaymentDetailsUpdate;

	fn update(&mut self, update: Self::Update) -> bool {
		let mut updated = false;

		// Update the underlying payment details if present. An entry with no record yet is not
		// given one here: only the writer that observed the transaction knows what the record
		// says, and it sets the field directly.
		if let (Some(payment_update), Some(details)) =
			(update.payment_update, self.details.as_mut())
		{
			updated |= details.update(payment_update);
		}

		if let Some(new_conflicting_txids) = update.conflicting_txids {
			if self.conflicting_txids != new_conflicting_txids {
				self.conflicting_txids = new_conflicting_txids;
				updated = true;
			}
		}

		if let Some(PaymentKind::Onchain { txid, .. }) =
			self.details.as_ref().map(|details| &details.kind)
		{
			let txid = *txid;
			let conflicts_len = self.conflicting_txids.len();
			self.conflicting_txids.retain(|conflicting_txid| *conflicting_txid != txid);
			updated |= self.conflicting_txids.len() != conflicts_len;
		}

		// Each funding-record write passes the candidate history as of its own round, so a
		// non-empty update replaces the stored list. An empty update (e.g. a non-funding
		// payment) leaves it untouched. Dropping an abandoned round, the only writer that
		// shrinks it, goes through the store's `mutate` instead.
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
		match &value.details {
			// An entry with no record yet carries nothing a payment-tracking merge could apply.
			None => Self {
				id: value.id,
				payment_update: None,
				conflicting_txids: None,
				candidates: value.candidates.clone(),
			},
			Some(details) => {
				let conflicting_txids = if value.conflicting_txids.is_empty() {
					None
				} else {
					Some(value.conflicting_txids.clone())
				};
				Self {
					id: details.id,
					payment_update: Some(details.to_update()),
					conflicting_txids,
					candidates: value.candidates.clone(),
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
	use lightning::util::ser::Writeable;
	let mut records = vec![1, 8]; // (1, estimated_fee)
	records.extend_from_slice(&estimated_fee_sat.to_be_bytes());
	if !outputs.is_empty() {
		let mut output_bytes = Vec::new();
		for output in outputs {
			output.write(&mut output_bytes).expect("in-memory write must succeed");
		}
		records.push(5); // (5, outputs)
		records.push(u8::try_from(output_bytes.len()).expect("test outputs must stay small"));
		records.extend_from_slice(&output_bytes);
	}
	records.extend_from_slice(&[9, 8]); // (9, feerate)
	records.extend_from_slice(&feerate.to_be_bytes());
	records.extend_from_slice(&[11, 8]); // (11, max_feerate)
	records.extend_from_slice(&feerate.to_be_bytes());
	records.extend_from_slice(&[13, 1, 1]); // (13, is_splice: true)
										 // BigSize length prefix over the TLV records above; single-byte as long as they stay short.
	let mut tlv_bytes = vec![u8::try_from(records.len()).expect("test TLV stream must stay small")];
	tlv_bytes.extend(records);
	lightning::util::ser::Readable::read(&mut &tlv_bytes[..])
		.expect("hand-built TLV stream must decode")
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;

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
}
