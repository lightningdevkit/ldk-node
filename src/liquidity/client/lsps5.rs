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
	LSPS5Error, ListWebhooksResponse, RemoveWebhookResponse, SetWebhookResponse,
};
use tokio::sync::oneshot;

use crate::connection::ConnectionManager;
use crate::liquidity::{
	select_all_lsps_for_protocol, select_lsps_for_protocol, LspConfig, LspNode,
	LIQUIDITY_REQUEST_TIMEOUT_SECS, LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
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
					return Err(Error::LiquiditySetWebhookFailed);
				},
			};

			pending_set_webhook_requests_lock.insert(request_id, sender);
		}

		match tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), receiver)
			.await
		{
			Ok(Ok(result)) => result.map(LSPS5SetWebhookResponse::from).map_err(|e| {
				log_error!(self.logger, "Failed to set webhook: {:?}", e);
				Error::LiquiditySetWebhookFailed
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
				Error::LiquidityListWebhooksFailed
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
						return Err(Error::LiquidityRemoveWebhookFailed);
					},
				};

			pending_remove_webhook_requests_lock.insert(request_id.clone(), sender);
		}

		match tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), receiver)
			.await
		{
			Ok(Ok(result)) => result.map(|_| ()).map_err(|e| {
				log_error!(self.logger, "Failed to remove webhook: {:?}", e);
				Error::LiquidityRemoveWebhookFailed
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

	/// Resolves the set of LSPs a webhook operation should target.
	///
	/// If `override_node_id` is `Some`, only that LSP is targeted (returned as a single-element
	/// list). If it is `None`, every LSP that supports LSPS5 is targeted, waiting briefly for
	/// protocol discovery to complete if it is still in flight.
	async fn get_lsps5_nodes(
		&self, override_node_id: Option<&PublicKey>,
	) -> Result<Vec<LspConfig>, Error> {
		if override_node_id.is_some() {
			return self.get_lsps5_node(override_node_id).await.map(|node| vec![node]);
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

		let lsps5_nodes = select_all_lsps_for_protocol(&self.lsp_nodes, 5);
		if lsps5_nodes.is_empty() {
			return Err(Error::LiquiditySourceUnavailable);
		}
		Ok(lsps5_nodes)
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

/// The result of registering a webhook with a single LSP via [`LSPS5Liquidity::set_webhook`].
///
/// A call to [`LSPS5Liquidity::set_webhook`] may target multiple LSPs (when no specific `node_id`
/// is given), so one of these is returned per LSP that accepted the registration.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LSPS5SetWebhookResult {
	/// The node ID of the LSP the webhook was registered with.
	pub counterparty_node_id: PublicKey,
	/// The LSP's response to the registration.
	pub response: LSPS5SetWebhookResponse,
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
/// Should be retrieved by calling [`Node::lsps5_liquidity`].
///
/// On the client side, this handler allows registering webhook endpoints with an LSP to receive
/// push notifications for Lightning events while offline. On the service side, it allows notifying
/// clients about such events.
///
/// [bLIP-55 / LSPS5]: https://github.com/lightning/blips/blob/master/blip-0055.md
/// [`Node::lsps5_liquidity`]: crate::Node::lsps5_liquidity
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct LSPS5Liquidity {
	runtime: Arc<Runtime>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Arc<LSPS5Client<Arc<Logger>>>,
	logger: Arc<Logger>,
}

impl LSPS5Liquidity {
	pub(crate) fn new(
		runtime: Arc<Runtime>, connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Arc<LSPS5Client<Arc<Logger>>>, logger: Arc<Logger>,
	) -> Self {
		Self { runtime, connection_manager, liquidity_source, logger }
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl LSPS5Liquidity {
	/// Connects to one or more LSPs and registers a webhook URL for receiving LSPS5 notifications.
	///
	/// The webhook will receive signed push notifications for Lightning events such as incoming
	/// payments while the client is offline. Because each LSP stores and calls the webhook
	/// independently, the webhook has to be registered with every LSP that should be able to reach
	/// the client.
	///
	/// If `node_id` is `Some`, the webhook is registered with that single LSP only. If `node_id`
	/// is `None`, the webhook is registered with *every* LSP that supports LSPS5 (as added via
	/// [`crate::Builder::add_liquidity_source`] or [`crate::Liquidity::add_liquidity_source`]),
	/// allowing the webhook to be configured once for all of them.
	///
	/// Returns one [`LSPS5SetWebhookResult`] per LSP that accepted the registration. When targeting
	/// multiple LSPs, registration is best-effort: failures for individual LSPs are logged and
	/// skipped rather than aborting the whole call. An error is only returned if no LSP supporting
	/// LSPS5 is available, or if the registration failed for every targeted LSP.
	pub fn set_webhook(
		&self, app_name: String, webhook_url: String, node_id: Option<PublicKey>,
	) -> Result<Vec<LSPS5SetWebhookResult>, Error> {
		let lsps5_nodes = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps5_nodes(node_id.as_ref()).await })?;

		let mut results = Vec::with_capacity(lsps5_nodes.len());
		let mut last_error = None;
		for lsps5_node in lsps5_nodes {
			let counterparty_node_id = lsps5_node.node_id;

			if let Err(e) = self.connect(&lsps5_node) {
				log_error!(
					self.logger,
					"Failed to connect to LSPS5 node {} to set webhook: {:?}",
					counterparty_node_id,
					e
				);
				last_error = Some(e);
				continue;
			}

			let liquidity_source = Arc::clone(&self.liquidity_source);
			let app_name = app_name.clone();
			let webhook_url = webhook_url.clone();
			let result = self.runtime.block_on(async move {
				liquidity_source
					.lsps5_set_webhook(app_name, webhook_url, Some(&counterparty_node_id))
					.await
			});

			match result {
				Ok(response) => {
					results.push(LSPS5SetWebhookResult { counterparty_node_id, response })
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to set webhook at LSPS5 node {}: {:?}",
						counterparty_node_id,
						e
					);
					last_error = Some(e);
				},
			}
		}

		if results.is_empty() {
			return Err(last_error.unwrap_or(Error::LiquiditySetWebhookFailed));
		}

		Ok(results)
	}

	/// Connects to the configured LSP and lists all currently configured webhooks.
	///
	/// If `node_id` is `None` and multiple LSPs support LSPS5, the first one registered
	/// via [`crate::Builder::add_liquidity_source`] or [`crate::Liquidity::add_liquidity_source`] is used.
	pub fn list_webhooks(
		&self, node_id: Option<PublicKey>,
	) -> Result<LSPS5ListWebhooksResponse, Error> {
		let lsps5_node = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps5_node(node_id.as_ref()).await })?;

		self.connect(&lsps5_node)?;

		let liquidity_source = Arc::clone(&self.liquidity_source);
		self.runtime.block_on(async move {
			liquidity_source.lsps5_list_webhooks(Some(&lsps5_node.node_id)).await
		})
	}

	/// Connects to one or more LSPs and removes a previously-configured webhook.
	///
	/// Because each LSP stores the webhook independently, removal has to be performed against every
	/// LSP the webhook was registered with.
	///
	/// If `node_id` is `Some`, the webhook is removed from that single LSP only. If `node_id` is
	/// `None`, the webhook is removed from *every* LSP that supports LSPS5 (as added via
	/// [`crate::Builder::add_liquidity_source`] or [`crate::Liquidity::add_liquidity_source`]).
	///
	/// Returns the node IDs of the LSPs the webhook was successfully removed from. When targeting
	/// multiple LSPs, removal is best-effort: failures for individual LSPs (including attempts to
	/// remove a webhook that does not exist at that LSP) are logged and skipped. An error is only
	/// returned if no LSP supporting LSPS5 is available, or if removal failed for every targeted LSP.
	pub fn remove_webhook(
		&self, app_name: String, node_id: Option<PublicKey>,
	) -> Result<Vec<PublicKey>, Error> {
		let lsps5_nodes = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps5_nodes(node_id.as_ref()).await })?;

		let mut removed = Vec::with_capacity(lsps5_nodes.len());
		let mut last_error = None;
		for lsps5_node in lsps5_nodes {
			let counterparty_node_id = lsps5_node.node_id;

			if let Err(e) = self.connect(&lsps5_node) {
				log_error!(
					self.logger,
					"Failed to connect to LSPS5 node {} to remove webhook: {:?}",
					counterparty_node_id,
					e
				);
				last_error = Some(e);
				continue;
			}

			let liquidity_source = Arc::clone(&self.liquidity_source);
			let app_name = app_name.clone();
			let result = self.runtime.block_on(async move {
				liquidity_source.lsps5_remove_webhook(app_name, Some(&counterparty_node_id)).await
			});

			match result {
				Ok(()) => removed.push(counterparty_node_id),
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to remove webhook at LSPS5 node {}: {:?}",
						counterparty_node_id,
						e
					);
					last_error = Some(e);
				},
			}
		}

		if removed.is_empty() {
			return Err(last_error.unwrap_or(Error::LiquidityRemoveWebhookFailed));
		}

		Ok(removed)
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
