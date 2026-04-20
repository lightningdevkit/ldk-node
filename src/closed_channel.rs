// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use bitcoin::OutPoint;
use lightning::events::ClosureReason;
use lightning::ln::msgs::DecodeError;
use lightning::ln::types::ChannelId;
use lightning::util::ser::{Readable, Writeable, Writer};
use lightning::{_init_and_read_len_prefixed_tlv_fields, write_tlv_fields};

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::types::UserChannelId;

/// Details of a closed channel.
///
/// Returned by [`Node::list_closed_channels`].
///
/// [`Node::list_closed_channels`]: crate::Node::list_closed_channels
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ClosedChannelDetails {
	/// The channel's ID at the time it was closed.
	pub channel_id: ChannelId,
	/// The local identifier of the channel.
	pub user_channel_id: UserChannelId,
	/// The node ID of the channel's counterparty.
	pub counterparty_node_id: Option<PublicKey>,
	/// The channel's funding transaction outpoint.
	pub funding_txo: Option<OutPoint>,
	/// The channel's capacity in satoshis.
	pub channel_capacity_sats: Option<u64>,
	/// Our local balance in millisatoshis at the time of channel closure.
	pub last_local_balance_msat: Option<u64>,
	/// Indicates whether we initiated the channel opening.
	///
	/// `true` if the channel was opened by us (outbound), `false` if opened by the counterparty
	/// (inbound). This will be `false` for channels opened prior to this field being tracked.
	pub is_outbound: bool,
	/// The reason for the channel closure.
	pub closure_reason: Option<ClosureReason>,
	/// The timestamp, in seconds since start of the UNIX epoch, when the channel was closed.
	pub closed_at: u64,
}

impl Writeable for ClosedChannelDetails {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		write_tlv_fields!(writer, {
			(0, self.channel_id, required),
			(2, self.user_channel_id, required),
			(4, self.counterparty_node_id, option),
			(6, self.funding_txo, option),
			(8, self.channel_capacity_sats, option),
			(10, self.last_local_balance_msat, option),
			(12, self.is_outbound, required),
			(14, self.closure_reason, upgradable_option),
			(16, self.closed_at, required),
		});
		Ok(())
	}
}

impl Readable for ClosedChannelDetails {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let unix_time_secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, channel_id, required),
			(2, user_channel_id, required),
			(4, counterparty_node_id, option),
			(6, funding_txo, option),
			(8, channel_capacity_sats, option),
			(10, last_local_balance_msat, option),
			(12, is_outbound, required),
			(14, closure_reason, upgradable_option),
			(16, closed_at, (default_value, unix_time_secs)),
		});
		Ok(ClosedChannelDetails {
			channel_id: channel_id.0.ok_or(DecodeError::InvalidValue)?,
			user_channel_id: user_channel_id.0.ok_or(DecodeError::InvalidValue)?,
			counterparty_node_id,
			funding_txo,
			channel_capacity_sats,
			last_local_balance_msat,
			is_outbound: is_outbound.0.ok_or(DecodeError::InvalidValue)?,
			closure_reason,
			closed_at: closed_at.0.ok_or(DecodeError::InvalidValue)?,
		})
	}
}

pub(crate) struct ClosedChannelDetailsUpdate(pub UserChannelId);

impl StorableObjectUpdate<ClosedChannelDetails> for ClosedChannelDetailsUpdate {
	fn id(&self) -> UserChannelId {
		self.0
	}
}

impl StorableObject for ClosedChannelDetails {
	type Id = UserChannelId;
	type Update = ClosedChannelDetailsUpdate;

	fn id(&self) -> UserChannelId {
		self.user_channel_id
	}

	fn update(&mut self, _update: Self::Update) -> bool {
		// Closed channel records are immutable once written.
		false
	}

	fn to_update(&self) -> Self::Update {
		ClosedChannelDetailsUpdate(self.user_channel_id)
	}
}

impl StorableObjectId for UserChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0.to_be_bytes())
	}
}
