// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Per-channel state tracking.

pub(crate) mod store;

use std::ops::Deref;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, FeeRate};
use lightning::events::NegotiationFailureReason;
use lightning::ln::funding::FundingContribution;
use lightning::ln::types::ChannelId;

use crate::event::{Event, EventQueue};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_error, log_info, LdkLogger};
use crate::types::{ChannelManager, ChannelRecordStore, UserChannelId, Wallet};
use crate::Error;

pub(crate) use self::store::MAX_SPLICE_ATTEMPTS;
use self::store::{ChannelRecord, ChannelRecordUpdate, SpliceIntent, SpliceKind};

/// Resubmits user-initiated splices that LDK dropped before durably recording them.
///
/// LDK only persists a splice once its negotiation reaches `AwaitingSignatures`, and it abandons
/// an earlier negotiation whenever the peer disconnects (which includes restarting the node). The
/// splice entry points persist a [`SpliceIntent`] before handing the contribution to LDK; this
/// type drives that intent back into [`ChannelManager::funding_contributed`] until the splice
/// either locks (clearing the intent on `ChannelReady`) or fails for a reason that retrying
/// cannot address.
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
	channel_record_store: Arc<ChannelRecordStore>,
	event_queue: Arc<EventQueue<L>>,
	logger: L,
}

impl<L: Deref + Clone> SpliceRetrier<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, wallet: Arc<Wallet>,
		fee_estimator: Arc<OnchainFeeEstimator>, channel_record_store: Arc<ChannelRecordStore>,
		event_queue: Arc<EventQueue<L>>, logger: L,
	) -> Self {
		Self { channel_manager, wallet, fee_estimator, channel_record_store, event_queue, logger }
	}

	/// Reconciles persisted splice intents against live channel state. Run once at startup.
	pub(crate) async fn reconcile(&self) {
		let records = self.channel_record_store.list_filter(|r| r.pending_splice().is_some());
		for record in records {
			let ChannelRecord::Funded {
				user_channel_id, counterparty_node_id, pending_splice, ..
			} = record;
			let intent = match pending_splice {
				Some(intent) => intent,
				None => continue,
			};

			let channel = self
				.channel_manager
				.list_channels_with_counterparty(&counterparty_node_id)
				.into_iter()
				.find(|c| c.user_channel_id == user_channel_id.0);
			let channel = match channel {
				Some(channel) => channel,
				None => {
					// The channel is gone; there is nothing to splice anymore.
					let _ = self.channel_record_store.remove(&user_channel_id);
					continue;
				},
			};

			if channel.funding_txo != Some(intent.pre_splice_funding_txo) {
				// The funding moved on, so the splice (or a replacement) locked.
				let _ = self.channel_record_store.remove(&user_channel_id);
				continue;
			}

			// `splice_channel` is a read-only probe of LDK's splice state. It fails when we
			// already have a splice in flight (a held contribution, an in-progress negotiation,
			// or one awaiting signatures), all of which LDK drives to completion on its own.
			let template = match self
				.channel_manager
				.splice_channel(&channel.channel_id, &counterparty_node_id)
			{
				Ok(template) => template,
				Err(_) => continue,
			};

			// The template's prior contribution is our last negotiated one. LDK persists a splice
			// once negotiated, so its presence means the intent was carried out unless the intent
			// was a fee bump at a higher feerate than what was negotiated.
			let should_retry = match (&intent.kind, template.prior_contribution()) {
				(SpliceKind::Rbf {}, Some(prior)) => {
					prior.feerate() < intent.contribution.feerate()
				},
				(SpliceKind::Rbf {}, None) => {
					// The splice to bump is gone entirely; surface rather than guess.
					self.abandon(user_channel_id, channel.channel_id, counterparty_node_id).await;
					continue;
				},
				(_, Some(_)) => false,
				(_, None) => true,
			};
			if !should_retry {
				continue;
			}

			if intent.attempts >= MAX_SPLICE_ATTEMPTS {
				self.abandon(user_channel_id, channel.channel_id, counterparty_node_id).await;
				continue;
			}

			log_info!(
				self.logger,
				"Resubmitting splice for channel {} with counterparty {}",
				channel.channel_id,
				counterparty_node_id,
			);
			let _ =
				self.submit(&channel.channel_id, &counterparty_node_id, user_channel_id, intent);
		}
	}

	/// Applies a `SpliceNegotiationFailed` event to any matching splice intent, retrying when the
	/// failure is recoverable. Returns whether the failure should be surfaced to the user.
	pub(crate) async fn on_negotiation_failed(
		&self, user_channel_id: UserChannelId, reason: NegotiationFailureReason,
		contribution: Option<FundingContribution>,
	) -> bool {
		let record = match self.channel_record_store.get(&user_channel_id) {
			Some(record) => record,
			None => return true,
		};
		let ChannelRecord::Funded { channel_id, counterparty_node_id, pending_splice, .. } = record;
		let mut intent = match pending_splice {
			Some(intent) => intent,
			None => return true,
		};

		// Only act on failures of the splice we are tracking. A mismatch means the failure
		// concerns some other attempt (e.g., a stale event replayed after the user initiated a
		// new splice), in which case the record must be left alone.
		if contribution.as_ref() != Some(&intent.contribution) {
			return true;
		}

		if intent.attempts >= MAX_SPLICE_ATTEMPTS {
			let _ = self.channel_record_store.remove(&user_channel_id);
			return true;
		}

		match reason {
			NegotiationFailureReason::PeerDisconnected => {
				// The same contribution remains valid. Skip if LDK already has a splice in
				// flight for this channel (e.g., the startup reconciler resubmitted first).
				if self.channel_manager.splice_channel(&channel_id, &counterparty_node_id).is_err()
				{
					return false;
				}
				log_info!(
					self.logger,
					"Resubmitting splice for channel {} with counterparty {} after disconnect",
					channel_id,
					counterparty_node_id,
				);
				let _ = self.submit(&channel_id, &counterparty_node_id, user_channel_id, intent);
				false
			},
			NegotiationFailureReason::FeeRateTooLow
			| NegotiationFailureReason::ContributionInvalid => {
				// The contribution went stale (e.g., another splice negotiation outpaced ours,
				// turning the resubmission into an underpaying fee bump of it). Rebuild a fresh
				// contribution from the original call's parameters.
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
						intent.contribution = contribution;
						let _ = self.submit(
							&channel_id,
							&counterparty_node_id,
							user_channel_id,
							intent,
						);
						false
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Abandoning splice for channel {}: failed to rebuild contribution: {:?}",
							channel_id,
							e,
						);
						let _ = self.channel_record_store.remove(&user_channel_id);
						true
					},
				}
			},
			_ => {
				// Terminal failure; retrying cannot address it.
				let _ = self.channel_record_store.remove(&user_channel_id);
				true
			},
		}
	}

	/// Clears any splice intent made obsolete by a locked funding transaction.
	pub(crate) fn on_channel_ready(
		&self, user_channel_id: UserChannelId, funding_txo: Option<bitcoin::OutPoint>,
	) {
		let record = match self.channel_record_store.get(&user_channel_id) {
			Some(record) => record,
			None => return,
		};
		// Only clear an intent predating the locked funding transaction. An intent with a
		// matching pre-splice funding outpoint was created after the lock and is still pending.
		let clear = match (record.pending_splice(), funding_txo) {
			(Some(intent), Some(funding_txo)) => {
				intent.pre_splice_funding_txo.into_bitcoin_outpoint() != funding_txo
			},
			(Some(_), None) => false,
			(None, _) => true,
		};
		if clear {
			let _ = self.channel_record_store.remove(&user_channel_id);
		}
	}

	/// Clears any splice intent for a closed channel, as there is nothing left to splice.
	pub(crate) fn on_channel_closed(&self, user_channel_id: UserChannelId) {
		let _ = self.channel_record_store.remove(&user_channel_id);
	}

	/// Persists the incremented attempt count and hands the contribution back to LDK. The count
	/// is persisted first so that a crash mid-submission cannot lead to unbounded retries.
	fn submit(
		&self, channel_id: &ChannelId, counterparty_node_id: &PublicKey,
		user_channel_id: UserChannelId, mut intent: SpliceIntent,
	) -> Result<(), Error> {
		intent.attempts += 1;
		let contribution = intent.contribution.clone();
		let update = ChannelRecordUpdate { user_channel_id, pending_splice: Some(Some(intent)) };
		self.channel_record_store.update(update)?;

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
		let min_feerate =
			template.min_rbf_feerate().map_or(est_feerate, |min_rbf| est_feerate.max(min_rbf));
		let max_feerate = FeeRate::from_sat_per_kwu(min_feerate.to_sat_per_kwu() * 3 / 2);

		match kind {
			SpliceKind::In { amount_sats } => template
				.splice_in(
					Amount::from_sat(*amount_sats),
					min_feerate,
					max_feerate,
					Arc::clone(&self.wallet),
				)
				.await
				.map_err(|_| Error::ChannelSplicingFailed),
			SpliceKind::Out { outputs } => template
				.splice_out(outputs.clone(), min_feerate, max_feerate)
				.map_err(|_| Error::ChannelSplicingFailed),
			SpliceKind::Rbf {} => template
				.rbf_prior_contribution(None, max_feerate, Arc::clone(&self.wallet))
				.await
				.map_err(|_| Error::ChannelSplicingFailed),
		}
	}

	/// Gives up on a splice intent and surfaces the failure to the user.
	async fn abandon(
		&self, user_channel_id: UserChannelId, channel_id: ChannelId,
		counterparty_node_id: PublicKey,
	) {
		log_error!(
			self.logger,
			"Abandoning splice for channel {} with counterparty {}",
			channel_id,
			counterparty_node_id,
		);
		let _ = self.channel_record_store.remove(&user_channel_id);
		let event =
			Event::SpliceNegotiationFailed { channel_id, user_channel_id, counterparty_node_id };
		if let Err(e) = self.event_queue.add_event(event).await {
			log_error!(self.logger, "Failed to push to event queue: {}", e);
		}
	}
}
