// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Durable facts about the transactions a channel produces, as the producers of those
//! transactions reported them.
//!
//! A fact is immutable: it records what one producer knew at the moment it handed a transaction
//! over, keyed by that transaction's id. Several producers may describe the same transaction —
//! a funding transaction is reported when it is built and again when the channel reaches
//! pending — so records are merged rather than replaced, and a producer reporting a different
//! value for something already recorded is rejected instead of overwriting it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Sequence, Transaction, Txid};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::types::ChannelId;
use lightning::util::persist::PageToken;
use lightning::util::ser::Writeable;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::config::{CHANNEL_TX_FACTS_MAX_RECORDS, CHANNEL_TX_FACTS_MAX_RECORD_BYTES};
use crate::data_store::{StorableObject, StorableObjectId};
use crate::hex_utils;
use crate::payment::store::{Channel, TransactionType};
use crate::payment::PaymentDirection;
use crate::types::{ChainMonitor, ChannelManager, Sweeper, UserChannelId};

/// The part a transaction output plays in a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelOutputRole {
	/// The output holding a channel's funds, spendable only by the channel's commitment and
	/// closing transactions.
	Funding,
	/// An anchor output of a commitment transaction, spendable to fee-bump that transaction.
	Anchor,
	/// An HTLC output of a commitment transaction.
	Htlc,
	/// An output a channel resolved to this node, spendable by the on-chain wallet.
	Spendable,
}

impl_writeable_tlv_based_enum!(ChannelOutputRole,
	(0, Funding) => {},
	(2, Anchor) => {},
	(4, Htlc) => {},
	(6, Spendable) => {},
);

/// One output of a transaction that a channel controls, and the channel controlling it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelOutputFact {
	/// The index of the output within its transaction.
	pub vout: u32,
	/// What the output is for.
	pub role: ChannelOutputRole,
	/// The `node_id` of the channel's counterparty.
	pub counterparty_node_id: PublicKey,
	/// The channel controlling the output.
	pub channel_id: ChannelId,
	/// The channel's local identifier, when the producer of this fact knew it. It survives the
	/// temporary-to-final `channel_id` transition, unlike `channel_id` itself.
	pub user_channel_id: Option<UserChannelId>,
}

impl_writeable_tlv_based!(ChannelOutputFact, {
	(0, vout, required),
	(2, role, required),
	(4, counterparty_node_id, required),
	(6, channel_id, required),
	(8, user_channel_id, option),
});

/// This node's share of an interactively negotiated funding transaction, and the funding payment
/// the transaction belongs to.
///
/// The amount and the fee are `None` for a candidate this node contributed nothing to, e.g. a
/// counterparty-initiated round before one of ours replaced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalFundingFigures {
	/// The funding payment this transaction is a candidate of.
	pub funding_payment_id: PaymentId,
	/// This node's share of the funding amount, in millisatoshis.
	pub amount_msat: Option<u64>,
	/// This node's share of the transaction's on-chain fee, in millisatoshis.
	pub fee_paid_msat: Option<u64>,
	/// Whether this node's share moves funds into or out of its on-chain wallet.
	pub direction: PaymentDirection,
}

impl_writeable_tlv_based!(LocalFundingFigures, {
	(0, funding_payment_id, required),
	(2, amount_msat, option),
	(4, fee_paid_msat, option),
	(6, direction, required),
});

/// What this node's producers reported about one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelTxFacts {
	/// The transaction these facts are about.
	pub txid: Txid,
	/// Outputs of this transaction controlled by a channel rather than by the wallet.
	pub outputs: Vec<ChannelOutputFact>,
	/// What this transaction is, when a producer identified it directly.
	pub self_role: Option<TransactionType>,
	/// This node's share of an interactive-funding candidate, and the funding record it belongs
	/// to.
	pub local_figures: Option<LocalFundingFigures>,
	/// The chain tip this node was at when it last learned something new about the transaction.
	/// It dates the record for retention; it is not a fact about the transaction, and so is the
	/// one part of a record a later report may move.
	pub recorded_at_height: u32,
}

impl_writeable_tlv_based!(ChannelTxFacts, {
	(0, txid, required),
	(2, outputs, optional_vec),
	(4, self_role, option),
	(6, local_figures, option),
	(8, recorded_at_height, required),
});

impl ChannelTxFacts {
	/// Facts about the transaction `txid`, to be filled in with what a producer reported.
	pub(crate) fn new(txid: Txid) -> Self {
		Self {
			txid,
			outputs: Vec::new(),
			self_role: None,
			local_figures: None,
			recorded_at_height: 0,
		}
	}

	/// Dates these facts at the chain tip the node is at while reporting them.
	pub(crate) fn reported_at_height(mut self, height: u32) -> Self {
		self.recorded_at_height = height;
		self
	}

	/// Records `vouts` of this transaction as controlled by `channel` in `role`.
	pub(crate) fn with_outputs(
		mut self, channel: &Channel, user_channel_id: Option<UserChannelId>,
		role: ChannelOutputRole, vouts: impl IntoIterator<Item = u32>,
	) -> Self {
		self.outputs.extend(vouts.into_iter().map(|vout| ChannelOutputFact {
			vout,
			role,
			counterparty_node_id: channel.counterparty_node_id,
			channel_id: channel.channel_id,
			user_channel_id,
		}));
		self
	}

	/// Facts about `outpoints`, all controlled by `channel` in `role`, as one record per
	/// transaction they belong to.
	pub(crate) fn per_transaction(
		channel: &Channel, user_channel_id: Option<UserChannelId>, role: ChannelOutputRole,
		outpoints: impl IntoIterator<Item = (Txid, u32)>,
	) -> Vec<Self> {
		let mut grouped: Vec<(Txid, Vec<u32>)> = Vec::new();
		for (txid, vout) in outpoints {
			match grouped.iter_mut().find(|(recorded, _)| *recorded == txid) {
				Some((_, vouts)) => {
					if !vouts.contains(&vout) {
						vouts.push(vout);
					}
				},
				None => grouped.push((txid, vec![vout])),
			}
		}
		grouped
			.into_iter()
			.map(|(txid, vouts)| {
				Self::new(txid).with_outputs(channel, user_channel_id, role, vouts)
			})
			.collect()
	}

	/// Records what this transaction is.
	pub(crate) fn with_self_role(mut self, self_role: TransactionType) -> Self {
		self.self_role = Some(self_role);
		self
	}

	/// Records this node's share of an interactively negotiated funding candidate, and the funding
	/// payment the candidate belongs to.
	pub(crate) fn with_local_figures(mut self, local_figures: LocalFundingFigures) -> Self {
		self.local_figures = Some(local_figures);
		self
	}

	/// Merges `incoming` into these facts, returning the result, or `None` when `incoming` adds
	/// nothing to what is already recorded.
	///
	/// Outputs are unioned by `vout`, while `self_role` and `local_figures` are filled in only
	/// where they are still absent. Re-reporting a fact is therefore a no-op, which is what lets
	/// a producer replay its event without consequence. Reporting a *different* value for
	/// something already recorded is rejected, leaving the recorded facts as they were, and so is
	/// a report that would take the record past the size a single record is allowed.
	///
	/// A merge that changes something dates the record at the incoming report's height, so that
	/// retention measures how long ago this node last learned anything about the transaction.
	pub(crate) fn merged_with(
		mut self, incoming: &ChannelTxFacts,
	) -> Result<Option<Self>, ChannelTxFactsRejection> {
		if self.txid != incoming.txid {
			return Err(ChannelTxFactsRejection::Txid {
				recorded: self.txid,
				incoming: incoming.txid,
			});
		}

		let mut changed = false;
		for output in &incoming.outputs {
			match self.outputs.iter().find(|recorded| recorded.vout == output.vout) {
				Some(recorded) if recorded == output => {},
				Some(recorded) => {
					return Err(ChannelTxFactsRejection::Output {
						recorded: recorded.clone(),
						incoming: output.clone(),
					})
				},
				None => {
					self.outputs.push(output.clone());
					changed = true;
				},
			}
		}

		match (&self.self_role, &incoming.self_role) {
			(Some(recorded), Some(incoming)) if recorded != incoming => {
				return Err(ChannelTxFactsRejection::SelfRole {
					recorded: recorded.clone(),
					incoming: incoming.clone(),
				})
			},
			(None, Some(incoming)) => {
				self.self_role = Some(incoming.clone());
				changed = true;
			},
			_ => {},
		}

		match (&self.local_figures, &incoming.local_figures) {
			(Some(recorded), Some(incoming)) if recorded != incoming => {
				return Err(ChannelTxFactsRejection::LocalFigures {
					recorded: recorded.clone(),
					incoming: incoming.clone(),
				})
			},
			(None, Some(incoming)) => {
				self.local_figures = Some(incoming.clone());
				changed = true;
			},
			_ => {},
		}

		if !changed {
			return Ok(None);
		}
		self.recorded_at_height = self.recorded_at_height.max(incoming.recorded_at_height);
		self.size_checked().map(Some)
	}

	/// These facts, or a rejection when storing them would take one record past the size a
	/// record is allowed.
	///
	/// A record is written whole, so its size is the one resource a producer drives without
	/// creating a record of its own: every channel-controlled output of a transaction lands on
	/// that transaction's record, and a counterparty decides how many HTLCs a commitment
	/// transaction carries. What a refused report would have described stays undescribed, and
	/// what that costs is for the producer that reported it to weigh.
	pub(crate) fn size_checked(self) -> Result<Self, ChannelTxFactsRejection> {
		let bytes = self.serialized_length();
		if bytes > CHANNEL_TX_FACTS_MAX_RECORD_BYTES {
			return Err(ChannelTxFactsRejection::TooLarge {
				bytes,
				limit: CHANNEL_TX_FACTS_MAX_RECORD_BYTES,
			});
		}
		Ok(self)
	}
}

/// What became of a producer's report of what a transaction is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FactsRecordOutcome {
	/// Everything reported is on record.
	Recorded,
	/// Part of what was reported is not on record, because recording it would have taken the
	/// facts past the resources they are allowed. Nothing was lost, so what the refusal costs
	/// is the reporting producer's to weigh: a transaction reported without a classification
	/// for a producer that has nothing left to withhold, a round left unsigned for one that
	/// will not release a transaction it cannot measure.
	Incomplete,
}

/// Whether a report about a transaction this node holds no record of at all is subject to the
/// number of records the store may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FactsAdmission {
	/// Refused once the store holds as many records as it is allowed to.
	///
	/// This is what bounds the store: a counterparty opening and closing channels, or replacing
	/// a negotiated funding again and again, drives records about transactions this node only
	/// reports on, and each of them costs nothing to refuse beyond a transaction going
	/// unclassified.
	Capped,
	/// Admitted beside however many records the store holds, and counted like any other, so that
	/// capped reports are refused the sooner.
	///
	/// This is for the one report whose refusal costs more than the record: a round this node is
	/// about to sign, which it will not release without its own share of it on record. How many
	/// such records there can be is a question of how many rounds this node signs, which is its
	/// own decision, and each carries no outputs.
	Exempt,
}

/// The channels this node still holds on-chain state for, as the retention of recorded facts
/// consults them.
pub(crate) trait ChannelLiveness: Send + Sync {
	/// The channels the node's channel manager, chain monitor or output sweeper still knows
	/// about, or `None` when that state cannot be consulted at all. Nothing is dropped while the
	/// answer is `None`: without it there is no way to tell which facts are still needed.
	fn live_channels(&self) -> Option<HashSet<ChannelId>>;
}

/// The node's own channel state, as [`ChannelLiveness`].
///
/// The handles are weak because the node's channel state holds the wallet in turn, through the
/// keys manager, so strong ones here would keep both alive for good. A handle that no longer
/// upgrades means the node is being torn down, which is no time to be dropping records.
pub(crate) struct NodeChannelLiveness {
	channel_manager: Weak<ChannelManager>,
	chain_monitor: Weak<ChainMonitor>,
	output_sweeper: Weak<Sweeper>,
}

impl NodeChannelLiveness {
	pub(crate) fn new(
		channel_manager: &Arc<ChannelManager>, chain_monitor: &Arc<ChainMonitor>,
		output_sweeper: &Arc<Sweeper>,
	) -> Self {
		Self {
			channel_manager: Arc::downgrade(channel_manager),
			chain_monitor: Arc::downgrade(chain_monitor),
			output_sweeper: Arc::downgrade(output_sweeper),
		}
	}
}

impl ChannelLiveness for NodeChannelLiveness {
	fn live_channels(&self) -> Option<HashSet<ChannelId>> {
		let channel_manager = self.channel_manager.upgrade()?;
		let chain_monitor = self.chain_monitor.upgrade()?;
		let output_sweeper = self.output_sweeper.upgrade()?;

		Some(live_channels_of(
			channel_manager.list_channels().into_iter().map(|channel| channel.channel_id),
			chain_monitor.list_monitors(),
			output_sweeper.tracked_spendable_outputs().into_iter().map(|output| output.channel_id),
		))
	}
}

/// The channels named by a node's open channels, by the monitors it holds and by the spendable
/// outputs its sweeper tracks, each named once.
///
/// A channel counts as held if any one of the three names it: an open channel can still produce
/// transactions, a monitor can still claim from one, and a tracked output has yet to be swept.
/// A tracked output that names no channel — one the sweeper was given without one — says nothing
/// about which channel is held and is left out.
pub(crate) fn live_channels_of(
	channels: impl IntoIterator<Item = ChannelId>, monitors: impl IntoIterator<Item = ChannelId>,
	tracked_outputs: impl IntoIterator<Item = Option<ChannelId>>,
) -> HashSet<ChannelId> {
	let mut live: HashSet<ChannelId> = channels.into_iter().collect();
	live.extend(monitors);
	live.extend(tracked_outputs.into_iter().flatten());
	live
}

/// How far the pruning of recorded facts has walked the store, and how many records that walk
/// found there.
///
/// The walk is what keeps the store's size known: it visits every record over consecutive chain
/// tips, so the count it arrives at is the store's own, without a second pass over it and without
/// holding an index of its keys in memory. Between walks the count follows the records created
/// and dropped, so it is exact except for records created during a walk that the walk had already
/// gone past — those are counted by the walk after, which bounds how far the store can run past
/// its limit at one walk's worth of growth.
pub(crate) struct FactsRetention {
	/// Where the walk resumes and what it has counted, held by the pruning pass alone.
	walk: tokio::sync::Mutex<FactsWalk>,
	/// How many records the store holds. `None` until a walk has completed, until when nothing
	/// is refused for want of room.
	count: Mutex<Option<usize>>,
}

/// The pruning pass's place in its walk of the store.
pub(crate) struct FactsWalk {
	/// Where the next batch resumes, or `None` to walk the store from the start.
	pub cursor: Option<PageToken>,
	/// How many records this walk has counted so far.
	pub seen: usize,
}

impl FactsRetention {
	pub(crate) fn new() -> Self {
		Self {
			walk: tokio::sync::Mutex::new(FactsWalk { cursor: None, seen: 0 }),
			count: Mutex::new(None),
		}
	}

	/// Takes the pruning pass's place in its walk, for as long as the guard lives.
	pub(crate) async fn walk(&self) -> tokio::sync::MutexGuard<'_, FactsWalk> {
		self.walk.lock().await
	}

	/// Whether the store has room for a record it does not hold yet.
	pub(crate) fn has_room(&self) -> bool {
		self.count.lock().expect("lock").map_or(true, |count| count < CHANNEL_TX_FACTS_MAX_RECORDS)
	}

	/// Notes that a record was created.
	pub(crate) fn record_created(&self) {
		if let Some(count) = self.count.lock().expect("lock").as_mut() {
			*count = count.saturating_add(1);
		}
	}

	/// Notes that a record was dropped.
	pub(crate) fn record_dropped(&self) {
		if let Some(count) = self.count.lock().expect("lock").as_mut() {
			*count = count.saturating_sub(1);
		}
	}

	/// Notes that a walk of the whole store ended having counted `seen` records.
	pub(crate) fn walk_completed(&self, seen: usize) {
		*self.count.lock().expect("lock") = Some(seen);
	}

	#[cfg(test)]
	pub(crate) fn counted(&self) -> Option<usize> {
		*self.count.lock().expect("lock")
	}
}

/// What deciding whether a transaction's facts are still needed takes, beyond the facts
/// themselves.
pub(crate) struct RetentionCheck<'a> {
	/// The height of the chain tip the decision is taken at.
	pub tip_height: u32,
	/// How many blocks a record outlives the last thing this node learned about its transaction.
	pub retention_blocks: u32,
	/// The channels this node still holds on-chain state for.
	pub live_channels: &'a HashSet<ChannelId>,
	/// The transactions the pending payment store still refers to — its records' own
	/// transactions, their interactive-funding candidates, the rounds that locked and the
	/// conflicts wallet sync listed. A payment is pending exactly while its classification can
	/// still be written onto it, so a transaction named here has yet to reach its record.
	pub pending_txids: &'a HashSet<Txid>,
	/// Whether every funding output the facts record has been spent by a transaction confirmed
	/// at least `2 * ANTI_REORG_DELAY` deep whose own payment has settled. `true` for facts
	/// recording no funding output, which nothing closes.
	pub funding_spends_settled: bool,
}

impl ChannelTxFacts {
	/// Whether these facts have outlived every use this node has for them.
	///
	/// All of it must hold at once, and the age cap is what makes the answer bounded for facts
	/// the other checks are blind to — a transaction for a channel that never reached the
	/// channel manager, the chain monitor or the sweeper satisfies them vacuously.
	pub(crate) fn is_prunable(&self, check: &RetentionCheck<'_>) -> bool {
		if check.tip_height < self.recorded_at_height.saturating_add(check.retention_blocks) {
			return false;
		}
		if self.outputs.iter().any(|output| check.live_channels.contains(&output.channel_id)) {
			return false;
		}
		if check.pending_txids.contains(&self.txid) {
			return false;
		}
		check.funding_spends_settled
	}

	/// The outputs of this transaction a channel holds its funds in.
	pub(crate) fn funding_vouts(&self) -> impl Iterator<Item = u32> + '_ {
		self.outputs
			.iter()
			.filter(|output| output.role == ChannelOutputRole::Funding)
			.map(|output| output.vout)
	}
}

impl StorableObjectId for Txid {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(self.as_byte_array())
	}

	fn decode_from_hex_str(s: &str) -> Option<Self> {
		let bytes: [u8; 32] = hex_utils::to_vec(s)?.try_into().ok()?;
		Some(Txid::from_byte_array(bytes))
	}
}

impl StorableObject for ChannelTxFacts {
	type Id = Txid;

	fn id(&self) -> Self::Id {
		self.txid
	}
}

/// A reported fact that was not recorded, leaving what is on record as it was.
///
/// Most of these mean two producers disagree about the same transaction, which they cannot both
/// be right about: facts are immutable, so the recorded value stands and the reported one is
/// dropped. The remaining one is a report the record has no room for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelTxFactsRejection {
	/// The reported facts are about a different transaction altogether.
	Txid { recorded: Txid, incoming: Txid },
	/// The same output is reported with a different role or a different channel.
	Output { recorded: ChannelOutputFact, incoming: ChannelOutputFact },
	/// The transaction is reported as being something else than it is recorded as.
	SelfRole { recorded: TransactionType, incoming: TransactionType },
	/// This node's share of the transaction is reported differently than it is recorded.
	LocalFigures { recorded: LocalFundingFigures, incoming: LocalFundingFigures },
	/// Recording the report would take the transaction's record past the size one record is
	/// allowed.
	TooLarge { bytes: usize, limit: usize },
	/// The store holds as many records as it is allowed to, and this report is about a
	/// transaction it holds no record of.
	NoRoom { limit: usize },
}

impl ChannelTxFactsRejection {
	/// Whether the report was refused for want of room rather than because it contradicts what is
	/// on record. Both leave the recorded facts as they were, but only a contradiction says a
	/// producer is wrong about something.
	pub(crate) fn is_resource_limit(&self) -> bool {
		matches!(self, Self::TooLarge { .. } | Self::NoRoom { .. })
	}
}

impl fmt::Display for ChannelTxFactsRejection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Txid { recorded, incoming } => {
				write!(f, "transaction {} reported as {}", recorded, incoming)
			},
			Self::Output { recorded, incoming } => {
				write!(f, "output {:?} reported as {:?}", recorded, incoming)
			},
			Self::SelfRole { recorded, incoming } => {
				write!(f, "transaction type {:?} reported as {:?}", recorded, incoming)
			},
			Self::LocalFigures { recorded, incoming } => {
				write!(f, "local funding figures {:?} reported as {:?}", recorded, incoming)
			},
			Self::TooLarge { bytes, limit } => {
				write!(f, "record of {} bytes exceeds the {} bytes allowed", bytes, limit)
			},
			Self::NoRoom { limit } => {
				write!(f, "no room for a further record beside the {} already held", limit)
			},
		}
	}
}

/// The recorded facts a transaction's classification rests on: what this node's channels reported
/// about the transaction itself, and what they reported about the transactions its inputs spend.
#[derive(Clone, Debug, Default)]
pub(crate) struct TxProvenance {
	/// What was reported about the transaction itself, if anything.
	self_facts: Option<ChannelTxFacts>,
	/// What was reported about the transactions the inputs spend, keyed by transaction id. Only
	/// the transactions the inputs actually reference are represented.
	parent_facts: HashMap<Txid, ChannelTxFacts>,
}

impl TxProvenance {
	/// The provenance assembled from the facts recorded for a transaction and for the
	/// transactions its inputs spend.
	pub(crate) fn new(
		self_facts: Option<ChannelTxFacts>, parent_facts: HashMap<Txid, ChannelTxFacts>,
	) -> Self {
		Self { self_facts, parent_facts }
	}

	/// What `tx` is, as far as these facts can tell; see [`classify`].
	pub(crate) fn classify(&self, tx: &Transaction) -> Option<TransactionType> {
		classify(tx, self.self_facts.as_ref(), &self.parent_facts)
	}

	/// This node's share of the transaction, for a candidate of an interactively negotiated
	/// funding a producer reported the figures of.
	pub(crate) fn local_figures(&self) -> Option<&LocalFundingFigures> {
		self.self_facts.as_ref()?.local_figures.as_ref()
	}
}

/// What a transaction is, derived from what this node's channels recorded about it and about the
/// transactions its inputs spend.
///
/// `self_facts` are the facts recorded for `tx`, `parent_facts` those recorded for the
/// transactions `tx` spends from, keyed by transaction id. Anything these cannot account for is
/// left unclassified rather than guessed: without a channel of this node's laying claim to an
/// output, a transaction is an ordinary on-chain payment.
pub(crate) fn classify(
	tx: &Transaction, self_facts: Option<&ChannelTxFacts>,
	parent_facts: &HashMap<Txid, ChannelTxFacts>,
) -> Option<TransactionType> {
	// A producer that named the transaction outright is the most reliable answer there is, and
	// the only one that stays put across a re-broadcast or a replacement of the transaction.
	if let Some(self_role) = self_facts.and_then(|facts| facts.self_role.as_ref()) {
		return Some(self_role.clone());
	}

	let spent: Vec<&ChannelOutputFact> = tx
		.input
		.iter()
		.filter_map(|input| {
			parent_facts.get(&input.previous_output.txid).and_then(|parent| {
				parent.outputs.iter().find(|output| output.vout == input.previous_output.vout)
			})
		})
		.collect();
	let created: &[ChannelOutputFact] = self_facts.map_or(&[], |facts| facts.outputs.as_slice());
	let funds: Vec<&ChannelOutputFact> =
		created.iter().filter(|output| output.role == ChannelOutputRole::Funding).collect();

	let spent_funding = in_role(&spent, ChannelOutputRole::Funding);
	if let Some(funding) = spent_funding.first() {
		// Moving a channel's funds into a new funding output is what an interactive negotiation
		// produces, whichever side of it this node is on.
		if !funds.is_empty() {
			let channels = channels_of(spent_funding.iter().copied().chain(funds.iter().copied()));
			return Some(TransactionType::InteractiveFunding { channels });
		}
		if is_cooperative_close(tx) {
			return Some(TransactionType::CooperativeClose {
				counterparty_node_id: funding.counterparty_node_id,
				channel_id: funding.channel_id,
			});
		}
		if is_commitment(tx) {
			return Some(TransactionType::UnilateralClose {
				counterparty_node_id: funding.counterparty_node_id,
				channel_id: funding.channel_id,
			});
		}
		// The funding output is gone in a shape none of the transactions a channel produces has.
		// Naming it anyway would put a guess on a payment record that nothing later corrects.
		return None;
	}

	let spent_anchors = in_role(&spent, ChannelOutputRole::Anchor);
	if let Some(anchor) = spent_anchors.first() {
		return Some(TransactionType::AnchorBump {
			counterparty_node_id: anchor.counterparty_node_id,
			channel_id: anchor.channel_id,
		});
	}

	let spent_htlcs = in_role(&spent, ChannelOutputRole::Htlc);
	if let Some(htlc) = spent_htlcs.first() {
		return Some(TransactionType::Claim {
			counterparty_node_id: htlc.counterparty_node_id,
			channel_id: htlc.channel_id,
		});
	}

	let spent_spendable = in_role(&spent, ChannelOutputRole::Spendable);
	if !spent_spendable.is_empty() {
		return Some(TransactionType::Sweep { channels: channels_of(spent_spendable) });
	}

	if !funds.is_empty() {
		return Some(TransactionType::Funding { channels: channels_of(funds) });
	}

	None
}

/// The outputs among `outputs` a channel controls in `role`.
fn in_role<'a>(
	outputs: &[&'a ChannelOutputFact], role: ChannelOutputRole,
) -> Vec<&'a ChannelOutputFact> {
	outputs.iter().copied().filter(|output| output.role == role).collect()
}

/// The channels controlling `outputs`, each named once, in the order the outputs name them.
fn channels_of<'a>(outputs: impl IntoIterator<Item = &'a ChannelOutputFact>) -> Vec<Channel> {
	let mut channels: Vec<Channel> = Vec::new();
	for output in outputs {
		let channel = Channel {
			counterparty_node_id: output.counterparty_node_id,
			channel_id: output.channel_id,
		};
		if !channels.contains(&channel) {
			channels.push(channel);
		}
	}
	channels
}

/// Whether `tx` has the shape BOLT 2 gives a cooperative closing transaction: the sole spend of
/// the funding output, final and valid from the moment it is signed.
fn is_cooperative_close(tx: &Transaction) -> bool {
	tx.input.len() == 1
		&& tx.input[0].sequence == Sequence::MAX
		&& tx.lock_time.to_consensus_u32() == 0
}

/// Whether `tx` has the shape BOLT 3 gives a commitment transaction: the sole spend of the
/// funding output, with the upper byte of its sequence and of its locktime set to the constants
/// that mark the remainder of both as the obscured commitment number.
fn is_commitment(tx: &Transaction) -> bool {
	tx.input.len() == 1
		&& (tx.input[0].sequence.0 >> 24) as u8 == 0x80
		&& (tx.lock_time.to_consensus_u32() >> 24) as u8 == 0x20
}

#[cfg(test)]
mod tests {
	use bitcoin::absolute::LockTime;
	use bitcoin::transaction::Version;
	use bitcoin::{Amount, OutPoint, ScriptBuf, TxIn, TxOut, Witness};
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	fn test_txid(byte: u8) -> Txid {
		Txid::from_byte_array([byte; 32])
	}

	fn test_channel(byte: u8) -> Channel {
		let counterparty_node_id = PublicKey::from_slice(&[
			0x02, 0xc6, 0x04, 0x7f, 0x94, 0x41, 0xed, 0x7d, 0x6d, 0x30, 0x45, 0x40, 0x6e, 0x95,
			0xc0, 0x7c, 0xd8, 0x5c, 0x77, 0x8e, 0x4b, 0x8c, 0xef, 0x3c, 0xa7, 0xab, 0xac, 0x09,
			0xb9, 0x5c, 0x70, 0x9e, 0xe5,
		])
		.expect("static test key is valid");
		Channel { counterparty_node_id, channel_id: ChannelId([byte; 32]) }
	}

	fn other_counterparty() -> PublicKey {
		PublicKey::from_slice(&[
			0x02, 0x4d, 0x4b, 0x6c, 0xd1, 0x36, 0x10, 0x32, 0xca, 0x9b, 0xd2, 0xae, 0xb9, 0xd9,
			0x00, 0xaa, 0x4d, 0x45, 0xd9, 0xea, 0xd8, 0x0a, 0xc9, 0x42, 0x33, 0x74, 0xc4, 0x51,
			0xa7, 0x25, 0x4d, 0x07, 0x66,
		])
		.expect("static test key is valid")
	}

	fn with_local_figures(txid: Txid, local_figures: LocalFundingFigures) -> ChannelTxFacts {
		ChannelTxFacts { local_figures: Some(local_figures), ..ChannelTxFacts::new(txid) }
	}

	fn round_trip<T: Readable + Writeable + PartialEq + std::fmt::Debug>(object: &T) {
		let encoded = object.encode();
		let decoded: T = Readable::read(&mut &encoded[..]).expect("round trip");
		assert_eq!(&decoded, object);
	}

	fn full_facts() -> ChannelTxFacts {
		let channel = test_channel(1);
		let facts = ChannelTxFacts::new(test_txid(7))
			.with_outputs(&channel, Some(UserChannelId(42)), ChannelOutputRole::Funding, [0])
			.with_outputs(&channel, None, ChannelOutputRole::Anchor, [1])
			.with_outputs(&channel, None, ChannelOutputRole::Htlc, [2, 3])
			.with_outputs(&channel, None, ChannelOutputRole::Spendable, [4])
			.with_self_role(TransactionType::InteractiveFunding {
				channels: vec![channel.clone()],
			});
		ChannelTxFacts {
			local_figures: Some(LocalFundingFigures {
				funding_payment_id: PaymentId([9u8; 32]),
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(2_500),
				direction: PaymentDirection::Outbound,
			}),
			..facts
		}
	}

	#[test]
	fn facts_round_trip_through_tlv() {
		let facts = full_facts();
		round_trip(&facts);
		for output in &facts.outputs {
			round_trip(output);
			round_trip(&output.role);
		}
		round_trip(facts.local_figures.as_ref().expect("figures are set"));

		// A record a producer only partially filled in round-trips as such rather than picking up
		// defaults for what it left out.
		let sparse = ChannelTxFacts::new(test_txid(8));
		round_trip(&sparse);
		let decoded: ChannelTxFacts =
			Readable::read(&mut &sparse.encode()[..]).expect("round trip");
		assert!(decoded.outputs.is_empty());
		assert_eq!(decoded.self_role, None);
		assert_eq!(decoded.local_figures, None);
	}

	#[test]
	fn outpoints_group_into_one_record_per_transaction() {
		let channel = test_channel(1);
		let records = ChannelTxFacts::per_transaction(
			&channel,
			Some(UserChannelId(7)),
			ChannelOutputRole::Spendable,
			[
				(test_txid(1), 0),
				(test_txid(2), 4),
				(test_txid(1), 3),
				// A producer reporting the same outpoint twice contributes it once.
				(test_txid(2), 4),
			],
		);

		assert_eq!(records.len(), 2);
		assert_eq!(records[0].txid, test_txid(1));
		assert_eq!(
			records[0].outputs.iter().map(|output| output.vout).collect::<Vec<_>>(),
			vec![0, 3]
		);
		assert_eq!(records[1].txid, test_txid(2));
		assert_eq!(
			records[1].outputs.iter().map(|output| output.vout).collect::<Vec<_>>(),
			vec![4]
		);
		assert!(records.iter().flat_map(|facts| &facts.outputs).all(|output| {
			output.role == ChannelOutputRole::Spendable
				&& output.channel_id == channel.channel_id
				&& output.user_channel_id == Some(UserChannelId(7))
		}));
	}

	#[test]
	fn facts_key_round_trips_through_its_hex_encoding() {
		let txid = test_txid(3);
		let encoded = txid.encode_to_hex_str();
		assert_eq!(encoded.len(), 64);
		assert_eq!(Txid::decode_from_hex_str(&encoded), Some(txid));
		assert_eq!(Txid::decode_from_hex_str("not hex"), None);
		assert_eq!(Txid::decode_from_hex_str("00"), None);
	}

	#[test]
	fn replaying_a_fact_changes_nothing() {
		let facts = full_facts();
		assert_eq!(facts.clone().merged_with(&facts), Ok(None));

		// A producer that reports only part of what is already recorded is likewise a no-op, which
		// is what a replay of an earlier event looks like once a later one has filled the record
		// in.
		let channel = test_channel(1);
		let partial = ChannelTxFacts::new(test_txid(7)).with_outputs(
			&channel,
			Some(UserChannelId(42)),
			ChannelOutputRole::Funding,
			[0],
		);
		assert_eq!(facts.clone().merged_with(&partial), Ok(None));
	}

	#[test]
	fn outputs_of_two_producers_merge_into_one_record() {
		let channel = test_channel(1);
		let other = test_channel(2);
		let txid = test_txid(7);

		// A batched sweep resolves outputs of two different channels; each producer reports only
		// its own.
		let first = ChannelTxFacts::new(txid).with_outputs(
			&channel,
			None,
			ChannelOutputRole::Spendable,
			[0, 2],
		);
		let second =
			ChannelTxFacts::new(txid).with_outputs(&other, None, ChannelOutputRole::Spendable, [1]);

		let merged = first.merged_with(&second).expect("disjoint outputs merge").expect("changed");
		assert_eq!(merged.outputs.len(), 3);
		let mut vouts: Vec<u32> = merged.outputs.iter().map(|output| output.vout).collect();
		vouts.sort_unstable();
		assert_eq!(vouts, vec![0, 1, 2]);
		assert_eq!(
			merged.outputs.iter().find(|output| output.vout == 1).map(|output| output.channel_id),
			Some(other.channel_id)
		);
	}

	#[test]
	fn a_second_role_for_one_output_is_rejected() {
		let channel = test_channel(1);
		let txid = test_txid(7);
		let recorded =
			ChannelTxFacts::new(txid).with_outputs(&channel, None, ChannelOutputRole::Funding, [0]);
		let conflicting =
			ChannelTxFacts::new(txid).with_outputs(&channel, None, ChannelOutputRole::Anchor, [0]);

		match recorded.clone().merged_with(&conflicting) {
			Err(ChannelTxFactsRejection::Output { recorded, incoming }) => {
				assert_eq!(recorded.role, ChannelOutputRole::Funding);
				assert_eq!(incoming.role, ChannelOutputRole::Anchor);
			},
			other => panic!("expected an output conflict, got {:?}", other),
		}

		// The same output attributed to a different channel is a conflict too, rather than the
		// later producer's channel silently winning.
		let other_channel = Channel {
			counterparty_node_id: other_counterparty(),
			channel_id: ChannelId([2u8; 32]),
		};
		let reattributed = ChannelTxFacts::new(txid).with_outputs(
			&other_channel,
			None,
			ChannelOutputRole::Funding,
			[0],
		);
		assert!(matches!(
			recorded.merged_with(&reattributed),
			Err(ChannelTxFactsRejection::Output { .. })
		));
	}

	#[test]
	fn a_second_transaction_type_is_rejected() {
		let channel = test_channel(1);
		let txid = test_txid(7);
		let recorded = ChannelTxFacts::new(txid)
			.with_self_role(TransactionType::Funding { channels: vec![channel.clone()] });
		let conflicting =
			ChannelTxFacts::new(txid).with_self_role(TransactionType::InteractiveFunding {
				channels: vec![channel.clone()],
			});

		match recorded.clone().merged_with(&conflicting) {
			Err(ChannelTxFactsRejection::SelfRole { recorded, incoming }) => {
				assert_eq!(recorded, TransactionType::Funding { channels: vec![channel.clone()] });
				assert_eq!(
					incoming,
					TransactionType::InteractiveFunding { channels: vec![channel] }
				);
			},
			other => panic!("expected a transaction type conflict, got {:?}", other),
		}
	}

	#[test]
	fn a_second_set_of_local_figures_is_rejected() {
		let txid = test_txid(7);
		let figures = LocalFundingFigures {
			funding_payment_id: PaymentId([9u8; 32]),
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(2_500),
			direction: PaymentDirection::Outbound,
		};
		let recorded = with_local_figures(txid, figures.clone());
		let conflicting =
			with_local_figures(txid, LocalFundingFigures { fee_paid_msat: Some(5_000), ..figures });

		assert!(matches!(
			recorded.merged_with(&conflicting),
			Err(ChannelTxFactsRejection::LocalFigures { .. })
		));
	}

	#[test]
	fn facts_about_another_transaction_are_rejected() {
		let recorded = ChannelTxFacts::new(test_txid(7));
		let other = ChannelTxFacts::new(test_txid(8));
		assert_eq!(
			recorded.merged_with(&other),
			Err(ChannelTxFactsRejection::Txid { recorded: test_txid(7), incoming: test_txid(8) })
		);
	}

	#[test]
	fn a_rejected_merge_leaves_the_record_untouched() {
		let channel = test_channel(1);
		let txid = test_txid(7);
		let recorded =
			ChannelTxFacts::new(txid).with_outputs(&channel, None, ChannelOutputRole::Funding, [0]);

		// The addition the producer got right comes with one it got wrong; neither lands.
		let conflicting = ChannelTxFacts::new(txid)
			.with_outputs(&channel, None, ChannelOutputRole::Htlc, [1])
			.with_outputs(&channel, None, ChannelOutputRole::Anchor, [0]);

		assert!(recorded.clone().merged_with(&conflicting).is_err());
		assert_eq!(recorded.outputs.len(), 1);
		assert_eq!(recorded.outputs[0].role, ChannelOutputRole::Funding);
	}

	#[test]
	fn a_transaction_type_fills_in_only_while_absent() {
		let channel = test_channel(1);
		let txid = test_txid(7);
		let role = TransactionType::UnilateralClose {
			counterparty_node_id: channel.counterparty_node_id,
			channel_id: channel.channel_id,
		};

		let empty = ChannelTxFacts::new(txid);
		let filled = empty
			.merged_with(&ChannelTxFacts::new(txid).with_self_role(role.clone()))
			.expect("fills in")
			.expect("changed");
		assert_eq!(filled.self_role, Some(role.clone()));

		// A producer reporting the same type again adds nothing, so nothing is written.
		assert_eq!(
			filled.clone().merged_with(&ChannelTxFacts::new(txid).with_self_role(role)),
			Ok(None)
		);
	}

	#[test]
	fn local_figures_fill_in_only_while_absent() {
		let txid = test_txid(7);
		let figures = LocalFundingFigures {
			funding_payment_id: PaymentId([9u8; 32]),
			amount_msat: None,
			fee_paid_msat: None,
			direction: PaymentDirection::Inbound,
		};

		let filled = ChannelTxFacts::new(txid)
			.merged_with(&with_local_figures(txid, figures.clone()))
			.expect("fills in")
			.expect("changed");
		assert_eq!(filled.local_figures, Some(figures.clone()));

		assert_eq!(filled.merged_with(&with_local_figures(txid, figures)), Ok(None));
	}
	/// The transaction whose outputs the classification cases below spend.
	const PARENT: u8 = 0x11;

	/// A transaction spending `inputs`, each input carrying `sequence`.
	fn spending_tx(inputs: &[(Txid, u32)], sequence: Sequence, lock_time: u32) -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::from_consensus(lock_time),
			input: inputs
				.iter()
				.map(|(txid, vout)| TxIn {
					previous_output: OutPoint { txid: *txid, vout: *vout },
					script_sig: ScriptBuf::new(),
					sequence,
					witness: Witness::new(),
				})
				.collect(),
			output: vec![TxOut { value: Amount::from_sat(1_000), script_pubkey: ScriptBuf::new() }],
		}
	}

	/// The single spend of `PARENT`'s first output, in the shape BOLT 2 gives a cooperative
	/// closing transaction.
	fn cooperative_close_shaped() -> Transaction {
		spending_tx(&[(test_txid(PARENT), 0)], Sequence::MAX, 0)
	}

	/// The single spend of `PARENT`'s first output, in the shape BOLT 3 gives a commitment
	/// transaction: the obscured commitment number split across sequence and locktime.
	fn commitment_shaped() -> Transaction {
		spending_tx(&[(test_txid(PARENT), 0)], Sequence(0x80_12_34_56), 0x20_ab_cd_ef)
	}

	/// The single spend of `PARENT`'s first output in no shape a channel produces: replaceable,
	/// and without a commitment number.
	fn unrecognised_shaped() -> Transaction {
		spending_tx(&[(test_txid(PARENT), 0)], Sequence(0xff_ff_ff_fd), 0)
	}

	fn parents(facts: impl IntoIterator<Item = ChannelTxFacts>) -> HashMap<Txid, ChannelTxFacts> {
		facts.into_iter().map(|facts| (facts.txid, facts)).collect()
	}

	/// Facts recording `PARENT`'s outputs `vouts` as controlled by `channel` in `role`.
	fn parent_outputs(
		channel: &Channel, role: ChannelOutputRole, vouts: impl IntoIterator<Item = u32>,
	) -> ChannelTxFacts {
		ChannelTxFacts::new(test_txid(PARENT)).with_outputs(channel, None, role, vouts)
	}

	/// Facts recording `tx`'s first output as `channel`'s funding output.
	fn funds(tx: &Transaction, channel: &Channel, vout: u32) -> ChannelTxFacts {
		ChannelTxFacts::new(tx.compute_txid()).with_outputs(
			channel,
			Some(UserChannelId(42)),
			ChannelOutputRole::Funding,
			[vout],
		)
	}

	#[test]
	fn a_reported_role_settles_what_a_transaction_is() {
		let channel = test_channel(1);
		let tx = cooperative_close_shaped();
		let recorded = parents([parent_outputs(&channel, ChannelOutputRole::Funding, [0])]);

		// Left to its shape alone, the transaction is a cooperative close.
		assert_eq!(
			classify(&tx, None, &recorded),
			Some(TransactionType::CooperativeClose {
				counterparty_node_id: channel.counterparty_node_id,
				channel_id: channel.channel_id,
			})
		);

		// The channel that produced it says otherwise, and it is the one that knows.
		let reported = TransactionType::UnilateralClose {
			counterparty_node_id: channel.counterparty_node_id,
			channel_id: channel.channel_id,
		};
		let self_facts = ChannelTxFacts::new(tx.compute_txid()).with_self_role(reported.clone());
		assert_eq!(classify(&tx, Some(&self_facts), &recorded), Some(reported));
	}

	#[test]
	fn spending_and_creating_a_funding_output_is_an_interactive_funding() {
		let channel = test_channel(1);
		let tx = unrecognised_shaped();
		let self_facts = funds(&tx, &channel, 0);

		assert_eq!(
			classify(
				&tx,
				Some(&self_facts),
				&parents([parent_outputs(&channel, ChannelOutputRole::Funding, [0])]),
			),
			Some(TransactionType::InteractiveFunding { channels: vec![channel] })
		);
	}

	#[test]
	fn a_final_single_spend_of_a_funding_output_is_a_cooperative_close() {
		let channel = test_channel(1);
		assert_eq!(
			classify(
				&cooperative_close_shaped(),
				None,
				&parents([parent_outputs(&channel, ChannelOutputRole::Funding, [0])]),
			),
			Some(TransactionType::CooperativeClose {
				counterparty_node_id: channel.counterparty_node_id,
				channel_id: channel.channel_id,
			})
		);
	}

	#[test]
	fn a_commitment_shaped_spend_of_a_funding_output_is_a_unilateral_close() {
		let channel = test_channel(1);
		assert_eq!(
			classify(
				&commitment_shaped(),
				None,
				&parents([parent_outputs(&channel, ChannelOutputRole::Funding, [0])]),
			),
			Some(TransactionType::UnilateralClose {
				counterparty_node_id: channel.counterparty_node_id,
				channel_id: channel.channel_id,
			})
		);
	}

	#[test]
	fn an_unrecognised_spend_of_a_funding_output_is_left_unnamed() {
		let channel = test_channel(1);
		let recorded = parents([parent_outputs(&channel, ChannelOutputRole::Funding, [0])]);

		// Neither template matches and nothing reported the transaction, so there is no answer
		// to give. A close of either kind would be a guess.
		assert_eq!(classify(&unrecognised_shaped(), None, &recorded), None);

		// A second input rules both templates out as well, whatever the first input looks like.
		let two_inputs =
			spending_tx(&[(test_txid(PARENT), 0), (test_txid(PARENT + 1), 0)], Sequence::MAX, 0);
		assert_eq!(classify(&two_inputs, None, &recorded), None);
	}

	#[test]
	fn spending_an_anchor_output_is_an_anchor_bump() {
		let channel = test_channel(1);
		let tx = spending_tx(
			&[(test_txid(PARENT), 1), (test_txid(PARENT + 9), 0)],
			Sequence(0xff_ff_ff_fd),
			0,
		);

		assert_eq!(
			classify(
				&tx,
				None,
				&parents([parent_outputs(&channel, ChannelOutputRole::Anchor, [1])]),
			),
			Some(TransactionType::AnchorBump {
				counterparty_node_id: channel.counterparty_node_id,
				channel_id: channel.channel_id,
			})
		);
	}

	#[test]
	fn spending_an_htlc_output_is_a_claim() {
		let channel = test_channel(1);
		let tx = spending_tx(&[(test_txid(PARENT), 2)], Sequence(0xff_ff_ff_fd), 0);

		assert_eq!(
			classify(&tx, None, &parents([parent_outputs(&channel, ChannelOutputRole::Htlc, [2])]),),
			Some(TransactionType::Claim {
				counterparty_node_id: channel.counterparty_node_id,
				channel_id: channel.channel_id,
			})
		);
	}

	#[test]
	fn spending_resolved_outputs_is_a_sweep_naming_every_channel() {
		let channel = test_channel(1);
		let other = test_channel(2);
		let tx = spending_tx(
			&[(test_txid(PARENT), 0), (test_txid(PARENT), 1), (test_txid(PARENT + 1), 0)],
			Sequence(0xff_ff_ff_fd),
			0,
		);

		// One sweep resolving outputs of two channels is associated with both of them.
		let recorded = parents([
			parent_outputs(&channel, ChannelOutputRole::Spendable, [0, 1]),
			ChannelTxFacts::new(test_txid(PARENT + 1)).with_outputs(
				&other,
				None,
				ChannelOutputRole::Spendable,
				[0],
			),
		]);
		assert_eq!(
			classify(&tx, None, &recorded),
			Some(TransactionType::Sweep { channels: vec![channel, other] })
		);
	}

	#[test]
	fn creating_a_funding_output_alone_is_a_funding_naming_every_channel() {
		let channel = test_channel(1);
		let other = test_channel(2);
		// Nothing channel-controlled is spent: the wallet pays for both funding outputs.
		let tx = spending_tx(&[(test_txid(PARENT + 20), 0)], Sequence(0xff_ff_ff_fd), 0);
		let self_facts = funds(&tx, &channel, 0).with_outputs(
			&other,
			Some(UserChannelId(43)),
			ChannelOutputRole::Funding,
			[1],
		);

		assert_eq!(
			classify(&tx, Some(&self_facts), &HashMap::new()),
			Some(TransactionType::Funding { channels: vec![channel, other] })
		);
	}

	#[test]
	fn an_ordinary_wallet_spend_is_left_unnamed() {
		let tx = spending_tx(&[(test_txid(PARENT), 0)], Sequence(0xff_ff_ff_fd), 0);

		// Nothing was ever reported about the transaction or about what it spends.
		assert_eq!(classify(&tx, None, &HashMap::new()), None);

		// Nor does spending an output a channel left alone make the transaction a channel's.
		let channel = test_channel(1);
		let recorded = parents([parent_outputs(&channel, ChannelOutputRole::Spendable, [7])]);
		assert_eq!(classify(&tx, None, &recorded), None);
	}

	#[test]
	fn provenance_answers_from_the_facts_it_holds() {
		let channel = test_channel(1);
		let tx = cooperative_close_shaped();
		let figures = LocalFundingFigures {
			funding_payment_id: PaymentId([9u8; 32]),
			amount_msat: Some(1_000_000),
			fee_paid_msat: Some(2_500),
			direction: PaymentDirection::Outbound,
		};

		let empty = TxProvenance::default();
		assert_eq!(empty.classify(&tx), None);
		assert_eq!(empty.local_figures(), None);

		let provenance = TxProvenance::new(
			Some(with_local_figures(tx.compute_txid(), figures.clone())),
			parents([parent_outputs(&channel, ChannelOutputRole::Funding, [0])]),
		);
		assert_eq!(
			provenance.classify(&tx),
			Some(TransactionType::CooperativeClose {
				counterparty_node_id: channel.counterparty_node_id,
				channel_id: channel.channel_id,
			})
		);
		assert_eq!(provenance.local_figures(), Some(&figures));
	}

	/// Facts about a funding transaction of `channel` whose age is measured from `height`.
	fn funding_facts(channel: &Channel, height: u32) -> ChannelTxFacts {
		ChannelTxFacts::new(test_txid(20))
			.with_outputs(channel, None, ChannelOutputRole::Funding, [0])
			.reported_at_height(height)
	}

	/// A retention check that would drop the facts it is given: nothing is held, nothing is
	/// pending, the funding is spent and settled, and the age cap has long passed.
	fn everything_resolved<'a>(
		live_channels: &'a HashSet<ChannelId>, pending_txids: &'a HashSet<Txid>,
	) -> RetentionCheck<'a> {
		RetentionCheck {
			tip_height: 100_000,
			retention_blocks: 52_560,
			live_channels,
			pending_txids,
			funding_spends_settled: true,
		}
	}

	#[test]
	fn facts_of_a_resolved_channel_are_prunable() {
		let channel = test_channel(1);
		let (live, pending) = (HashSet::new(), HashSet::new());
		assert!(funding_facts(&channel, 10).is_prunable(&everything_resolved(&live, &pending)));
	}

	#[test]
	fn facts_are_kept_until_the_age_cap_has_passed() {
		let channel = test_channel(1);
		let (live, pending) = (HashSet::new(), HashSet::new());
		let facts = funding_facts(&channel, 50_000);

		let mut check = everything_resolved(&live, &pending);
		check.tip_height = 50_000 + 52_560 - 1;
		assert!(!facts.is_prunable(&check), "a block short of the cap is short of it");

		check.tip_height = 50_000 + 52_560;
		assert!(facts.is_prunable(&check));
	}

	#[test]
	fn facts_are_kept_while_the_node_still_holds_their_channel() {
		let channel = test_channel(1);
		let pending = HashSet::new();
		let live: HashSet<ChannelId> = [channel.channel_id].into_iter().collect();
		assert!(!funding_facts(&channel, 10).is_prunable(&everything_resolved(&live, &pending)));

		// Another channel being held says nothing about this one.
		let other: HashSet<ChannelId> = [test_channel(2).channel_id].into_iter().collect();
		assert!(funding_facts(&channel, 10).is_prunable(&everything_resolved(&other, &pending)));
	}

	#[test]
	fn facts_are_kept_while_a_pending_payment_names_their_transaction() {
		let channel = test_channel(1);
		let facts = funding_facts(&channel, 10);
		let live = HashSet::new();
		let pending: HashSet<Txid> = [facts.txid].into_iter().collect();
		assert!(!facts.is_prunable(&everything_resolved(&live, &pending)));
	}

	#[test]
	fn facts_are_kept_until_the_funding_they_record_is_spent_and_settled() {
		let channel = test_channel(1);
		let (live, pending) = (HashSet::new(), HashSet::new());
		let mut check = everything_resolved(&live, &pending);
		check.funding_spends_settled = false;

		assert!(!funding_facts(&channel, 10).is_prunable(&check));

		// Facts recording no funding of their own have no spend of one to wait for: what a
		// commitment transaction's anchors and HTLCs say is answered by the age cap and by
		// whether the channel is still held.
		let no_funding = ChannelTxFacts::new(test_txid(21))
			.with_outputs(&channel, None, ChannelOutputRole::Anchor, [0])
			.reported_at_height(10);
		let mut settled = check;
		settled.funding_spends_settled = true;
		assert!(no_funding.is_prunable(&settled));
	}

	#[test]
	fn a_channel_any_of_the_three_sources_names_counts_as_held() {
		let (open, monitored, swept) = (ChannelId([1; 32]), ChannelId([2; 32]), ChannelId([3; 32]));

		assert_eq!(live_channels_of([], [], []), HashSet::new());
		assert_eq!(live_channels_of([open], [], []), [open].into_iter().collect());
		assert_eq!(live_channels_of([], [monitored], []), [monitored].into_iter().collect());
		assert_eq!(live_channels_of([], [], [Some(swept)]), [swept].into_iter().collect());

		// A tracked output without a channel names none, and a channel several sources name is
		// named once.
		assert_eq!(
			live_channels_of([open], [open, monitored], [Some(swept), None]),
			[open, monitored, swept].into_iter().collect(),
		);
	}

	#[test]
	fn a_report_that_would_outgrow_one_record_is_refused() {
		let channel = test_channel(1);
		let recorded = ChannelTxFacts::new(test_txid(30)).with_outputs(
			&channel,
			None,
			ChannelOutputRole::Htlc,
			0..8,
		);

		// One output costs well under a hundred bytes, so a report of this many cannot fit.
		let oversized = ChannelTxFacts::new(test_txid(30)).with_outputs(
			&channel,
			None,
			ChannelOutputRole::Htlc,
			8..40_000,
		);
		match recorded.clone().merged_with(&oversized) {
			Err(ChannelTxFactsRejection::TooLarge { bytes, limit }) => {
				assert!(bytes > limit, "{} is not past {}", bytes, limit);
				assert_eq!(limit, CHANNEL_TX_FACTS_MAX_RECORD_BYTES);
			},
			Ok(merged) => panic!(
				"unexpected merge outcome: {} outputs recorded",
				merged.map_or(0, |facts| facts.outputs.len()),
			),
			Err(e) => panic!("unexpected rejection {:?}", e),
		}
		// The refusal is what the caller sees; what is on record is untouched, as it is for a
		// contradiction.
		assert_eq!(recorded.clone().merged_with(&recorded).unwrap(), None);
		assert!(oversized.size_checked().is_err());
		assert!(recorded.size_checked().is_ok());
	}

	#[test]
	fn a_record_is_dated_at_the_last_report_that_added_to_it() {
		let channel = test_channel(1);
		let first = ChannelTxFacts::new(test_txid(31))
			.with_outputs(&channel, None, ChannelOutputRole::Funding, [0])
			.reported_at_height(700);

		// A replay adds nothing, so it writes nothing and cannot refresh the record's age.
		let replay = first.clone().reported_at_height(900);
		assert_eq!(first.clone().merged_with(&replay).unwrap(), None);

		let later = ChannelTxFacts::new(test_txid(31))
			.with_outputs(&channel, None, ChannelOutputRole::Anchor, [1])
			.reported_at_height(900);
		let merged = first.merged_with(&later).unwrap().expect("the anchor is new");
		assert_eq!(merged.recorded_at_height, 900);
	}

	#[test]
	fn the_census_bounds_admission_only_once_a_walk_has_counted_the_store() {
		let retention = FactsRetention::new();
		assert_eq!(retention.counted(), None);
		// Nothing is refused while the store's size is unknown, however much is created.
		for _ in 0..CHANNEL_TX_FACTS_MAX_RECORDS + 1 {
			retention.record_created();
		}
		assert!(retention.has_room());

		retention.walk_completed(CHANNEL_TX_FACTS_MAX_RECORDS - 1);
		assert!(retention.has_room());
		retention.record_created();
		assert!(!retention.has_room(), "the store is full");
		retention.record_dropped();
		assert!(retention.has_room(), "dropping a record makes room");
	}
}
