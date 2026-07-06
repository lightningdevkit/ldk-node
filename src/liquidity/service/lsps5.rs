// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::runtime::Runtime;
use bitcoin::secp256k1::PublicKey;
use lightning_liquidity::lsps5::event::LSPS5ServiceEvent;
use lightning_liquidity::lsps5::msgs::LSPS5ProtocolError;

use crate::config::{
	LSPS5_EXPIRY_NOTIFICATION_THRESHOLD_BLOCKS, LSPS5_EXPIRY_RENOTIFY_INTERVAL_BLOCKS,
	LSPS5_WEBHOOK_MAX_RESPONSE_SIZE, LSPS5_WEBHOOK_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::types::{ChannelManager, LiquidityManager, PeerManager};
use crate::Error;

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
	/// Note that this deviates from bLIP-55, which envisions the client coming online in time to
	/// receive the payment that triggered the notification. LDK fails the forward back upstream as
	/// soon as it sees that the client is offline rather than holding the HTLC, so by the time we
	/// notify, that payment is already gone and only a retry from the sender can succeed.
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
				"Skipping notify payment incoming for client {}: rate limited.",
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

		handler.notify_liquidity_management_request(client_id).map_err(|e| {
			match e {
				LSPS5ProtocolError::SlowDownError => log_debug!(
					self.logger,
					"Skipping liquidity management request notification for client {}: rate limited.",
					client_id
				),
				_ => log_error!(
					self.logger,
					"Failed to notify liquidity management request for client {}: {:?}",
					client_id,
					e
				),
			}
			Error::LiquidityNotifyWebhookFailed
		})
	}

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
					"Skipping notify expiry soon for client {}: rate limited.",
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

	/// Notifies offline clients holding HTLCs that are approaching their expiry.
	///
	/// A client that doesn't come online and settle before an HTLC's `cltv_expiry` loses the
	/// payment, and LDK force-closes the channel shortly after. We notify ahead of that deadline
	/// so the client has a chance to come online and settle cooperatively instead.
	pub(crate) fn check_expiring_htlcs(&self) {
		if self.liquidity_manager.lsps5_service_handler().is_none() {
			return;
		}

		let current_height = self.channel_manager.current_best_block().height;

		// Notifications are rate limited per client rather than per channel, so we collapse a
		// client's channels to their single most urgent deadline.
		let mut deadlines: HashMap<PublicKey, u32> = HashMap::new();
		for channel in self.channel_manager.list_channels() {
			for htlc in channel.pending_outbound_htlcs {
				if htlc.cltv_expiry.saturating_sub(current_height)
					> LSPS5_EXPIRY_NOTIFICATION_THRESHOLD_BLOCKS
				{
					continue;
				}

				deadlines
					.entry(channel.counterparty.node_id)
					.and_modify(|deadline| *deadline = (*deadline).min(htlc.cltv_expiry))
					.or_insert(htlc.cltv_expiry);
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
					"Sending webhook notification for {} to {}: {:?}",
					app_name.as_str(),
					url.as_str(),
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
									"Webhook call failed with status {} for {} to {}",
									response.status_code,
									app_name.as_str(),
									url.as_str()
								);
							}
						},
						Err(e) => {
							log_error!(
								logger,
								"Failed to send webhook notification for {} to {}: {}",
								app_name.as_str(),
								url.as_str(),
								e
							);
						},
					}
				})
			},
		}
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
}
