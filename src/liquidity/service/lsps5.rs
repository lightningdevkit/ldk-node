// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Instant;

use bitcoin::secp256k1::PublicKey;
use lightning::chain::channelmonitor::HTLC_FAIL_BACK_BUFFER;
use lightning::ln::channel_state::InboundHTLCStateDetails;
use lightning::ln::channelmanager::InterceptId;
use lightning::ln::types::ChannelId;
use lightning_liquidity::lsps5::event::LSPS5ServiceEvent;
use lightning_liquidity::lsps5::msgs::LSPS5ProtocolError;

use crate::config::{
	LSPS5_EXPIRY_NOTIFICATION_THRESHOLD_BLOCKS, LSPS5_EXPIRY_RENOTIFY_INTERVAL_BLOCKS,
	LSPS5_INTERCEPT_HOLD_TIMEOUT, LSPS5_WEBHOOK_MAX_RESPONSE_SIZE, LSPS5_WEBHOOK_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::runtime::Runtime;
use crate::types::{ChannelManager, LiquidityManager, PeerManager};
use crate::Error;

#[derive(Clone, Copy)]
pub(crate) struct PendingIntercept {
	client_id: PublicKey,
	channel_id: ChannelId,
	amt_to_forward_msat: u64,
	deadline: Instant,
}

pub(crate) struct LSPS5ServiceLiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) peer_manager: RwLock<Option<Weak<PeerManager>>>,
	pub(crate) expiry_notified_at: Mutex<HashMap<PublicKey, u32>>,
	pub(crate) runtime: Arc<Runtime>,
	pub(crate) logger: L,
	pub(crate) pending_intercepts: Mutex<HashMap<InterceptId, PendingIntercept>>,
}

impl<L: Deref> LSPS5ServiceLiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn set_peer_manager(&self, peer_manager: Weak<PeerManager>) {
		*self.peer_manager.write().expect("lock") = Some(peer_manager);
	}

	/// Notifies a client that a payment is incoming for them.
	///
	/// The notification is sent while we hold the HTLC for them.
	pub(crate) fn notify_payment_incoming(&self, client_id: PublicKey) {
		let Some(handler) = self.liquidity_manager.lsps5_service_handler() else {
			return;
		};

		if !self.is_client_offline(&client_id) {
			return;
		}

		handler.notify_payment_incoming(client_id).unwrap_or_else(|e| match e {
			LSPS5ProtocolError::SlowDownError => log_debug!(
				self.logger,
				"Skipping payment incoming notification for client {}: rate limited.",
				client_id
			),
			_ => log_error!(
				self.logger,
				"Failed to notify payment incoming for client {}: {:?}",
				client_id,
				e
			),
		})
	}

	pub(crate) fn notify_liquidity_management_request(
		&self, client_id: PublicKey,
	) -> Result<(), Error> {
		let Some(handler) = self.liquidity_manager.lsps5_service_handler() else {
			return Err(Error::LiquiditySourceUnavailable);
		};

		if !self.is_client_offline(&client_id) {
			log_debug!(
				self.logger,
				"Skipping liquidity management request notification for client {}: already connected.",
				client_id
			);
			return Ok(());
		}

		handler.notify_liquidity_management_request(client_id).map_err(|e| match e {
			LSPS5ProtocolError::SlowDownError => {
				log_debug!(
					self.logger,
					"Skipping liquidity management request notification for client {}: rate limited.",
					client_id
				);
				Error::LiquidityNotifyWebhookRateLimited
			},
			_ => {
				log_error!(
					self.logger,
					"Failed to notify liquidity management request for client {}: {:?}",
					client_id,
					e
				);
				Error::LiquidityNotifyWebhookFailed
			},
		})
	}

	/// Notifies a client that onion messages are waiting for them.
	///
	/// The messages are held in our onion message mailbox and delivered once the client
	/// reconnects.
	pub(crate) fn notify_onion_message_incoming(&self, client_id: PublicKey) {
		let Some(handler) = self.liquidity_manager.lsps5_service_handler() else {
			return;
		};

		if !self.is_client_offline(&client_id) {
			return;
		}

		handler.notify_onion_message_incoming(client_id).unwrap_or_else(|e| match e {
			LSPS5ProtocolError::SlowDownError => log_debug!(
				self.logger,
				"Skipping onion message incoming notification for client {}: rate limited.",
				client_id
			),
			_ => log_error!(
				self.logger,
				"Failed to notify onion message incoming for client {}: {:?}",
				client_id,
				e
			),
		})
	}

	fn notify_expiry_soon(&self, client_id: PublicKey, timeout: u32) -> bool {
		let Some(handler) = self.liquidity_manager.lsps5_service_handler() else {
			return false;
		};

		if !self.is_client_offline(&client_id) {
			return false;
		}

		match handler.notify_expiry_soon(client_id, timeout) {
			Ok(()) => true,
			Err(LSPS5ProtocolError::SlowDownError) => {
				log_debug!(
					self.logger,
					"Skipping expiry soon notification for client {}: rate limited.",
					client_id
				);
				false
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to notify expiry soon for client {}: {:?}",
					client_id,
					e
				);
				false
			},
		}
	}

	/// Notifies offline clients about pending HTLCs that are approaching the height at which we'd
	/// be forced to close their channel.
	///
	/// A client that doesn't come online and settle an HTLC in time costs us the channel, as LDK
	/// has to enforce it on-chain. We notify ahead of that deadline so the client has a chance to
	/// come online and settle cooperatively instead.
	pub(crate) fn check_expiring_htlcs(&self) {
		if self.liquidity_manager.lsps5_service_handler().is_none() {
			return;
		}

		let current_height = self.channel_manager.current_best_block().height;

		// Notifications are rate limited per client rather than per channel, so we collapse a
		// client's channels to their single most urgent deadline.
		let mut deadlines: HashMap<PublicKey, u32> = HashMap::new();
		for channel in self.channel_manager.list_channels() {
			// LDK force-closes `LATENCY_GRACE_PERIOD_BLOCKS` after an outbound HTLC's
			// `cltv_expiry`, and `CLTV_CLAIM_BUFFER` before an inbound one's. Neither is exported,
			// so we use `HTLC_FAIL_BACK_BUFFER` instead, which notifies a few blocks early.
			let outbound = channel.pending_outbound_htlcs.iter().map(|htlc| htlc.cltv_expiry);

			// We only force-close over inbound HTLCs we know the preimage for.
			let inbound = channel
				.pending_inbound_htlcs
				.iter()
				.filter(|htlc| {
					htlc.state == Some(InboundHTLCStateDetails::AwaitingRemoteRevokeToRemoveFulfill)
				})
				.map(|htlc| htlc.cltv_expiry.saturating_sub(HTLC_FAIL_BACK_BUFFER));

			for deadline in outbound.chain(inbound) {
				if deadline.saturating_sub(current_height)
					> LSPS5_EXPIRY_NOTIFICATION_THRESHOLD_BLOCKS
				{
					continue;
				}

				deadlines
					.entry(channel.counterparty.node_id)
					.and_modify(|current| *current = (*current).min(deadline))
					.or_insert(deadline);
			}
		}

		let mut notified_at = self.expiry_notified_at.lock().expect("lock");

		// Forget clients whose HTLCs resolved, so a later one notifies immediately again.
		notified_at.retain(|client_id, _| deadlines.contains_key(client_id));

		for (client_id, timeout) in deadlines {
			let is_due = notified_at.get(&client_id).is_none_or(|last_height| {
				current_height.saturating_sub(*last_height) >= LSPS5_EXPIRY_RENOTIFY_INTERVAL_BLOCKS
			});

			if is_due && self.notify_expiry_soon(client_id, timeout) {
				notified_at.insert(client_id, current_height);
			}
		}
	}

	/// Resolves the SCID a sender asked us to forward over to one of our channels.
	///
	/// Returns `None` for SCIDs that aren't ours, in particular the intercept SCIDs LSPS2 hands
	/// out.
	pub(crate) fn resolve_client_channel(&self, scid: u64) -> Option<(PublicKey, ChannelId)> {
		self.liquidity_manager.lsps5_service_handler()?;

		self.channel_manager.list_channels().into_iter().find_map(|channel| {
			let matches = channel.short_channel_id == Some(scid)
				|| channel.inbound_scid_alias == Some(scid)
				|| channel.outbound_scid_alias == Some(scid);
			matches.then(|| (channel.counterparty.node_id, channel.channel_id))
		})
	}

	/// Holds an HTLC intercepted for an offline client while we try to wake them.
	///
	/// We notify the client's webhooks and then wait for them to connect and their channel to
	/// become usable, forwarding as soon as it is. If they don't make it within
	/// [`LSPS5_INTERCEPT_HOLD_TIMEOUT`], we fail the HTLC back so the sender can retry against a
	/// client that is by then hopefully online.
	pub(crate) fn handle_htlc_intercepted(
		&self, client_id: PublicKey, channel_id: ChannelId, intercept_id: InterceptId,
		amt_to_forward_msat: u64,
	) {
		if self.liquidity_manager.lsps5_service_handler().is_none() {
			self.fail_intercepted_htlc(intercept_id);
			return;
		}

		self.pending_intercepts.lock().expect("lock").insert(
			intercept_id,
			PendingIntercept {
				client_id,
				channel_id,
				amt_to_forward_msat,
				deadline: Instant::now() + LSPS5_INTERCEPT_HOLD_TIMEOUT,
			},
		);

		self.notify_payment_incoming(client_id);
	}

	fn is_client_offline(&self, client_id: &PublicKey) -> bool {
		match self.peer_manager.read().expect("lock").as_ref().and_then(|w| w.upgrade()) {
			Some(pm) => pm.peer_by_node_id(client_id).is_none(),
			None => {
				log_debug!(
					self.logger,
					"No peer manager available, assuming client {} is offline.",
					client_id
				);
				true
			},
		}
	}

	/// Forwards HTLCs held for clients that came online, and fails those whose hold expired.
	pub(crate) fn sweep_pending_intercepts(&self) {
		let mut ready = Vec::new();
		let mut expired = Vec::new();

		{
			let mut pending = self.pending_intercepts.lock().expect("lock");
			if pending.is_empty() {
				return;
			}

			let now = Instant::now();
			pending.retain(|intercept_id, intercept| {
				let is_usable = self
					.channel_manager
					.list_channels_with_counterparty(&intercept.client_id)
					.iter()
					.any(|c| c.channel_id == intercept.channel_id && c.is_usable);

				if is_usable {
					ready.push((*intercept_id, *intercept));
					false
				} else if now >= intercept.deadline {
					expired.push((*intercept_id, intercept.client_id));
					false
				} else {
					true
				}
			});
		}

		for (intercept_id, intercept) in ready {
			match self.channel_manager.forward_intercepted_htlc(
				intercept_id,
				&intercept.channel_id,
				intercept.client_id,
				intercept.amt_to_forward_msat,
			) {
				Ok(()) => {
					log_info!(
						self.logger,
						"LSPS5 client {} came online, forwarded the HTLC we held for them.",
						intercept.client_id
					);
				},
				Err(e) => {
					// The channel may have stopped being usable between our check and this call, so
					// we fail the HTLC rather than keep holding it.
					log_error!(
						self.logger,
						"Failed to forward HTLC held for LSPS5 client {}: {:?}",
						intercept.client_id,
						e
					);
					self.fail_intercepted_htlc(intercept_id);
				},
			}
		}

		for (intercept_id, client_id) in expired {
			log_debug!(
				self.logger,
				"LSPS5 client {} did not come online in time, failing HTLC back.",
				client_id
			);
			self.fail_intercepted_htlc(intercept_id);
		}
	}

	fn fail_intercepted_htlc(&self, intercept_id: InterceptId) {
		if let Err(e) = self.channel_manager.fail_intercepted_htlc(intercept_id) {
			log_error!(self.logger, "Failed to fail back intercepted HTLC: {:?}", e);
		}
	}

	pub(crate) async fn handle_event(&self, event: LSPS5ServiceEvent)
	where
		L: Clone + Send + Sync + 'static,
	{
		match event {
			LSPS5ServiceEvent::SendWebhookNotification {
				counterparty_node_id: _,
				app_name,
				url,
				notification,
				headers,
			} => {
				if self.liquidity_manager.lsps5_service_handler().is_none() {
					log_error!(
						self.logger,
						"Received unexpected LSPS5ServiceEvent::SendWebhookNotification event!"
					);
					return;
				}

				log_info!(
					self.logger,
					"Sending webhook notification for {}: {:?}",
					app_name.as_str(),
					notification
				);

				let notification_body = notification.to_request_body();
				let logger = self.logger.clone();

				// `url` is client-supplied, so awaiting delivery here would let any client stall
				// the liquidity event loop for the full timeout. Deliver out of band instead.
				self.runtime.spawn_cancellable_background_task(async move {
					let result = bitreq::post(url.as_str())
						.with_headers(headers)
						.with_body(notification_body)
						.with_timeout(LSPS5_WEBHOOK_TIMEOUT_SECS)
						.with_max_redirects(0)
						.with_max_body_size(LSPS5_WEBHOOK_MAX_RESPONSE_SIZE)
						.send_async()
						.await;

					match result {
						Ok(response) => {
							if response.status_code != 200 {
								log_error!(
									logger,
									"Webhook call failed with status {} for {}",
									response.status_code,
									app_name.as_str(),
								);
							}
						},
						Err(e) => {
							log_error!(
								logger,
								"Failed to send webhook notification for {}: {}",
								app_name.as_str(),
								e
							);
						},
					}
				})
			},
		}
	}
}
