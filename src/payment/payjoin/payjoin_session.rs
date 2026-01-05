// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::{OutPoint, Transaction, Txid};
use lightning::ln::channelmanager::PaymentId;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use crate::data_store::{StorableObject, StorableObjectUpdate};

/// Represents a payjoin session with persisted events
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayjoinSession {
	/// Session identifier (uses PaymentId from PaymentDetails)
	pub session_id: PaymentId,

	/// Direction of the payjoin (Send or Receive)
	pub direction: PayjoinDirection,

	/// HPKE public key of receiver (only for sender sessions)
	pub receiver_pubkey: Option<Vec<u8>>,

	/// The amount transferred.
	pub amount_msat: Option<u64>,

	/// The fee rate in satoshis per kilo-weight-unit
	pub fee_rate_kwu: u64,

	/// The fees that were paid for this payment.
	pub fee_paid_msat: Option<u64>,

	/// The transaction identifier of this payment.
	/// Will be None for Receive sessions until the session is completed and the transaction is known.
	pub txid: Option<Txid>,

	/// Serialized session events as JSON strings
	pub events: Vec<SerializedSessionEvent>,

	/// The fallback transaction (if any) that the sender created for a receive session.
	/// This is broadcasted if the payjoin transacion fails.
	pub fallback_tx: Option<Transaction>,

	/// A list of inputs that were seen before in this session.
	/// This is used to detect if the sender is reusing inputs across multiple attempts (only for receiver sessions).
	pub inputs_seen: Vec<OutPoint>,

	/// Current status of the session
	pub status: PayjoinStatus,

	/// Unix timestamp of session completion (if completed)
	pub completed_at: Option<u64>,

	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	pub latest_update_timestamp: u64,
}

impl PayjoinSession {
	pub fn new(
		session_id: PaymentId, direction: PayjoinDirection, receiver_pubkey: Option<Vec<u8>>,
		amount_msat: Option<u64>, fee_rate_kwu: u64, fee_paid_msat: Option<u64>,
		txid: Option<Txid>, fallback_tx: Option<Transaction>,
	) -> Self {
		let latest_update_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		Self {
			session_id,
			direction,
			receiver_pubkey,
			amount_msat,
			fee_rate_kwu,
			fee_paid_msat,
			txid,
			events: Vec::new(),
			fallback_tx,
			inputs_seen: Vec::new(),
			status: PayjoinStatus::Active,
			completed_at: None,
			latest_update_timestamp,
		}
	}
}

impl_writeable_tlv_based!(PayjoinSession, {
	(0, session_id, required),
	(2, direction, required),
	(4, receiver_pubkey, option),
	(6, amount_msat, option),
	(8, fee_rate_kwu, required),
	(10, fee_paid_msat, option),
	(12, txid, option),
	(14, events, required_vec),
	(16, fallback_tx, option),
	(18, inputs_seen, optional_vec),
	(20, status, required),
	(22, completed_at, option),
	(24, latest_update_timestamp, (default_value, 0u64)),
});

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PayjoinDirection {
	/// The session is for sending a payment
	Send,
	/// The session is for receiving a payment
	Receive,
}

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
	/// JSON representation of the event
	pub event_json: String,
	/// Unix timestamp of when the event occurred
	pub created_at: u64,
}

impl_writeable_tlv_based!(SerializedSessionEvent, {
	(0, event_json, required),
	(2, created_at, required),
});

impl_writeable_tlv_based_enum!(PayjoinDirection,
	(0, Send) => {},
	(2, Receive) => {}
);

impl_writeable_tlv_based_enum!(PayjoinStatus,
	(0, Active) => {},
	(2, Completed) => {},
	(4, Failed) => {}
);

/// Represents a payjoin session with persisted events
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PayjoinSessionUpdate {
	pub session_id: PaymentId,
	pub receiver_pubkey: Option<Option<Vec<u8>>>,
	pub fee_paid_msat: Option<Option<u64>>,
	pub txid: Option<Option<Txid>>,
	pub events: Option<Vec<SerializedSessionEvent>>,
	pub fallback_tx: Option<Option<Transaction>>,
	pub inputs_seen: Option<Vec<OutPoint>>,
	pub status: Option<PayjoinStatus>,
	pub completed_at: Option<Option<u64>>,
}

impl From<&PayjoinSession> for PayjoinSessionUpdate {
	fn from(value: &PayjoinSession) -> Self {
		Self {
			session_id: value.session_id,
			receiver_pubkey: Some(value.receiver_pubkey.clone()),
			fee_paid_msat: Some(value.fee_paid_msat),
			txid: Some(value.txid),
			events: Some(value.events.clone()),
			fallback_tx: Some(value.fallback_tx.clone()),
			inputs_seen: Some(value.inputs_seen.clone()),
			status: Some(value.status),
			completed_at: Some(value.completed_at),
		}
	}
}

impl StorableObject for PayjoinSession {
	type Id = PaymentId;
	type Update = PayjoinSessionUpdate;

	fn id(&self) -> Self::Id {
		self.session_id
	}

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

		if let Some(receiver_pubkey_opt) = &update.receiver_pubkey {
			update_if_necessary!(self.receiver_pubkey, receiver_pubkey_opt.clone());
		}
		if let Some(fee_paid_msat_opt) = update.fee_paid_msat {
			update_if_necessary!(self.fee_paid_msat, fee_paid_msat_opt);
		}
		if let Some(txid_opt) = update.txid {
			update_if_necessary!(self.txid, txid_opt);
		}
		if let Some(events_opt) = &update.events {
			update_if_necessary!(self.events, events_opt.clone());
		}
		if let Some(fallback_tx_opt) = update.fallback_tx {
			update_if_necessary!(self.fallback_tx, fallback_tx_opt);
		}
		if let Some(txids_input_seen_before_opt) = update.inputs_seen {
			update_if_necessary!(self.inputs_seen, txids_input_seen_before_opt);
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
