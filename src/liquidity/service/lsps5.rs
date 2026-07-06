// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::sync::{Arc, RwLock, Weak};

use bitcoin::secp256k1::PublicKey;
use lightning_liquidity::lsps5::event::LSPS5ServiceEvent;

use crate::logger::{log_error, log_info, LdkLogger};
use crate::types::{LiquidityManager, PeerManager};
use crate::Error;

pub(crate) struct LSPS5ServiceLiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) peer_manager: RwLock<Option<Weak<PeerManager>>>,
	pub(crate) logger: L,
}

impl<L: Deref> LSPS5ServiceLiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn set_peer_manager(&self, peer_manager: Weak<PeerManager>) {
		*self.peer_manager.write().expect("lock") = Some(peer_manager);
	}

	pub(crate) fn notify_payment_incoming(&self, client_id: PublicKey) {
		if !self.is_client_offline(&client_id) {
			return;
		}

		if let Some(handler) = self.liquidity_manager.lsps5_service_handler() {
			handler.notify_payment_incoming(client_id).unwrap_or_else(|e| {
				log_error!(
					self.logger,
					"Failed to notify payment incoming for client {}: {:?}",
					client_id,
					e
				);
			})
		} else {
			log_error!(self.logger, "LSPS5 service handler is not available.");
		}
	}

	pub(crate) fn notify_expiry_soon(
		&self, client_id: PublicKey, timeout: u32,
	) -> Result<(), Error> {
		let handler = self
			.liquidity_manager
			.lsps5_service_handler()
			.ok_or(Error::LiquiditySourceUnavailable)?;
		handler.notify_expiry_soon(client_id, timeout).map_err(|_| Error::LiquidityRequestFailed)
	}

	pub(crate) fn notify_liquidity_management_request(
		&self, client_id: PublicKey,
	) -> Result<(), Error> {
		let handler = self
			.liquidity_manager
			.lsps5_service_handler()
			.ok_or(Error::LiquiditySourceUnavailable)?;
		handler
			.notify_liquidity_management_request(client_id)
			.map_err(|_| Error::LiquidityRequestFailed)
	}

	pub(crate) fn notify_onion_message_incoming(&self, client_id: PublicKey) {
		if !self.is_client_offline(&client_id) {
			return;
		}

		if let Some(handler) = self.liquidity_manager.lsps5_service_handler() {
			handler.notify_onion_message_incoming(client_id).unwrap_or_else(|e| {
				log_error!(
					self.logger,
					"Failed to notify onion message incoming for client {}: {:?}",
					client_id,
					e
				);
			})
		}
	}

	pub(crate) async fn handle_event(&self, event: LSPS5ServiceEvent) {
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

				let result = bitreq::post(url.as_str())
					.with_headers(headers)
					.with_body(notification_body)
					.send_async()
					.await;

				match result {
					Ok(response) => {
						if response.status_code != 200 {
							log_error!(
								self.logger,
								"Webhook call failed with status {} for {} to {}",
								response.status_code,
								app_name.as_str(),
								url.as_str()
							);
						}
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to send webhook notification for {} to {}: {}",
							app_name.as_str(),
							url.as_str(),
							e
						);
					},
				}
			},
		}
	}

	fn is_client_offline(&self, client_id: &PublicKey) -> bool {
		match self.peer_manager.read().expect("lock").as_ref().and_then(|w| w.upgrade()) {
			Some(pm) => pm.peer_by_node_id(client_id).is_none(),
			None => false,
		}
	}
}
