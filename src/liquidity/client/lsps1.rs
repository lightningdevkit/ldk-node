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
use lightning_liquidity::lsps1::event::LSPS1ClientEvent;
use lightning_liquidity::lsps1::msgs::{
	LSPS1ChannelInfo, LSPS1Options, LSPS1OrderId, LSPS1OrderParams,
};
use tokio::sync::oneshot;

use crate::connection::ConnectionManager;
use crate::liquidity::{
	select_lsps_for_protocol, LspConfig, LspNode, LIQUIDITY_REQUEST_TIMEOUT_SECS,
	LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{LiquidityManager, Wallet};
use crate::Error;

pub(crate) struct LSPS1LiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_lsps1_opening_params_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS1OpeningParamsResponse>>>,
	pub(crate) pending_lsps1_create_order_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS1OrderStatus>>>,
	pub(crate) pending_lsps1_check_order_status_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS1OrderStatus>>>,
	pub(crate) lsps1_order_lsp_map: Mutex<HashMap<LSPS1OrderId, PublicKey>>,
	pub(crate) discovery_done_rx: tokio::sync::watch::Receiver<bool>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) logger: L,
}

impl<L: Deref> LSPS1LiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) async fn lsps1_request_opening_params(
		&self, node_id: &PublicKey,
	) -> Result<LSPS1OpeningParamsResponse, Error> {
		let lsps1_node = select_lsps_for_protocol(&self.lsp_nodes, 1, Some(node_id))
			.ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (request_sender, request_receiver) = oneshot::channel();
		{
			let mut pending_opening_params_requests_lock =
				self.pending_lsps1_opening_params_requests.lock().expect("lock");
			let request_id = client_handler.request_supported_options(lsps1_node.node_id);
			pending_opening_params_requests_lock.insert(request_id, request_sender);
		}

		tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), request_receiver)
			.await
			.map_err(|e| {
				log_error!(self.logger, "Liquidity request timed out: {}", e);
				Error::LiquidityRequestFailed
			})?
			.map_err(|e| {
				log_error!(self.logger, "Failed to handle response from liquidity service: {}", e);
				Error::LiquidityRequestFailed
			})
	}

	pub(crate) async fn lsps1_request_channel(
		&self, lsp_balance_sat: u64, client_balance_sat: u64, channel_expiry_blocks: u32,
		announce_channel: bool, refund_address: bitcoin::Address, node_id: &PublicKey,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsps1_node = select_lsps_for_protocol(&self.lsp_nodes, 1, Some(node_id))
			.ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let lsp_limits = self.lsps1_request_opening_params(node_id).await?.supported_options;
		let channel_size_sat = lsp_balance_sat + client_balance_sat;

		if channel_size_sat < lsp_limits.min_channel_balance_sat
			|| channel_size_sat > lsp_limits.max_channel_balance_sat
		{
			log_error!(
				self.logger,
				"Requested channel size of {}sat doesn't meet the LSP-provided limits (min: {}sat, max: {}sat).",
				channel_size_sat,
				lsp_limits.min_channel_balance_sat,
				lsp_limits.max_channel_balance_sat
			);
			return Err(Error::LiquidityRequestFailed);
		}

		if lsp_balance_sat < lsp_limits.min_initial_lsp_balance_sat
			|| lsp_balance_sat > lsp_limits.max_initial_lsp_balance_sat
		{
			log_error!(
				self.logger,
				"Requested LSP-side balance of {}sat doesn't meet the LSP-provided limits (min: {}sat, max: {}sat).",
				lsp_balance_sat,
				lsp_limits.min_initial_lsp_balance_sat,
				lsp_limits.max_initial_lsp_balance_sat
			);
			return Err(Error::LiquidityRequestFailed);
		}

		if client_balance_sat < lsp_limits.min_initial_client_balance_sat
			|| client_balance_sat > lsp_limits.max_initial_client_balance_sat
		{
			log_error!(
				self.logger,
				"Requested client-side balance of {}sat doesn't meet the LSP-provided limits (min: {}sat, max: {}sat).",
				client_balance_sat,
				lsp_limits.min_initial_client_balance_sat,
				lsp_limits.max_initial_client_balance_sat
			);
			return Err(Error::LiquidityRequestFailed);
		}

		let order_params = LSPS1OrderParams {
			lsp_balance_sat,
			client_balance_sat,
			required_channel_confirmations: lsp_limits.min_required_channel_confirmations,
			funding_confirms_within_blocks: lsp_limits.min_funding_confirms_within_blocks,
			channel_expiry_blocks,
			token: lsps1_node.token.clone(),
			announce_channel,
		};

		let (request_sender, request_receiver) = oneshot::channel();
		let request_id;
		{
			let mut pending_create_order_requests_lock =
				self.pending_lsps1_create_order_requests.lock().expect("lock");
			request_id = client_handler.create_order(
				&lsps1_node.node_id,
				order_params.clone(),
				Some(refund_address),
			);
			pending_create_order_requests_lock.insert(request_id.clone(), request_sender);
		}

		let response = tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			request_receiver,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Liquidity request with ID {:?} timed out: {}", request_id, e);
			Error::LiquidityRequestFailed
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to handle response from liquidity service: {}", e);
			Error::LiquidityRequestFailed
		})?;

		if response.order_params != order_params {
			log_error!(
				self.logger,
				"Aborting LSPS1 request as LSP-provided parameters don't match our order. Expected: {:?}, Received: {:?}", order_params, response.order_params
			);
			return Err(Error::LiquidityRequestFailed);
		}

		self.lsps1_order_lsp_map
			.lock()
			.expect("lock")
			.insert(response.order_id.clone(), lsps1_node.node_id);

		Ok(response)
	}

	pub(crate) async fn lsps1_check_order_status(
		&self, order_id: LSPS1OrderId,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsp_node_id = {
			let lock = self.lsps1_order_lsp_map.lock().expect("lock");
			*lock.get(&order_id).ok_or_else(|| {
				log_error!(self.logger, "No LSP node ID found for LSPS1 order ID {:?}.", order_id);
				Error::LiquiditySourceUnavailable
			})?
		};

		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (request_sender, request_receiver) = oneshot::channel();
		{
			let mut pending_check_order_status_requests_lock =
				self.pending_lsps1_check_order_status_requests.lock().expect("lock");
			let request_id = client_handler.check_order_status(&lsp_node_id, order_id);
			pending_check_order_status_requests_lock.insert(request_id, request_sender);
		}

		let response = tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			request_receiver,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Liquidity request timed out: {}", e);
			Error::LiquidityRequestFailed
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to handle response from liquidity service: {}", e);
			Error::LiquidityRequestFailed
		})?;

		Ok(response)
	}

	pub(crate) async fn handle_next_event(&self, event: LSPS1ClientEvent) {
		match event {
			LSPS1ClientEvent::SupportedOptionsReady {
				request_id,
				counterparty_node_id,
				supported_options,
			} => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if let Some(sender) = self
						.pending_lsps1_opening_params_requests
						.lock()
						.expect("lock")
						.remove(&request_id)
					{
						let response = LSPS1OpeningParamsResponse { supported_options };

						match sender.send(response) {
							Ok(()) => (),
							Err(_) => {
								log_error!(
									self.logger,
									"Failed to handle response for request {:?} from liquidity service",
									request_id
								);
							},
						}
					} else {
						debug_assert!(
							false,
							"Received response from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received response from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(
						self.logger,
						"Received unexpected LSPS1Client::SupportedOptionsReady event!"
					);
				}
			},
			LSPS1ClientEvent::OrderCreated {
				request_id,
				counterparty_node_id,
				order_id,
				order,
				payment,
				channel,
			} => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if let Some(sender) = self
						.pending_lsps1_create_order_requests
						.lock()
						.expect("lock")
						.remove(&request_id)
					{
						let response = LSPS1OrderStatus {
							order_id,
							order_params: order,
							payment_options: payment.into(),
							channel_state: channel,
						};

						match sender.send(response) {
							Ok(()) => (),
							Err(_) => {
								log_error!(
									self.logger,
									"Failed to handle response for request {:?} from liquidity service",
									request_id
								);
							},
						}
					} else {
						debug_assert!(
							false,
							"Received response from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received response from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(self.logger, "Received unexpected LSPS1Client::OrderCreated event!");
				}
			},
			LSPS1ClientEvent::OrderStatus {
				request_id,
				counterparty_node_id,
				order_id,
				order,
				payment,
				channel,
			} => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if let Some(sender) = self
						.pending_lsps1_check_order_status_requests
						.lock()
						.expect("lock")
						.remove(&request_id)
					{
						let response = LSPS1OrderStatus {
							order_id,
							order_params: order,
							payment_options: payment.into(),
							channel_state: channel,
						};

						match sender.send(response) {
							Ok(()) => (),
							Err(_) => {
								log_error!(
									self.logger,
									"Failed to handle response for request {:?} from liquidity service",
									request_id
								);
							},
						}
					} else {
						debug_assert!(
							false,
							"Received response from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received response from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(self.logger, "Received unexpected LSPS1Client::OrderStatus event!");
				}
			},
			_ => {
				log_error!(self.logger, "Received unexpected LSPS1Client liquidity event!");
			},
		}
	}

	async fn get_lsps1_node(
		&self, override_node_id: Option<&PublicKey>,
	) -> Result<LspConfig, Error> {
		if let Some(node) = select_lsps_for_protocol(&self.lsp_nodes, 1, override_node_id) {
			return Ok(node);
		}

		// LSP protocol discovery may still be in flight, we wait briefly for it to finish, then re-check.
		if !*self.discovery_done_rx.borrow() {
			log_debug!(
				self.logger,
				"No LSPS1 node available yet, waiting for protocol discovery to complete."
			);
			let mut rx = self.discovery_done_rx.clone();
			let _ = tokio::time::timeout(
				Duration::from_secs(LSPS_DISCOVERY_WAIT_TIMEOUT_SECS),
				rx.wait_for(|done| *done),
			)
			.await;
		}

		select_lsps_for_protocol(&self.lsp_nodes, 1, override_node_id)
			.ok_or(Error::LiquiditySourceUnavailable)
	}
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS1OpeningParamsResponse {
	supported_options: LSPS1Options,
}

/// Represents the status of an LSPS1 channel request.
#[derive(Debug, Clone)]
pub struct LSPS1OrderStatus {
	/// The id of the channel order.
	pub order_id: LSPS1OrderId,
	/// The parameters of channel order.
	pub order_params: LSPS1OrderParams,
	/// Contains details about how to pay for the order.
	pub payment_options: LSPS1PaymentInfo,
	/// Contains information about the channel state.
	pub channel_state: Option<LSPS1ChannelInfo>,
}

#[cfg(not(feature = "uniffi"))]
type LSPS1PaymentInfo = lightning_liquidity::lsps1::msgs::LSPS1PaymentInfo;

#[cfg(feature = "uniffi")]
type LSPS1PaymentInfo = crate::ffi::LSPS1PaymentInfo;

/// A liquidity handler allowing to request channels via the [bLIP-51 / LSPS1] protocol.
///
/// Should be retrieved by calling [`Node::liquidity`].
///
/// To open [bLIP-52 / LSPS2] JIT channels, please refer to
/// [`Bolt11Payment::receive_via_jit_channel`].
///
/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
/// [`Node::liquidity`]: crate::Node::liquidity
/// [`Bolt11Payment::receive_via_jit_channel`]: crate::payment::Bolt11Payment::receive_via_jit_channel
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct LSPS1Liquidity {
	runtime: Arc<Runtime>,
	wallet: Arc<Wallet>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Option<Arc<LSPS1LiquiditySource<Arc<Logger>>>>,
	logger: Arc<Logger>,
}

impl LSPS1Liquidity {
	pub(crate) fn new(
		runtime: Arc<Runtime>, wallet: Arc<Wallet>,
		connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Option<Arc<LSPS1LiquiditySource<Arc<Logger>>>>, logger: Arc<Logger>,
	) -> Self {
		Self { runtime, wallet, connection_manager, liquidity_source, logger }
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl LSPS1Liquidity {
	/// Connects to the configured LSP and places an order for an inbound channel.
	///
	/// The channel will be opened after one of the returned payment options has successfully been
	/// paid.
	pub fn request_channel(
		&self, lsp_balance_sat: u64, client_balance_sat: u64, channel_expiry_blocks: u32,
		announce_channel: bool,
	) -> Result<LSPS1OrderStatus, Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let lsps1_node =
			self.runtime.block_on(async { liquidity_source.get_lsps1_node(None).await })?;

		let con_node_id = lsps1_node.node_id;
		let con_addr = lsps1_node.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", lsps1_node.node_id, lsps1_node.address);

		let refund_address = self.wallet.get_new_address()?;

		let liquidity_source = Arc::clone(&liquidity_source);
		let response = self.runtime.block_on(async move {
			liquidity_source
				.lsps1_request_channel(
					lsp_balance_sat,
					client_balance_sat,
					channel_expiry_blocks,
					announce_channel,
					refund_address,
					&con_node_id,
				)
				.await
		})?;

		Ok(response)
	}

	/// Connects to the specified configured LSP and places an order for an inbound channel.
	///
	/// The channel will be opened after one of the returned payment options has successfully been
	/// paid.
	pub fn request_channel_from_lsp(
		&self, lsp_balance_sat: u64, client_balance_sat: u64, channel_expiry_blocks: u32,
		announce_channel: bool, node_id: PublicKey,
	) -> Result<LSPS1OrderStatus, Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let lsps1_node = self
			.runtime
			.block_on(async { liquidity_source.get_lsps1_node(Some(&node_id)).await })?;

		let con_node_id = lsps1_node.node_id;
		let con_addr = lsps1_node.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", lsps1_node.node_id, lsps1_node.address);

		let refund_address = self.wallet.get_new_address()?;

		let liquidity_source = Arc::clone(&liquidity_source);
		let response = self.runtime.block_on(async move {
			liquidity_source
				.lsps1_request_channel(
					lsp_balance_sat,
					client_balance_sat,
					channel_expiry_blocks,
					announce_channel,
					refund_address,
					&con_node_id,
				)
				.await
		})?;

		Ok(response)
	}

	/// Connects to the configured LSP and checks for the status of a previously-placed order.
	pub fn check_order_status(&self, order_id: LSPS1OrderId) -> Result<LSPS1OrderStatus, Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let lsp_node_id = {
			let lock = liquidity_source.lsps1_order_lsp_map.lock().expect("lock");
			*lock.get(&order_id).ok_or_else(|| {
				log_error!(self.logger, "No LSP node ID found for LSPS1 order ID {:?}.", order_id);
				Error::LiquiditySourceUnavailable
			})?
		};

		let lsps1_node =
			select_lsps_for_protocol(&liquidity_source.lsp_nodes, 1, Some(&lsp_node_id))
				.ok_or(Error::LiquiditySourceUnavailable)?;

		let con_node_id = lsps1_node.node_id;
		let con_addr = lsps1_node.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		let liquidity_source = Arc::clone(&liquidity_source);
		let response = self
			.runtime
			.block_on(async move { liquidity_source.lsps1_check_order_status(order_id).await })?;
		Ok(response)
	}
}
