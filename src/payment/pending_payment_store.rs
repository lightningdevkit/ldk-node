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
/// Each part of an entry is written by a different subsystem and is present on its own schedule,
/// so all of them are optional. A user-initiated splice is persisted with nothing but its
/// [`SpliceIntent`] before its contribution is handed to LDK; signing a round of it adds the
/// round to `candidates` and names the `funding_channels` it belongs to, still without a
/// transaction anyone has seen; wallet sync adds `details` once it observes the transaction, and
/// records `conflicting_txids` for any wallet transaction; the `ChannelReady` arm records
/// `locked_rounds`. A splice uses all of them; the fields do not partition by payment type. An
/// entry holding none of them tracks nothing and is removed.
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
	/// The live splice intent, or `None` for a non-splice payment or a splice that has
	/// locked. It is owned by the splice entry points and the splice tracker — persisted at
	/// splice initiation and cleared once the splice locks or its failure is surfaced — and
	/// outlives the rounds negotiated under it, because a fee bump is a fresh negotiation LDK
	/// likewise abandons if the peer disconnects before signing, and shares the bumped round's
	/// record rather than getting one of its own.
	pub splice_intent: Option<SpliceIntent>,
	/// The candidates LDK promoted to the channel's funding, as `ChannelReady` reported them.
	/// A zero-conf splice locks before its transaction confirms, and every later splice builds
	/// on it, so such a round can still confirm once the channel's funding has moved on from
	/// it and once the channel has closed, when LDK holds it no longer. Kept apart from the
	/// candidates, which each funding-record write replaces as a whole.
	pub locked_rounds: Vec<Txid>,
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
		Self {
			id: details.id,
			details: Some(details),
			conflicting_txids,
			funding_channels: Vec::new(),
			candidates,
			splice_intent,
			locked_rounds: Vec::new(),
		}
	}

	pub(crate) fn pending_splice(id: PaymentId, intent: SpliceIntent) -> Self {
		Self {
			id,
			details: None,
			conflicting_txids: Vec::new(),
			funding_channels: Vec::new(),
			candidates: Vec::new(),
			splice_intent: Some(intent),
			locked_rounds: Vec::new(),
		}
	}

	/// An entry for the rounds of an interactive funding of `funding_channels` this node has
	/// signed, before any transaction of it has been observed and therefore before a payment
	/// record for it exists.
	pub(crate) fn signed_rounds(
		id: PaymentId, funding_channels: Vec<Channel>, candidates: Vec<FundingTxCandidate>,
		splice_intent: Option<SpliceIntent>,
	) -> Self {
		Self {
			id,
			details: None,
			conflicting_txids: Vec::new(),
			funding_channels,
			candidates,
			splice_intent,
			locked_rounds: Vec::new(),
		}
	}

	/// The full payment details, or `None` for a splice whose transaction has not been observed.
	pub(crate) fn details(&self) -> Option<&PaymentDetails> {
		self.details.as_ref()
	}

	/// Transaction IDs that have replaced or conflict with this payment.
	pub(crate) fn conflicting_txids(&self) -> &[Txid] {
		&self.conflicting_txids
	}

	/// The rounds LDK promoted to the channel's funding, as `ChannelReady` reported them.
	pub(crate) fn locked_rounds(&self) -> &[Txid] {
		&self.locked_rounds
	}

	/// Records that LDK promoted the round with the given txid to the channel's funding. Returns
	/// whether the record changed: a round recorded as promoted already leaves it as it is.
	pub(crate) fn record_locked_round(&mut self, txid: Txid) -> bool {
		if self.locked_rounds.contains(&txid) {
			return false;
		}
		self.locked_rounds.push(txid);
		true
	}

	/// The splice intent this record carries, if it is a splice that has not yet locked.
	pub(crate) fn splice_intent(&self) -> Option<&SpliceIntent> {
		self.splice_intent.as_ref()
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
		self.details.is_none()
			&& self.splice_intent.is_none()
			&& self.candidates.is_empty()
			&& self.locked_rounds.is_empty()
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, id, required),
	(2, details, option),
	(4, conflicting_txids, optional_vec),
	(6, funding_channels, optional_vec),
	(8, candidates, optional_vec),
	(10, splice_intent, option),
	(12, locked_rounds, optional_vec),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub candidates: Vec<FundingTxCandidate>,
	/// The splice intent to set (`Some(Some(..))`) or clear (`Some(None)`), or `None` to leave it
	/// unchanged. Clearing the intent of an entry that tracks nothing else is done by removing the
	/// entry, not through this field.
	pub splice_intent: Option<Option<SpliceIntent>>,
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

		if let Some(new_splice_intent) = update.splice_intent {
			if self.splice_intent != new_splice_intent {
				self.splice_intent = new_splice_intent;
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
		match &value.details {
			// An entry with no record yet carries nothing a payment-tracking merge could apply
			// beyond its intent, which the entry that holds it owns outright.
			None => Self {
				id: value.id,
				payment_update: None,
				conflicting_txids: None,
				candidates: value.candidates.clone(),
				splice_intent: value.splice_intent.clone().map(Some),
			},
			Some(details) => {
				let conflicting_txids = if value.conflicting_txids.is_empty() {
					None
				} else {
					Some(value.conflicting_txids.clone())
				};
				// Leave the splice intent unchanged: it is owned by the splice entry points and the
				// splice tracker, never by a payment-tracking merge. Emitting the current value
				// here would let an `insert_or_update` of a payment record (e.g. from wallet sync,
				// built without an intent) clobber a live intent to `None`.
				Self {
					id: details.id,
					payment_update: Some(details.to_update()),
					conflicting_txids,
					candidates: value.candidates.clone(),
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
	test_funding_contribution_inheriting(
		estimated_fee_sat,
		feerate,
		prevtxs,
		outputs,
		change_output,
		&[],
		&[],
	)
}

/// Like [`test_funding_contribution_with_parts`], but recording `inherited_inputs` and
/// `inherited_output_scripts` as parts a still-pending splice attempt reserved before this
/// contribution, as LDK records them at the hand-off of a fee bump built from the round it
/// replaces: the contribution's `reserved_inputs` and `reserved_outputs` leave them out.
#[cfg(test)]
pub(crate) fn test_funding_contribution_inheriting(
	estimated_fee_sat: u64, feerate: u64, prevtxs: &[bitcoin::Transaction],
	outputs: &[bitcoin::TxOut], change_output: Option<&bitcoin::TxOut>,
	inherited_inputs: &[bitcoin::OutPoint], inherited_output_scripts: &[bitcoin::ScriptBuf],
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
	if !inherited_inputs.is_empty() || !inherited_output_scripts.is_empty() {
		// (17, pending_components): a length-prefixed TLV stream of its own.
		let mut components = Vec::new();
		if !inherited_inputs.is_empty() {
			let mut bytes = Vec::new();
			for outpoint in inherited_inputs {
				outpoint.write(&mut bytes).expect("in-memory write must succeed");
			}
			components.push(1); // (1, inputs)
			BigSize(bytes.len() as u64)
				.write(&mut components)
				.expect("in-memory write must succeed");
			components.extend(bytes);
		}
		if !inherited_output_scripts.is_empty() {
			let mut bytes = Vec::new();
			for script in inherited_output_scripts {
				script.write(&mut bytes).expect("in-memory write must succeed");
			}
			components.push(3); // (3, output_scripts)
			BigSize(bytes.len() as u64)
				.write(&mut components)
				.expect("in-memory write must succeed");
			components.extend(bytes);
		}
		let mut component_bytes = Vec::new();
		BigSize(components.len() as u64)
			.write(&mut component_bytes)
			.expect("in-memory write must succeed");
		component_bytes.extend(components);
		records.push(17);
		BigSize(component_bytes.len() as u64)
			.write(&mut records)
			.expect("in-memory write must succeed");
		records.extend(component_bytes);
	}
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

/// Like [`test_funding_contribution`], but with the given input-selection feerate in sat/kwu and
/// an input spending output 0 — which must be P2WPKH — of each given previous transaction.
#[cfg(test)]
pub(crate) fn test_funding_contribution_with_inputs(
	feerate: u64, prevtxs: &[bitcoin::Transaction],
) -> FundingContribution {
	test_funding_contribution_with_parts(0, feerate, prevtxs, &[], None)
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
		let record = PendingPaymentDetails::pending_splice(id, intent);

		let encoded = record.encode();
		let decoded = PendingPaymentDetails::read(&mut &encoded[..]).unwrap();
		assert_eq!(record, decoded);
		assert_eq!(decoded.id(), id);
		assert!(decoded.details().is_none());
	}

	/// A fee bump's contribution inherits the inputs and change of the round it replaces, which
	/// LDK records in the contribution at the hand-off so that `reserved_inputs` and
	/// `reserved_outputs` leave them out: what a failure of the bump releases, and what a retry
	/// must reserve again. That record is a private field the contribution's `PartialEq` ignores,
	/// so a persisted intent's round trip is checked through those accessors.
	#[test]
	fn pending_splice_keeps_the_contribution_reserved_parts() {
		use std::str::FromStr;

		use bitcoin::{Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, WPubkeyHash};

		let prevtx = |seed: u8| Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: bitcoin::absolute::LockTime::ZERO,
			input: vec![TxIn::default()],
			output: vec![TxOut {
				value: Amount::from_sat(10_000),
				script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([seed; 20])),
			}],
		};
		let prevtxs = [prevtx(1), prevtx(2)];
		let outpoint = |tx: &Transaction| OutPoint { txid: tx.compute_txid(), vout: 0 };
		let script = |seed: u8| ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([seed; 20]));
		let change = TxOut { value: Amount::from_sat(21_000), script_pubkey: script(9) };
		let splice_out = TxOut { value: Amount::from_sat(50_000), script_pubkey: script(8) };
		let reserved = |contribution: &FundingContribution| {
			(
				contribution.reserved_inputs().map(|input| input.outpoint()).collect::<Vec<_>>(),
				contribution.reserved_outputs().cloned().collect::<Vec<_>>(),
			)
		};

		// Without the record, every part counts as reserved.
		let plain = test_funding_contribution_with_parts(
			0,
			300,
			&prevtxs,
			&[splice_out.clone()],
			Some(&change),
		);
		assert_eq!(
			reserved(&plain),
			(prevtxs.iter().map(outpoint).collect(), vec![splice_out.clone(), change.clone()])
		);

		// The bump reuses the first input and the change address of the round it replaces; the
		// second input and the splice-out output are its own.
		let contribution = test_funding_contribution_inheriting(
			0,
			300,
			&prevtxs,
			&[splice_out.clone()],
			Some(&change),
			&[outpoint(&prevtxs[0])],
			&[change.script_pubkey.clone()],
		);
		let expected = (vec![outpoint(&prevtxs[1])], vec![splice_out]);
		assert_eq!(reserved(&contribution), expected);

		let intent = SpliceIntent {
			counterparty_node_id: PublicKey::from_str(
				"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
			)
			.unwrap(),
			channel_id: ChannelId([11u8; 32]),
			pre_splice_funding_txo: LdkOutPoint { txid: test_txid(12), index: 0 },
			contribution,
			kind: SpliceKind::Rbf {},
		};
		let record = PendingPaymentDetails::pending_splice(PaymentId([10u8; 32]), intent);

		let encoded = record.encode();
		let decoded = PendingPaymentDetails::read(&mut &encoded[..]).unwrap();
		assert_eq!(record, decoded);
		let intent = decoded.splice_intent.expect("a pending splice decoded without its intent");
		assert_eq!(reserved(&intent.contribution), expected);
	}

	#[test]
	fn tracked_payment_round_trips() {
		// An entry without a payment record round-trips in `pending_splice_round_trips`; here we
		// cover one carrying the record and its candidate history.
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
