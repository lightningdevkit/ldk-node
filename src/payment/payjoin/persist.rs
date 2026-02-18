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

use lightning::util::ser::Readable;
use lightning::{io::Cursor, ln::channelmanager::PaymentId, util::persist::KVStoreSync};

use crate::io::{
	PAYJOIN_SESSION_STORE_PRIMARY_NAMESPACE, PAYJOIN_SESSION_STORE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::payment::payjoin::payjoin_session::{PayjoinDirection, PayjoinSession};
use crate::types::{DynStore, PayjoinSessionStore};

use payjoin::persist::AsyncSessionPersister;
use payjoin::receive::v2::SessionEvent as ReceiverSessionEvent;

pub(crate) struct KVStorePayjoinReceiverPersister {
	session_id: PaymentId,
	kv_store: Arc<PayjoinSessionStore>,
}

impl KVStorePayjoinReceiverPersister {
	pub fn new(
		session_id: PaymentId, amount_msat: Option<u64>, kv_store: Arc<DynStore>,
		logger: Arc<Logger>,
	) -> Result<Self, Error> {
		let sessions = Self::load_all_sessions(&kv_store, &logger)?;
		let data_store = Arc::new(PayjoinSessionStore::new(
			sessions,
			PAYJOIN_SESSION_STORE_PRIMARY_NAMESPACE.to_string(),
			PAYJOIN_SESSION_STORE_SECONDARY_NAMESPACE.to_string(),
			kv_store,
			logger,
		));

		let session = PayjoinSession::new(session_id, PayjoinDirection::Receive, None, amount_msat);

		data_store.insert(session)?;

		Ok(Self { session_id, kv_store: data_store })
	}

	pub(super) fn session_id(&self) -> PaymentId {
		self.session_id
	}

	pub(super) fn get_session(&self) -> Option<PayjoinSession> {
		self.kv_store.get(&self.session_id)
	}

	/// Reconstruct persister from existing session
	pub fn from_session(
		session_id: PaymentId, kv_store: Arc<DynStore>, logger: Arc<Logger>,
	) -> Result<Self, Error> {
		let sessions = Self::load_all_sessions(&kv_store, &logger)?;
		let data_store = Arc::new(PayjoinSessionStore::new(
			sessions,
			PAYJOIN_SESSION_STORE_PRIMARY_NAMESPACE.to_string(),
			PAYJOIN_SESSION_STORE_SECONDARY_NAMESPACE.to_string(),
			kv_store,
			logger,
		));

		if data_store.get(&session_id).is_none() {
			return Err(Error::InvalidPaymentId);
		}

		Ok(Self { session_id, kv_store: data_store })
	}

	/// Load all sessions from KV store
	fn load_all_sessions(
		kv_store: &Arc<DynStore>, logger: &Arc<Logger>,
	) -> Result<Vec<PayjoinSession>, Error> {
		let keys = KVStoreSync::list(
			&**kv_store,
			PAYJOIN_SESSION_STORE_PRIMARY_NAMESPACE,
			PAYJOIN_SESSION_STORE_SECONDARY_NAMESPACE,
		)
		.map_err(|e| {
			log_error!(logger, "Failed to list payjoin sessions: {:?}", e);
			Error::PersistenceFailed
		})?;

		let mut sessions = Vec::new();
		for key in keys {
			match KVStoreSync::read(
				&**kv_store,
				PAYJOIN_SESSION_STORE_PRIMARY_NAMESPACE,
				PAYJOIN_SESSION_STORE_SECONDARY_NAMESPACE,
				&key,
			) {
				Ok(data) => {
					let mut reader = Cursor::new(&data[..]);
					match PayjoinSession::read(&mut reader) {
						Ok(session) => sessions.push(session),
						Err(e) => {
							log_error!(
								logger,
								"Failed to deserialize PayjoinSession for key {}: {:?}. Skipping corrupted session.",
								key, e
							);
							continue;
						},
					}
				},
				Err(e) => {
					log_error!(
						logger,
						"Failed to read PayjoinSession data for key {}: {:?}",
						key,
						e
					);
					continue;
				},
			}
		}

		Ok(sessions)
	}

	/// Get all active Receiver session IDs
	pub fn get_active_session_ids(
		kv_store: Arc<DynStore>, logger: Arc<Logger>,
	) -> Result<Vec<PaymentId>, Error> {
		let sessions = Self::load_all_sessions(&kv_store, &logger)?;
		Ok(sessions
			.into_iter()
			.filter(|s| {
				s.direction == PayjoinDirection::Receive && s.status == PayjoinStatus::Active
			})
			.map(|s| s.session_id)
			.collect())
	}

	/// Get all inactive Receiver sessions (for cleanup)
	pub fn get_inactive_sessions(
		kv_store: Arc<DynStore>, logger: Arc<Logger>,
	) -> Result<Vec<(PaymentId, u64)>, Error> {
		let sessions = Self::load_all_sessions(&kv_store, &logger)?;
		Ok(sessions
			.into_iter()
			.filter(|s| {
				s.direction == PayjoinDirection::Receive
					&& s.status != PayjoinStatus::Active
					&& s.completed_at.is_some()
			})
			.map(|s| (s.session_id, s.completed_at.unwrap()))
			.collect())
	}
}

impl AsyncSessionPersister for KVStorePayjoinReceiverPersister {
	type SessionEvent = ReceiverSessionEvent;
	type InternalStorageError = Error;

	fn save_event(
		&self, event: Self::SessionEvent,
	) -> impl std::future::Future<Output = Result<(), Self::InternalStorageError>> + Send {
		async move {
			let mut session = self.kv_store.get(&self.session_id).ok_or(Error::InvalidPaymentId)?;

			let event_json = serde_json::to_string(&event).map_err(|_| Error::PersistenceFailed)?;

			session.events.push(SerializedSessionEvent {
				event_json,
				created_at: SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.unwrap_or(Duration::from_secs(0))
					.as_secs(),
			});

			self.kv_store.insert_or_update(session)?;

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
			let session = self.kv_store.get(&self.session_id).ok_or(Error::InvalidPaymentId)?;

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
			let mut session = self.kv_store.get(&self.session_id).ok_or(Error::InvalidPaymentId)?;

			session.completed_at = Some(now());
			session.status = PayjoinStatus::Completed;

			self.kv_store.insert_or_update(session)?;

			Ok(())
		}
	}
}

// Helper function for timestamp
fn now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs()
}
