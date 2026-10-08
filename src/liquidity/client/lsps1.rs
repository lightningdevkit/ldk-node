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
use lightning::offers::offer::Amount;
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps1::event::LSPS1ClientEvent;
use lightning_liquidity::lsps1::msgs::{
	LSPS1ChannelInfo, LSPS1Options, LSPS1OrderId, LSPS1OrderParams,
	LSPS1PaymentInfo as LdkLSPS1PaymentInfo,
};
use tokio::sync::oneshot;

use crate::connection::ConnectionManager;
use crate::liquidity::{
	select_lsps_for_protocol, LspConfig, LspNode, PendingRequest, PendingRequestGuard,
	LIQUIDITY_REQUEST_TIMEOUT_SECS, LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{LiquidityManager, Wallet};
use crate::Error;

/// Checks that the LSP-provided payment options are consistent with the order.
///
/// For each offered option, the advertised total must equal the fee plus the client balance of
/// the order, and any embedded BOLT11 invoice or BOLT12 offer must ask for exactly that total.
/// This prevents an LSP from advertising a small total while embedding a payment request for a
/// larger amount.
fn payment_options_are_consistent(payment: &LdkLSPS1PaymentInfo, order: &LSPS1OrderParams) -> bool {
	let totals_match = |fee_total_sat: u64, order_total_sat: u64| {
		order.client_balance_sat.checked_add(fee_total_sat) == Some(order_total_sat)
	};
	let total_msat = |order_total_sat: u64| order_total_sat.checked_mul(1_000);

	if let Some(bolt11) = payment.bolt11.as_ref() {
		if !totals_match(bolt11.fee_total_sat, bolt11.order_total_sat)
			|| bolt11.invoice.amount_milli_satoshis() != total_msat(bolt11.order_total_sat)
		{
			return false;
		}
	}

	if let Some(bolt12) = payment.bolt12.as_ref() {
		let offer_amount_msat = match bolt12.offer.amount() {
			Some(Amount::Bitcoin { amount_msats }) => Some(amount_msats),
			_ => None,
		};
		if !totals_match(bolt12.fee_total_sat, bolt12.order_total_sat)
			|| offer_amount_msat != total_msat(bolt12.order_total_sat)
		{
			return false;
		}
	}

	if let Some(onchain) = payment.onchain.as_ref() {
		if !totals_match(onchain.fee_total_sat, onchain.order_total_sat) {
			return false;
		}
	}

	true
}

pub(crate) struct LSPS1Client<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_opening_params_requests:
		Mutex<HashMap<LSPSRequestId, PendingRequest<LSPS1OpeningParamsResponse>>>,
	pub(crate) pending_create_order_requests:
		Mutex<HashMap<LSPSRequestId, PendingRequest<LSPS1OrderStatus>>>,
	pub(crate) pending_check_order_status_requests:
		Mutex<HashMap<LSPSRequestId, PendingRequest<LSPS1OrderStatus>>>,
	pub(crate) discovery_done_rx: tokio::sync::watch::Receiver<bool>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) logger: L,
}

impl<L: Deref> LSPS1Client<L>
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
		let _pending_request = {
			let mut pending_opening_params_requests_lock =
				self.pending_opening_params_requests.lock().expect("lock");
			let request_id = client_handler.request_supported_options(lsps1_node.node_id);
			PendingRequestGuard::insert(
				&self.pending_opening_params_requests,
				&mut pending_opening_params_requests_lock,
				request_id,
				request_sender,
			)
		};

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
		let _pending_request = {
			let mut pending_create_order_requests_lock =
				self.pending_create_order_requests.lock().expect("lock");
			request_id = client_handler.create_order(
				&lsps1_node.node_id,
				order_params.clone(),
				Some(refund_address.clone()),
			);
			PendingRequestGuard::insert(
				&self.pending_create_order_requests,
				&mut pending_create_order_requests_lock,
				request_id.clone(),
				request_sender,
			)
		};

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

		if let Some(received_refund_address) = response
			.payment_options
			.onchain
			.as_ref()
			.and_then(|o| o.refund_onchain_address.as_ref())
			.filter(|addr| addr.script_pubkey() != refund_address.script_pubkey())
		{
			log_error!(
				self.logger,
				"Aborting LSPS1 request as LSP-provided refund address doesn't match our order. Expected: {}, Received: {}", refund_address, received_refund_address
			);
			return Err(Error::LiquidityRequestFailed);
		}

		Ok(response)
	}

	pub(crate) async fn lsps1_check_order_status(
		&self, order_id: LSPS1OrderId, lsp_node_id: PublicKey,
	) -> Result<LSPS1OrderStatus, Error> {
		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (request_sender, request_receiver) = oneshot::channel();
		let _pending_request = {
			let mut pending_check_order_status_requests_lock =
				self.pending_check_order_status_requests.lock().expect("lock");
			let request_id = client_handler.check_order_status(&lsp_node_id, order_id);
			PendingRequestGuard::insert(
				&self.pending_check_order_status_requests,
				&mut pending_check_order_status_requests_lock,
				request_id,
				request_sender,
			)
		};

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

	pub(crate) async fn handle_event(&self, event: LSPS1ClientEvent) {
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
					if let Some(request) = self
						.pending_opening_params_requests
						.lock()
						.expect("lock")
						.remove(&request_id)
					{
						let response = LSPS1OpeningParamsResponse { supported_options };

						match request.sender.send(response) {
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
					if let Some(request) =
						self.pending_create_order_requests.lock().expect("lock").remove(&request_id)
					{
						if !payment_options_are_consistent(&payment, &order) {
							log_error!(
								self.logger,
								"Rejecting LSPS1 order {:?} as the LSP-provided payment options are inconsistent with the order: {:?}",
								order_id,
								payment
							);
							return;
						}

						let response = LSPS1OrderStatus {
							order_id,
							order_params: order,
							payment_options: payment.into(),
							channel_state: channel,
							counterparty_node_id,
						};

						match request.sender.send(response) {
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
					if let Some(request) = self
						.pending_check_order_status_requests
						.lock()
						.expect("lock")
						.remove(&request_id)
					{
						if !payment_options_are_consistent(&payment, &order) {
							log_error!(
								self.logger,
								"Rejecting LSPS1 order {:?} as the LSP-provided payment options are inconsistent with the order: {:?}",
								order_id,
								payment
							);
							return;
						}

						let response = LSPS1OrderStatus {
							order_id,
							order_params: order,
							payment_options: payment.into(),
							channel_state: channel,
							counterparty_node_id,
						};

						match request.sender.send(response) {
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

		let has_undiscovered_protocol =
			self.lsp_nodes.read().expect("lock").iter().any(|n| n.supported_protocols.is_none());

		// LSP protocol discovery may still be in flight, we wait briefly for it to finish, then re-check.
		if has_undiscovered_protocol && !*self.discovery_done_rx.borrow() {
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
	/// The node id of the LSP.
	pub counterparty_node_id: PublicKey,
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
	liquidity_source: Arc<LSPS1Client<Arc<Logger>>>,
	logger: Arc<Logger>,
}

impl LSPS1Liquidity {
	pub(crate) fn new(
		runtime: Arc<Runtime>, wallet: Arc<Wallet>,
		connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Arc<LSPS1Client<Arc<Logger>>>, logger: Arc<Logger>,
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
	///
	/// If `node_id` is `None` and multiple LSPs support LSPS1, the first one registered
	/// via [`crate::Builder::add_liquidity_source`] or [`crate::Liquidity::add_liquidity_source`] is used.
	pub fn request_channel(
		&self, lsp_balance_sat: u64, client_balance_sat: u64, channel_expiry_blocks: u32,
		announce_channel: bool, node_id: Option<PublicKey>,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsps1_node = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps1_node(node_id.as_ref()).await })?;

		let con_node_id = lsps1_node.node_id;
		let con_addr = lsps1_node.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", lsps1_node.node_id, lsps1_node.address);

		let refund_address = self.runtime.block_on(self.wallet.get_new_address())?;

		let liquidity_source = Arc::clone(&self.liquidity_source);
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

	/// Connects to the configured LSP and checks for the status of a previously-placed order with the given node ID.
	pub fn check_order_status(
		&self, order_id: LSPS1OrderId, lsp_node_id: PublicKey,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsps1_node = self
			.runtime
			.block_on(async { self.liquidity_source.get_lsps1_node(Some(&lsp_node_id)).await })?;

		let con_node_id = lsps1_node.node_id;
		let con_addr = lsps1_node.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		let liquidity_source = Arc::clone(&self.liquidity_source);
		let response = self.runtime.block_on(async move {
			liquidity_source.lsps1_check_order_status(order_id, lsp_node_id).await
		})?;
		Ok(response)
	}
}

#[cfg(all(test, not(feature = "uniffi")))]
mod tests {
	use std::str::FromStr;

	use lightning::ln::msgs::SocketAddress;
	use lightning_liquidity::lsps1::msgs::LSPS1PaymentInfo as LdkLSPS1PaymentInfo;
	use tokio::sync::oneshot;

	use super::*;
	use crate::builder::NodeBuilder;
	use crate::entropy::NodeEntropy;
	use crate::io::test_utils::InMemoryStore;
	use crate::Node;

	// A valid invoice for 1,000,000 sat (10 mBTC).
	const INVOICE_1M_SAT: &str = "lnbc10m1pn8g2j4pp575tg4wt8jwgu2lvtk3aj6hy7mc6tnupw07wwkxcvyhtt3wlzw0zsdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdv6dzdeg0ww2eyqqqqryqqqqthqqpysp5fkd3k2rzvwdt2av068p58evf6eg50q0eftfhrpugaxkuyje4d25q9qrsgqqkfmnn67s5g6hadrcvf5h0l7p92rtlkwrfqdvc7uuf6lew0czxksvqhyux3zjrl3tlakwhtvezwl24zshnfumukwh0yntqsng9z6glcquvw7kc";
	const INVOICE_AMOUNT_SAT: u64 = 1_000_000;
	const CLIENT_BALANCE_SAT: u64 = 1;

	fn lsp_node_id() -> PublicKey {
		PublicKey::from_str("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
			.unwrap()
	}

	fn build_node() -> Node {
		let mut builder = NodeBuilder::new();
		builder.set_log_facade_logger();
		builder.add_liquidity_source(
			lsp_node_id(),
			SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port: 9735 },
			None,
			false,
		);
		builder
			.build_with_store(NodeEntropy::from_seed_bytes([42u8; 64]), InMemoryStore::new())
			.unwrap()
	}

	fn order_params() -> LSPS1OrderParams {
		LSPS1OrderParams {
			lsp_balance_sat: 100_000,
			client_balance_sat: CLIENT_BALANCE_SAT,
			required_channel_confirmations: 0,
			funding_confirms_within_blocks: 1,
			channel_expiry_blocks: 144,
			token: None,
			announce_channel: false,
		}
	}

	fn bolt11_payment_info(fee_total_sat: u64, order_total_sat: u64) -> LdkLSPS1PaymentInfo {
		serde_json::from_str(&format!(
			r#"{{
				"bolt11": {{
					"state": "EXPECT_PAYMENT",
					"expires_at": "2035-01-01T00:00:00Z",
					"fee_total_sat": "{fee_total_sat}",
					"order_total_sat": "{order_total_sat}",
					"invoice": "{INVOICE_1M_SAT}"
				}},
				"bolt12": null,
				"onchain": null
			}}"#
		))
		.unwrap()
	}

	fn onchain_payment_info(fee_total_sat: u64, order_total_sat: u64) -> LdkLSPS1PaymentInfo {
		serde_json::from_str(&format!(
			r#"{{
				"bolt11": null,
				"bolt12": null,
				"onchain": {{
					"state": "EXPECT_PAYMENT",
					"expires_at": "2035-01-01T00:00:00Z",
					"fee_total_sat": "{fee_total_sat}",
					"order_total_sat": "{order_total_sat}",
					"address": "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
					"min_onchain_payment_confirmations": 1,
					"min_fee_for_0conf": 253
				}}
			}}"#
		))
		.unwrap()
	}

	// Delivers the given LSP response for a pending create-order request (or a pending
	// check-order-status request if `via_order_status` is set) and returns whether the client
	// forwarded it to the waiting caller.
	fn response_is_forwarded(
		node: &Node, payment: LdkLSPS1PaymentInfo, via_order_status: bool,
	) -> bool {
		let client = node.liquidity_source.lsps1_client();
		let handler = client.liquidity_manager.lsps1_client_handler().unwrap();
		let order_id = LSPS1OrderId("order".to_owned());
		let (sender, receiver) = oneshot::channel();

		let (request_id, _pending_request) = if via_order_status {
			let request_id = handler.check_order_status(&lsp_node_id(), order_id.clone());
			let mut lock = client.pending_check_order_status_requests.lock().unwrap();
			let guard = PendingRequestGuard::insert(
				&client.pending_check_order_status_requests,
				&mut lock,
				request_id.clone(),
				sender,
			);
			(request_id, guard)
		} else {
			let request_id = handler.create_order(&lsp_node_id(), order_params(), None);
			let mut lock = client.pending_create_order_requests.lock().unwrap();
			let guard = PendingRequestGuard::insert(
				&client.pending_create_order_requests,
				&mut lock,
				request_id.clone(),
				sender,
			);
			(request_id, guard)
		};

		let event = if via_order_status {
			LSPS1ClientEvent::OrderStatus {
				request_id,
				counterparty_node_id: lsp_node_id(),
				order_id,
				order: order_params(),
				payment,
				channel: None,
			}
		} else {
			LSPS1ClientEvent::OrderCreated {
				request_id,
				counterparty_node_id: lsp_node_id(),
				order_id,
				order: order_params(),
				payment,
				channel: None,
			}
		};

		let event_client = Arc::clone(&client);
		node.runtime.block_on(async move {
			event_client.handle_event(event).await;
			receiver.await.is_ok()
		})
	}

	#[test]
	fn accepts_consistent_payment_options() {
		let node = build_node();
		let fee_total_sat = INVOICE_AMOUNT_SAT - CLIENT_BALANCE_SAT;
		for via_order_status in [false, true] {
			assert!(response_is_forwarded(
				&node,
				bolt11_payment_info(fee_total_sat, INVOICE_AMOUNT_SAT),
				via_order_status
			));
			assert!(response_is_forwarded(
				&node,
				onchain_payment_info(fee_total_sat, INVOICE_AMOUNT_SAT),
				via_order_status
			));
		}
	}

	#[test]
	fn rejects_invoice_amount_disagreeing_with_order_total() {
		let node = build_node();
		// The LSP advertises a one-satoshi total, but the embedded invoice asks for 1,000,000 sat.
		for via_order_status in [false, true] {
			assert!(!response_is_forwarded(&node, bolt11_payment_info(0, 1), via_order_status));
		}
	}

	#[test]
	fn rejects_fee_disagreeing_with_order_total() {
		let node = build_node();
		let fee_total_sat = INVOICE_AMOUNT_SAT - CLIENT_BALANCE_SAT - 1;
		for via_order_status in [false, true] {
			assert!(!response_is_forwarded(
				&node,
				bolt11_payment_info(fee_total_sat, INVOICE_AMOUNT_SAT),
				via_order_status
			));
			assert!(!response_is_forwarded(
				&node,
				onchain_payment_info(fee_total_sat, INVOICE_AMOUNT_SAT),
				via_order_status
			));
		}
	}
}
