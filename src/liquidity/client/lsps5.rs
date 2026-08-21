// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use lightning::log_debug;
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps5::event::LSPS5ClientEvent;
use lightning_liquidity::lsps5::msgs::{
	LSPS5Error, LSPS5ProtocolError, ListWebhooksResponse, RemoveWebhookResponse, SetWebhookResponse,
};
use tokio::sync::oneshot;

use crate::connection::ConnectionManager;
use crate::liquidity::service::lsps5::LSPS5ServiceLiquiditySource;
use crate::liquidity::{
	select_lsps_for_protocol, LspConfig, LspNode, LIQUIDITY_REQUEST_TIMEOUT_SECS,
	LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::LiquidityManager;
use crate::Error;

pub(crate) struct LSPS5Client<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_set_webhook_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<Result<SetWebhookResponse, LSPS5Error>>>>,
	pub(crate) pending_list_webhooks_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<Result<ListWebhooksResponse, LSPS5Error>>>>,
	pub(crate) pending_remove_webhook_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<Result<RemoveWebhookResponse, LSPS5Error>>>>,
	pub(crate) discovery_done_rx: tokio::sync::watch::Receiver<bool>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) logger: L,
}

impl<L: Deref> LSPS5Client<L>
where
	L::Target: LdkLogger,
{
	pub(crate) async fn lsps5_set_webhook(
		&self, app_name: String, webhook_url: String, node_id: Option<&PublicKey>,
	) -> Result<LSPS5SetWebhookResponse, Error> {
		let lsps5_node = select_lsps_for_protocol(&self.lsp_nodes, 5, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;
		let client_handler = self.liquidity_manager.lsps5_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS5 liquidity client was not configured.");
			Error::LiquiditySourceUnavailable
		})?;

		let (sender, receiver) = oneshot::channel();
		{
			let mut pending_set_webhook_requests_lock =
				self.pending_set_webhook_requests.lock().expect("lock");

			let request_id = match client_handler.set_webhook(
				lsps5_node.node_id,
				app_name.clone(),
				webhook_url.clone(),
			) {
				Ok(request_id) => request_id,
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to send set webhook request to liquidity service: {:?}",
						e
					);
					return Err(self.map_lsps5_error(&e, Error::LiquiditySetWebhookFailed));
				},
			};

			pending_set_webhook_requests_lock.insert(request_id, sender);
		}

		match tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), receiver)
			.await
		{
			Ok(Ok(result)) => result.map(LSPS5SetWebhookResponse::from).map_err(|e| {
				log_error!(self.logger, "Failed to set webhook: {:?}", e);
				self.map_lsps5_error(&e, Error::LiquiditySetWebhookFailed)
			}),
			Ok(Err(e)) => {
				log_error!(
					self.logger,
					"Failed to handle response from liquidity service: {:?}",
					e
				);
				Err(Error::LiquidityRequestFailed)
			},
			Err(e) => {
				log_error!(self.logger, "Liquidity request timed out: {}", e);
				Err(Error::LiquidityRequestFailed)
			},
		}
	}

	pub(crate) async fn lsps5_list_webhooks(
		&self, node_id: Option<&PublicKey>,
	) -> Result<LSPS5ListWebhooksResponse, Error> {
		let lsps5_node = select_lsps_for_protocol(&self.lsp_nodes, 5, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;
		let client_handler = self.liquidity_manager.lsps5_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS5 liquidity client was not configured.");
			Error::LiquiditySourceUnavailable
		})?;

		let (sender, receiver) = oneshot::channel();
		{
			let mut pending_list_webhooks_requests_lock =
				self.pending_list_webhooks_requests.lock().expect("lock");
			let request_id = client_handler.list_webhooks(lsps5_node.node_id);
			pending_list_webhooks_requests_lock.insert(request_id.clone(), sender);
		}

		match tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), receiver)
			.await
		{
			Ok(Ok(result)) => result.map(LSPS5ListWebhooksResponse::from).map_err(|e| {
				log_error!(self.logger, "Failed to list webhooks: {:?}", e);
				self.map_lsps5_error(&e, Error::LiquidityListWebhooksFailed)
			}),
			Ok(Err(e)) => {
				log_error!(
					self.logger,
					"Failed to handle response from liquidity service: {:?}",
					e
				);
				Err(Error::LiquidityRequestFailed)
			},
			Err(e) => {
				log_error!(self.logger, "Liquidity request timed out: {}", e);
				Err(Error::LiquidityRequestFailed)
			},
		}
	}

	pub(crate) async fn lsps5_remove_webhook(
		&self, app_name: String, node_id: Option<&PublicKey>,
	) -> Result<(), Error> {
		let lsps5_node = select_lsps_for_protocol(&self.lsp_nodes, 5, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;
		let client_handler = self.liquidity_manager.lsps5_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS5 liquidity client was not configured.");
			Error::LiquiditySourceUnavailable
		})?;

		let (sender, receiver) = oneshot::channel();
		{
			let mut pending_remove_webhook_requests_lock =
				self.pending_remove_webhook_requests.lock().expect("lock");
			let request_id =
				match client_handler.remove_webhook(lsps5_node.node_id, app_name.clone()) {
					Ok(request_id) => request_id,
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to send remove webhook request to liquidity service: {:?}",
							e
						);
						return Err(self.map_lsps5_error(&e, Error::LiquidityRemoveWebhookFailed));
					},
				};

			pending_remove_webhook_requests_lock.insert(request_id.clone(), sender);
		}

		match tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), receiver)
			.await
		{
			Ok(Ok(result)) => result.map(|_| ()).map_err(|e| {
				log_error!(self.logger, "Failed to remove webhook: {:?}", e);
				self.map_lsps5_error(&e, Error::LiquidityRemoveWebhookFailed)
			}),
			Ok(Err(e)) => {
				log_error!(
					self.logger,
					"Failed to handle response from liquidity service: {:?}",
					e
				);
				Err(Error::LiquidityRequestFailed)
			},
			Err(e) => {
				log_error!(self.logger, "Liquidity request timed out: {}", e);
				Err(Error::LiquidityRequestFailed)
			},
		}
	}

	pub(crate) async fn handle_event(&self, event: LSPS5ClientEvent) {
		match event {
			LSPS5ClientEvent::WebhookRegistered {
				request_id,
				counterparty_node_id,
				num_webhooks,
				max_webhooks,
				no_change,
				..
			} => {
				if !self.is_expected_counterparty(
					&counterparty_node_id,
					"LSPS5Client::WebhookRegistered".into(),
				) {
					return;
				}

				let response = Ok(SetWebhookResponse { num_webhooks, max_webhooks, no_change });
				self.deliver_response(&self.pending_set_webhook_requests, &request_id, response);
			},
			LSPS5ClientEvent::WebhookRegistrationFailed {
				request_id,
				counterparty_node_id,
				error,
				app_name,
				url,
			} => {
				if !self.is_expected_counterparty(
					&counterparty_node_id,
					"LSPS5Client::WebhookRegistrationFailed".into(),
				) {
					return;
				}

				log_error!(
					self.logger,
					"Webhook registration failed for app '{}' with url '{}': {:?}",
					app_name.as_str(),
					url.as_str(),
					error
				);
				self.deliver_response(&self.pending_set_webhook_requests, &request_id, Err(error));
			},
			LSPS5ClientEvent::WebhooksListed {
				request_id,
				counterparty_node_id,
				app_names,
				max_webhooks,
			} => {
				if !self.is_expected_counterparty(
					&counterparty_node_id,
					"LSPS5Client::WebhooksListed".into(),
				) {
					return;
				}

				let response = Ok(ListWebhooksResponse { app_names, max_webhooks });
				self.deliver_response(&self.pending_list_webhooks_requests, &request_id, response);
			},
			LSPS5ClientEvent::WebhookRemoved { request_id, counterparty_node_id, .. } => {
				if !self.is_expected_counterparty(
					&counterparty_node_id,
					"LSPS5Client::WebhookRemoved".into(),
				) {
					return;
				}

				self.deliver_response(
					&self.pending_remove_webhook_requests,
					&request_id,
					Ok(RemoveWebhookResponse {}),
				);
			},
			LSPS5ClientEvent::WebhookRemovalFailed {
				request_id,
				counterparty_node_id,
				error,
				app_name,
			} => {
				if !self.is_expected_counterparty(
					&counterparty_node_id,
					"LSPS5Client::WebhookRemovalFailed".into(),
				) {
					return;
				}

				log_error!(
					self.logger,
					"Webhook removal failed for app '{}': {:?}",
					app_name.as_str(),
					error
				);
				self.deliver_response(
					&self.pending_remove_webhook_requests,
					&request_id,
					Err(error),
				);
			},
		}
	}

	fn map_lsps5_error(&self, e: &LSPS5Error, fallback: Error) -> Error {
		match e {
			LSPS5Error::Protocol(LSPS5ProtocolError::TooManyWebhooks) => {
				Error::LiquidityWebhookLimitExceeded
			},
			LSPS5Error::Protocol(LSPS5ProtocolError::NoPriorActivityError) => {
				Error::LiquidityWebhookNoPriorActivity
			},
			LSPS5Error::Protocol(LSPS5ProtocolError::AppNameNotFound) => {
				Error::LiquidityWebhookAppNameNotFound
			},
			LSPS5Error::Protocol(
				LSPS5ProtocolError::AppNameTooLong
				| LSPS5ProtocolError::WebhookUrlTooLong
				| LSPS5ProtocolError::UrlParse
				| LSPS5ProtocolError::UnsupportedProtocol,
			) => Error::LiquidityWebhookInvalid,
			_ => fallback,
		}
	}

	fn is_expected_counterparty(&self, counterparty_node_id: &PublicKey, event: String) -> bool {
		if self.lsp_nodes.read().expect("lock").iter().any(|n| n.node_id == *counterparty_node_id) {
			true
		} else {
			log_error!(self.logger, "Received unexpected {} event!", event);
			false
		}
	}

	fn deliver_response<T>(
		&self, pending: &Mutex<HashMap<LSPSRequestId, oneshot::Sender<T>>>,
		request_id: &LSPSRequestId, response: T,
	) {
		match pending.lock().expect("lock").remove(request_id) {
			Some(sender) => {
				if sender.send(response).is_err() {
					log_error!(
						self.logger,
						"Failed to handle response for request {:?} from liquidity service",
						request_id
					);
				}
			},
			None => {
				debug_assert!(
					false,
					"Received response from liquidity service for unknown request."
				);
				log_error!(
					self.logger,
					"Received response from liquidity service for unknown request."
				);
			},
		}
	}

	async fn get_lsps5_node(
		&self, override_node_id: Option<&PublicKey>,
	) -> Result<LspConfig, Error> {
		if let Some(node) = select_lsps_for_protocol(&self.lsp_nodes, 5, override_node_id) {
			return Ok(node);
		}

		let has_undiscovered_protocol =
			self.lsp_nodes.read().expect("lock").iter().any(|n| n.supported_protocols.is_none());

		// LSP protocol discovery may still be in flight, we wait briefly for it to finish, then re-check.
		if has_undiscovered_protocol && !*self.discovery_done_rx.borrow() {
			log_debug!(
				self.logger,
				"No LSPS5 node available yet, waiting for protocol discovery to complete."
			);
			let mut rx = self.discovery_done_rx.clone();
			let _ = tokio::time::timeout(
				Duration::from_secs(LSPS_DISCOVERY_WAIT_TIMEOUT_SECS),
				rx.wait_for(|done| *done),
			)
			.await;
		}

		select_lsps_for_protocol(&self.lsp_nodes, 5, override_node_id)
			.ok_or(Error::LiquiditySourceUnavailable)
	}
}

/// The response to a [`LSPS5Liquidity::set_webhook`] request.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LSPS5SetWebhookResponse {
	/// The current number of webhooks registered for this client.
	pub num_webhooks: u32,
	/// The maximum number of webhooks allowed by the LSP.
	pub max_webhooks: u32,
	/// Whether this was an unchanged registration (same `app_name` and URL).
	pub no_change: bool,
}

impl From<SetWebhookResponse> for LSPS5SetWebhookResponse {
	fn from(response: SetWebhookResponse) -> Self {
		Self {
			num_webhooks: response.num_webhooks,
			max_webhooks: response.max_webhooks,
			no_change: response.no_change,
		}
	}
}

/// The response to a [`LSPS5Liquidity::list_webhooks`] request.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LSPS5ListWebhooksResponse {
	/// The app names with a currently registered webhook.
	pub app_names: Vec<String>,
	/// The maximum number of webhooks allowed by the LSP.
	pub max_webhooks: u32,
}

impl From<ListWebhooksResponse> for LSPS5ListWebhooksResponse {
	fn from(response: ListWebhooksResponse) -> Self {
		Self {
			app_names: response
				.app_names
				.into_iter()
				.map(|name| name.as_str().to_string())
				.collect(),
			max_webhooks: response.max_webhooks,
		}
	}
}

/// A liquidity handler for managing LSPS5 webhook notifications.
///
/// Should be retrieved by calling [`Liquidity::lsps5`].
///
/// On the client side, this handler allows registering webhook endpoints with an LSP to receive
/// push notifications for Lightning events while offline. On the service side, it allows notifying
/// clients about such events.
///
/// [bLIP-55 / LSPS5]: https://github.com/lightning/blips/blob/master/blip-0055.md
/// [`Liquidity::lsps5`]: crate::Liquidity::lsps5
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct LSPS5Liquidity {
	runtime: Arc<Runtime>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Arc<LSPS5Client<Arc<Logger>>>,
	lsps5_service: Arc<LSPS5ServiceLiquiditySource<Arc<Logger>>>,
	logger: Arc<Logger>,
}

impl LSPS5Liquidity {
	pub(crate) fn new(
		runtime: Arc<Runtime>, connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Arc<LSPS5Client<Arc<Logger>>>,
		lsps5_service: Arc<LSPS5ServiceLiquiditySource<Arc<Logger>>>, logger: Arc<Logger>,
	) -> Self {
		Self { runtime, connection_manager, liquidity_source, lsps5_service, logger }
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl LSPS5Liquidity {
	/// Registers a webhook URL with a single LSP for receiving LSPS5 notifications.
	///
	/// The webhook will receive signed push notifications for Lightning events such as incoming
	/// payments while the client is offline.
	///
	/// Webhooks are stored per LSP, so this has to be called once per LSP that should be able to
	/// reach the client. Give each LSP a distinct URL: bLIP-55 expects the notification delivery
	/// service to verify the `x-lsps5-signature` header against the node ID of the LSP that URL
	/// belongs to.
	///
	/// Note that LSPs typically reject registrations from clients they have no prior relationship
	/// with. LDK Node's own LSPS5 service requires an open channel, or an in-flight LSPS1 or LSPS2
	/// flow, and otherwise fails with [`Error::LiquidityWebhookNoPriorActivity`].
	pub fn set_webhook(
		&self, app_name: String, webhook_url: String, node_id: PublicKey,
	) -> Result<LSPS5SetWebhookResponse, Error> {
		let lsps5_node = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps5_node(Some(&node_id)).await })?;

		self.connect(&lsps5_node)?;

		let liquidity_source = Arc::clone(&self.liquidity_source);
		self.runtime.block_on(async move {
			liquidity_source
				.lsps5_set_webhook(app_name, webhook_url, Some(&lsps5_node.node_id))
				.await
		})
	}

	/// Lists all webhooks currently registered with the given LSP.
	///
	/// Webhooks are stored per LSP, so this returns only the `app_name`s registered with `node_id`.
	pub fn list_webhooks(&self, node_id: PublicKey) -> Result<LSPS5ListWebhooksResponse, Error> {
		let lsps5_node = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps5_node(Some(&node_id)).await })?;

		self.connect(&lsps5_node)?;

		let liquidity_source = Arc::clone(&self.liquidity_source);
		self.runtime.block_on(async move {
			liquidity_source.lsps5_list_webhooks(Some(&lsps5_node.node_id)).await
		})
	}

	/// Removes a previously-configured webhook from a single LSP.
	///
	/// Returns an error if `node_id` is not a configured LSP supporting LSPS5, or if the LSP rejected
	/// the removal.
	pub fn remove_webhook(&self, app_name: String, node_id: PublicKey) -> Result<(), Error> {
		let lsps5_node = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps5_node(Some(&node_id)).await })?;

		self.connect(&lsps5_node)?;

		let liquidity_source = Arc::clone(&self.liquidity_source);
		self.runtime.block_on(async move {
			liquidity_source.lsps5_remove_webhook(app_name, Some(&lsps5_node.node_id)).await
		})
	}

	/// Notifies a client that we intend to manage the liquidity on their channels.
	///
	/// Should be called by LSP operators when their own policy decides to reclaim or adjust
	/// liquidity for `client_node_id` (e.g. before closing or splicing a channel), so the client
	/// has a chance to come online and cooperate. Sends a notification to every webhook the
	/// client has registered with us.
	///
	/// Note that notifications are rate limited per client, so calling this repeatedly in quick
	/// succession will fail after the first call.
	pub fn notify_liquidity_management_request(
		&self, client_node_id: PublicKey,
	) -> Result<(), Error> {
		self.lsps5_service.notify_liquidity_management_request(client_node_id)
	}
}

impl LSPS5Liquidity {
	fn connect(&self, lsps5_node: &LspConfig) -> Result<(), Error> {
		let con_node_id = lsps5_node.node_id;
		let con_addr = lsps5_node.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", lsps5_node.node_id, lsps5_node.address);
		Ok(())
	}
}
