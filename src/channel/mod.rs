// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Persistence of in-flight user-initiated splices, so a splice LDK has not durably learned of
//! yet can be recognized — and whatever it reserved recovered — after a restart.

use std::sync::Arc;

use bitcoin::absolute::LockTime;
use bitcoin::secp256k1::PublicKey;
use bitcoin::transaction::Version;
use bitcoin::{OutPoint, Transaction, TxIn};
use lightning::chain::transaction::OutPoint as LdkOutPoint;
use lightning::ln::channel_state::{SpliceCandidateDetails, SpliceCandidateStatus};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;

use crate::data_store::StorableObject;
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::payment::pending_payment_store::{
	PendingPaymentDetails, PendingPaymentDetailsUpdate, SpliceIntent, SpliceKind,
};
use crate::payment::store::PaymentDetails;
use crate::payment::PaymentStatus;
use crate::types::{ChannelManager, PaymentStore, PendingPaymentStore};
use crate::wallet::{random_payment_id, SignedFundingRetraction, Wallet};
use crate::Error;

/// Whether two contributions describe the same splice attempt. LDK may adjust a contribution
/// during negotiation — the quiescence tie-breaker rebuilds the acceptor's copy at a fresh
/// feerate, touching only its fee fields and change value — so fees and feerates do not identify
/// an attempt. Its inputs and outputs do: they are what the user asked to move. Contributions
/// carrying neither (channel-balance-only attempts) fall back to full equality.
fn is_same_splice(a: &FundingContribution, b: &FundingContribution) -> bool {
	if a.inputs().is_empty()
		&& a.outputs().is_empty()
		&& b.inputs().is_empty()
		&& b.outputs().is_empty()
	{
		return a == b;
	}
	a.inputs().iter().map(|i| i.outpoint()).eq(b.inputs().iter().map(|i| i.outpoint()))
		&& a.outputs() == b.outputs()
}

/// Tracks each user-initiated splice through a persisted [`SpliceIntent`] for as long as LDK is
/// not guaranteed to remember the splice itself: LDK only persists a splice once its negotiation
/// reaches `AwaitingSignatures`, and it abandons an in-progress negotiation whenever the peer
/// disconnects — which includes stopping the node.
///
/// The intent is written before the contribution is handed to LDK, undone when LDK rejects the
/// hand-off synchronously, and cleared once the splice locks, its failure is surfaced, or its
/// channel closes. The record exists for recovery, not retry: a splice still recorded at the
/// next startup identifies one that was in flight when the node stopped, so [`Self::reconcile`]
/// can release anything it still reserves, and events about the splice can be described in
/// terms of the original request.
pub(crate) struct SpliceTracker {
	channel_manager: Arc<ChannelManager>,
	wallet: Arc<Wallet>,
	pending_payment_store: Arc<PendingPaymentStore>,
	payment_store: Arc<PaymentStore>,
	/// Serializes [`Self::submit`]'s persist-and-hand-off sequence with
	/// [`Self::on_negotiation_failed`]'s settling of the intent. Without it, the failure event of
	/// a synchronously rejected hand-off could settle the just-written intent while `submit` is
	/// still deciding whether to keep it.
	submit_lock: tokio::sync::Mutex<()>,
	logger: Arc<Logger>,
}

impl SpliceTracker {
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, wallet: Arc<Wallet>,
		pending_payment_store: Arc<PendingPaymentStore>, payment_store: Arc<PaymentStore>,
		logger: Arc<Logger>,
	) -> Self {
		Self {
			channel_manager,
			wallet,
			pending_payment_store,
			payment_store,
			submit_lock: tokio::sync::Mutex::new(()),
			logger,
		}
	}

	/// Reconciles the persisted splice intents against live channel state, releasing whatever the
	/// wallet still holds for splices that did not survive the restart. LDK only persists a
	/// splice once its negotiation reaches `AwaitingSignatures`, so a splice lost earlier leaves
	/// no trace in LDK — the intent record is what recognizes the loss. Run once at startup,
	/// before background chain syncing and event processing start, so nothing can act on the
	/// stale reservations first.
	///
	/// Recovery is silent: no failure event is fabricated for a splice lost this way, since the
	/// initiating call already returned and the channel simply shows no pending splice anymore.
	pub(crate) async fn reconcile(&self) {
		let records = self.pending_payment_store.list_filter(|p| p.splice_intent().is_some()).await;
		for record in records {
			let payment_id = record.id();
			let Some(intent) = record.splice_intent().cloned() else {
				continue;
			};

			let channel = self
				.channel_manager
				.list_channels_with_counterparty(&intent.counterparty_node_id)
				.into_iter()
				.find(|c| c.channel_id == intent.channel_id);
			let Some(channel) = channel else {
				// The channel is gone; there is nothing to splice anymore.
				log_info!(
					self.logger,
					"Dropping the recorded splice of closed channel {} with counterparty {}",
					intent.channel_id,
					intent.counterparty_node_id,
				);
				self.release_contribution(intent.channel_id, &intent.contribution).await;
				self.clear_persisted_intent(payment_id, |i| *i == intent).await;
				continue;
			};

			if channel.funding_txo != Some(intent.pre_splice_funding_txo) {
				// The funding moved on while the node was down: the recorded splice, a
				// replacement, or a counterparty splice locked — the same situation a live lock
				// event resolves, so resolve it the same way.
				self.on_channel_ready(
					intent.counterparty_node_id,
					intent.channel_id,
					channel.funding_txo.map(|txo| txo.into_bitcoin_outpoint()),
				)
				.await;
				continue;
			}

			let candidates = channel
				.splice_details
				.as_ref()
				.map(|details| details.candidates.as_slice())
				.unwrap_or(&[]);
			match decide_reconcile(candidates) {
				ReconcileDecision::Keep => {
					// A kept record may still reserve more than LDK's surviving rounds use —
					// extras a fee bump lost with the restart had reserved. Release the
					// difference.
					let extras = unclaimed_inputs(&intent.contribution, candidates);
					if let Err(e) = self.wallet.unlock_outpoints(&extras).await {
						log_error!(
							self.logger,
							"Failed to release unused splice inputs on channel {}: {}",
							intent.channel_id,
							e,
						);
					}
				},
				ReconcileDecision::Lost => {
					log_info!(
						self.logger,
						"Dropping a splice on channel {} with counterparty {} that did not survive \
						the restart",
						intent.channel_id,
						intent.counterparty_node_id,
					);
					self.release_contribution(intent.channel_id, &intent.contribution).await;
					self.clear_persisted_intent(payment_id, |i| *i == intent).await;
				},
			}
		}
	}

	/// Persists a user-initiated splice as an intent and hands its contribution to
	/// [`ChannelManager::funding_contributed`]. The intent — and any wallet state staged on the
	/// splice's behalf — is durable before the hand-off, so no splice is ever in flight without a
	/// persisted record of it. A newer splice supersedes whatever intent its channel carried: at
	/// most one splice is ever in flight per channel, and a fee bump replaces the splice it bumps.
	///
	/// On any failure the persisted intent is undone and the error returned for the caller to
	/// surface. A failure before the hand-off also releases whatever the wallet holds for the
	/// contribution; a synchronous rejection leaves that to the `DiscardFunding` event LDK queues.
	///
	/// [`ChannelManager::funding_contributed`]: lightning::ln::channelmanager::ChannelManager::funding_contributed
	pub(crate) async fn submit(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
		pre_splice_funding_txo: LdkOutPoint, contribution: FundingContribution, kind: SpliceKind,
	) -> Result<(), Error> {
		let _guard = self.submit_lock.lock().await;
		let intent = SpliceIntent {
			counterparty_node_id,
			channel_id,
			pre_splice_funding_txo,
			contribution: contribution.clone(),
			kind,
		};
		// A splice whose intent cannot be persisted is not attempted at all, rather than
		// attempted without restart coverage.
		let (payment_id, restore) = match self.persist_intent(intent).await {
			Ok(persisted) => persisted,
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to persist the splice intent for channel {} with counterparty {}: {:?}",
					channel_id,
					counterparty_node_id,
					e,
				);
				self.release_contribution(channel_id, &contribution).await;
				return Err(e);
			},
		};
		// Flush wallet state staged on the splice's behalf (e.g. input locks) only now that the
		// intent record is durable: whatever the wallet holds for a splice must never outlive the
		// record through which a later startup would release it.
		if let Err(e) = self.wallet.persist_staged().await {
			log_error!(
				self.logger,
				"Failed to persist staged wallet state for splicing channel {} with counterparty \
				{}: {:?}",
				channel_id,
				counterparty_node_id,
				e,
			);
			self.discard_persisted_intent(&payment_id, restore).await;
			self.release_contribution(channel_id, &contribution).await;
			return Err(e);
		}
		if let Err(e) = self.channel_manager.funding_contributed(
			&channel_id,
			&counterparty_node_id,
			contribution,
			None,
		) {
			log_error!(
				self.logger,
				"LDK rejected the splice contribution for channel {} with counterparty {}: {:?}",
				channel_id,
				counterparty_node_id,
				e,
			);
			// LDK returns the contribution through a `DiscardFunding` event, whose handling
			// releases whatever the wallet holds for it.
			self.discard_persisted_intent(&payment_id, restore).await;
			return Err(Error::ChannelSplicingFailed);
		}
		Ok(())
	}

	/// Releases everything the wallet may still hold for a contribution that is going nowhere:
	/// its inputs are unlocked and its would-be transaction is canceled, freeing the addresses of
	/// its change and splice-out outputs.
	async fn release_contribution(
		&self, channel_id: ChannelId, contribution: &FundingContribution,
	) {
		let inputs: Vec<OutPoint> =
			contribution.inputs().iter().map(|input| input.outpoint()).collect();
		if let Err(e) = self.wallet.unlock_outpoints(&inputs).await {
			log_error!(
				self.logger,
				"Failed to release the inputs of a splice contribution on channel {}: {}",
				channel_id,
				e,
			);
		}
		let tx = Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: inputs
				.into_iter()
				.map(|previous_output| TxIn { previous_output, ..TxIn::default() })
				.collect(),
			output: contribution
				.outputs()
				.iter()
				.chain(contribution.change_output())
				.cloned()
				.collect(),
		};
		if let Err(e) = self.wallet.cancel_tx(tx).await {
			log_error!(
				self.logger,
				"Failed to release the outputs of a splice contribution on channel {}: {}",
				channel_id,
				e,
			);
		}
	}

	/// Persists `intent` before its contribution is handed to LDK, outliving a restart that —
	/// until the negotiation reaches `AwaitingSignatures` — LDK's own state does not.
	///
	/// Reuses the channel's existing splice intent record when one is present — so a splice and
	/// its later fee bumps share one [`PaymentId`] and at most one intent ever exists per
	/// channel, which `Wallet::find_splice_payment_id` relies on — otherwise generates a fresh
	/// id. Returns the id and, for restoring on a rejected hand-off, `None` when a fresh record
	/// was created or `Some(prior)` when an existing record's intent was replaced.
	async fn persist_intent(
		&self, intent: SpliceIntent,
	) -> Result<(PaymentId, Option<Option<SpliceIntent>>), Error> {
		let existing = self
			.pending_payment_store
			.list_filter(|p| {
				p.splice_intent().is_some_and(|i| {
					i.channel_id == intent.channel_id
						&& i.counterparty_node_id == intent.counterparty_node_id
				})
			})
			.await
			.into_iter()
			.next();
		match existing {
			Some(record) => {
				let payment_id = record.id();
				let prior = record.splice_intent().cloned();
				self.pending_payment_store
					.update(PendingPaymentDetailsUpdate {
						id: payment_id,
						payment_update: None,
						conflicting_txids: None,
						candidates: Vec::new(),
						splice_intent: Some(Some(intent)),
					})
					.await?;
				Ok((payment_id, Some(prior)))
			},
			None => {
				let payment_id = random_payment_id();
				self.pending_payment_store
					.insert(PendingPaymentDetails::pending_splice(payment_id, intent))
					.await?;
				Ok((payment_id, None))
			},
		}
	}

	/// Undoes a splice intent persisted for a hand-off that then failed before LDK took the
	/// splice: restores an existing record's prior intent, or removes a freshly created record.
	async fn discard_persisted_intent(
		&self, payment_id: &PaymentId, restore: Option<Option<SpliceIntent>>,
	) {
		let result = match restore {
			Some(prior) => self
				.pending_payment_store
				.update(PendingPaymentDetailsUpdate {
					id: *payment_id,
					payment_update: None,
					conflicting_txids: None,
					candidates: Vec::new(),
					splice_intent: Some(prior),
				})
				.await
				.map(|_| ()),
			None => self.pending_payment_store.remove(payment_id).await,
		};
		if let Err(e) = result {
			log_error!(
				self.logger,
				"Failed to undo the intent of rejected splice payment {}: a stale intent record \
				may be left behind: {}",
				payment_id,
				e,
			);
		}
	}

	/// Clears the persisted intent behind a splice that settled — it locked, its failure was
	/// surfaced, or its channel closed — but only while `still_applies` holds for the stored
	/// intent: a mismatch means a newer splice took over the channel's record in the meantime,
	/// and its intent must stay. A record with no classified funding payment behind it is removed
	/// entirely; otherwise the record stays — with the intent cleared — so the payment keeps
	/// graduating.
	async fn clear_persisted_intent<F: Fn(&SpliceIntent) -> bool>(
		&self, payment_id: PaymentId, still_applies: F,
	) {
		let still_applies = &still_applies;
		let result: Result<(), Error> = async {
			let mut remove_bare_record = false;
			// The `move` closure would capture a plain `bool` by copy, so hand it a reference; the
			// borrow ends with the mutate's future, before the flag is read below.
			let removal_flag = &mut remove_bare_record;
			let payment_store = Arc::clone(&self.payment_store);
			self.pending_payment_store
				.mutate_async(&payment_id, move |existing| async move {
					let Some(record) = existing else {
						return Ok(None);
					};
					match record.splice_intent() {
						Some(intent) if still_applies(intent) => {},
						_ => return Ok(None),
					}
					let recorded = payment_store.get(&payment_id).await?;
					let replacement = record_with_intent_cleared(Some(record), recorded);
					// A bare intent record with no payment to promote it into cannot be cleared
					// in place; it is removed below.
					*removal_flag = replacement.is_none();
					Ok(replacement)
				})
				.await?;
			if remove_bare_record {
				self.pending_payment_store
					.remove_if(&payment_id, |record| {
						record.details().is_none()
							&& record.splice_intent().is_some_and(still_applies)
					})
					.await?;
			}
			Ok(())
		}
		.await;
		if let Err(e) = result {
			log_error!(
				self.logger,
				"Failed to clear the persisted intent of splice payment {}: a stale intent record \
				may be left behind: {}",
				payment_id,
				e,
			);
		}
	}

	/// Records the funding payment of a splice whose transaction this node has just signed but
	/// not yet handed back to LDK, so the record durably precedes any broadcast: the counterparty
	/// cannot broadcast before receiving our `tx_signatures`, which only
	/// [`ChannelManager::funding_transaction_signed`] releases. Holding the submit lock keeps the
	/// channel's intent record from changing hands mid-write — a concurrent [`Self::submit`]
	/// replacing the intent, or a failure event settling it.
	///
	/// [`ChannelManager::funding_transaction_signed`]: lightning::ln::channelmanager::ChannelManager::funding_transaction_signed
	pub(crate) async fn on_funding_ready_for_signing(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId, tx: &Transaction,
	) -> Result<Option<SignedFundingRetraction>, Error> {
		let _guard = self.submit_lock.lock().await;
		self.wallet.record_signed_funding(counterparty_node_id, channel_id, tx).await
	}

	/// Retracts the funding payment recorded by [`Self::on_funding_ready_for_signing`] when LDK
	/// then refused the signed transaction, so the aborted round does not linger as a payment
	/// nothing can ever confirm, or as a recorded candidate that no later round's classification
	/// would carry (the candidate history may only grow, so such a list would be refused
	/// wholesale). Holding the submit lock orders the retraction before any concurrent splice
	/// submission touching the same record.
	pub(crate) async fn on_funding_signing_failed(
		&self, retraction: Option<SignedFundingRetraction>,
	) {
		let Some(retraction) = retraction else {
			return;
		};
		let _guard = self.submit_lock.lock().await;
		self.wallet.retract_signed_funding(retraction).await;
	}

	/// Begins settling the recorded splice a failure event concerns, snapshotting the intent
	/// `contribution` identifies — if any; a failure of some other attempt (e.g. one superseded
	/// by a fee bump, whose failure LDK reports separately) identifies nothing and settles
	/// nothing. The returned [`FailureSettlement`] holds the submit lock until it is settled or
	/// dropped, so no new splice can take the channel's record in between: without it, a failure
	/// event could settle the intent of an identical splice submitted while the event was being
	/// reported, or race `submit`'s undo of a synchronously rejected hand-off.
	///
	/// Settle only once the user-facing event is durably queued, and drop the settlement when
	/// queueing fails: LDK then replays the failure event, and a cleared intent must mean the
	/// failure was reported.
	pub(crate) async fn on_negotiation_failed(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
		contribution: Option<&FundingContribution>,
	) -> FailureSettlement<'_> {
		let guard = self.submit_lock.lock().await;
		let mut matched = None;
		if let Some(contribution) = contribution {
			matched = self.record_for_channel(counterparty_node_id, channel_id).await.and_then(
				|record| {
					let intent = record.splice_intent()?;
					is_same_splice(&intent.contribution, contribution)
						.then(|| (record.id(), intent.clone()))
				},
			);
		}
		FailureSettlement { tracker: self, _guard: guard, matched }
	}

	/// Settles any persisted intent made obsolete by a newly locked funding transaction. An
	/// intent whose pre-splice outpoint is the newly locked funding was created after the lock
	/// and stays; one LDK still holds as a queued splice candidate is refreshed to the new
	/// funding rather than settled. Also resolves [`Self::reconcile`]'s case of a funding that
	/// moved while the node was down — the same situation, minus the event.
	pub(crate) async fn on_channel_ready(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
		funding_txo: Option<OutPoint>,
	) {
		let Some(funding_txo) = funding_txo else {
			return;
		};
		let Some(record) = self.record_for_channel(counterparty_node_id, channel_id).await else {
			return;
		};
		let payment_id = record.id();
		let Some(intent) = record.splice_intent().cloned() else {
			return;
		};
		if intent.pre_splice_funding_txo.into_bitcoin_outpoint() == funding_txo {
			return;
		}

		// LDK queues a splice initiated while another is pending and carries it across the
		// pending splice's lock. A candidate still holding the intent's contribution is that
		// queued splice: it is still live, so re-anchor the intent to the funding it now builds
		// on instead of settling it.
		let channel = self
			.channel_manager
			.list_channels_with_counterparty(&counterparty_node_id)
			.into_iter()
			.find(|c| c.channel_id == channel_id);
		if let Some(channel) = &channel {
			let candidates = channel
				.splice_details
				.as_ref()
				.map(|details| details.candidates.as_slice())
				.unwrap_or(&[]);
			let still_held = candidates.iter().any(|candidate| {
				candidate
					.contribution
					.as_ref()
					.is_some_and(|c| is_same_splice(c, &intent.contribution))
			});
			if still_held {
				if let Some(new_funding_txo) = channel.funding_txo {
					self.refresh_intent_funding(payment_id, &intent, new_funding_txo).await;
				}
				return;
			}
		}

		// The lock settled the intent. When the locked funding did not consume the intent's
		// inputs — a replacement attempt or a counterparty-initiated splice locked instead —
		// release them for other spends; a lock that spent them released nothing.
		let inputs: Vec<OutPoint> =
			intent.contribution.inputs().iter().map(|i| i.outpoint()).collect();
		if !self.wallet.tx_spends_outpoints(funding_txo.txid, &inputs) {
			if let Err(e) = self.wallet.unlock_outpoints(&inputs).await {
				log_error!(
					self.logger,
					"Failed to release the inputs of a settled splice on channel {}: {}",
					channel_id,
					e,
				);
			}
		}
		self.clear_persisted_intent(payment_id, |i| *i == intent).await;
	}

	/// Re-anchors a still-live intent to the funding outpoint it now builds on, but only while
	/// the record still carries the intent this decision was made for.
	async fn refresh_intent_funding(
		&self, payment_id: PaymentId, intent: &SpliceIntent, new_funding_txo: LdkOutPoint,
	) {
		let refreshed = SpliceIntent { pre_splice_funding_txo: new_funding_txo, ..intent.clone() };
		let result = self
			.pending_payment_store
			.mutate(&payment_id, |existing| {
				let mut record = existing?.clone();
				if record.splice_intent() != Some(intent) {
					return None;
				}
				let update = PendingPaymentDetailsUpdate {
					id: payment_id,
					payment_update: None,
					conflicting_txids: None,
					candidates: Vec::new(),
					splice_intent: Some(Some(refreshed)),
				};
				record.update(update).then_some(record)
			})
			.await;
		if let Err(e) = result {
			log_error!(
				self.logger,
				"Failed to re-anchor the intent of queued splice payment {}: {}",
				payment_id,
				e,
			);
		}
	}

	/// Settles any persisted intent for a closed channel, as there is nothing left to splice.
	pub(crate) async fn on_channel_closed(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
	) {
		if let Some(record) = self.record_for_channel(counterparty_node_id, channel_id).await {
			self.clear_persisted_intent(record.id(), |_| true).await;
		}
	}

	/// Returns the pending record carrying a splice intent for the given channel, if any. A fee
	/// bump reuses the channel's existing intent record, so at most one record matches.
	async fn record_for_channel(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
	) -> Option<PendingPaymentDetails> {
		self.pending_payment_store
			.list_filter(|p| {
				p.splice_intent().is_some_and(|i| {
					i.channel_id == channel_id && i.counterparty_node_id == counterparty_node_id
				})
			})
			.await
			.into_iter()
			.next()
	}
}

/// The in-progress settlement of a splice failure, returned by
/// [`SpliceTracker::on_negotiation_failed`]. It snapshots the recorded intent the failure
/// identifies and holds the submit lock, so the record cannot change hands between the snapshot
/// and [`Self::settle`].
pub(crate) struct FailureSettlement<'a> {
	tracker: &'a SpliceTracker,
	_guard: tokio::sync::MutexGuard<'a, ()>,
	/// The record and intent the failure identifies, if any.
	matched: Option<(PaymentId, SpliceIntent)>,
}

impl FailureSettlement<'_> {
	/// The parameters of the API call behind the splice the failure identifies, if any.
	pub(crate) fn originating_kind(&self) -> Option<&SpliceKind> {
		self.matched.as_ref().map(|(_, intent)| &intent.kind)
	}

	/// Settles the snapshotted intent, if any. Call only once the user-facing failure event is
	/// durably queued.
	pub(crate) async fn settle(self) {
		let FailureSettlement { tracker, _guard, matched } = self;
		if let Some((payment_id, intent)) = matched {
			tracker.clear_persisted_intent(payment_id, move |i| *i == intent).await;
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
	existing: Option<PendingPaymentDetails>, recorded: Option<PaymentDetails>,
) -> Option<PendingPaymentDetails> {
	match existing {
		Some(PendingPaymentDetails::PendingSplice { .. }) => recorded
			.filter(|details| details.status == PaymentStatus::Pending)
			.map(|details| PendingPaymentDetails::tracked(details, Vec::new(), Vec::new(), None)),
		Some(mut tracked @ PendingPaymentDetails::Tracked { .. }) => {
			let update = PendingPaymentDetailsUpdate {
				id: tracked.id(),
				payment_update: None,
				conflicting_txids: None,
				candidates: Vec::new(),
				splice_intent: Some(None),
			};
			tracked.update(update).then_some(tracked)
		},
		None => None,
	}
}

/// What [`SpliceTracker::reconcile`] should do with a persisted intent whose channel and funding
/// are unchanged, decided from the splice rounds LDK reports on the channel.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileDecision {
	/// LDK still holds a splice of ours; leave the intent in place until the splice settles.
	Keep,
	/// LDK holds no splice of ours: the recorded splice died with the restart, so whatever was
	/// reserved for it is released and the intent dropped.
	Lost,
}

/// Decides the startup action for a persisted intent from the channel's [`SpliceDetails`]
/// candidates.
///
/// [`SpliceDetails`]: lightning::ln::channel_state::SpliceDetails
fn decide_reconcile(candidates: &[SpliceCandidateDetails]) -> ReconcileDecision {
	// A round short of `Negotiated` is one LDK still drives on its own: only `AwaitingSignatures`
	// survives a restart, and LDK resumes the signature exchange itself on reconnect.
	let in_flight = candidates
		.iter()
		.any(|candidate| !matches!(candidate.status, SpliceCandidateStatus::Negotiated { .. }));
	if in_flight {
		return ReconcileDecision::Keep;
	}

	// LDK persists a splice once negotiated, so a negotiated candidate carrying a local
	// contribution is a splice of ours LDK sees through to lock — even one negotiated at a
	// different feerate than a recorded fee bump asked for. Without one, only counterparty
	// rounds (or nothing) survived: the recorded splice is gone.
	if candidates.iter().any(|candidate| candidate.contribution.is_some()) {
		ReconcileDecision::Keep
	} else {
		ReconcileDecision::Lost
	}
}

/// The inputs `contribution` reserved that no candidate's own contribution still claims — extras
/// a splice attempt lost with the restart had reserved. A counterparty-only round carries no
/// contribution and claims nothing.
fn unclaimed_inputs(
	contribution: &FundingContribution, candidates: &[SpliceCandidateDetails],
) -> Vec<OutPoint> {
	let claimed: Vec<OutPoint> = candidates
		.iter()
		.filter_map(|candidate| candidate.contribution.as_ref())
		.flat_map(|contribution| contribution.inputs().iter().map(|input| input.outpoint()))
		.collect();
	contribution
		.inputs()
		.iter()
		.map(|input| input.outpoint())
		.filter(|outpoint| !claimed.contains(outpoint))
		.collect()
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bitcoin::hashes::Hash;
	use bitcoin::{Amount, Txid};

	use super::*;
	use crate::payment::pending_payment_store::{
		test_funding_contribution, test_funding_contribution_with_feerate,
		test_funding_contribution_with_inputs, test_funding_contribution_with_outputs,
	};
	use crate::payment::store::{ConfirmationStatus, PaymentKind};
	use crate::payment::PaymentDirection;

	fn test_intent() -> SpliceIntent {
		SpliceIntent {
			counterparty_node_id: PublicKey::from_str(
				"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
			)
			.unwrap(),
			channel_id: ChannelId([7u8; 32]),
			pre_splice_funding_txo: LdkOutPoint {
				txid: Txid::from_byte_array([3u8; 32]),
				index: 0,
			},
			contribution: test_funding_contribution(),
			kind: SpliceKind::Rbf {},
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

		let replacement =
			record_with_intent_cleared(Some(existing.clone()), Some(recorded.clone()));
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
		assert!(record_with_intent_cleared(Some(existing.clone()), Some(recorded)).is_none());
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

		let replacement = record_with_intent_cleared(Some(existing.clone()), Some(details.clone()));
		let replacement = replacement.expect("the entry must survive with its intent cleared");
		assert_eq!(replacement.details(), Some(&details));
		assert!(replacement.splice_intent().is_none());
	}

	#[test]
	fn contributions_match_by_inputs_and_outputs() {
		use bitcoin::{ScriptBuf, TxOut};

		let outputs =
			vec![TxOut { value: Amount::from_sat(1_000), script_pubkey: ScriptBuf::new() }];
		// Fee fields differ, inputs and outputs agree: the same attempt. LDK may adjust a
		// contribution during negotiation — the quiescence tie-breaker rebuilds the acceptor's
		// copy at a fresh feerate — and events then carry the adjusted copy, which must still
		// identify the recorded splice.
		let a = test_funding_contribution_with_outputs(253, &outputs);
		let b = test_funding_contribution_with_outputs(500, &outputs);
		assert!(is_same_splice(&a, &b));

		// Different outputs are a different attempt.
		let other = vec![TxOut { value: Amount::from_sat(2_000), script_pubkey: ScriptBuf::new() }];
		assert!(!is_same_splice(&a, &test_funding_contribution_with_outputs(253, &other)));

		// Contributions moving nothing (no inputs, no outputs) only match themselves exactly.
		assert!(is_same_splice(&test_funding_contribution(), &test_funding_contribution()));
		assert!(!is_same_splice(
			&test_funding_contribution(),
			&test_funding_contribution_with_feerate(500)
		));
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

	/// A previous transaction with a P2WPKH output at index 0 for a contribution input to spend;
	/// `seed` varies the output script, and with it the txid.
	fn test_prevtx(seed: u8) -> Transaction {
		use bitcoin::{ScriptBuf, TxOut, WPubkeyHash};

		Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![TxIn::default()],
			output: vec![TxOut {
				value: Amount::from_sat(10_000),
				script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([seed; 20])),
			}],
		}
	}

	/// While any round is short of `Negotiated`, LDK drives the splice itself; the intent stays
	/// in place until the splice settles.
	#[test]
	fn reconcile_keeps_the_intent_while_ldk_drives_a_round() {
		let in_flight = SpliceCandidateDetails {
			contribution: Some(test_funding_contribution()),
			status: SpliceCandidateStatus::AwaitingSignatures {
				is_initiator: true,
				funding_feerate_sat_per_1000_weight: 253,
				new_channel_value_satoshis: 100_000,
				txid: Txid::from_byte_array([9u8; 32]),
			},
		};
		assert_eq!(decide_reconcile(&[in_flight]), ReconcileDecision::Keep);
	}

	/// A negotiated candidate carrying a local contribution is a splice LDK sees through to lock;
	/// nothing was lost. This holds on zero-conf channels too, where the pre-splice funding
	/// outpoint has not moved on yet.
	#[test]
	fn reconcile_trusts_a_negotiated_contribution() {
		let negotiated = [negotiated_candidate(Some(test_funding_contribution()))];
		assert_eq!(decide_reconcile(&negotiated), ReconcileDecision::Keep);
	}

	/// A fee bump that only survives as a candidate negotiated at a lower feerate than requested
	/// is not lost: the recorded bump is moot, but the splice lives on and locks. The old
	/// higher-feerate attempt's extra reservations are released through the input difference, not
	/// by dropping the record.
	#[test]
	fn reconcile_keeps_a_bump_negotiated_at_a_lower_feerate() {
		let lower = [negotiated_candidate(Some(test_funding_contribution_with_feerate(253)))];
		assert_eq!(decide_reconcile(&lower), ReconcileDecision::Keep);
	}

	/// With no contribution of ours in LDK — no splice at all, or only a counterparty round — the
	/// recorded splice died with the restart.
	#[test]
	fn reconcile_finds_the_splice_lost_when_ldk_holds_no_contribution() {
		assert_eq!(decide_reconcile(&[]), ReconcileDecision::Lost);
		let counterparty_only = [negotiated_candidate(None)];
		assert_eq!(decide_reconcile(&counterparty_only), ReconcileDecision::Lost);
	}

	/// The inputs a kept record reserves beyond what LDK's candidates still claim are identified
	/// for release; a counterparty-only round claims nothing and must not suppress the
	/// difference.
	#[test]
	fn unclaimed_inputs_are_those_no_candidate_contribution_uses() {
		let prevtxs: Vec<Transaction> = (1u8..=3).map(test_prevtx).collect();
		let outpoint = |tx: &Transaction| OutPoint { txid: tx.compute_txid(), vout: 0 };
		let recorded = test_funding_contribution_with_inputs(253, &prevtxs);

		// Every input still claimed by a surviving candidate: nothing to release.
		let all =
			[negotiated_candidate(Some(test_funding_contribution_with_inputs(253, &prevtxs)))];
		assert!(unclaimed_inputs(&recorded, &all).is_empty());

		// A candidate claiming two of the three inputs: the third is released, even with a
		// counterparty-only round alongside.
		let partial = [
			negotiated_candidate(None),
			negotiated_candidate(Some(test_funding_contribution_with_inputs(253, &prevtxs[..2]))),
		];
		assert_eq!(unclaimed_inputs(&recorded, &partial), vec![outpoint(&prevtxs[2])]);

		// No candidates at all: everything is released.
		assert_eq!(
			unclaimed_inputs(&recorded, &[]),
			prevtxs.iter().map(outpoint).collect::<Vec<_>>()
		);
	}
}
