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
/// Tracks where a channel sits in its lifecycle. Each variant currently holds only the channel's
/// identity; the per-feature state and the transitions between lifecycle states are added by the
/// work that needs them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelRecord {
	/// A channel whose funding transaction has not yet been observed.
	Unfunded {
		user_channel_id: UserChannelId,
		counterparty_node_id: PublicKey,
		channel_id: ChannelId,
	},
	/// A channel whose funding transaction exists.
	Funded {
		user_channel_id: UserChannelId,
		counterparty_node_id: PublicKey,
		channel_id: ChannelId,
	},
	/// A channel that has been closed.
	Closed {
		user_channel_id: UserChannelId,
		counterparty_node_id: PublicKey,
		channel_id: ChannelId,
	},
}

impl_writeable_tlv_based_enum!(ChannelRecord,
	(0, Unfunded) => {
		(0, user_channel_id, required),
		(2, counterparty_node_id, required),
		(4, channel_id, required),
	},
	(2, Funded) => {
		(0, user_channel_id, required),
		(2, counterparty_node_id, required),
		(4, channel_id, required),
	},
	(4, Closed) => {
		(0, user_channel_id, required),
		(2, counterparty_node_id, required),
		(4, channel_id, required),
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
}

impl StorableObject for ChannelRecord {
	type Id = UserChannelId;
	type Update = ChannelRecordUpdate;

	fn id(&self) -> Self::Id {
		match self {
			ChannelRecord::Unfunded { user_channel_id, .. }
			| ChannelRecord::Funded { user_channel_id, .. }
			| ChannelRecord::Closed { user_channel_id, .. } => *user_channel_id,
		}
	}

	fn update(&mut self, _update: Self::Update) -> bool {
		// Records currently carry only the channel's identity, so there is nothing to mutate in
		// place. Lifecycle transitions are performed by replacing the record once a consumer of
		// this store exists.
		false
	}

	fn to_update(&self) -> Self::Update {
		Self::Update { user_channel_id: self.id() }
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

		for record in [
			ChannelRecord::Unfunded { user_channel_id, counterparty_node_id, channel_id },
			ChannelRecord::Funded { user_channel_id, counterparty_node_id, channel_id },
			ChannelRecord::Closed { user_channel_id, counterparty_node_id, channel_id },
		] {
			let encoded = record.encode();
			let decoded = ChannelRecord::read(&mut &encoded[..]).unwrap();
			assert_eq!(record, decoded);
			assert_eq!(decoded.id(), user_channel_id);
		}
	}
}
