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

use std::fmt;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Txid;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::types::ChannelId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectId};
use crate::hex_utils;
use crate::payment::store::{Channel, TransactionType};
use crate::payment::PaymentDirection;
use crate::types::UserChannelId;

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
}

impl_writeable_tlv_based!(ChannelTxFacts, {
	(0, txid, required),
	(2, outputs, optional_vec),
	(4, self_role, option),
	(6, local_figures, option),
});

impl ChannelTxFacts {
	/// Facts about the transaction `txid`, to be filled in with what a producer reported.
	pub(crate) fn new(txid: Txid) -> Self {
		Self { txid, outputs: Vec::new(), self_role: None, local_figures: None }
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

	/// Records what this transaction is.
	pub(crate) fn with_self_role(mut self, self_role: TransactionType) -> Self {
		self.self_role = Some(self_role);
		self
	}

	/// Records this node's share of an interactively negotiated funding transaction.
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
	/// something already recorded is rejected, leaving the recorded facts as they were.
	pub(crate) fn merged_with(
		mut self, incoming: &ChannelTxFacts,
	) -> Result<Option<Self>, ChannelTxFactsConflict> {
		if self.txid != incoming.txid {
			return Err(ChannelTxFactsConflict::Txid {
				recorded: self.txid,
				incoming: incoming.txid,
			});
		}

		let mut changed = false;
		for output in &incoming.outputs {
			match self.outputs.iter().find(|recorded| recorded.vout == output.vout) {
				Some(recorded) if recorded == output => {},
				Some(recorded) => {
					return Err(ChannelTxFactsConflict::Output {
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
				return Err(ChannelTxFactsConflict::SelfRole {
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
				return Err(ChannelTxFactsConflict::LocalFigures {
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

		Ok(changed.then_some(self))
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

/// A reported fact that contradicts one already recorded for the same transaction.
///
/// Facts are immutable, so this means two producers disagree about the same transaction, which
/// they cannot both be right about. The recorded value stands and the reported one is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelTxFactsConflict {
	/// The reported facts are about a different transaction altogether.
	Txid { recorded: Txid, incoming: Txid },
	/// The same output is reported with a different role or a different channel.
	Output { recorded: ChannelOutputFact, incoming: ChannelOutputFact },
	/// The transaction is reported as being something else than it is recorded as.
	SelfRole { recorded: TransactionType, incoming: TransactionType },
	/// This node's share of the transaction is reported differently than it is recorded.
	LocalFigures { recorded: LocalFundingFigures, incoming: LocalFundingFigures },
}

impl fmt::Display for ChannelTxFactsConflict {
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
		}
	}
}

#[cfg(test)]
mod tests {
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

	fn round_trip<T: Readable + Writeable + PartialEq + std::fmt::Debug>(object: &T) {
		let encoded = object.encode();
		let decoded: T = Readable::read(&mut &encoded[..]).expect("round trip");
		assert_eq!(&decoded, object);
	}

	fn full_facts() -> ChannelTxFacts {
		let channel = test_channel(1);
		ChannelTxFacts::new(test_txid(7))
			.with_outputs(&channel, Some(UserChannelId(42)), ChannelOutputRole::Funding, [0])
			.with_outputs(&channel, None, ChannelOutputRole::Anchor, [1])
			.with_outputs(&channel, None, ChannelOutputRole::Htlc, [2, 3])
			.with_outputs(&channel, None, ChannelOutputRole::Spendable, [4])
			.with_self_role(TransactionType::InteractiveFunding { channels: vec![channel.clone()] })
			.with_local_figures(LocalFundingFigures {
				funding_payment_id: PaymentId([9u8; 32]),
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(2_500),
				direction: PaymentDirection::Outbound,
			})
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
			Err(ChannelTxFactsConflict::Output { recorded, incoming }) => {
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
			Err(ChannelTxFactsConflict::Output { .. })
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
			Err(ChannelTxFactsConflict::SelfRole { recorded, incoming }) => {
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
		let recorded = ChannelTxFacts::new(txid).with_local_figures(figures.clone());
		let conflicting = ChannelTxFacts::new(txid)
			.with_local_figures(LocalFundingFigures { fee_paid_msat: Some(5_000), ..figures });

		assert!(matches!(
			recorded.merged_with(&conflicting),
			Err(ChannelTxFactsConflict::LocalFigures { .. })
		));
	}

	#[test]
	fn facts_about_another_transaction_are_rejected() {
		let recorded = ChannelTxFacts::new(test_txid(7));
		let other = ChannelTxFacts::new(test_txid(8));
		assert_eq!(
			recorded.merged_with(&other),
			Err(ChannelTxFactsConflict::Txid { recorded: test_txid(7), incoming: test_txid(8) })
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
			.merged_with(&ChannelTxFacts::new(txid).with_local_figures(figures.clone()))
			.expect("fills in")
			.expect("changed");
		assert_eq!(filled.local_figures, Some(figures.clone()));

		assert_eq!(
			filled.merged_with(&ChannelTxFacts::new(txid).with_local_figures(figures)),
			Ok(None)
		);
	}
}
