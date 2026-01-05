// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::payment::payjoin::payjoin_session::{PayjoinStatus, SerializedSessionEvent};
use crate::Error;
use std::{
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{OutPoint, Transaction, Txid};
use lightning::ln::channelmanager::PaymentId;

use crate::payment::payjoin::payjoin_session::{PayjoinDirection, PayjoinSession};
use crate::types::PayjoinSessionStore;

use payjoin::persist::AsyncSessionPersister;
use payjoin::receive::v2::SessionEvent as ReceiverSessionEvent;

pub(crate) struct KVStorePayjoinReceiverPersister {
	session_id: PaymentId,
	payjoin_session_store: Arc<PayjoinSessionStore>,
}

impl KVStorePayjoinReceiverPersister {
	pub fn new(
		session_id: PaymentId, amount_msat: Option<u64>,
		payjoin_session_store: Arc<PayjoinSessionStore>, fee_rate_kwu: u64,
		fee_paid_msat: Option<u64>, txid: Option<Txid>, fallback_tx: Option<Transaction>,
	) -> Result<Self, Error> {
		let session = PayjoinSession::new(
			session_id,
			PayjoinDirection::Receive,
			None,
			amount_msat,
			fee_rate_kwu,
			fee_paid_msat,
			txid,
			fallback_tx,
		);

		payjoin_session_store.insert(session)?;

		Ok(Self { session_id, payjoin_session_store })
	}

	pub(super) fn session_id(&self) -> PaymentId {
		self.session_id
	}

	pub(super) fn get_session(&self) -> Option<PayjoinSession> {
		self.payjoin_session_store.get(&self.session_id)
	}

	/// Reconstruct persister from existing session
	pub fn from_session(
		session_id: PaymentId, payjoin_session_store: Arc<PayjoinSessionStore>,
	) -> Result<Self, Error> {
		if payjoin_session_store.get(&session_id).is_none() {
			return Err(Error::InvalidPaymentId);
		}

		Ok(Self { session_id, payjoin_session_store })
	}

	pub fn insert_input_seen(&self, input: OutPoint) -> Result<bool, Error> {
		let mut session = self.get_session().ok_or(Error::InvalidPaymentId)?;
		if session.inputs_seen.contains(&input) {
			return Ok(true);
		}
		session.inputs_seen.push(input);
		self.payjoin_session_store.insert_or_update(session)?;
		Ok(false)
	}
}

impl AsyncSessionPersister for KVStorePayjoinReceiverPersister {
	type SessionEvent = ReceiverSessionEvent;
	type InternalStorageError = Error;

	fn save_event(
		&self, event: Self::SessionEvent,
	) -> impl std::future::Future<Output = Result<(), Self::InternalStorageError>> + Send {
		async move {
			let mut session =
				self.payjoin_session_store.get(&self.session_id).ok_or(Error::InvalidPaymentId)?;

			let event_json = serde_json::to_string(&event).map_err(|_| Error::PersistenceFailed)?;

			session.events.push(SerializedSessionEvent {
				event_json,
				created_at: SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.unwrap_or(Duration::from_secs(0))
					.as_secs(),
			});

			self.payjoin_session_store.insert_or_update(session)?;

			Ok(())
		}
	}

	fn load(
		&self,
	) -> impl std::future::Future<
		Output = Result<
			Box<dyn Iterator<Item = Self::SessionEvent> + Send>,
			Self::InternalStorageError,
		>,
	> + Send {
		async move {
			let session =
				self.payjoin_session_store.get(&self.session_id).ok_or(Error::InvalidPaymentId)?;

			let events: Vec<ReceiverSessionEvent> = session
				.events
				.iter()
				.map(|e| serde_json::from_str(&e.event_json))
				.collect::<Result<Vec<_>, _>>()
				.map_err(|_| Error::PersistenceFailed)?;

			Ok(Box::new(events.into_iter()) as Box<dyn Iterator<Item = Self::SessionEvent> + Send>)
		}
	}

	fn close(
		&self,
	) -> impl std::future::Future<Output = Result<(), Self::InternalStorageError>> + Send {
		async move {
			let mut session =
				self.payjoin_session_store.get(&self.session_id).ok_or(Error::InvalidPaymentId)?;

			session.completed_at = Some(now());
			session.status = PayjoinStatus::Completed;

			self.payjoin_session_store.insert_or_update(session)?;

			Ok(())
		}
	}
}

// Helper function for timestamp
fn now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs()
}
