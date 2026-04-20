// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::types::ChannelId;

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::types::UserChannelId;

/// Persistent per-channel state tracked by LDK Node, keyed by [`UserChannelId`].
///
/// Durably stores channel flags at `ChannelPending` time so they remain accessible when the
/// channel closes, even after a restart or a [`ReplayEvent`]. The `Funded` variant is designed
/// to be extended with a `pending_splice` field in a future PR to support splice retry across
/// restarts and peer disconnects.
///
/// [`ReplayEvent`]: lightning::events::ReplayEvent
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelRecord {
	/// State for a live channel whose funding transaction exists.
	Funded {
		user_channel_id: UserChannelId,
		/// The node ID of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The channel's ID at the time the `ChannelPending` event fired.
		channel_id: ChannelId,
		/// Whether we opened the channel (outbound) or the counterparty did (inbound).
		is_outbound: bool,
		/// Whether the channel was publicly announced.
		is_announced: bool,
	},
}

impl_writeable_tlv_based_enum!(ChannelRecord,
	(0, Funded) => {
		(0, user_channel_id, required),
		(2, counterparty_node_id, required),
		(4, channel_id, required),
		(6, is_outbound, required),
		(8, is_announced, required),
	},
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelRecordUpdate {
	pub user_channel_id: UserChannelId,
}

impl StorableObjectUpdate<ChannelRecord> for ChannelRecordUpdate {
	fn id(&self) -> UserChannelId {
		self.user_channel_id
	}
}

impl StorableObject for ChannelRecord {
	type Id = UserChannelId;
	type Update = ChannelRecordUpdate;

	fn id(&self) -> UserChannelId {
		match self {
			ChannelRecord::Funded { user_channel_id, .. } => *user_channel_id,
		}
	}

	fn update(&mut self, _update: Self::Update) -> bool {
		// ChannelRecord fields are immutable once written in this version. Returning false
		// makes insert_or_update a no-op when the record already exists, ensuring idempotency
		// on ChannelPending replay.
		false
	}

	fn to_update(&self) -> Self::Update {
		ChannelRecordUpdate { user_channel_id: self.id() }
	}
}

impl StorableObjectId for UserChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0.to_be_bytes())
	}
}

#[cfg(test)]
mod tests {
	use lightning::ln::types::ChannelId;
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	fn make_record(is_outbound: bool, is_announced: bool) -> ChannelRecord {
		let user_channel_id = UserChannelId(42);
		// A valid compressed public key: prefix 0x02 followed by 32 bytes.
		let counterparty_node_id = PublicKey::from_slice(&[2u8; 33]).expect("valid pubkey");
		let channel_id = ChannelId([3u8; 32]);
		ChannelRecord::Funded {
			user_channel_id,
			counterparty_node_id,
			channel_id,
			is_outbound,
			is_announced,
		}
	}

	#[test]
	fn channel_record_roundtrips() {
		for (is_outbound, is_announced) in
			[(true, false), (false, true), (true, true), (false, false)]
		{
			let record = make_record(is_outbound, is_announced);
			let encoded = record.encode();
			let decoded = ChannelRecord::read(&mut &encoded[..]).expect("decode succeeds");
			assert_eq!(record, decoded);
			assert_eq!(decoded.id(), UserChannelId(42));
			assert!(matches!(
				decoded,
				ChannelRecord::Funded {
					is_outbound: dec_out,
					is_announced: dec_ann,
					..
				} if dec_out == is_outbound && dec_ann == is_announced
			));
		}
	}
}
