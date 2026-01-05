// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lightning::ln::channelmanager::PaymentId;
use payjoin::persist::AsyncSessionPersister;
use payjoin::receive::v2::SessionEvent as ReceiverSessionEvent;

use crate::payment::payjoin::payjoin_session::{
	PayjoinSession, PayjoinSessionKind, SerializedSessionEvent,
};
use crate::types::PayjoinSessionStore;
use crate::Error;

pub(crate) struct KVStorePayjoinReceiverPersister {
	session_id: PaymentId,
	payjoin_session_store: Arc<PayjoinSessionStore>,
	/// For a new session.`None` when resuming a session that already exists.
	new_session: Option<PayjoinSession>,
}

impl KVStorePayjoinReceiverPersister {
	pub fn new(session_id: PaymentId, payjoin_session_store: Arc<PayjoinSessionStore>) -> Self {
		let new_session =
			PayjoinSession::new(session_id, PayjoinSessionKind::Receive { fallback_tx: None });

		Self { session_id, payjoin_session_store, new_session: Some(new_session) }
	}

	pub fn session_id(&self) -> PaymentId {
		self.session_id
	}

	pub async fn get_session(&self) -> Result<PayjoinSession, Error> {
		self.payjoin_session_store.get(&self.session_id).await?.ok_or(Error::InvalidPaymentId)
	}

	/// Reconstruct persister from existing session
	pub async fn from_session(
		session_id: PaymentId, payjoin_session_store: Arc<PayjoinSessionStore>,
	) -> Result<Self, Error> {
		if payjoin_session_store.get(&session_id).await?.is_none() {
			return Err(Error::InvalidPaymentId);
		}

		Ok(Self { session_id, payjoin_session_store, new_session: None })
	}
}

impl AsyncSessionPersister for KVStorePayjoinReceiverPersister {
	type SessionEvent = ReceiverSessionEvent;
	type InternalStorageError = Error;

	fn save_event(
		&self, event: Self::SessionEvent,
	) -> impl std::future::Future<Output = Result<(), Self::InternalStorageError>> + Send {
		async move {
			let event_bytes = serde_json::to_vec(&event).map_err(|_| Error::PersistenceFailed)?;

			let event = SerializedSessionEvent {
				event_bytes,
				created_at: SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.unwrap_or(Duration::from_secs(0))
					.as_secs(),
			};

			let new_session = self.new_session.clone();
			self.payjoin_session_store
				.mutate(&self.session_id, |existing| {
					// Create the record with its first event if it doesn't exist yet
					let mut session = existing.cloned().or(new_session)?;
					session.events.push(event);
					Some(session)
				})
				.await?
				.ok_or(Error::InvalidPaymentId)?;

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
			let session = self.get_session().await?;

			let events: Vec<ReceiverSessionEvent> = session
				.events
				.iter()
				.map(|e| serde_json::from_slice(&e.event_bytes))
				.collect::<Result<Vec<_>, _>>()
				.map_err(|_| Error::PersistenceFailed)?;

			Ok(Box::new(events.into_iter()) as Box<dyn Iterator<Item = Self::SessionEvent> + Send>)
		}
	}

	fn close(
		&self,
	) -> impl std::future::Future<Output = Result<(), Self::InternalStorageError>> + Send {
		async move {
			let mut session = self.get_session().await?;

			session.completed_at = Some(
				SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.unwrap_or(Duration::from_secs(0))
					.as_secs(),
			);

			self.payjoin_session_store.insert_or_update(session).await?;

			Ok(())
		}
	}
}
