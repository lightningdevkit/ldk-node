// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::secp256k1::PublicKey;
use bitcoin::TxOut;
use lightning::chain::transaction::OutPoint;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::types::UserChannelId;

/// The number of times a splice intent is resubmitted before it is abandoned and the failure is
/// surfaced to the user.
pub(crate) const MAX_SPLICE_ATTEMPTS: u8 = 3;

/// A user-initiated splice that has been handed to LDK but is not yet guaranteed to survive a
/// restart. LDK only persists a splice once its negotiation reaches `AwaitingSignatures`, so until
/// the new funding transaction locks we keep enough state to resubmit the splice ourselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpliceIntent {
	/// The channel's funding outpoint when the splice was initiated. It only changes once a splice
	/// locks, so a mismatch with the channel's current funding outpoint means the splice (or a
	/// replacement) completed and there is nothing left to resubmit.
	pub pre_splice_funding_txo: OutPoint,
	/// The contribution handed to [`ChannelManager::funding_contributed`], resubmitted verbatim.
	///
	/// [`ChannelManager::funding_contributed`]: lightning::ln::channelmanager::ChannelManager::funding_contributed
	pub contribution: FundingContribution,
	/// The parameters of the originating API call, used to rebuild a fresh contribution when the
	/// stored one has become stale (e.g., its feerate is no longer sufficient).
	pub kind: SpliceKind,
	/// The number of times the contribution has been resubmitted to LDK after the originating API
	/// call handed it off.
	pub attempts: u8,
}

impl_writeable_tlv_based!(SpliceIntent, {
	(0, pre_splice_funding_txo, required),
	(2, contribution, required),
	(4, kind, required),
	(6, attempts, required),
});

/// The parameters of the API call that initiated a splice.
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

/// Persistent per-channel state tracked by LDK Node, keyed by [`UserChannelId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelRecord {
	/// State for a channel whose funding transaction exists, currently limited to an in-flight
	/// splice intent. A record without a pending splice intent is removed from the store.
	Funded {
		user_channel_id: UserChannelId,
		counterparty_node_id: PublicKey,
		channel_id: ChannelId,
		pending_splice: Option<SpliceIntent>,
	},
}

impl ChannelRecord {
	pub(crate) fn pending_splice(&self) -> Option<&SpliceIntent> {
		match self {
			ChannelRecord::Funded { pending_splice, .. } => pending_splice.as_ref(),
		}
	}
}

impl_writeable_tlv_based_enum!(ChannelRecord,
	(0, Funded) => {
		(0, user_channel_id, required),
		(2, counterparty_node_id, required),
		(4, channel_id, required),
		(6, pending_splice, option),
	},
);

impl StorableObjectId for UserChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0.to_be_bytes())
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelRecordUpdate {
	pub user_channel_id: UserChannelId,
	pub pending_splice: Option<Option<SpliceIntent>>,
}

impl StorableObject for ChannelRecord {
	type Id = UserChannelId;
	type Update = ChannelRecordUpdate;

	fn id(&self) -> Self::Id {
		match self {
			ChannelRecord::Funded { user_channel_id, .. } => *user_channel_id,
		}
	}

	fn update(&mut self, update: Self::Update) -> bool {
		let mut updated = false;
		match self {
			ChannelRecord::Funded { pending_splice, .. } => {
				if let Some(new_pending_splice) = update.pending_splice {
					if *pending_splice != new_pending_splice {
						*pending_splice = new_pending_splice;
						updated = true;
					}
				}
			},
		}
		updated
	}

	fn to_update(&self) -> Self::Update {
		match self {
			ChannelRecord::Funded { user_channel_id, pending_splice, .. } => Self::Update {
				user_channel_id: *user_channel_id,
				pending_splice: Some(pending_splice.clone()),
			},
		}
	}
}

impl StorableObjectUpdate<ChannelRecord> for ChannelRecordUpdate {
	fn id(&self) -> <ChannelRecord as StorableObject>::Id {
		self.user_channel_id
	}
}

#[cfg(test)]
mod tests {
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	#[test]
	fn channel_record_is_serializable() {
		let user_channel_id = UserChannelId(42);
		let counterparty_node_id = bitcoin::secp256k1::PublicKey::from_slice(&[2u8; 33]).unwrap();
		let channel_id = ChannelId([3u8; 32]);
		let record = ChannelRecord::Funded {
			user_channel_id,
			counterparty_node_id,
			channel_id,
			pending_splice: None,
		};

		let encoded = record.encode();
		let decoded = ChannelRecord::read(&mut &encoded[..]).unwrap();
		assert_eq!(record, decoded);
		assert_eq!(decoded.id(), user_channel_id);
	}
}
