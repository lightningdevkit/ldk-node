// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Retrying user-initiated splices that LDK dropped before durably recording them.

use std::ops::Deref;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, OutPoint};
use lightning::events::NegotiationFailureReason;
use lightning::ln::channel_state::{SpliceCandidateDetails, SpliceCandidateStatus};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;

use crate::data_store::StorableObject;
use crate::event::{Event, EventQueue};
use crate::fee_estimator::{
	max_funding_feerate, ConfirmationTarget, FeeEstimator, OnchainFeeEstimator,
};
use crate::logger::{log_error, log_info, LdkLogger};
use crate::payment::pending_payment_store::{
	PendingPaymentDetails, PendingPaymentDetailsUpdate, SpliceIntent, SpliceKind,
	MAX_SPLICE_ATTEMPTS,
};
use crate::payment::store::PaymentDetails;
use crate::payment::PaymentStatus;
use crate::types::{ChannelManager, PaymentStore, PendingPaymentStore, UserChannelId, Wallet};
use crate::Error;

/// The action to take on a `SpliceNegotiationFailed` for a splice intent we track, decided purely
/// from the failure `reason` and the intent's attempt count so the decision matrix can be
/// unit-tested without a live channel. A failure for a splice we don't track is surfaced directly
/// (see [`SpliceRetrier::on_negotiation_failed`]) and never reaches here.
#[derive(Debug, PartialEq, Eq)]
enum RetryDecision {
	/// Give up: clear the intent and surface the failure to the user.
	Abandon,
	/// Resubmit the stored contribution unchanged (a transient failure such as a disconnect).
	ResubmitStored,
	/// Rebuild a fresh contribution from the original parameters (the stored one went stale).
	Rebuild,
}

fn decide_retry(reason: &NegotiationFailureReason, attempts: u8) -> RetryDecision {
	if !reason.is_retriable() || attempts >= MAX_SPLICE_ATTEMPTS {
		return RetryDecision::Abandon;
	}
	match reason {
		// The stored contribution is still valid after a transient failure.
		NegotiationFailureReason::PeerDisconnected | NegotiationFailureReason::Unknown => {
			RetryDecision::ResubmitStored
		},
		// The remaining retriable reasons (`FeeRateTooLow`, `ContributionInvalid`) mean the stored
		// contribution went stale.
		_ => RetryDecision::Rebuild,
	}
}

/// Resubmits user-initiated splices that LDK dropped before durably recording them.
///
/// LDK only persists a splice once its negotiation reaches `AwaitingSignatures`, and it abandons an
/// earlier negotiation whenever the peer disconnects (which includes restarting the node). The
/// splice entry points persist a [`SpliceIntent`] before handing the contribution to LDK; this type
/// drives that intent back into [`ChannelManager::funding_contributed`] until the splice either
/// locks (clearing the intent) or fails for a reason retrying cannot address.
///
/// Resubmitting does not require the peer to be connected: LDK holds on to the contribution and
/// initiates quiescence once the peer reconnects.
///
/// [`ChannelManager::funding_contributed`]: lightning::ln::channelmanager::ChannelManager::funding_contributed
pub(crate) struct SpliceRetrier<L: Deref + Clone>
where
	L::Target: LdkLogger,
{
	channel_manager: Arc<ChannelManager>,
	wallet: Arc<Wallet>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	pending_payment_store: Arc<PendingPaymentStore>,
	payment_store: Arc<PaymentStore>,
	event_queue: Arc<EventQueue<L>>,
	logger: L,
}

impl<L: Deref + Clone> SpliceRetrier<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, wallet: Arc<Wallet>,
		fee_estimator: Arc<OnchainFeeEstimator>, pending_payment_store: Arc<PendingPaymentStore>,
		payment_store: Arc<PaymentStore>, event_queue: Arc<EventQueue<L>>, logger: L,
	) -> Self {
		Self {
			channel_manager,
			wallet,
			fee_estimator,
			pending_payment_store,
			payment_store,
			event_queue,
			logger,
		}
	}

	/// Reconciles persisted splice intents against live channel state. Run once at startup to pick
	/// up splices LDK dropped before durably recording them — including those lost to a crash before
	/// LDK persisted anything.
	pub(crate) async fn reconcile(&self) {
		let records = self.pending_payment_store.list_filter(|p| p.splice_intent().is_some());
		for record in records {
			let id = record.id();
			let has_payment = self.has_payment(&record);
			let Some(intent) = record.splice_intent().cloned() else {
				continue;
			};

			let channel = self
				.channel_manager
				.list_channels_with_counterparty(&intent.counterparty_node_id)
				.into_iter()
				.find(|c| c.user_channel_id == intent.user_channel_id.0);
			let channel = match channel {
				Some(channel) => channel,
				None => {
					// The channel is gone; there is nothing to splice anymore.
					self.clear_intent(id, has_payment).await;
					continue;
				},
			};

			if channel.funding_txo != Some(intent.pre_splice_funding_txo) {
				// The funding moved on, so the splice (or a replacement) locked.
				self.clear_intent(id, has_payment).await;
				continue;
			}

			let candidates = channel
				.splice_details
				.as_ref()
				.map(|details| details.candidates.as_slice())
				.unwrap_or(&[]);
			match decide_reconcile(&intent, candidates) {
				ReconcileDecision::Keep => continue,
				ReconcileDecision::Abandon => {
					self.abandon(id, has_payment, &intent).await;
					continue;
				},
				ReconcileDecision::Resubmit => {},
			}

			if intent.attempts >= MAX_SPLICE_ATTEMPTS {
				self.abandon(id, has_payment, &intent).await;
				continue;
			}

			log_info!(
				self.logger,
				"Resubmitting splice for channel {} with counterparty {}",
				channel.channel_id,
				intent.counterparty_node_id,
			);
			let counterparty_node_id = intent.counterparty_node_id;
			let _ = self.submit(id, &channel.channel_id, &counterparty_node_id, intent).await;
		}
	}

	/// Persists the incremented attempt count and hands the contribution back to LDK. The count is
	/// persisted first so that a crash mid-submission cannot lead to unbounded retries.
	async fn submit(
		&self, id: PaymentId, channel_id: &ChannelId, counterparty_node_id: &PublicKey,
		mut intent: SpliceIntent,
	) -> Result<(), Error> {
		intent.attempts += 1;
		let contribution = intent.contribution.clone();
		let update = PendingPaymentDetailsUpdate {
			id,
			payment_update: None,
			conflicting_txids: None,
			candidates: Vec::new(),
			splice_intent: Some(Some(intent)),
		};
		self.pending_payment_store.update(update).await?;

		self.channel_manager
			.funding_contributed(channel_id, counterparty_node_id, contribution, None)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to resubmit splice for channel {} with counterparty {}: {:?}",
					channel_id,
					counterparty_node_id,
					e,
				);
				Error::ChannelSplicingFailed
			})
	}

	/// Drops a splice intent: removes a record with no classified funding payment behind it
	/// entirely, or keeps the record — with the intent cleared — when a payment exists, so the
	/// payment keeps graduating.
	async fn clear_intent(&self, id: PaymentId, has_payment: bool) {
		if has_payment {
			let _ = self
				.pending_payment_store
				.mutate(&id, |existing| {
					record_with_intent_cleared(existing, self.payment_store.get(&id))
				})
				.await;
		} else {
			let _ = self.pending_payment_store.remove(&id).await;
		}
	}

	/// Gives up on a splice intent and surfaces the failure to the user.
	async fn abandon(&self, id: PaymentId, has_payment: bool, intent: &SpliceIntent) {
		log_error!(
			self.logger,
			"Abandoning splice for channel {} with counterparty {}",
			intent.channel_id,
			intent.counterparty_node_id,
		);
		self.clear_intent(id, has_payment).await;
		let event = Event::SpliceNegotiationFailed {
			channel_id: intent.channel_id,
			user_channel_id: intent.user_channel_id,
			counterparty_node_id: intent.counterparty_node_id,
		};
		if let Err(e) = self.event_queue.add_event(event).await {
			log_error!(self.logger, "Failed to push to event queue: {}", e);
		}
	}

	/// Applies a `SpliceNegotiationFailed` to any matching splice intent, retrying recoverable
	/// failures. Returns whether the failure should be surfaced to the user (i.e. the splice is
	/// given up on).
	pub(crate) async fn on_negotiation_failed(
		&self, user_channel_id: UserChannelId, reason: NegotiationFailureReason,
		contribution: Option<FundingContribution>,
	) -> bool {
		let Some(record) = self.record_for_channel(user_channel_id) else {
			return true;
		};
		let id = record.id();
		let has_payment = self.has_payment(&record);
		let Some(intent) = record.splice_intent().cloned() else {
			return true;
		};

		// Only act on failures of the splice we are tracking. A mismatch means the failure concerns
		// some other attempt (e.g. a stale event replayed after a newer splice was initiated).
		if contribution.as_ref() != Some(&intent.contribution) {
			return true;
		}

		let channel_id = intent.channel_id;
		let counterparty_node_id = intent.counterparty_node_id;
		match decide_retry(&reason, intent.attempts) {
			RetryDecision::Abandon => {
				self.clear_intent(id, has_payment).await;
				true
			},
			RetryDecision::ResubmitStored => {
				// The same contribution remains valid; resubmit it. Skip if LDK already has a splice
				// in flight for this channel (e.g. the startup reconciler resubmitted first).
				if self.channel_manager.splice_channel(&channel_id, &counterparty_node_id).is_err()
				{
					return false;
				}
				log_info!(
					self.logger,
					"Resubmitting splice for channel {} with counterparty {} after a recoverable failure",
					channel_id,
					counterparty_node_id,
				);
				let _ = self.submit(id, &channel_id, &counterparty_node_id, intent).await;
				false
			},
			RetryDecision::Rebuild => {
				// The stored contribution went stale; rebuild a fresh one from the original params.
				match self
					.rebuild_contribution(&channel_id, &counterparty_node_id, &intent.kind)
					.await
				{
					Ok(contribution) => {
						log_info!(
							self.logger,
							"Resubmitting rebuilt splice for channel {} with counterparty {}",
							channel_id,
							counterparty_node_id,
						);
						let mut intent = intent;
						intent.contribution = contribution;
						let _ = self.submit(id, &channel_id, &counterparty_node_id, intent).await;
						false
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Abandoning splice for channel {}: failed to rebuild contribution: {:?}",
							channel_id,
							e,
						);
						self.clear_intent(id, has_payment).await;
						true
					},
				}
			},
		}
	}

	/// Clears any splice intent made obsolete by a newly locked funding transaction.
	pub(crate) async fn on_channel_ready(
		&self, user_channel_id: UserChannelId, funding_txo: Option<OutPoint>,
	) {
		let Some(record) = self.record_for_channel(user_channel_id) else {
			return;
		};
		let id = record.id();
		let has_payment = self.has_payment(&record);
		let Some(intent) = record.splice_intent() else {
			return;
		};
		// Only clear an intent that predates the locked funding. An intent whose pre-splice outpoint
		// still matches the newly locked funding was created after this lock and is still pending.
		let clear = match funding_txo {
			Some(funding_txo) => {
				intent.pre_splice_funding_txo.into_bitcoin_outpoint() != funding_txo
			},
			None => false,
		};
		if clear {
			self.clear_intent(id, has_payment).await;
		}
	}

	/// Clears any splice intent for a closed channel, as there is nothing left to splice.
	pub(crate) async fn on_channel_closed(&self, user_channel_id: UserChannelId) {
		if let Some(record) = self.record_for_channel(user_channel_id) {
			let has_payment = self.has_payment(&record);
			self.clear_intent(record.id(), has_payment).await;
		}
	}

	/// Returns the pending record carrying a splice intent for the given channel, if any.
	fn record_for_channel(&self, user_channel_id: UserChannelId) -> Option<PendingPaymentDetails> {
		self.pending_payment_store
			.list_filter(|p| {
				p.splice_intent().is_some_and(|i| i.user_channel_id == user_channel_id)
			})
			.into_iter()
			.next()
	}

	/// Whether a classified funding payment exists for this record: its own details, or a
	/// still-`Pending` record in the payment store — the latter without the former means a crash
	/// landed between classification's two store writes, so the entry is still pre-broadcast
	/// while the payment record already exists.
	fn has_payment(&self, record: &PendingPaymentDetails) -> bool {
		record.details().is_some()
			|| self
				.payment_store
				.get(&record.id())
				.is_some_and(|details| details.status == PaymentStatus::Pending)
	}

	/// Builds a fresh contribution from the parameters of the originating API call, mirroring the
	/// corresponding [`Node`] method.
	///
	/// [`Node`]: crate::Node
	async fn rebuild_contribution(
		&self, channel_id: &ChannelId, counterparty_node_id: &PublicKey, kind: &SpliceKind,
	) -> Result<FundingContribution, Error> {
		let template = self
			.channel_manager
			.splice_channel(channel_id, counterparty_node_id)
			.map_err(|_| Error::ChannelSplicingFailed)?;

		let est_feerate = self.fee_estimator.estimate_fee_rate(ConfirmationTarget::ChannelFunding);
		let max_feerate = max_funding_feerate(est_feerate);
		let feerate = match template.min_rbf_feerate() {
			Some(min_rbf_feerate) if min_rbf_feerate <= max_feerate => {
				est_feerate.max(min_rbf_feerate)
			},
			_ => est_feerate,
		};

		match kind {
			SpliceKind::In { amount_sats } => template
				.splice_in(
					Amount::from_sat(*amount_sats),
					feerate,
					max_feerate,
					Arc::clone(&self.wallet),
				)
				.await
				.map_err(|_| Error::ChannelSplicingFailed),
			SpliceKind::Out { outputs } => template
				.splice_out(outputs.clone(), feerate, max_feerate)
				.map_err(|_| Error::ChannelSplicingFailed),
			SpliceKind::Rbf {} => template
				.rbf_prior_contribution(None, max_feerate, Arc::clone(&self.wallet))
				.await
				.map_err(|_| Error::ChannelSplicingFailed),
		}
	}
}

/// The replacement for a pending record whose splice intent is being dropped. A tracked record
/// keeps its payment details with just the intent cleared. A pre-broadcast record whose payment
/// was classified but never mirrored into the pending store — a crash between classification's
/// two store writes — is promoted so the payment keeps graduating and its txids stay mapped; a
/// payment no longer `Pending` graduated already and must not be re-indexed, so its entry is
/// left alone for removal.
fn record_with_intent_cleared(
	existing: Option<&PendingPaymentDetails>, recorded: Option<PaymentDetails>,
) -> Option<PendingPaymentDetails> {
	match existing {
		Some(PendingPaymentDetails::PendingSplice { .. }) => recorded
			.filter(|details| details.status == PaymentStatus::Pending)
			.map(|details| PendingPaymentDetails::tracked(details, Vec::new(), Vec::new(), None)),
		Some(tracked @ PendingPaymentDetails::Tracked { .. }) => {
			let update = PendingPaymentDetailsUpdate {
				id: tracked.id(),
				payment_update: None,
				conflicting_txids: None,
				candidates: Vec::new(),
				splice_intent: Some(None),
			};
			let mut updated = tracked.clone();
			updated.update(update).then_some(updated)
		},
		None => None,
	}
}

/// What [`SpliceRetrier::reconcile`] should do with a persisted intent, decided from the splice
/// rounds LDK reports on the channel.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileDecision {
	/// Leave the intent in place without resubmitting.
	Keep,
	/// Hand the stored contribution back to LDK.
	Resubmit,
	/// Give up: drop the intent and surface the failure to the user.
	Abandon,
}

/// Decides the startup action for a persisted intent from the channel's [`SpliceDetails`]
/// candidates.
///
/// [`SpliceDetails`]: lightning::ln::channel_state::SpliceDetails
fn decide_reconcile(
	intent: &SpliceIntent, candidates: &[SpliceCandidateDetails],
) -> ReconcileDecision {
	// A round short of `Negotiated` is one LDK still drives on its own: only `AwaitingSignatures`
	// survives a restart, and LDK resumes the signature exchange itself on reconnect.
	let in_flight = candidates
		.iter()
		.any(|candidate| !matches!(candidate.status, SpliceCandidateStatus::Negotiated { .. }));
	if in_flight {
		return ReconcileDecision::Keep;
	}

	// LDK persists a splice once negotiated, so a negotiated candidate carrying our contribution
	// means the intent was carried out — unless the intent was a fee bump at a higher feerate
	// than negotiated. Rounds carry our contribution forward, so the newest one holds it.
	let negotiated = candidates.iter().rev().find_map(|candidate| candidate.contribution.as_ref());
	match (&intent.kind, negotiated) {
		(SpliceKind::Rbf {}, Some(prior)) => {
			if prior.feerate() < intent.contribution.feerate() {
				ReconcileDecision::Resubmit
			} else {
				ReconcileDecision::Keep
			}
		},
		// The splice to bump is gone entirely; surface rather than guess.
		(SpliceKind::Rbf {}, None) => ReconcileDecision::Abandon,
		(_, Some(_)) => ReconcileDecision::Keep,
		(_, None) => ReconcileDecision::Resubmit,
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bitcoin::hashes::Hash;
	use bitcoin::Txid;
	use lightning::chain::transaction::OutPoint;

	use lightning::ln::funding::FundingContribution;

	use super::*;
	use crate::payment::pending_payment_store::{
		test_funding_contribution, test_funding_contribution_with_feerate, PendingPaymentDetails,
	};
	use crate::payment::store::{ConfirmationStatus, PaymentDetails, PaymentKind};
	use crate::payment::{PaymentDirection, PaymentStatus};
	use crate::types::UserChannelId;

	#[test]
	fn decide_retry_matrix() {
		use NegotiationFailureReason::*;

		// A non-retriable reason gives up regardless of attempts.
		assert_eq!(decide_retry(&LocallyCanceled, 0), RetryDecision::Abandon);
		// Retriable, but the resubmission budget is exhausted -> give up.
		assert_eq!(decide_retry(&PeerDisconnected, MAX_SPLICE_ATTEMPTS), RetryDecision::Abandon);
		// Transient failures resubmit the stored contribution.
		assert_eq!(decide_retry(&PeerDisconnected, 0), RetryDecision::ResubmitStored);
		assert_eq!(decide_retry(&Unknown, MAX_SPLICE_ATTEMPTS - 1), RetryDecision::ResubmitStored);
		// A stale contribution is rebuilt from the original parameters.
		assert_eq!(decide_retry(&FeeRateTooLow, 0), RetryDecision::Rebuild);
		assert_eq!(decide_retry(&ContributionInvalid, 0), RetryDecision::Rebuild);
	}

	fn test_intent() -> SpliceIntent {
		SpliceIntent {
			user_channel_id: UserChannelId(42),
			counterparty_node_id: PublicKey::from_str(
				"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
			)
			.unwrap(),
			channel_id: ChannelId([7u8; 32]),
			pre_splice_funding_txo: OutPoint { txid: Txid::from_byte_array([3u8; 32]), index: 0 },
			contribution: test_funding_contribution(),
			kind: SpliceKind::Rbf {},
			attempts: 0,
		}
	}

	fn payment_details(id: PaymentId, status: PaymentStatus) -> PaymentDetails {
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
			status,
		)
	}

	/// A crash between classification's two store writes leaves the pending entry pre-broadcast
	/// while the classified payment record exists. Dropping the intent must promote the entry
	/// rather than remove it, so the payment keeps graduating and its txids stay mapped.
	#[test]
	fn intent_clearing_promotes_a_pre_broadcast_record_over_a_classified_payment() {
		let id = PaymentId([9u8; 32]);
		let existing = PendingPaymentDetails::pending_splice(id, test_intent());
		let recorded = payment_details(id, PaymentStatus::Pending);

		let replacement = record_with_intent_cleared(Some(&existing), Some(recorded.clone()));
		let replacement = replacement.expect("the entry must be promoted, not removed");
		assert_eq!(replacement.details(), Some(&recorded));
		assert!(replacement.splice_intent().is_none());
	}

	/// A payment that already advanced beyond `Pending` graduated and lost its pending entry;
	/// promotion must not re-index it.
	#[test]
	fn intent_clearing_does_not_reindex_an_advanced_payment() {
		let id = PaymentId([9u8; 32]);
		let existing = PendingPaymentDetails::pending_splice(id, test_intent());
		let recorded = payment_details(id, PaymentStatus::Succeeded);
		assert!(record_with_intent_cleared(Some(&existing), Some(recorded)).is_none());
	}

	/// A tracked record keeps its payment details; only the intent is cleared.
	#[test]
	fn intent_clearing_keeps_a_tracked_record() {
		let id = PaymentId([9u8; 32]);
		let details = payment_details(id, PaymentStatus::Pending);
		let existing = PendingPaymentDetails::tracked(
			details.clone(),
			Vec::new(),
			Vec::new(),
			Some(test_intent()),
		);

		let replacement = record_with_intent_cleared(Some(&existing), Some(details.clone()));
		let replacement = replacement.expect("the entry must survive with its intent cleared");
		assert_eq!(replacement.details(), Some(&details));
		assert!(replacement.splice_intent().is_none());
	}

	fn negotiated_candidate(contribution: Option<FundingContribution>) -> SpliceCandidateDetails {
		SpliceCandidateDetails {
			contribution,
			status: SpliceCandidateStatus::Negotiated {
				txid: Txid::from_byte_array([9u8; 32]),
				new_channel_value_satoshis: 100_000,
			},
		}
	}

	/// While any round is short of `Negotiated`, LDK drives the splice itself; the intent stays
	/// untouched until it settles.
	#[test]
	fn reconcile_keeps_the_intent_while_ldk_drives_a_round() {
		let intent = test_intent();
		let in_flight = SpliceCandidateDetails {
			contribution: Some(test_funding_contribution()),
			status: SpliceCandidateStatus::AwaitingSignatures {
				is_initiator: true,
				funding_feerate_sat_per_1000_weight: 253,
				new_channel_value_satoshis: 100_000,
				txid: Txid::from_byte_array([9u8; 32]),
			},
		};
		assert_eq!(decide_reconcile(&intent, &[in_flight]), ReconcileDecision::Keep);
	}

	/// A negotiated candidate carrying our contribution means the intent was carried out. This
	/// holds on zero-conf channels too, where the `splice_channel` probe this replaced reported
	/// no prior contribution and a splice-in would have been resubmitted as a duplicate.
	#[test]
	fn reconcile_trusts_a_negotiated_contribution() {
		let mut intent = test_intent();
		intent.kind = SpliceKind::In { amount_sats: 10_000 };
		let negotiated = [negotiated_candidate(Some(test_funding_contribution()))];
		assert_eq!(decide_reconcile(&intent, &negotiated), ReconcileDecision::Keep);
	}

	/// With no contribution of ours in LDK — no splice at all, or only a counterparty round — the
	/// original splice is resubmitted, while a fee bump has nothing left to replace and gives up.
	#[test]
	fn reconcile_resubmits_when_ldk_holds_no_contribution() {
		let mut intent = test_intent();
		intent.kind = SpliceKind::In { amount_sats: 10_000 };
		assert_eq!(decide_reconcile(&intent, &[]), ReconcileDecision::Resubmit);
		let counterparty_only = [negotiated_candidate(None)];
		assert_eq!(decide_reconcile(&intent, &counterparty_only), ReconcileDecision::Resubmit);

		intent.kind = SpliceKind::Rbf {};
		assert_eq!(decide_reconcile(&intent, &[]), ReconcileDecision::Abandon);
		let counterparty_only = [negotiated_candidate(None)];
		assert_eq!(decide_reconcile(&intent, &counterparty_only), ReconcileDecision::Abandon);
	}

	/// A fee bump resubmits only when it improves on the negotiated feerate, judged against the
	/// newest round since rounds carry the contribution forward.
	#[test]
	fn reconcile_resubmits_a_bump_only_at_a_higher_feerate() {
		let mut intent = test_intent();
		intent.contribution = test_funding_contribution_with_feerate(500);

		let lower = [negotiated_candidate(Some(test_funding_contribution_with_feerate(253)))];
		assert_eq!(decide_reconcile(&intent, &lower), ReconcileDecision::Resubmit);

		let higher = [negotiated_candidate(Some(test_funding_contribution_with_feerate(1000)))];
		assert_eq!(decide_reconcile(&intent, &higher), ReconcileDecision::Keep);

		let bumped_meanwhile = [
			negotiated_candidate(Some(test_funding_contribution_with_feerate(253))),
			negotiated_candidate(Some(test_funding_contribution_with_feerate(1000))),
		];
		assert_eq!(decide_reconcile(&intent, &bumped_meanwhile), ReconcileDecision::Keep);
	}
}
