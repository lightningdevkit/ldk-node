// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Persistence of in-flight channel opens this node initiated, so an open LDK has not durably
//! learned of yet can be recognized — and its loss reported — after a restart.

use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::events::{ClosureReason, Event as LdkEvent};
use lightning::ln::channelmanager::PaymentId;

use crate::data_store::{StorableObject, UpdatableObject};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::payment::pending_payment_store::{
	OpenChannelIntent, PendingPaymentDetails, PendingPaymentDetailsUpdate,
};
use crate::types::{PendingPaymentStore, UserChannelId};
use crate::wallet::random_payment_id;
use crate::Error;

/// Tracks each channel open this node initiated through a persisted [`OpenChannelIntent`] for as
/// long as LDK is not guaranteed to remember the channel itself. LDK persists a channel only once
/// it is funded. An outbound channel whose `accept_channel` has not arrived when the peer
/// disconnects — which includes stopping the node — is kept in memory to be resumed, so it is
/// gone once the process restarts, with no event reporting it; one further along is closed with
/// `ClosureReason::DisconnectedPeer`, which LDK does persist and replay.
///
/// The intent is written before the open is handed to LDK, undone when LDK rejects the open
/// synchronously, and cleared once LDK reports the channel pending or closed. The entry carrying
/// the intent is the one wallet sync later files the funding payment under, so the payment id
/// returned to the caller identifies the funding payment from the start. An intent still on
/// record at the next startup identifies an open that was in flight when the node stopped:
/// [`Self::reconcile`] settles the ones LDK still lists and reports the others as lost.
pub(crate) struct OpenChannelTracker {
	pending_payment_store: Arc<PendingPaymentStore>,
	logger: Arc<Logger>,
}

/// A channel LDK lists, as [`OpenChannelTracker::reconcile`] matches the persisted intents
/// against it.
pub(crate) struct ListedChannel {
	pub counterparty_node_id: PublicKey,
	pub user_channel_id: UserChannelId,
	/// Whether the channel is funded. LDK persists a channel only once it is; an unfunded channel
	/// it lists is one it kept in memory across a disconnect, to resume it with the peer.
	pub funded: bool,
}

impl OpenChannelTracker {
	pub(crate) fn new(
		pending_payment_store: Arc<PendingPaymentStore>, logger: Arc<Logger>,
	) -> Self {
		Self { pending_payment_store, logger }
	}

	/// Persists `intent` under a fresh payment id — the id the funding payment will be recorded
	/// under — before the open is handed to LDK, so that no event about the channel can precede
	/// it. Returns the id.
	pub(crate) async fn persist_intent(
		&self, intent: OpenChannelIntent,
	) -> Result<PaymentId, Error> {
		let payment_id = random_payment_id();
		self.pending_payment_store
			.insert(PendingPaymentDetails::pending_open(payment_id, intent))
			.await?;
		Ok(payment_id)
	}

	/// Undoes an intent persisted for an open LDK then rejected synchronously, so that the next
	/// startup does not report an open that never started. Best effort: a failure is logged and
	/// the stale entry is left to be reported as a lost open.
	pub(crate) async fn discard_persisted_intent(
		&self, payment_id: PaymentId, intent: &OpenChannelIntent,
	) {
		let result = self
			.pending_payment_store
			.remove_if(&payment_id, |record| {
				record.details().is_none() && record.open_channel_intent() == Some(intent)
			})
			.await;
		if let Err(e) = result {
			log_error!(
				self.logger,
				"Failed to undo the intent of rejected channel open {}: a stale intent record may \
				be left behind: {}",
				intent.user_channel_id.0,
				e,
			);
		}
	}

	/// The payment id under which the funding of the live open of `user_channel_id` with
	/// `counterparty_node_id` is to be recorded, if such an open is on record.
	pub(crate) async fn funding_payment_id(
		&self, counterparty_node_id: PublicKey, user_channel_id: UserChannelId,
	) -> Option<PaymentId> {
		self.pending_payment_store
			.list_filter(|record| concerns_open(record, counterparty_node_id, user_channel_id))
			.await
			.first()
			.map(|record| record.id())
	}

	/// Clears the persisted intent of the open of `user_channel_id` with `counterparty_node_id`
	/// once LDK reported its channel pending — LDK persists the channel from then on — or closed.
	/// A record that tracks anything else stays with the intent cleared; one left with nothing to
	/// track is removed. Idempotent, so a replayed event doesn't settle anything twice. Every
	/// record of the open is attempted; the first failure is logged and returned, and the intent
	/// it left behind is settled, or reported again, by the next startup's [`Self::reconcile`].
	pub(crate) async fn settle(
		&self, counterparty_node_id: PublicKey, user_channel_id: UserChannelId,
	) -> Result<(), Error> {
		let records = self
			.pending_payment_store
			.list_filter(|record| concerns_open(record, counterparty_node_id, user_channel_id))
			.await;
		let mut first_error = None;
		for record in records {
			let payment_id = record.id();
			let result: Result<(), Error> = async {
				let mut remove_bare_record = false;
				let removal_flag = &mut remove_bare_record;
				self.pending_payment_store
					.mutate(&payment_id, move |existing| {
						let record = existing?;
						if !concerns_open(record, counterparty_node_id, user_channel_id) {
							return None;
						}
						let replacement = record_with_open_intent_cleared(record);
						// A bare intent record cannot be cleared in place; it is removed below.
						*removal_flag = replacement.is_none();
						replacement
					})
					.await?;
				if remove_bare_record {
					self.pending_payment_store
						.remove_if(&payment_id, |record| {
							record.details().is_none()
								&& record.candidates().is_empty()
								&& record.locked_rounds().is_empty()
								&& record.splice_intent().is_none()
								&& concerns_open(record, counterparty_node_id, user_channel_id)
						})
						.await?;
				}
				Ok(())
			}
			.await;
			if let Err(e) = result {
				log_error!(
					self.logger,
					"Failed to clear the persisted intent of channel open {} (payment {}): a stale \
					intent record may be left behind: {}",
					user_channel_id.0,
					payment_id,
					e,
				);
				first_error.get_or_insert(e);
			}
		}
		first_error.map_or(Ok(()), Err)
	}

	/// Reconciles the persisted intents against the channels LDK lists (`listed`), at startup
	/// after LDK replayed the events it persisted. An intent whose channel LDK lists funded is
	/// settled without a report: LDK persists the channel from then on and reports whatever
	/// becomes of it. One whose channel LDK lists unfunded is left alone: LDK has the channel in
	/// memory only, kept across a disconnect to resume it with the peer — the node was stopped
	/// and started again without the process restarting — and the event LDK reports about it in
	/// the end settles the intent. One whose channel LDK does not list is gone with no event: its
	/// intent is returned for the caller to report as a lost open, and settled by that report.
	///
	/// Nothing is reported unless the replay completed (`replay_complete`): an event the handler
	/// failed on stays queued in LDK along with every event behind it, and may be the
	/// `ChannelClosed` or `ChannelPending` of a recorded open. Reporting the open as lost would
	/// then duplicate, or contradict, LDK's own report once the background processor delivers it.
	/// The intents stay on record for that report, or for the next startup, to settle.
	pub(crate) async fn reconcile(
		&self, replay_complete: bool, listed: impl IntoIterator<Item = ListedChannel>,
	) -> Vec<OpenChannelIntent> {
		let records =
			self.pending_payment_store.list_filter(|p| p.open_channel_intent().is_some()).await;
		if records.is_empty() {
			return Vec::new();
		}
		if !replay_complete {
			log_info!(
				self.logger,
				"Leaving {} channel open intent(s) unreconciled: LDK's event replay did not \
				complete, so the events settling them may still be queued",
				records.len(),
			);
			return Vec::new();
		}

		let listed: Vec<ListedChannel> = listed.into_iter().collect();
		let mut lost = Vec::new();
		for record in records {
			let Some(intent) = record.open_channel_intent().cloned() else {
				continue;
			};
			match listed_channel(&intent, &listed) {
				Some(channel) if channel.funded => {
					log_info!(
						self.logger,
						"Channel open {} with {} is funded; settling its intent",
						intent.user_channel_id.0,
						intent.counterparty_node_id,
					);
					// A failure is logged by `settle`; the intent is left for the next startup.
					let _ = self.settle(intent.counterparty_node_id, intent.user_channel_id).await;
				},
				Some(_) => {
					log_info!(
						self.logger,
						"Channel open {} with {} is still being negotiated; leaving its intent",
						intent.user_channel_id.0,
						intent.counterparty_node_id,
					);
				},
				None => {
					log_info!(
						self.logger,
						"Channel open {} with {} did not survive the restart; reporting it as closed",
						intent.user_channel_id.0,
						intent.counterparty_node_id,
					);
					lost.push(intent);
				},
			}
		}
		lost
	}
}

/// Whether `record` carries the live intent of the open of `user_channel_id` with
/// `counterparty_node_id`.
fn concerns_open(
	record: &PendingPaymentDetails, counterparty_node_id: PublicKey, user_channel_id: UserChannelId,
) -> bool {
	record.open_channel_intent().is_some_and(|intent| {
		intent.counterparty_node_id == counterparty_node_id
			&& intent.user_channel_id == user_channel_id
	})
}

/// The channel LDK lists for the open `intent` records, if any. Both the counterparty and the
/// `user_channel_id` are matched: a counterparty may have several channels with this node, and
/// `user_channel_id` alone is only unique per node by convention.
fn listed_channel<'a>(
	intent: &OpenChannelIntent, listed: &'a [ListedChannel],
) -> Option<&'a ListedChannel> {
	listed.iter().find(|channel| {
		channel.counterparty_node_id == intent.counterparty_node_id
			&& channel.user_channel_id == intent.user_channel_id
	})
}

/// The event reporting the loss of the open `intent` records, shaped as the `ChannelClosed` LDK
/// would have reported had it closed the channel at the disconnect: the channel is only ever
/// known by its temporary id, was never funded, and never held a balance.
pub(crate) fn lost_open_close_event(intent: &OpenChannelIntent) -> LdkEvent {
	LdkEvent::ChannelClosed {
		channel_id: intent.temporary_channel_id,
		user_channel_id: intent.user_channel_id.0,
		reason: ClosureReason::DisconnectedPeer,
		counterparty_node_id: Some(intent.counterparty_node_id),
		channel_capacity_sats: None,
		channel_funding_txo: None,
		last_local_balance_msat: None,
	}
}

/// The replacement for a pending record whose open-channel intent is being dropped. A record
/// that still tracks something else — the funding payment wallet sync already filed under it —
/// keeps it with just the intent cleared. A record left with nothing to track has nothing to
/// keep and is left for the caller to remove (see [`OpenChannelTracker::settle`]).
fn record_with_open_intent_cleared(
	existing: &PendingPaymentDetails,
) -> Option<PendingPaymentDetails> {
	let mut record = existing.clone();
	let update = PendingPaymentDetailsUpdate {
		id: record.id(),
		payment_update: None,
		conflicting_txids: None,
		candidates: Vec::new(),
		splice_intent: None,
		open_channel_intent: Some(None),
	};
	record.update(update);
	(!record.is_empty()).then_some(record)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bitcoin::hashes::Hash;
	use bitcoin::Txid;
	use lightning::ln::types::ChannelId;

	use super::*;
	use crate::data_store::KeepAllEntries;
	use crate::io::test_utils::InMemoryStore;
	use crate::io::{
		PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use crate::payment::store::{ConfirmationStatus, PaymentDetails, PaymentKind};
	use crate::payment::{PaymentDirection, PaymentStatus};
	use crate::types::{DynStore, DynStoreWrapper};

	fn pending_payment_store() -> PendingPaymentStore {
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		PendingPaymentStore::new(
			Vec::new(),
			KeepAllEntries,
			PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
			PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
			store,
			Arc::new(Logger::new_log_facade()),
		)
	}

	fn counterparty() -> PublicKey {
		PublicKey::from_str("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
			.unwrap()
	}

	fn intent(user_channel_id: u128) -> OpenChannelIntent {
		OpenChannelIntent {
			counterparty_node_id: counterparty(),
			user_channel_id: UserChannelId(user_channel_id),
			temporary_channel_id: ChannelId([user_channel_id as u8; 32]),
		}
	}

	fn funding_payment(id: PaymentId) -> PaymentDetails {
		PaymentDetails::new(
			id,
			PaymentKind::Onchain {
				txid: Txid::from_byte_array([1u8; 32]),
				status: ConfirmationStatus::Unconfirmed,
				tx_type: None,
			},
			Some(1_000_000),
			Some(500),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	fn listed(
		counterparty_node_id: PublicKey, user_channel_id: u128, funded: bool,
	) -> ListedChannel {
		ListedChannel {
			counterparty_node_id,
			user_channel_id: UserChannelId(user_channel_id),
			funded,
		}
	}

	/// Settling and matching key on the counterparty and the `user_channel_id` together: another
	/// open to the same peer is a different open.
	#[test]
	fn an_intent_matches_only_the_channel_listed_under_its_counterparty_and_id() {
		let open = intent(7);
		assert!(listed_channel(&open, &[listed(counterparty(), 7, true)]).is_some());
		assert!(listed_channel(&open, &[listed(counterparty(), 8, true)]).is_none());
		let other = PublicKey::from_str(
			"02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
		)
		.unwrap();
		assert!(listed_channel(&open, &[listed(other, 7, true)]).is_none());
		assert!(listed_channel(&open, &[]).is_none());
	}

	/// A lost open is reported as the close LDK would have reported at the disconnect, under the
	/// identifiers the caller knows the channel by.
	#[test]
	fn a_lost_open_is_reported_as_a_disconnect_close() {
		let open = intent(7);
		match lost_open_close_event(&open) {
			LdkEvent::ChannelClosed {
				channel_id,
				user_channel_id,
				reason,
				counterparty_node_id,
				channel_capacity_sats,
				channel_funding_txo,
				last_local_balance_msat,
			} => {
				assert_eq!(channel_id, open.temporary_channel_id);
				assert_eq!(user_channel_id, 7);
				assert_eq!(reason, ClosureReason::DisconnectedPeer);
				assert_eq!(counterparty_node_id, Some(counterparty()));
				assert_eq!(channel_capacity_sats, None);
				assert_eq!(channel_funding_txo, None);
				assert_eq!(last_local_balance_msat, None);
			},
			event => panic!("unexpected event {:?}", event),
		}
	}

	/// A record that tracks the funding payment keeps it with the intent cleared; a bare one is
	/// removed. The open to the same peer under another `user_channel_id` is left alone.
	#[tokio::test]
	async fn settling_clears_only_the_intent_of_the_settled_open() {
		let store = Arc::new(pending_payment_store());
		let tracked_id = PaymentId([1u8; 32]);
		let mut tracked =
			PendingPaymentDetails::new(funding_payment(tracked_id), Vec::new(), Vec::new());
		tracked.open_channel_intent = Some(intent(7));
		store.insert(tracked).await.unwrap();
		let bare_id = PaymentId([2u8; 32]);
		store.insert(PendingPaymentDetails::pending_open(bare_id, intent(7))).await.unwrap();
		let other_id = PaymentId([3u8; 32]);
		store.insert(PendingPaymentDetails::pending_open(other_id, intent(8))).await.unwrap();

		let tracker =
			OpenChannelTracker::new(Arc::clone(&store), Arc::new(Logger::new_log_facade()));
		tracker.settle(counterparty(), UserChannelId(7)).await.unwrap();

		let tracked = store.get(&tracked_id).await.unwrap().expect("the tracked record stays");
		assert!(tracked.open_channel_intent().is_none());
		assert!(tracked.details().is_some());
		assert!(store.get(&bare_id).await.unwrap().is_none(), "a bare intent record is removed");
		let other = store.get(&other_id).await.unwrap().expect("the other open is untouched");
		assert_eq!(other.open_channel_intent(), Some(&intent(8)));

		// Settling again finds nothing to do.
		tracker.settle(counterparty(), UserChannelId(7)).await.unwrap();
		assert!(store.get(&tracked_id).await.unwrap().is_some());
	}

	/// The funding payment id of a live open is its record's id, and there is none for an open
	/// that was settled.
	#[tokio::test]
	async fn the_funding_payment_id_is_that_of_the_live_intent_record() {
		let store = Arc::new(pending_payment_store());
		let tracker =
			OpenChannelTracker::new(Arc::clone(&store), Arc::new(Logger::new_log_facade()));
		let id = tracker.persist_intent(intent(7)).await.unwrap();
		assert_eq!(tracker.funding_payment_id(counterparty(), UserChannelId(7)).await, Some(id));
		assert_eq!(tracker.funding_payment_id(counterparty(), UserChannelId(8)).await, None);

		tracker.discard_persisted_intent(id, &intent(7)).await;
		assert_eq!(tracker.funding_payment_id(counterparty(), UserChannelId(7)).await, None);
		assert!(store.get(&id).await.unwrap().is_none());
	}

	/// With LDK's replay incomplete, the intents are left as they are and nothing is reported.
	#[tokio::test]
	async fn reconcile_reports_nothing_while_the_replay_is_incomplete() {
		let store = Arc::new(pending_payment_store());
		let tracker =
			OpenChannelTracker::new(Arc::clone(&store), Arc::new(Logger::new_log_facade()));
		let id = tracker.persist_intent(intent(7)).await.unwrap();

		assert!(tracker.reconcile(false, []).await.is_empty());
		assert_eq!(
			store.get(&id).await.unwrap().and_then(|r| r.open_channel_intent().cloned()),
			Some(intent(7))
		);

		// With the replay complete and the channel listed funded, the open survived: its intent
		// is settled without a report.
		let survivor = tracker.persist_intent(intent(8)).await.unwrap();
		assert_eq!(
			tracker.reconcile(true, [listed(counterparty(), 8, true)]).await,
			vec![intent(7)],
			"only the open LDK no longer lists is reported",
		);
		assert!(store.get(&survivor).await.unwrap().is_none(), "a settled bare intent is removed");
		// The lost open is left on record for the report's `ChannelClosed` to settle.
		assert!(store.get(&id).await.unwrap().is_some());
	}

	/// An open whose channel LDK lists unfunded is still in flight — LDK kept it in memory across
	/// the node's stop and start — so it is neither settled nor reported.
	#[tokio::test]
	async fn reconcile_leaves_an_open_ldk_still_negotiates_alone() {
		let store = Arc::new(pending_payment_store());
		let tracker =
			OpenChannelTracker::new(Arc::clone(&store), Arc::new(Logger::new_log_facade()));
		let id = tracker.persist_intent(intent(7)).await.unwrap();

		assert!(tracker.reconcile(true, [listed(counterparty(), 7, false)]).await.is_empty());
		assert_eq!(
			store.get(&id).await.unwrap().and_then(|r| r.open_channel_intent().cloned()),
			Some(intent(7)),
			"the intent of an open still being negotiated stays",
		);
	}
}
