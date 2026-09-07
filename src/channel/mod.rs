// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Persistence of in-flight user-initiated splices, so a splice LDK has not durably learned of
//! yet can be recognized — and whatever it reserved recovered — after a restart.

use std::fmt;
use std::sync::Arc;

use bitcoin::absolute::LockTime;
use bitcoin::secp256k1::PublicKey;
use bitcoin::transaction::Version;
use bitcoin::{OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid};
use lightning::chain::chaininterface::FundingCandidate;
use lightning::chain::transaction::OutPoint as LdkOutPoint;
use lightning::ln::channel_state::{ChannelDetails, SpliceCandidateDetails, SpliceCandidateStatus};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;

use crate::data_store::StorableObject;
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::payment::pending_payment_store::{
	PendingPaymentDetails, PendingPaymentDetailsUpdate, SpliceIntent, SpliceKind,
};
use crate::payment::{PaymentKind, TransactionType};
use crate::types::{ChannelManager, PendingPaymentStore};
use crate::wallet::{funding_candidates, random_payment_id, Wallet};
use crate::Error;

/// Whether two contributions describe the same splice attempt. LDK may adjust a contribution
/// during negotiation — the quiescence tie-breaker rebuilds the acceptor's copy at a fresh
/// feerate, touching only its fee fields and change value — so fees and feerates do not identify
/// an attempt. Its inputs and outputs do: they are what the user asked to move. Contributions
/// carrying neither (channel-balance-only attempts) fall back to full equality.
pub(crate) fn is_same_splice(a: &FundingContribution, b: &FundingContribution) -> bool {
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
/// channel closes. The record exists for recovery, not retry: a splice still recorded at the next
/// startup identifies one that was in flight when the node stopped, so [`Self::reconcile`] can
/// release what it still reserves where nothing else will, and events about the splice can be
/// described in terms of the original request. Each splice has a record of its own — a channel may
/// carry several, a pending splice and the splices queued behind it — so that each is recognized
/// and described whatever became of the others; only a fee bump joins the record of the round it
/// replaces.
pub(crate) struct SpliceTracker {
	channel_manager: Arc<ChannelManager>,
	wallet: Arc<Wallet>,
	pending_payment_store: Arc<PendingPaymentStore>,
	/// Serializes everything that reads or settles a channel's intent records against
	/// [`Self::submit`]'s read-funding, persist and hand-off sequence: the settling of intents by
	/// [`Self::on_negotiation_failed`], [`Self::on_channel_ready`] and
	/// [`Self::on_channel_closed`], the funding record [`Self::on_funding_ready_for_signing`]
	/// files under an intent's id, and the startup pass of [`Self::reconcile`]. Without it, the
	/// failure event of a synchronously rejected
	/// hand-off could settle the just-written intent while `submit` is still deciding whether to
	/// keep it, and a lock event handled between `submit`'s funding read and its persist could
	/// leave the new intent anchored at a funding the channel has moved past, which nothing would
	/// settle. Every public entry point takes it; the `_locked` variants assume it is held and
	/// must not take it again (tokio's mutex is not reentrant). It nests outward of the wallet's
	/// locks and the stores', which are taken while it is held and never hold it. An event
	/// handler waiting on it waits for a `submit` to finish its bounded sequence, nothing more.
	submit_lock: tokio::sync::Mutex<()>,
	logger: Arc<Logger>,
}

impl SpliceTracker {
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, wallet: Arc<Wallet>,
		pending_payment_store: Arc<PendingPaymentStore>, logger: Arc<Logger>,
	) -> Self {
		Self {
			channel_manager,
			wallet,
			pending_payment_store,
			submit_lock: tokio::sync::Mutex::new(()),
			logger,
		}
	}

	/// Reconciles the persisted splice intents against live channel state, releasing whatever the
	/// wallet still holds for a splice that did not survive the restart and nothing else will
	/// release. LDK only persists a splice once its negotiation reaches `AwaitingSignatures`, so a
	/// splice lost earlier leaves no trace in LDK's channel state — the intent record is what
	/// recognizes the loss. A round LDK did write is another matter: LDK either still holds it, or
	/// failed it as it was last written — the failure is replayed at startup — and returns what it
	/// reserved through `DiscardFunding`; a round of a channel that closed meanwhile is watched by
	/// the channel's monitor until the close matures. Such rounds are left to those events. Run
	/// once at startup, before background chain syncing and event processing start, so nothing can
	/// act on the stale reservations first. Holds the submit lock throughout, as the event handlers
	/// do.
	///
	/// Recovery fabricates no failure event for a splice lost this way: the initiating call
	/// already returned and the channel simply shows no pending splice anymore. LDK itself may
	/// report the loss — a contribution it was still queueing or negotiating when it was last
	/// persisted is failed as it is written, and the failure replayed at startup. That replay
	/// runs after this reconciliation, so the report carries the splice's parameters only if
	/// `decide_reconcile` kept the intent: a splice queued behind a pending one of ours, or a fee
	/// bump of one, is reported with its parameters; a channel's only splice, whose intent
	/// settled here, without them.
	pub(crate) async fn reconcile(&self) {
		let guard = self.submit_lock.lock().await;
		let records = self.pending_payment_store.list_filter(|p| p.splice_intent().is_some()).await;
		for record in records {
			let payment_id = record.id();
			let Some(intent) = record.splice_intent().cloned() else {
				continue;
			};

			let channel = self.channel(intent.counterparty_node_id, intent.channel_id);
			let Some(channel) = channel else {
				// The channel is gone; there is nothing to splice anymore. What the wallet holds
				// for the intent is released only while no recorded round exists: a round the
				// closed channel's monitor watches is either spent by the close or returned through
				// the `DiscardFunding` event the monitor queues once the close matures, and a
				// recorded round the monitor never watched — the counterparty's `commitment_signed`
				// never arrived before the node stopped — is released by neither, as at
				// `ChannelClosed`. A bare intent has no such round — LDK never wrote the splice —
				// so nothing else would release it.
				log_info!(
					self.logger,
					"Dropping the recorded splice of closed channel {} with counterparty {}",
					intent.channel_id,
					intent.counterparty_node_id,
				);
				if record.candidates().is_empty() {
					self.release_contribution(intent.channel_id, &intent.contribution, &[], None)
						.await;
				}
				// TODO(#1037): once inputs are locked at coin selection, the parts of the
				// contribution no recorded round uses stay locked with no record to release them
				// from after the intent is cleared here: release them before clearing. And
				// `release_contribution` swallows a failed release, which then leaves locks no
				// record names either: keep the intent when the release fails. The same holds for a
				// recorded round the monitor never watched: nothing releases its inputs once the
				// intent is cleared here.
				self.clear_persisted_intent(payment_id, |i| *i == intent).await;
				continue;
			};

			if channel.funding_txo != Some(intent.pre_splice_funding_txo) {
				// The funding moved on while the node was down: the recorded splice, a
				// replacement, or a counterparty splice locked — the same situation a live lock
				// event resolves, so resolve it the same way.
				if let Some(funding_txo) = channel.funding_txo {
					self.settle_superseded_intents_locked(
						&guard,
						intent.counterparty_node_id,
						intent.channel_id,
						funding_txo.into_bitcoin_outpoint(),
						Some(&channel),
					)
					.await;
				}
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
					self.release_contribution(
						intent.channel_id,
						&intent.contribution,
						candidates,
						None,
					)
					.await;
					// TODO(#1037): `release_contribution` swallows a failed release. Once inputs
					// are locked at coin selection, a failure here leaves locks no record names
					// after the intent is cleared: keep the intent when the release fails.
					self.clear_persisted_intent(payment_id, |i| *i == intent).await;
				},
			}
		}
	}

	/// Persists a user-initiated splice as an intent and hands its contribution to
	/// [`ChannelManager::funding_contributed`]. The intent — and any wallet state staged on the
	/// splice's behalf — is durable before the hand-off, so no splice is ever in flight without a
	/// persisted record of it. Each splice gets a record of its own; only a fee bump joins the
	/// record of the round it replaces ([`Self::persist_intent`]).
	///
	/// The intent is anchored at the channel's funding as it stands under the submit lock, not at
	/// `pre_splice_funding_txo`, the funding the caller read before building the contribution: a
	/// splice locking in between moves the funding, and an intent anchored at the old one would
	/// never be settled by the lock that superseded it. A funding that moved refuses a fee bump —
	/// the round it was built to replace has locked — and a splice-in, whose inputs the locked
	/// round may have spent; a splice-out carries no wallet inputs and proceeds, as LDK
	/// re-validates its amount against the live balance ([`check_submission`]). Intents anchored
	/// at a funding the channel has moved past are settled first, as their lock event would.
	///
	/// On any failure the persisted intent is undone and the error returned for the caller to
	/// surface. A failure before the hand-off also releases what the wallet holds for the
	/// contribution and no other round claims ([`Self::release_contribution`]): a fee bump built
	/// by adjusting the fee of the round it replaces — `prior`, the contribution it was built
	/// from — reuses that round's inputs and change address, which a refusal must leave to the
	/// round that has locked meanwhile. A synchronous rejection leaves the release to the
	/// `DiscardFunding` event LDK queues.
	///
	/// [`ChannelManager::funding_contributed`]: lightning::ln::channelmanager::ChannelManager::funding_contributed
	pub(crate) async fn submit(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
		pre_splice_funding_txo: LdkOutPoint, contribution: FundingContribution, kind: SpliceKind,
		prior: Option<FundingContribution>,
	) -> Result<(), Error> {
		let guard = self.submit_lock.lock().await;
		let channel = self.channel(counterparty_node_id, channel_id);
		let live_funding_txo = channel.as_ref().and_then(|channel| channel.funding_txo);
		let candidates = channel
			.as_ref()
			.and_then(|channel| channel.splice_details.as_ref())
			.map(|details| details.candidates.as_slice())
			.unwrap_or(&[]);
		let funding_txo = match check_submission(pre_splice_funding_txo, live_funding_txo, &kind) {
			Ok(funding_txo) => funding_txo,
			Err(refusal) => {
				log_error!(
					self.logger,
					"Refusing to splice channel {} with counterparty {}: {}",
					channel_id,
					counterparty_node_id,
					refusal,
				);
				// TODO(#1037): `release_contribution` swallows a failed release. Once inputs are
				// locked at coin selection, a failure here leaves locks no record names: surface
				// it, or persist an intent for `reconcile` to release from.
				self.release_contribution(channel_id, &contribution, candidates, prior.as_ref())
					.await;
				return Err(Error::ChannelSplicingFailed);
			},
		};
		// LDK promotes a zero-conf splice as soon as `splice_locked` is exchanged and only queues
		// the `ChannelReady` event whose handling settles the locked splice's intent. A splice
		// submitted in between builds on the new funding while the channel still carries that
		// intent: settle it here as the event would.
		self.settle_superseded_intents_locked(
			&guard,
			counterparty_node_id,
			channel_id,
			funding_txo.into_bitcoin_outpoint(),
			channel.as_ref(),
		)
		.await;
		let intent = SpliceIntent {
			counterparty_node_id,
			channel_id,
			pre_splice_funding_txo: funding_txo,
			contribution: contribution.clone(),
			kind,
		};
		// A splice whose intent cannot be persisted is not attempted at all, rather than
		// attempted without restart coverage.
		let (payment_id, restore) = match self.persist_intent(intent, channel.as_ref()).await {
			Ok(persisted) => persisted,
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to persist the splice intent for channel {} with counterparty {}: {:?}",
					channel_id,
					counterparty_node_id,
					e,
				);
				// TODO(#1037): as at the refusal above, a failed release here leaves locks no
				// record names.
				self.release_contribution(channel_id, &contribution, candidates, prior.as_ref())
					.await;
				return Err(e);
			},
		};
		// Flush wallet state staged on the splice's behalf (e.g. input locks) only now that the
		// intent record is durable: whatever the wallet holds for a splice must never outlive the
		// record through which a later startup would release it.
		// TODO(#1037): nothing is staged yet, and #1037 persists its input locks at coin
		// selection, ahead of the intent. Stage them instead, so that this flush is what makes
		// them durable.
		if let Err(e) = self.wallet.persist_staged().await {
			log_error!(
				self.logger,
				"Failed to persist staged wallet state for splicing channel {} with counterparty \
				{}: {:?}",
				channel_id,
				counterparty_node_id,
				e,
			);
			// TODO(#1037): the intent is discarded before the release; a release that fails
			// leaves locks no record names. Keep the intent instead when the release fails.
			self.discard_persisted_intent(&payment_id, restore).await;
			self.release_contribution(channel_id, &contribution, candidates, prior.as_ref()).await;
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
			// LDK returns the contribution through a `DiscardFunding` event, whose handling frees
			// the addresses the wallet marked for it.
			// TODO(#1037): the handler ignores the event's inputs; once inputs are locked at coin
			// selection, it must unlock them as well.
			self.discard_persisted_intent(&payment_id, restore).await;
			return Err(Error::ChannelSplicingFailed);
		}
		Ok(())
	}

	/// Releases what the wallet may still hold for a contribution that is going nowhere, short of
	/// what another contribution claims as well — a splice candidate LDK holds for the channel, or
	/// the round a fee bump was built from (`prior`): the remaining inputs are unlocked and a
	/// transaction paying the remaining outputs is canceled, freeing the addresses of its change
	/// and splice-out outputs ([`unclaimed_parts`]). A fee bump built by adjusting the fee of the
	/// round it replaces reuses that round's inputs and change address; released along with the
	/// bump, they would be free for other spends while the round can still confirm.
	async fn release_contribution(
		&self, channel_id: ChannelId, contribution: &FundingContribution,
		candidates: &[SpliceCandidateDetails], prior: Option<&FundingContribution>,
	) {
		let claimants = candidates.iter().filter_map(|c| c.contribution.as_ref()).chain(prior);
		let (inputs, outputs) = unclaimed_parts(contribution, claimants);
		if inputs.is_empty() && outputs.is_empty() {
			return;
		}
		// TODO(#1037): `cancel_tx` unlocks the transaction's inputs itself once inputs are locked
		// at coin selection, making this unlock redundant.
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
			output: outputs,
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
	/// Each splice gets a record of its own, so a channel may carry several: a splice queued
	/// behind a pending one negotiates as a splice of its own once the pending one locks. Only a
	/// fee bump joins an existing record, that of the round it replaces ([`place_intent`]), decided
	/// from the channel's pending records and the splice rounds LDK holds for the channel
	/// (`channel`, as the caller listed it). A record still anchored at another funding is one
	/// [`Self::submit`] just failed to settle or to re-anchor; the new splice is refused rather
	/// than recorded beside it. Returns the id and, for restoring on a rejected hand-off, `None`
	/// when a fresh record was created or `Some(prior)` when an existing record's intent was
	/// replaced (`prior` being `None` for a record that carried no intent).
	async fn persist_intent(
		&self, intent: SpliceIntent, channel: Option<&ChannelDetails>,
	) -> Result<(PaymentId, Option<Option<SpliceIntent>>), Error> {
		let records = self
			.pending_payment_store
			.list_filter(|p| concerns_channel(p, intent.counterparty_node_id, intent.channel_id))
			.await;
		let held_rounds: Vec<Txid> = channel
			.map(|channel| {
				funding_candidates(
					channel.splice_details.as_ref(),
					intent.counterparty_node_id,
					intent.channel_id,
				)
			})
			.unwrap_or_default()
			.into_iter()
			.map(|candidate| candidate.txid)
			.collect();
		match place_intent(&intent, &records, &held_rounds) {
			IntentPlacement::Refused => {
				log_error!(
					self.logger,
					"Refusing to splice channel {} with counterparty {}: the channel carries a \
					splice intent anchored at another funding",
					intent.channel_id,
					intent.counterparty_node_id,
				);
				Err(Error::ChannelSplicingFailed)
			},
			IntentPlacement::Reuse(payment_id) => {
				let prior = records
					.iter()
					.find(|record| record.id() == payment_id)
					.and_then(|record| record.splice_intent().cloned());
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
			IntentPlacement::Fresh => {
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
	/// intent: a mismatch means a fee bump took over the record in the meantime, and its intent
	/// must stay. A tracked record stays, with the intent cleared, so its payment keeps
	/// graduating. A bare intent record is removed, along with any payment record under its id:
	/// the signing-time recording of a splice round files a payment under an intent's id and
	/// promotes the entry in the same write, so a payment record found under a bare intent is the
	/// first half of a write that never completed, of a round whose signatures never left the
	/// node, so nothing can broadcast it and no entry would ever drive the record
	/// ([`Wallet::drop_unindexed_record_of_settled_intent`]). The record goes first: a bare intent
	/// left behind is found and settled again, an orphaned record would not be.
	async fn clear_persisted_intent<F: Fn(&SpliceIntent) -> bool>(
		&self, payment_id: PaymentId, still_applies: F,
	) {
		let still_applies = &still_applies;
		let result: Result<(), Error> = async {
			let mut remove_bare_record = false;
			// The `move` closure would capture a plain `bool` by copy, so hand it a reference; the
			// borrow ends with the mutate's future, before the flag is read below.
			let removal_flag = &mut remove_bare_record;
			self.pending_payment_store
				.mutate(&payment_id, move |existing| {
					let record = existing?;
					match record.splice_intent() {
						Some(intent) if still_applies(intent) => {},
						_ => return None,
					}
					let replacement = record_with_intent_cleared(record);
					// A bare intent record cannot be cleared in place; it is removed below.
					*removal_flag = replacement.is_none();
					replacement
				})
				.await?;
			if remove_bare_record {
				self.wallet.drop_unindexed_record_of_settled_intent(payment_id).await?;
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

	/// Begins settling the recorded splice a failure event concerns, snapshotting the intent
	/// `contribution` identifies among the channel's ([`record_of_failed_splice`]) — if any; a
	/// failure of some other attempt (e.g. one superseded by a fee bump, whose failure LDK
	/// reports separately) identifies nothing and settles nothing. The returned
	/// [`FailureSettlement`] holds the submit lock until it is settled or dropped, so no new
	/// splice can take the record in between: without it, a failure event could settle the intent
	/// of an identical splice submitted while the event was being reported, or race `submit`'s
	/// undo of a synchronously rejected hand-off.
	///
	/// Settle only once the user-facing event is durably queued, and drop the settlement when
	/// queueing fails: LDK then replays the failure event, and a cleared intent must mean the
	/// failure was reported.
	pub(crate) async fn on_negotiation_failed(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
		contribution: Option<&FundingContribution>,
	) -> FailureSettlement<'_> {
		let guard = self.submit_lock.lock().await;
		let records = self.intent_records_for_channel(counterparty_node_id, channel_id).await;
		let matched = record_of_failed_splice(&records, contribution);
		FailureSettlement { tracker: self, _guard: guard, matched }
	}

	/// Settles the persisted intents made obsolete by the channel's funding having moved on to
	/// `funding_txo`, the funding a `ChannelReady` event reports as locked
	/// ([`Self::settle_superseded_intents_locked`]). Takes the submit lock, so the settlement
	/// cannot interleave with a splice being submitted.
	pub(crate) async fn on_channel_ready(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
		funding_txo: Option<OutPoint>,
	) {
		let Some(funding_txo) = funding_txo else {
			return;
		};
		let guard = self.submit_lock.lock().await;
		let channel = self.channel(counterparty_node_id, channel_id);
		self.settle_superseded_intents_locked(
			&guard,
			counterparty_node_id,
			channel_id,
			funding_txo,
			channel.as_ref(),
		)
		.await;
	}

	/// Settles any persisted intent made obsolete by the channel's funding having moved on to
	/// `funding_txo`: the funding a `ChannelReady` event reports as locked, the one a new splice
	/// builds on ([`Self::submit`]), or the one [`Self::reconcile`] finds the channel at after a
	/// funding moved while the node was down — the same situation, minus the event. Each of the
	/// channel's intents is decided on its own
	/// ([`decide_on_lock`]), against the splice candidates LDK holds for the channel (`channel`,
	/// as the caller listed it): one whose pre-splice outpoint is that funding was created after
	/// the lock and stays; one LDK still holds as a queued splice candidate is re-anchored to the
	/// funding it now builds on rather than settled; any other is settled, and what the wallet
	/// holds for it is either spent by the locked round or returned by LDK through
	/// `DiscardFunding`. The caller holds the submit lock.
	async fn settle_superseded_intents_locked(
		&self, _guard: &tokio::sync::MutexGuard<'_, ()>, counterparty_node_id: PublicKey,
		channel_id: ChannelId, funding_txo: OutPoint, channel: Option<&ChannelDetails>,
	) {
		let records = self.intent_records_for_channel(counterparty_node_id, channel_id).await;
		let candidates = channel
			.and_then(|channel| channel.splice_details.as_ref())
			.map(|details| details.candidates.as_slice())
			.unwrap_or(&[]);
		for record in records {
			let payment_id = record.id();
			let Some(intent) = record.splice_intent().cloned() else {
				continue;
			};
			match decide_on_lock(&intent, funding_txo, candidates) {
				LockDecision::Keep => {},
				LockDecision::Refresh => {
					if let Some(new_funding_txo) = channel.and_then(|channel| channel.funding_txo) {
						self.refresh_intent_funding(payment_id, &intent, new_funding_txo).await;
					}
				},
				LockDecision::Settle => {
					// Nothing the wallet holds for the intent is released here. The inputs the
					// locked round spent are gone with it, and whatever a superseded round reserved
					// beyond them, LDK returns through the `DiscardFunding` events it queues at the
					// promotion. A guard on the wallet's transaction graph could not tell the two
					// apart: today the graph learns an interactive funding from sync alone. Once
					// inputs are locked at coin selection (#1037), releasing them here would free
					// the promoted round's inputs for a conflicting spend: #1037 prepares and
					// unlocks only `Funding`-typed broadcasts, and a splice round is broadcast as
					// `InteractiveFunding`.
					self.clear_persisted_intent(payment_id, |i| *i == intent).await;
				},
			}
		}
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

	/// Records the funding payment of a splice round this node has just signed but not yet handed
	/// back to LDK, through [`Wallet::record_signed_funding`], so the record precedes any
	/// broadcast: the counterparty cannot broadcast before receiving our `tx_signatures`, which
	/// only [`ChannelManager::funding_transaction_signed`] releases. Holding the submit lock keeps
	/// the channel's intent records — one of which the funding record adopts — from changing
	/// mid-write: a concurrent [`Self::submit`] adding or replacing an intent, or a lock or
	/// failure event settling one.
	///
	/// [`ChannelManager::funding_transaction_signed`]: lightning::ln::channelmanager::ChannelManager::funding_transaction_signed
	pub(crate) async fn on_funding_ready_for_signing(
		&self, tx: &Transaction, candidates: &[FundingCandidate],
	) -> Result<(), Error> {
		let _guard = self.submit_lock.lock().await;
		self.wallet.record_signed_funding(tx, candidates).await
	}

	/// Settles every persisted intent of a closed channel, as there is nothing left to splice.
	/// Takes the submit lock, so the settlement cannot interleave with a splice being submitted.
	/// Nothing the wallet holds for the intents is released here: a round the channel's monitor
	/// watches may still confirm, and what LDK reserved for the others it returns through
	/// `DiscardFunding` once the close matures. A signed round the monitor never watched — the
	/// counterparty's `commitment_signed` never arrived — is released by neither.
	// TODO(#1037): once inputs are locked at coin selection, such a round's inputs stay locked
	// with no record to release them from after its intent is cleared here. Release the parts of
	// the contribution no watched round uses before clearing.
	pub(crate) async fn on_channel_closed(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
	) {
		let _guard = self.submit_lock.lock().await;
		for record in self.intent_records_for_channel(counterparty_node_id, channel_id).await {
			self.clear_persisted_intent(record.id(), |_| true).await;
		}
	}

	/// Returns the pending records carrying a splice intent for the given channel: one per
	/// splice of the channel still in flight, a fee bump sharing the record of the round it
	/// replaces.
	async fn intent_records_for_channel(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
	) -> Vec<PendingPaymentDetails> {
		self.pending_payment_store
			.list_filter(|p| {
				p.splice_intent().is_some_and(|i| {
					i.channel_id == channel_id && i.counterparty_node_id == counterparty_node_id
				})
			})
			.await
	}

	/// The channel as LDK lists it, if it still does.
	fn channel(
		&self, counterparty_node_id: PublicKey, channel_id: ChannelId,
	) -> Option<ChannelDetails> {
		self.channel_manager
			.list_channels_with_counterparty(&counterparty_node_id)
			.into_iter()
			.find(|channel| channel.channel_id == channel_id)
	}
}

/// The in-progress settlement of a splice failure, returned by
/// [`SpliceTracker::on_negotiation_failed`]. It snapshots the recorded intent the failure
/// identifies and holds the submit lock, so the record cannot change between the snapshot and
/// [`Self::settle`].
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

/// Why a submission is refused once the channel's funding turns out to differ from the one the
/// caller built the contribution against, decided by [`check_submission`].
#[derive(Debug, PartialEq, Eq)]
enum SubmissionRefusal {
	/// LDK no longer lists the channel, or lists it without a funding.
	ChannelGone,
	/// The round a fee bump was built to replace has locked; there is nothing left to bump.
	BumpedRoundLocked,
	/// A splice locked while the splice-in's inputs were being selected, and may have spent
	/// them.
	InputsMayBeSpent,
}

impl fmt::Display for SubmissionRefusal {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::ChannelGone => write!(f, "the channel is gone or has no funding"),
			Self::BumpedRoundLocked => {
				write!(f, "the funding moved since the bump was built; its round has locked")
			},
			Self::InputsMayBeSpent => write!(
				f,
				"the funding moved since the splice was built; the locked round may have spent \
				its inputs"
			),
		}
	}
}

/// Whether a submission built against `requested_funding` may proceed now that the channel's
/// funding is `live_funding`, and at which funding to anchor its intent. A funding that has not
/// moved proceeds. One that has means a splice locked while the contribution was being built,
/// and only a splice-out proceeds, anchored at the live funding: it carries no wallet inputs,
/// and LDK re-validates its amount against the live balance. A fee bump was built to replace
/// that very round — a bump template is only offered for an unconfirmed, unlocked pending round
/// — and is refused rather than handed to LDK as a fresh splice reusing the locked round's
/// inputs. A splice-in is refused because its inputs were selected before the lock and may be
/// among those the locked round spent, which the wallet only learns from a sync: handed to LDK,
/// such a contribution would negotiate a splice whose transaction can never confirm, and neither
/// LDK nor this node would ever fail it. Refusing costs the caller one retry of a rare race.
// TODO(#1037): once inputs are locked from coin selection until the round is broadcast, and the
// round is applied to the wallet's transaction graph as it is broadcast, a wallet-selected input
// cannot be one a promoted round spent, and `InputsMayBeSpent` can go with its refusal.
// `BumpedRoundLocked` stays: a bump reuses the locked round's inputs. This needs the unlock and
// the graph insertion to happen together, as #1037's broadcast preparation does.
fn check_submission(
	requested_funding: LdkOutPoint, live_funding: Option<LdkOutPoint>, kind: &SpliceKind,
) -> Result<LdkOutPoint, SubmissionRefusal> {
	let live_funding = live_funding.ok_or(SubmissionRefusal::ChannelGone)?;
	if live_funding == requested_funding {
		return Ok(live_funding);
	}
	match kind {
		SpliceKind::Rbf {} => Err(SubmissionRefusal::BumpedRoundLocked),
		SpliceKind::In { .. } => Err(SubmissionRefusal::InputsMayBeSpent),
		SpliceKind::Out { .. } => Ok(live_funding),
	}
}

/// The parts of `contribution` none of `claimants` uses: the inputs none of them spends, and the
/// outputs — change included — paying a script none of them pays. Outputs are matched by script
/// rather than as a whole, as LDK's `DiscardFunding` matches them: a fee-adjusted bump pays its
/// change to the same address as the round it replaces, at a different amount.
fn unclaimed_parts<'a>(
	contribution: &FundingContribution,
	claimants: impl IntoIterator<Item = &'a FundingContribution>,
) -> (Vec<OutPoint>, Vec<TxOut>) {
	let mut claimed_inputs: Vec<OutPoint> = Vec::new();
	let mut claimed_scripts: Vec<&ScriptBuf> = Vec::new();
	for claimant in claimants {
		claimed_inputs.extend(claimant.inputs().iter().map(|input| input.outpoint()));
		claimed_scripts.extend(
			claimant
				.outputs()
				.iter()
				.chain(claimant.change_output())
				.map(|output| &output.script_pubkey),
		);
	}
	let inputs = contribution
		.inputs()
		.iter()
		.map(|input| input.outpoint())
		.filter(|outpoint| !claimed_inputs.contains(outpoint))
		.collect();
	let outputs = contribution
		.outputs()
		.iter()
		.chain(contribution.change_output())
		.filter(|output| !claimed_scripts.contains(&&output.script_pubkey))
		.cloned()
		.collect();
	(inputs, outputs)
}

/// Whether a pending record concerns the given channel's splices: it carries a splice intent for
/// the channel, or tracks an interactive funding payment of it.
fn concerns_channel(
	record: &PendingPaymentDetails, counterparty_node_id: PublicKey, channel_id: ChannelId,
) -> bool {
	if let Some(intent) = record.splice_intent() {
		return intent.channel_id == channel_id
			&& intent.counterparty_node_id == counterparty_node_id;
	}
	match record.details().map(|details| &details.kind) {
		Some(PaymentKind::Onchain {
			tx_type: Some(TransactionType::InteractiveFunding { channels }),
			..
		}) => channels.iter().any(|channel| {
			channel.channel_id == channel_id && channel.counterparty_node_id == counterparty_node_id
		}),
		_ => false,
	}
}

/// Where the intent of a new submission is recorded, decided by [`place_intent`].
#[derive(Debug, PartialEq, Eq)]
enum IntentPlacement {
	/// The channel carries an intent anchored at another funding, one [`SpliceTracker::submit`]
	/// just failed to settle or to re-anchor; the submission is refused.
	Refused,
	/// The submission joins the given record.
	Reuse(PaymentId),
	/// The submission gets a record of its own.
	Fresh,
}

/// Decides where the intent of a new submission is recorded, given the channel's pending records
/// (`records`: those carrying an intent for the channel or tracking a funding payment of it) and
/// the txids of the splice rounds LDK holds for the channel (`held_rounds`, the pending rounds
/// with a transaction; not the funding).
///
/// Every splice gets a record of its own, so that its failure is described from its own intent
/// and a restart recognizes it whatever became of the channel's other splices. A splice-in or
/// splice-out therefore always starts fresh: while a round of ours is pending and bumpable the
/// entry points refuse a new one, and a contribution LDK takes beside a pending splice — queued
/// behind it, or joining a round the counterparty is negotiating — is a splice of its own. Only
/// a fee bump joins an existing record, that of the round it replaces: the record tracking a
/// round LDK still holds — intent-less when an earlier bump failed and its settlement cleared
/// the intent — or else the channel's bare intent record, whose round negotiated but recorded
/// nothing (a splice-out to an external address). A bump joining a bare record shares its fate:
/// the bump's failure removes the record, so a later bump starts fresh.
fn place_intent(
	intent: &SpliceIntent, records: &[PendingPaymentDetails], held_rounds: &[Txid],
) -> IntentPlacement {
	let anchored_elsewhere = records.iter().any(|record| {
		record
			.splice_intent()
			.is_some_and(|i| i.pre_splice_funding_txo != intent.pre_splice_funding_txo)
	});
	if anchored_elsewhere {
		return IntentPlacement::Refused;
	}
	match intent.kind {
		SpliceKind::Rbf {} => {
			let tracks_held_round = |record: &&PendingPaymentDetails| {
				record.candidates().iter().any(|candidate| held_rounds.contains(&candidate.txid))
			};
			records
				.iter()
				.find(tracks_held_round)
				.or_else(|| records.iter().find(|record| record.splice_intent().is_some()))
				.map_or(IntentPlacement::Fresh, |record| IntentPlacement::Reuse(record.id()))
		},
		SpliceKind::In { .. } | SpliceKind::Out { .. } => IntentPlacement::Fresh,
	}
}

/// The record, and its intent, of the splice a failure event identifies by `contribution` among
/// the channel's intent records: the one whose intent's contribution is the same attempt
/// ([`is_same_splice`]). A failure that reports no contribution identifies nothing, as does one
/// whose contribution matches no recorded intent — an attempt superseded by a fee bump, whose
/// failure LDK reports separately.
fn record_of_failed_splice(
	records: &[PendingPaymentDetails], contribution: Option<&FundingContribution>,
) -> Option<(PaymentId, SpliceIntent)> {
	let contribution = contribution?;
	records.iter().find_map(|record| {
		let intent = record.splice_intent()?;
		is_same_splice(&intent.contribution, contribution).then(|| (record.id(), intent.clone()))
	})
}

/// What a lock of the funding a channel has moved on to — or a new splice building on it —
/// means for one of the channel's recorded intents, decided by [`decide_on_lock`].
#[derive(Debug, PartialEq, Eq)]
enum LockDecision {
	/// The intent is anchored at that funding: its splice was submitted after the lock.
	Keep,
	/// LDK still holds the intent's contribution as a splice candidate — a splice queued behind
	/// the one that locked, carried across the lock — so the intent is re-anchored to the new
	/// funding.
	Refresh,
	/// The lock superseded the intent's splice: the splice locked, a replacement or a
	/// counterparty splice locked instead, or the queued splice was failed at the lock. The
	/// intent is settled.
	///
	/// A queued splice fails at the lock when its contribution overlaps the promoted transaction.
	/// LDK takes it out of the queue and reports the failure after the `ChannelReady` of the
	/// lock, so the intent is settled here first and the failure surfaces without the splice's
	/// parameters. The overlap check at queue time — against this node's own contributions to the
	/// pending rounds — lets only a contribution naming an input or output the counterparty
	/// contributed to the promoted round get this far, which this node's wallet does not produce.
	Settle,
}

/// Decides what the channel's funding having moved on to `funding_txo` means for `intent`, given
/// the splice candidates LDK holds for the channel.
fn decide_on_lock(
	intent: &SpliceIntent, funding_txo: OutPoint, candidates: &[SpliceCandidateDetails],
) -> LockDecision {
	if intent.pre_splice_funding_txo.into_bitcoin_outpoint() == funding_txo {
		return LockDecision::Keep;
	}
	let still_held = candidates.iter().any(|candidate| {
		candidate.contribution.as_ref().is_some_and(|c| is_same_splice(c, &intent.contribution))
	});
	if still_held {
		LockDecision::Refresh
	} else {
		LockDecision::Settle
	}
}

/// The replacement for a pending record whose splice intent is being dropped. A tracked record
/// keeps its payment details with just the intent cleared. A bare intent record has nothing to
/// keep and is left for the caller to remove — never promoted over a payment record found under
/// its id, which is the first half of a write — for a round of ours, the signing write — that
/// never completed rather than a payment to keep graduating (see
/// [`SpliceTracker::clear_persisted_intent`]).
fn record_with_intent_cleared(existing: &PendingPaymentDetails) -> Option<PendingPaymentDetails> {
	match existing {
		PendingPaymentDetails::PendingSplice { .. } => None,
		PendingPaymentDetails::Tracked { .. } => {
			let mut tracked = existing.clone();
			let update = PendingPaymentDetailsUpdate {
				id: tracked.id(),
				payment_update: None,
				conflicting_txids: None,
				candidates: Vec::new(),
				splice_intent: Some(None),
			};
			tracked.update(update).then_some(tracked)
		},
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
	let claimants = candidates.iter().filter_map(|candidate| candidate.contribution.as_ref());
	unclaimed_parts(contribution, claimants).0
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
		test_funding_contribution_with_parts, FundingTxCandidate,
	};
	use crate::payment::store::{ConfirmationStatus, PaymentDetails, PaymentKind};
	use crate::payment::{PaymentDirection, PaymentStatus};
	use lightning::ln::channel_state::SpliceCandidateStatus;

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

	/// A bare intent entry has nothing to keep once its intent is cleared: it is removed rather
	/// than promoted, whatever the payment store holds under its id — a payment record there is
	/// the first half of a signing write that never completed, which the caller removes as well.
	#[test]
	fn intent_clearing_removes_a_bare_intent_entry() {
		let id = PaymentId([9u8; 32]);
		let existing = PendingPaymentDetails::pending_splice(id, test_intent());
		assert!(record_with_intent_cleared(&existing).is_none());
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

		let replacement = record_with_intent_cleared(&existing);
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
		let a = test_funding_contribution_with_outputs(0, 253, &outputs);
		let b = test_funding_contribution_with_outputs(0, 500, &outputs);
		assert!(is_same_splice(&a, &b));

		// Different outputs are a different attempt.
		let other = vec![TxOut { value: Amount::from_sat(2_000), script_pubkey: ScriptBuf::new() }];
		assert!(!is_same_splice(&a, &test_funding_contribution_with_outputs(0, 253, &other)));

		// Contributions moving nothing (no inputs, no outputs) only match themselves exactly.
		assert!(is_same_splice(&test_funding_contribution(), &test_funding_contribution()));
		assert!(!is_same_splice(
			&test_funding_contribution(),
			&test_funding_contribution_with_feerate(500)
		));
	}

	fn intent_with(
		kind: SpliceKind, funding_byte: u8, contribution: FundingContribution,
	) -> SpliceIntent {
		SpliceIntent {
			pre_splice_funding_txo: LdkOutPoint {
				txid: Txid::from_byte_array([funding_byte; 32]),
				index: 0,
			},
			contribution,
			kind,
			..test_intent()
		}
	}

	fn splice_out_contribution(value_sat: u64) -> FundingContribution {
		use bitcoin::{ScriptBuf, TxOut};
		let outputs =
			vec![TxOut { value: Amount::from_sat(value_sat), script_pubkey: ScriptBuf::new() }];
		test_funding_contribution_with_outputs(300, 253, &outputs)
	}

	/// A tracked record of the test channel whose funding payment names `txid` and whose history
	/// lists `candidates`, carrying `intent` if any.
	fn tracked_record(
		id: PaymentId, txid: Txid, candidates: &[Txid], intent: Option<SpliceIntent>,
	) -> PendingPaymentDetails {
		use crate::payment::store::Channel;
		let base = test_intent();
		let details = PaymentDetails::new(
			id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(TransactionType::InteractiveFunding {
					channels: vec![Channel {
						counterparty_node_id: base.counterparty_node_id,
						channel_id: base.channel_id,
					}],
				}),
			},
			Some(1_000_000),
			Some(500),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		);
		let candidates = candidates
			.iter()
			.map(|txid| FundingTxCandidate {
				txid: *txid,
				amount_msat: Some(1_000_000),
				fee_paid_msat: Some(500),
				awaiting_broadcast: false,
			})
			.collect();
		PendingPaymentDetails::tracked(details, Vec::new(), candidates, intent)
	}

	fn txid(byte: u8) -> Txid {
		Txid::from_byte_array([byte; 32])
	}

	/// A splice-in or splice-out is a splice of its own, whatever the channel already carries:
	/// a pending splice's bare intent, or the tracked record of its rounds.
	#[test]
	fn a_splice_in_or_out_gets_a_record_of_its_own() {
		let pending =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 3, splice_out_contribution(1_000));
		let records = vec![
			PendingPaymentDetails::pending_splice(PaymentId([1u8; 32]), pending.clone()),
			tracked_record(PaymentId([2u8; 32]), txid(0x10), &[txid(0x10)], Some(pending)),
		];
		let held = [txid(0x10)];

		let splice_in =
			intent_with(SpliceKind::In { amount_sats: 5_000 }, 3, test_funding_contribution());
		assert_eq!(place_intent(&splice_in, &records, &held), IntentPlacement::Fresh);
		let splice_out =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 3, splice_out_contribution(2_000));
		assert_eq!(place_intent(&splice_out, &records, &held), IntentPlacement::Fresh);
		assert_eq!(place_intent(&splice_out, &[], &[]), IntentPlacement::Fresh);
	}

	/// A fee bump joins the record of the round it replaces: the one tracking a round LDK still
	/// holds, even when that record carries no intent any more and the channel also carries a
	/// bare intent.
	#[test]
	fn a_bump_joins_the_record_tracking_a_held_round() {
		let bump = intent_with(SpliceKind::Rbf {}, 3, test_funding_contribution());
		let bare_id = PaymentId([1u8; 32]);
		let tracked_id = PaymentId([2u8; 32]);
		let records = vec![
			PendingPaymentDetails::pending_splice(
				bare_id,
				intent_with(
					SpliceKind::Out { outputs: Vec::new() },
					3,
					splice_out_contribution(1_000),
				),
			),
			tracked_record(tracked_id, txid(0x11), &[txid(0x10), txid(0x11)], None),
		];
		assert_eq!(
			place_intent(&bump, &records, &[txid(0x11)]),
			IntentPlacement::Reuse(tracked_id)
		);

		// The tracked record of a splice that already locked — its funding is no held round — is
		// not the bump's; the bare intent of the round LDK negotiated but the wallet did not record
		// is.
		assert_eq!(place_intent(&bump, &records, &[]), IntentPlacement::Reuse(bare_id));

		// With neither, the bump starts fresh.
		let locked_only = vec![tracked_record(tracked_id, txid(0x11), &[txid(0x11)], None)];
		assert_eq!(place_intent(&bump, &locked_only, &[]), IntentPlacement::Fresh);
	}

	/// An intent anchored at another funding is one the lock handling failed to settle or to
	/// re-anchor; nothing is recorded beside it, whatever the new splice's kind and whatever
	/// else the channel carries.
	#[test]
	fn an_intent_anchored_elsewhere_refuses_every_kind() {
		let stale =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 4, splice_out_contribution(1_000));
		let current =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 3, splice_out_contribution(2_000));
		let records = vec![
			PendingPaymentDetails::pending_splice(PaymentId([1u8; 32]), current),
			PendingPaymentDetails::pending_splice(PaymentId([2u8; 32]), stale),
		];
		for kind in [
			SpliceKind::In { amount_sats: 5_000 },
			SpliceKind::Out { outputs: Vec::new() },
			SpliceKind::Rbf {},
		] {
			let intent = intent_with(kind, 3, test_funding_contribution());
			assert_eq!(place_intent(&intent, &records, &[]), IntentPlacement::Refused);
		}
	}

	/// A failure identifies the record whose intent carries the failed contribution — fee fields
	/// aside — among the channel's; one reporting no contribution, or a contribution of no
	/// recorded intent, identifies nothing.
	#[test]
	fn a_failure_identifies_the_record_carrying_its_contribution() {
		let first =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 3, splice_out_contribution(1_000));
		let second =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 3, splice_out_contribution(2_000));
		let (first_id, second_id) = (PaymentId([1u8; 32]), PaymentId([2u8; 32]));
		let records = vec![
			PendingPaymentDetails::pending_splice(first_id, first.clone()),
			tracked_record(second_id, txid(0x10), &[txid(0x10)], Some(second.clone())),
		];

		let adjusted = {
			use bitcoin::{ScriptBuf, TxOut};
			let outputs =
				vec![TxOut { value: Amount::from_sat(2_000), script_pubkey: ScriptBuf::new() }];
			test_funding_contribution_with_outputs(900, 1_000, &outputs)
		};
		assert_eq!(record_of_failed_splice(&records, Some(&adjusted)), Some((second_id, second)));
		assert_eq!(
			record_of_failed_splice(&records, Some(&first.contribution)),
			Some((first_id, first))
		);
		assert_eq!(record_of_failed_splice(&records, Some(&splice_out_contribution(3_000))), None);
		assert_eq!(record_of_failed_splice(&records, None), None);
	}

	/// A lock keeps an intent anchored at the locked funding, re-anchors one LDK still holds as a
	/// candidate, and settles any other.
	#[test]
	fn a_lock_keeps_refreshes_or_settles_an_intent() {
		let intent =
			intent_with(SpliceKind::Out { outputs: Vec::new() }, 3, splice_out_contribution(1_000));
		let same_funding = intent.pre_splice_funding_txo.into_bitcoin_outpoint();
		let new_funding = OutPoint { txid: txid(0x20), vout: 0 };
		let held = [SpliceCandidateDetails {
			contribution: Some(splice_out_contribution(1_000)),
			status: SpliceCandidateStatus::WaitingOnLock,
		}];
		let other = [
			SpliceCandidateDetails {
				contribution: Some(splice_out_contribution(2_000)),
				status: SpliceCandidateStatus::WaitingOnLock,
			},
			SpliceCandidateDetails {
				contribution: None,
				status: SpliceCandidateStatus::WaitingOnLock,
			},
		];

		assert_eq!(decide_on_lock(&intent, same_funding, &other), LockDecision::Keep);
		assert_eq!(decide_on_lock(&intent, new_funding, &held), LockDecision::Refresh);
		assert_eq!(decide_on_lock(&intent, new_funding, &other), LockDecision::Settle);
		assert_eq!(decide_on_lock(&intent, new_funding, &[]), LockDecision::Settle);
	}

	/// A submission proceeds at the funding it was built against while that is still the
	/// channel's. Once the funding moved, only a splice-out proceeds, anchored at the live
	/// funding; a bump and a splice-in are refused, as is any submission for a channel LDK no
	/// longer lists with a funding.
	#[test]
	fn a_submission_is_checked_against_the_live_funding() {
		let requested = LdkOutPoint { txid: txid(0x30), index: 0 };
		let moved = LdkOutPoint { txid: txid(0x31), index: 0 };
		let kinds = [
			SpliceKind::In { amount_sats: 5_000 },
			SpliceKind::Out { outputs: Vec::new() },
			SpliceKind::Rbf {},
		];
		for kind in &kinds {
			assert_eq!(check_submission(requested, Some(requested), kind), Ok(requested));
			assert_eq!(
				check_submission(requested, None, kind),
				Err(SubmissionRefusal::ChannelGone)
			);
		}
		assert_eq!(
			check_submission(requested, Some(moved), &SpliceKind::Rbf {}),
			Err(SubmissionRefusal::BumpedRoundLocked)
		);
		assert_eq!(
			check_submission(requested, Some(moved), &SpliceKind::In { amount_sats: 5_000 }),
			Err(SubmissionRefusal::InputsMayBeSpent)
		);
		assert_eq!(
			check_submission(requested, Some(moved), &SpliceKind::Out { outputs: Vec::new() }),
			Ok(moved)
		);
	}

	/// The records concerning a channel's splices are those carrying an intent for it and those
	/// tracking an interactive funding of it; records of other channels and of other payments are
	/// not.
	#[test]
	fn records_concerning_a_channel() {
		let base = test_intent();
		let (cp, channel_id) = (base.counterparty_node_id, base.channel_id);
		let id = PaymentId([1u8; 32]);
		assert!(concerns_channel(
			&PendingPaymentDetails::pending_splice(id, base.clone()),
			cp,
			channel_id
		));
		assert!(concerns_channel(
			&tracked_record(id, txid(0x10), &[txid(0x10)], None),
			cp,
			channel_id
		));

		let other_channel = SpliceIntent { channel_id: ChannelId([8u8; 32]), ..base };
		assert!(!concerns_channel(
			&PendingPaymentDetails::pending_splice(id, other_channel),
			cp,
			channel_id
		));
		assert!(!concerns_channel(
			&tracked_record(id, txid(0x10), &[], None),
			cp,
			ChannelId([8u8; 32])
		));
		let plain = PendingPaymentDetails::new(
			payment_details(id, PaymentStatus::Pending),
			Vec::new(),
			Vec::new(),
		);
		assert!(!concerns_channel(&plain, cp, channel_id));
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
		use bitcoin::WPubkeyHash;

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

	/// Releasing a contribution spares the parts another contribution uses as well: a fee bump
	/// built by adjusting the fee of the round it replaces shares that round's inputs and change
	/// address — the change differing in amount only — so against that round nothing is released;
	/// against a candidate using only some of the parts, the rest is, a counterparty-only round
	/// alongside claiming nothing; against no other contribution, everything is.
	#[test]
	fn unclaimed_parts_spare_what_other_contributions_use() {
		use bitcoin::WPubkeyHash;

		let prevtxs: Vec<Transaction> = (1u8..=3).map(test_prevtx).collect();
		let outpoint = |tx: &Transaction| OutPoint { txid: tx.compute_txid(), vout: 0 };
		let script = |seed: u8| ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([seed; 20]));
		let change = |sats: u64| TxOut { value: Amount::from_sat(sats), script_pubkey: script(9) };
		let splice_out = TxOut { value: Amount::from_sat(50_000), script_pubkey: script(8) };
		let bump = test_funding_contribution_with_parts(
			0,
			300,
			&prevtxs,
			&[splice_out.clone()],
			Some(&change(20_000)),
		);

		let prior = test_funding_contribution_with_parts(
			0,
			253,
			&prevtxs,
			&[splice_out.clone()],
			Some(&change(21_000)),
		);
		assert_eq!(unclaimed_parts(&bump, [&prior]), (Vec::new(), Vec::new()));

		let partial =
			test_funding_contribution_with_parts(0, 253, &prevtxs[..2], &[], Some(&change(21_000)));
		let candidates = [negotiated_candidate(None), negotiated_candidate(Some(partial))];
		let claimants = candidates.iter().filter_map(|candidate| candidate.contribution.as_ref());
		assert_eq!(
			unclaimed_parts(&bump, claimants),
			(vec![outpoint(&prevtxs[2])], vec![splice_out.clone()])
		);

		assert_eq!(
			unclaimed_parts(&bump, []),
			(prevtxs.iter().map(outpoint).collect(), vec![splice_out, change(20_000)])
		);
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
