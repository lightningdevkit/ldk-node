// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to liquidity management.

use crate::chain::ChainSource;
use crate::connection::ConnectionManager;
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::types::{ChannelManager, KeysManager, LiquidityManager, PeerManager, Wallet};
use crate::{Config, Error};

use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::ln::msgs::SocketAddress;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, InvoiceBuilder, RoutingFees};
use lightning_liquidity::events::Event;
use lightning_liquidity::lsps0::ser::RequestId;
use lightning_liquidity::lsps1::client::LSPS1ClientConfig as LdkLSPS1ClientConfig;
use lightning_liquidity::lsps1::event::LSPS1ClientEvent;
use lightning_liquidity::lsps1::msgs::{ChannelInfo, LSPS1Options, OrderId, OrderParameters};
use lightning_liquidity::lsps2::client::LSPS2ClientConfig as LdkLSPS2ClientConfig;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;
use lightning_liquidity::LiquidityClientConfig;

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{PublicKey, Secp256k1};

use tokio::sync::oneshot;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const LIQUIDITY_REQUEST_TIMEOUT_SECS: u64 = 5;

struct LSPS1Client {
	lsp_node_id: PublicKey,
	lsp_address: SocketAddress,
	token: Option<String>,
	ldk_client_config: LdkLSPS1ClientConfig,
	pending_opening_params_requests:
		Mutex<HashMap<RequestId, oneshot::Sender<LSPS1OpeningParamsResponse>>>,
	pending_create_order_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS1OrderStatus>>>,
	pending_check_order_status_requests:
		Mutex<HashMap<RequestId, oneshot::Sender<LSPS1OrderStatus>>>,
}

struct LSPS2Client {
	lsp_node_id: PublicKey,
	lsp_address: SocketAddress,
	token: Option<String>,
	ldk_client_config: LdkLSPS2ClientConfig,
	pending_fee_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS2FeeResponse>>>,
	pending_buy_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS2BuyResponse>>>,
}

pub(crate) struct LiquiditySourceBuilder<L: Deref>
where
	L::Target: LdkLogger,
{
	lsps1_client: Option<LSPS1Client>,
	lsps2_client: Option<LSPS2Client>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	chain_source: Arc<ChainSource>,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref> LiquiditySourceBuilder<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		chain_source: Arc<ChainSource>, config: Arc<Config>, logger: L,
	) -> Self {
		let lsps1_client = None;
		let lsps2_client = None;
		Self {
			lsps1_client,
			lsps2_client,
			channel_manager,
			keys_manager,
			chain_source,
			config,
			logger,
		}
	}

	pub(crate) fn lsps1_client(
		&mut self, lsp_node_id: PublicKey, lsp_address: SocketAddress, token: Option<String>,
	) -> &mut Self {
		// TODO: allow to set max_channel_fees_msat
		let ldk_client_config = LdkLSPS1ClientConfig { max_channel_fees_msat: None };
		let pending_opening_params_requests = Mutex::new(HashMap::new());
		let pending_create_order_requests = Mutex::new(HashMap::new());
		let pending_check_order_status_requests = Mutex::new(HashMap::new());
		self.lsps1_client = Some(LSPS1Client {
			lsp_node_id,
			lsp_address,
			token,
			ldk_client_config,
			pending_opening_params_requests,
			pending_create_order_requests,
			pending_check_order_status_requests,
		});
		self
	}

	pub(crate) fn lsps2_client(
		&mut self, lsp_node_id: PublicKey, lsp_address: SocketAddress, token: Option<String>,
	) -> &mut Self {
		let ldk_client_config = LdkLSPS2ClientConfig {};
		let pending_fee_requests = Mutex::new(HashMap::new());
		let pending_buy_requests = Mutex::new(HashMap::new());
		self.lsps2_client = Some(LSPS2Client {
			lsp_node_id,
			lsp_address,
			token,
			ldk_client_config,
			pending_fee_requests,
			pending_buy_requests,
		});
		self
	}

	pub(crate) fn build(self) -> LiquiditySource<L> {
		let lsps1_client_config = self.lsps1_client.as_ref().map(|s| s.ldk_client_config.clone());
		let lsps2_client_config = self.lsps2_client.as_ref().map(|s| s.ldk_client_config.clone());
		let liquidity_client_config =
			Some(LiquidityClientConfig { lsps1_client_config, lsps2_client_config });

		let liquidity_manager = Arc::new(LiquidityManager::new(
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.channel_manager),
			Some(Arc::clone(&self.chain_source)),
			None,
			None,
			liquidity_client_config,
		));

		LiquiditySource {
			lsps1_client: self.lsps1_client,
			lsps2_client: self.lsps2_client,
			channel_manager: self.channel_manager,
			keys_manager: self.keys_manager,
			liquidity_manager,
			config: self.config,
			logger: self.logger,
		}
	}
}

pub(crate) struct LiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	lsps1_client: Option<LSPS1Client>,
	lsps2_client: Option<LSPS2Client>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	liquidity_manager: Arc<LiquidityManager>,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref> LiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn set_peer_manager(&self, peer_manager: Arc<PeerManager>) {
		let process_msgs_callback = move || peer_manager.process_events();
		self.liquidity_manager.set_process_msgs_callback(process_msgs_callback);
	}

	pub(crate) fn liquidity_manager(&self) -> &LiquidityManager {
		self.liquidity_manager.as_ref()
	}

	pub(crate) fn get_lsps1_lsp_details(&self) -> Option<(PublicKey, SocketAddress)> {
		self.lsps1_client.as_ref().map(|s| (s.lsp_node_id, s.lsp_address.clone()))
	}

	pub(crate) fn get_lsps2_lsp_details(&self) -> Option<(PublicKey, SocketAddress)> {
		self.lsps2_client.as_ref().map(|s| (s.lsp_node_id, s.lsp_address.clone()))
	}

	pub(crate) async fn handle_next_event(&self) {
		match self.liquidity_manager.next_event_async().await {
			Event::LSPS1Client(LSPS1ClientEvent::SupportedOptionsReady {
				request_id,
				counterparty_node_id,
				supported_options,
			}) => {
				if let Some(lsps1_client) = self.lsps1_client.as_ref() {
					if counterparty_node_id != lsps1_client.lsp_node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						return;
					}

					if let Some(sender) = lsps1_client
						.pending_opening_params_requests
						.lock()
						.unwrap()
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
			Event::LSPS1Client(LSPS1ClientEvent::OrderCreated {
				request_id,
				counterparty_node_id,
				order_id,
				order,
				payment,
				channel,
			}) => {
				if let Some(lsps1_client) = self.lsps1_client.as_ref() {
					if counterparty_node_id != lsps1_client.lsp_node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						return;
					}

					if let Some(sender) = lsps1_client
						.pending_create_order_requests
						.lock()
						.unwrap()
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
			Event::LSPS1Client(LSPS1ClientEvent::OrderStatus {
				request_id,
				counterparty_node_id,
				order_id,
				order,
				payment,
				channel,
			}) => {
				if let Some(lsps1_client) = self.lsps1_client.as_ref() {
					if counterparty_node_id != lsps1_client.lsp_node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						return;
					}

					if let Some(sender) = lsps1_client
						.pending_check_order_status_requests
						.lock()
						.unwrap()
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
			Event::LSPS2Client(LSPS2ClientEvent::OpeningParametersReady {
				request_id,
				counterparty_node_id,
				opening_fee_params_menu,
			}) => {
				if let Some(lsps2_client) = self.lsps2_client.as_ref() {
					if counterparty_node_id != lsps2_client.lsp_node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						return;
					}

					if let Some(sender) =
						lsps2_client.pending_fee_requests.lock().unwrap().remove(&request_id)
					{
						let response = LSPS2FeeResponse { opening_fee_params_menu };

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
						"Received unexpected LSPS2Client::OpeningParametersReady event!"
					);
				}
			},
			Event::LSPS2Client(LSPS2ClientEvent::InvoiceParametersReady {
				request_id,
				counterparty_node_id,
				intercept_scid,
				cltv_expiry_delta,
				..
			}) => {
				if let Some(lsps2_client) = self.lsps2_client.as_ref() {
					if counterparty_node_id != lsps2_client.lsp_node_id {
						debug_assert!(
							false,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						log_error!(
							self.logger,
							"Received response from unexpected LSP counterparty. This should never happen."
						);
						return;
					}

					if let Some(sender) =
						lsps2_client.pending_buy_requests.lock().unwrap().remove(&request_id)
					{
						let response = LSPS2BuyResponse { intercept_scid, cltv_expiry_delta };

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
						"Received unexpected LSPS2Client::InvoiceParametersReady event!"
					);
				}
			},
			e => {
				log_error!(self.logger, "Received unexpected liquidity event: {:?}", e);
			},
		}
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
				lsps1_client.pending_opening_params_requests.lock().unwrap();
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

		let order_params = OrderParameters {
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
				lsps1_client.pending_create_order_requests.lock().unwrap();
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
		&self, order_id: OrderId,
	) -> Result<LSPS1OrderStatus, Error> {
		let lsps1_client = self.lsps1_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;
		let client_handler = self.liquidity_manager.lsps1_client_handler().ok_or_else(|| {
			log_error!(self.logger, "LSPS1 liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (request_sender, request_receiver) = oneshot::channel();
		{
			let mut pending_check_order_status_requests_lock =
				lsps1_client.pending_check_order_status_requests.lock().unwrap();
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

	pub(crate) async fn lsps2_receive_to_jit_channel(
		&self, amount_msat: u64, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		max_total_lsp_fee_limit_msat: Option<u64>,
	) -> Result<(Bolt11Invoice, u64), Error> {
		let fee_response = self.lsps2_request_opening_fee_params().await?;

		let (min_total_fee_msat, min_opening_params) = fee_response
			.opening_fee_params_menu
			.into_iter()
			.filter_map(|params| {
				if amount_msat < params.min_payment_size_msat
					|| amount_msat > params.max_payment_size_msat
				{
					log_debug!(self.logger,
						"Skipping LSP-offered JIT parameters as the payment of {}msat doesn't meet LSP limits (min: {}msat, max: {}msat)",
						amount_msat,
						params.min_payment_size_msat,
						params.max_payment_size_msat
					);
					None
				} else {
					compute_opening_fee(amount_msat, params.min_fee_msat, params.proportional as u64)
						.map(|fee| (fee, params))
				}
			})
			.min_by_key(|p| p.0)
			.ok_or_else(|| {
				log_error!(self.logger, "Failed to handle response from liquidity service",);
				Error::LiquidityRequestFailed
			})?;

		if let Some(max_total_lsp_fee_limit_msat) = max_total_lsp_fee_limit_msat {
			if min_total_fee_msat > max_total_lsp_fee_limit_msat {
				log_error!(self.logger,
					"Failed to request inbound JIT channel as LSP's requested total opening fee of {}msat exceeds our fee limit of {}msat",
					min_total_fee_msat, max_total_lsp_fee_limit_msat
				);
				return Err(Error::LiquidityFeeTooHigh);
			}
		}

		log_debug!(
			self.logger,
			"Choosing cheapest liquidity offer, will pay {}msat in total LSP fees",
			min_total_fee_msat
		);

		let buy_response =
			self.lsps2_send_buy_request(Some(amount_msat), min_opening_params).await?;
		let invoice = self.lsps2_create_jit_invoice(
			buy_response,
			Some(amount_msat),
			description,
			expiry_secs,
		)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, min_total_fee_msat))
	}

	pub(crate) async fn lsps2_receive_variable_amount_to_jit_channel(
		&self, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
	) -> Result<(Bolt11Invoice, u64), Error> {
		let fee_response = self.lsps2_request_opening_fee_params().await?;

		let (min_prop_fee_ppm_msat, min_opening_params) = fee_response
			.opening_fee_params_menu
			.into_iter()
			.map(|params| (params.proportional as u64, params))
			.min_by_key(|p| p.0)
			.ok_or_else(|| {
				log_error!(self.logger, "Failed to handle response from liquidity service",);
				Error::LiquidityRequestFailed
			})?;

		if let Some(max_proportional_lsp_fee_limit_ppm_msat) =
			max_proportional_lsp_fee_limit_ppm_msat
		{
			if min_prop_fee_ppm_msat > max_proportional_lsp_fee_limit_ppm_msat {
				log_error!(self.logger,
					"Failed to request inbound JIT channel as LSP's requested proportional opening fee of {} ppm msat exceeds our fee limit of {} ppm msat",
					min_prop_fee_ppm_msat,
					max_proportional_lsp_fee_limit_ppm_msat
				);
				return Err(Error::LiquidityFeeTooHigh);
			}
		}

		log_debug!(
			self.logger,
			"Choosing cheapest liquidity offer, will pay {}ppm msat in proportional LSP fees",
			min_prop_fee_ppm_msat
		);

		let buy_response = self.lsps2_send_buy_request(None, min_opening_params).await?;
		let invoice =
			self.lsps2_create_jit_invoice(buy_response, None, description, expiry_secs)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, min_prop_fee_ppm_msat))
	}

	async fn lsps2_request_opening_fee_params(&self) -> Result<LSPS2FeeResponse, Error> {
		let lsps2_client = self.lsps2_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps2_client_handler().ok_or_else(|| {
			log_error!(self.logger, "Liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (fee_request_sender, fee_request_receiver) = oneshot::channel();
		{
			let mut pending_fee_requests_lock = lsps2_client.pending_fee_requests.lock().unwrap();
			let request_id = client_handler
				.request_opening_params(lsps2_client.lsp_node_id, lsps2_client.token.clone());
			pending_fee_requests_lock.insert(request_id, fee_request_sender);
		}

		tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			fee_request_receiver,
		)
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

	async fn lsps2_send_buy_request(
		&self, amount_msat: Option<u64>, opening_fee_params: OpeningFeeParams,
	) -> Result<LSPS2BuyResponse, Error> {
		let lsps2_client = self.lsps2_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps2_client_handler().ok_or_else(|| {
			log_error!(self.logger, "Liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (buy_request_sender, buy_request_receiver) = oneshot::channel();
		{
			let mut pending_buy_requests_lock = lsps2_client.pending_buy_requests.lock().unwrap();
			let request_id = client_handler
				.select_opening_params(lsps2_client.lsp_node_id, amount_msat, opening_fee_params)
				.map_err(|e| {
					log_error!(
						self.logger,
						"Failed to send buy request to liquidity service: {:?}",
						e
					);
					Error::LiquidityRequestFailed
				})?;
			pending_buy_requests_lock.insert(request_id, buy_request_sender);
		}

		let buy_response = tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			buy_request_receiver,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Liquidity request timed out: {}", e);
			Error::LiquidityRequestFailed
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to handle response from liquidity service: {:?}", e);
			Error::LiquidityRequestFailed
		})?;

		Ok(buy_response)
	}

	fn lsps2_create_jit_invoice(
		&self, buy_response: LSPS2BuyResponse, amount_msat: Option<u64>,
		description: &Bolt11InvoiceDescription, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let lsps2_client = self.lsps2_client.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		// LSPS2 requires min_final_cltv_expiry_delta to be at least 2 more than usual.
		let min_final_cltv_expiry_delta = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;
		let (payment_hash, payment_secret) = self
			.channel_manager
			.create_inbound_payment(None, expiry_secs, Some(min_final_cltv_expiry_delta))
			.map_err(|e| {
				log_error!(self.logger, "Failed to register inbound payment: {:?}", e);
				Error::InvoiceCreationFailed
			})?;

		let route_hint = RouteHint(vec![RouteHintHop {
			src_node_id: lsps2_client.lsp_node_id,
			short_channel_id: buy_response.intercept_scid,
			fees: RoutingFees { base_msat: 0, proportional_millionths: 0 },
			cltv_expiry_delta: buy_response.cltv_expiry_delta as u16,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}]);

		let payment_hash = sha256::Hash::from_slice(&payment_hash.0).map_err(|e| {
			log_error!(self.logger, "Invalid payment hash: {:?}", e);
			Error::InvoiceCreationFailed
		})?;

		let currency = self.config.network.into();
		let mut invoice_builder = InvoiceBuilder::new(currency)
			.invoice_description(description.clone())
			.payment_hash(payment_hash)
			.payment_secret(payment_secret)
			.current_timestamp()
			.min_final_cltv_expiry_delta(min_final_cltv_expiry_delta.into())
			.expiry_time(Duration::from_secs(expiry_secs.into()))
			.private_route(route_hint);

		if let Some(amount_msat) = amount_msat {
			invoice_builder = invoice_builder.amount_milli_satoshis(amount_msat).basic_mpp();
		}

		invoice_builder
			.build_signed(|hash| {
				Secp256k1::new()
					.sign_ecdsa_recoverable(hash, &self.keys_manager.get_node_secret_key())
			})
			.map_err(|e| {
				log_error!(self.logger, "Failed to build and sign invoice: {}", e);
				Error::InvoiceCreationFailed
			})
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
	pub order_id: OrderId,
	/// The parameters of channel order.
	pub order_params: OrderParameters,
	/// Contains details about how to pay for the order.
	pub payment_options: PaymentInfo,
	/// Contains information about the channel state.
	pub channel_state: Option<ChannelInfo>,
}

#[cfg(not(feature = "uniffi"))]
type PaymentInfo = lightning_liquidity::lsps1::msgs::PaymentInfo;

/// Details regarding how to pay for an order.
#[cfg(feature = "uniffi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentInfo {
	/// A Lightning payment using BOLT 11.
	pub bolt11: Option<lightning_liquidity::lsps1::msgs::Bolt11PaymentInfo>,
	/// An onchain payment.
	pub onchain: Option<OnchainPaymentInfo>,
}

#[cfg(feature = "uniffi")]
impl From<lightning_liquidity::lsps1::msgs::PaymentInfo> for PaymentInfo {
	fn from(value: lightning_liquidity::lsps1::msgs::PaymentInfo) -> Self {
		PaymentInfo { bolt11: value.bolt11, onchain: value.onchain.map(|o| o.into()) }
	}
}

/// An onchain payment.
#[cfg(feature = "uniffi")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnchainPaymentInfo {
	/// Indicates the current state of the payment.
	pub state: lightning_liquidity::lsps1::msgs::PaymentState,
	/// The datetime when the payment option expires.
	pub expires_at: chrono::DateTime<chrono::Utc>,
	/// The total fee the LSP will charge to open this channel in satoshi.
	pub fee_total_sat: u64,
	/// The amount the client needs to pay to have the requested channel openend.
	pub order_total_sat: u64,
	/// An on-chain address the client can send [`Self::order_total_sat`] to to have the channel
	/// opened.
	pub address: bitcoin::Address,
	/// The minimum number of block confirmations that are required for the on-chain payment to be
	/// considered confirmed.
	pub min_onchain_payment_confirmations: Option<u16>,
	/// The minimum fee rate for the on-chain payment in case the client wants the payment to be
	/// confirmed without a confirmation.
	pub min_fee_for_0conf: Arc<bitcoin::FeeRate>,
	/// The address where the LSP will send the funds if the order fails.
	pub refund_onchain_address: Option<bitcoin::Address>,
}

#[cfg(feature = "uniffi")]
impl From<lightning_liquidity::lsps1::msgs::OnchainPaymentInfo> for OnchainPaymentInfo {
	fn from(value: lightning_liquidity::lsps1::msgs::OnchainPaymentInfo) -> Self {
		Self {
			state: value.state,
			expires_at: value.expires_at,
			fee_total_sat: value.fee_total_sat,
			order_total_sat: value.order_total_sat,
			address: value.address,
			min_onchain_payment_confirmations: value.min_onchain_payment_confirmations,
			min_fee_for_0conf: Arc::new(value.min_fee_for_0conf),
			refund_onchain_address: value.refund_onchain_address,
		}
	}
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2FeeResponse {
	opening_fee_params_menu: Vec<OpeningFeeParams>,
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2BuyResponse {
	intercept_scid: u64,
	cltv_expiry_delta: u32,
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
pub struct LSPS1Liquidity {
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	wallet: Arc<Wallet>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>,
	logger: Arc<Logger>,
}

impl LSPS1Liquidity {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>, wallet: Arc<Wallet>,
		connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>, logger: Arc<Logger>,
	) -> Self {
		Self { runtime, wallet, connection_manager, liquidity_source, logger }
	}

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

		let rt_lock = self.runtime.read().unwrap();
		let runtime = rt_lock.as_ref().unwrap();

		let con_node_id = lsp_node_id;
		let con_addr = lsp_address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
			})
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", lsp_node_id, lsp_address);

		let refund_address = self.wallet.get_new_address()?;

		let liquidity_source = Arc::clone(&liquidity_source);
		let response = tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				liquidity_source
					.lsps1_request_channel(
						lsp_balance_sat,
						client_balance_sat,
						channel_expiry_blocks,
						announce_channel,
						refund_address,
					)
					.await
			})
		})?;

		Ok(response)
	}

	/// Connects to the configured LSP and checks for the status of a previously-placed order.
	pub fn check_order_status(&self, order_id: OrderId) -> Result<LSPS1OrderStatus, Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let (lsp_node_id, lsp_address) =
			liquidity_source.get_lsps1_lsp_details().ok_or(Error::LiquiditySourceUnavailable)?;

		let rt_lock = self.runtime.read().unwrap();
		let runtime = rt_lock.as_ref().unwrap();

		let con_node_id = lsp_node_id;
		let con_addr = lsp_address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
			})
		})?;

		let liquidity_source = Arc::clone(&liquidity_source);
		let response = tokio::task::block_in_place(move || {
			runtime
				.block_on(async move { liquidity_source.lsps1_check_order_status(order_id).await })
		})?;

		Ok(response)
	}
}
