// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::msgs::SocketAddress;
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps1::client::LSPS1ClientConfig as LdkLSPS1ClientConfig;
use lightning_liquidity::lsps1::msgs::{
	LSPS1ChannelInfo, LSPS1Options, LSPS1OrderId, LSPS1OrderParams,
};
use tokio::sync::oneshot;

use crate::connection::ConnectionManager;
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::Wallet;
use crate::Error;

use super::super::{LiquiditySource, LIQUIDITY_REQUEST_TIMEOUT_SECS};

pub(crate) struct LSPS1Client {
	pub(crate) lsp_node_id: PublicKey,
	pub(crate) lsp_address: SocketAddress,
	pub(crate) token: Option<String>,
	pub(crate) ldk_client_config: LdkLSPS1ClientConfig,
	pub(crate) pending_opening_params_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS1OpeningParamsResponse>>>,
	pub(crate) pending_create_order_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS1OrderStatus>>>,
	pub(crate) pending_check_order_status_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS1OrderStatus>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS1ClientConfig {
	pub node_id: PublicKey,
	pub address: SocketAddress,
	pub token: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS1OpeningParamsResponse {
	pub(crate) supported_options: LSPS1Options,
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

impl<L: Deref> LiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn get_lsps1_lsp_details(&self) -> Option<(PublicKey, SocketAddress)> {
		self.lsps1_client.as_ref().map(|s| (s.lsp_node_id, s.lsp_address.clone()))
	}

	pub(crate) async fn lsps1_request_opening_params(
		&self,
	) -> Result<LSPS1OpeningParamsResponse, Error> {
		let lsps1_client = self.lsps1_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (request_sender, request_receiver) = oneshot::channel();
		{
			let mut pending_opening_params_requests_lock =
				lsps1_client.pending_opening_params_requests.lock().expect("lock");
			let request_id = client_handler.request_supported_options(lsps1_client.lsp_node_id);
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
		announce_channel: bool, refund_address: bitcoin::Address,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsps1_client = self.lsps1_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;
		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let lsp_limits = self.lsps1_request_opening_params().await?.supported_options;
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
			token: lsps1_client.token.clone(),
			announce_channel,
		};

		let (request_sender, request_receiver) = oneshot::channel();
		let request_id;
		{
			let mut pending_create_order_requests_lock =
				lsps1_client.pending_create_order_requests.lock().expect("lock");
			request_id = client_handler.create_order(
				&lsps1_client.lsp_node_id,
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

		Ok(response)
	}

	pub(crate) async fn lsps1_check_order_status(
		&self, order_id: LSPS1OrderId,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsps1_client = self.lsps1_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;
		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (request_sender, request_receiver) = oneshot::channel();
		{
			let mut pending_check_order_status_requests_lock =
				lsps1_client.pending_check_order_status_requests.lock().expect("lock");
			let request_id = client_handler.check_order_status(&lsps1_client.lsp_node_id, order_id);
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
}

/// A liquidity handler allowing to request channels via the [bLIP-51 / LSPS1] protocol.
///
/// Should be retrieved by calling [`Node::lsps1_liquidity`].
///
/// To open [bLIP-52 / LSPS2] JIT channels, please refer to
/// [`Bolt11Payment::receive_via_jit_channel`].
///
/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
/// [`Node::lsps1_liquidity`]: crate::Node::lsps1_liquidity
/// [`Bolt11Payment::receive_via_jit_channel`]: crate::payment::Bolt11Payment::receive_via_jit_channel
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct LSPS1Liquidity {
	runtime: Arc<Runtime>,
	wallet: Arc<Wallet>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>,
	logger: Arc<Logger>,
}

impl LSPS1Liquidity {
	pub(crate) fn new(
		runtime: Arc<Runtime>, wallet: Arc<Wallet>,
		connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>, logger: Arc<Logger>,
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

		let (lsp_node_id, lsp_address) =
			liquidity_source.get_lsps1_lsp_details().ok_or(Error::LiquiditySourceUnavailable)?;

		let con_node_id = lsp_node_id;
		let con_addr = lsp_address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", lsp_node_id, lsp_address);

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
				)
				.await
		})?;

		Ok(response)
	}

	/// Connects to the configured LSP and checks for the status of a previously-placed order.
	pub fn check_order_status(&self, order_id: LSPS1OrderId) -> Result<LSPS1OrderStatus, Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let (lsp_node_id, lsp_address) =
			liquidity_source.get_lsps1_lsp_details().ok_or(Error::LiquiditySourceUnavailable)?;

		let con_node_id = lsp_node_id;
		let con_addr = lsp_address.clone();
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
