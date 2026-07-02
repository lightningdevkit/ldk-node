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
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::types::ChannelId;

use crate::data_store::StorableObject;
use crate::event::{Event, EventQueue};
use crate::logger::{log_error, log_info, LdkLogger};
use crate::payment::pending_payment_store::{
	PendingPaymentDetailsUpdate, SpliceIntent, SpliceKind, MAX_SPLICE_ATTEMPTS,
};
use crate::types::{ChannelManager, PendingPaymentStore};
use crate::Error;

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
	pending_payment_store: Arc<PendingPaymentStore>,
	event_queue: Arc<EventQueue<L>>,
	logger: L,
}

impl<L: Deref + Clone> SpliceRetrier<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, pending_payment_store: Arc<PendingPaymentStore>,
		event_queue: Arc<EventQueue<L>>, logger: L,
	) -> Self {
		Self { channel_manager, pending_payment_store, event_queue, logger }
	}

	/// Reconciles persisted splice intents against live channel state. Run once at startup to pick
	/// up splices LDK dropped before durably recording them — including those lost to a crash before
	/// LDK persisted anything.
	pub(crate) async fn reconcile(&self) {
		let records = self.pending_payment_store.list_filter(|p| p.splice_intent().is_some());
		for record in records {
			let id = record.id();
			let has_payment = record.details().is_some();
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

			// `splice_channel` is a read-only probe of LDK's splice state. It fails when we already
			// have a splice in flight (a held contribution, an in-progress negotiation, or one
			// awaiting signatures), all of which LDK drives to completion on its own.
			let template = match self
				.channel_manager
				.splice_channel(&channel.channel_id, &intent.counterparty_node_id)
			{
				Ok(template) => template,
				Err(_) => continue,
			};

			// LDK persists a splice once negotiated, so a prior contribution means the intent was
			// carried out — unless the intent was a fee bump at a higher feerate than negotiated.
			let should_retry = match (&intent.kind, template.prior_contribution()) {
				(SpliceKind::Rbf {}, Some(prior)) => {
					prior.feerate() < intent.contribution.feerate()
				},
				(SpliceKind::Rbf {}, None) => {
					// The splice to bump is gone entirely; surface rather than guess.
					self.abandon(id, has_payment, &intent).await;
					continue;
				},
				(_, Some(_)) => false,
				(_, None) => true,
			};
			if !should_retry {
				continue;
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

	/// Drops a splice intent: removes a pre-broadcast record entirely, or clears just the intent on
	/// a record that already carries a classified funding payment so the payment keeps graduating.
	async fn clear_intent(&self, id: PaymentId, has_payment: bool) {
		if has_payment {
			let update = PendingPaymentDetailsUpdate {
				id,
				payment_update: None,
				conflicting_txids: None,
				candidates: Vec::new(),
				splice_intent: Some(None),
			};
			let _ = self.pending_payment_store.update(update).await;
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
}
