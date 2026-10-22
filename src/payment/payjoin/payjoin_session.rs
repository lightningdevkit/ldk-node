// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::Transaction;
use lightning::ln::channelmanager::PaymentId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectUpdate, UpdatableObject};

/// Represents a payjoin session with persisted events
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayjoinSession {
	/// Session identifier (uses PaymentId from PaymentDetails)
	pub session_id: PaymentId,

	/// The kind of session.
	pub kind: PayjoinSessionKind,

	/// Serialized session events
	pub events: Vec<SerializedSessionEvent>,

	/// Current status of the session
	pub status: PayjoinStatus,

	/// Unix timestamp of session completion (if completed)
	pub completed_at: Option<u64>,

	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	pub latest_update_timestamp: u64,
}

impl PayjoinSession {
	pub fn new(session_id: PaymentId, kind: PayjoinSessionKind) -> Self {
		let latest_update_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		Self {
			session_id,
			kind,
			events: Vec::new(),
			status: PayjoinStatus::Active,
			completed_at: None,
			latest_update_timestamp,
		}
	}

	/// Returns the fallback transaction to broadcast if the payjoin doesn't complete, if known.
	pub(crate) fn fallback_tx(&self) -> Option<&Transaction> {
		match &self.kind {
			PayjoinSessionKind::Receive { fallback_tx } => fallback_tx.as_ref(),
		}
	}

	/// Records the fallback transaction to broadcast if the payjoin doesn't complete.
	pub(crate) fn set_fallback_tx(&mut self, tx: Transaction) {
		match &mut self.kind {
			PayjoinSessionKind::Receive { fallback_tx } => *fallback_tx = Some(tx),
		}
	}
}

impl_writeable_tlv_based!(PayjoinSession, {
	(0, session_id, required),
	(2, kind, required),
	(4, events, required_vec),
	(6, status, required),
	(8, completed_at, option),
	(10, latest_update_timestamp, (default_value, 0u64)),
});

/// The direction-specific data of a [`PayjoinSession`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayjoinSessionKind {
	/// We are receiving a payjoin.
	Receive {
		/// The sender's original transaction, broadcast if the payjoin doesn't complete.
		fallback_tx: Option<Transaction>,
	},
}

impl_writeable_tlv_based_enum!(PayjoinSessionKind,
	(0, Receive) => {
		(0, fallback_tx, option),
	}
);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PayjoinStatus {
	/// The session is active
	Active,
	/// The session has completed successfully
	Completed,
	/// The session has failed
	Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializedSessionEvent {
	/// Serialized event bytes.
	pub event_bytes: Vec<u8>,
	/// Unix timestamp of when the event occurred
	pub created_at: u64,
}

impl_writeable_tlv_based!(SerializedSessionEvent, {
	(0, event_bytes, required),
	(2, created_at, required),
});

impl_writeable_tlv_based_enum!(PayjoinStatus,
	(0, Active) => {},
	(2, Completed) => {},
	(4, Failed) => {}
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PayjoinSessionUpdate {
	pub session_id: PaymentId,
	pub kind: Option<PayjoinSessionKind>,
	pub events: Option<Vec<SerializedSessionEvent>>,
	pub status: Option<PayjoinStatus>,
	pub completed_at: Option<Option<u64>>,
}

impl From<&PayjoinSession> for PayjoinSessionUpdate {
	fn from(value: &PayjoinSession) -> Self {
		Self {
			session_id: value.session_id,
			kind: Some(value.kind.clone()),
			events: Some(value.events.clone()),
			status: Some(value.status),
			completed_at: Some(value.completed_at),
		}
	}
}

impl StorableObject for PayjoinSession {
	type Id = PaymentId;

	fn id(&self) -> Self::Id {
		self.session_id
	}
}

impl UpdatableObject for PayjoinSession {
	type Update = PayjoinSessionUpdate;

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(
			self.session_id, update.session_id,
			"We should only ever override data for the same id"
		);

		let mut updated = false;

		macro_rules! update_if_necessary {
			($val:expr, $update:expr) => {
				if $val != $update {
					$val = $update;
					updated = true;
				}
			};
		}

		if let Some(kind) = update.kind {
			update_if_necessary!(self.kind, kind);
		}
		if let Some(events_opt) = &update.events {
			update_if_necessary!(self.events, events_opt.clone());
		}
		if let Some(status_opt) = update.status {
			update_if_necessary!(self.status, status_opt);
		}
		if let Some(completed_at_opt) = update.completed_at {
			update_if_necessary!(self.completed_at, completed_at_opt);
		}

		if updated {
			self.latest_update_timestamp = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or(Duration::from_secs(0))
				.as_secs();
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

impl StorableObjectUpdate<PayjoinSession> for PayjoinSessionUpdate {
	fn id(&self) -> <PayjoinSession as StorableObject>::Id {
		self.session_id
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::absolute::LockTime;
	use bitcoin::transaction::Version;
	use bitcoin::{Amount, ScriptBuf, TxIn, TxOut};
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	fn dummy_tx() -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![TxIn::default()],
			output: vec![TxOut {
				value: Amount::from_sat(10_000),
				script_pubkey: ScriptBuf::new(),
			}],
		}
	}

	#[test]
	fn receive_session_round_trips() {
		let mut session = PayjoinSession::new(
			PaymentId([7; 32]),
			PayjoinSessionKind::Receive { fallback_tx: None },
		);
		assert_eq!(session.fallback_tx(), None);

		session.set_fallback_tx(dummy_tx());
		session.events.push(SerializedSessionEvent { event_bytes: vec![1, 2, 3], created_at: 42 });
		session.status = PayjoinStatus::Completed;
		session.completed_at = Some(43);

		let decoded = PayjoinSession::read(&mut &session.encode()[..]).unwrap();
		assert_eq!(decoded, session);
		assert_eq!(decoded.fallback_tx(), Some(&dummy_tx()));
	}
}
