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

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::log_warn;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, InvoiceBuilder, RoutingFees};
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;
use lightning_types::payment::PaymentHash;
use tokio::sync::oneshot;

use crate::connection::ConnectionManager;
use crate::liquidity::{
	select_all_lsps_for_protocol, select_lsps_for_protocol, LspConfig, LspNode,
	LIQUIDITY_REQUEST_TIMEOUT_SECS, LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::types::{ChannelManager, KeysManager, LiquidityManager};
use crate::{Config, Error};

pub(crate) struct LSPS2ClientLiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_lsps2_fee_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS2FeeResponse>>>,
	pub(crate) pending_lsps2_buy_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS2BuyResponse>>>,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) keys_manager: Arc<KeysManager>,
	pub(crate) discovery_done_rx: tokio::sync::watch::Receiver<bool>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) config: Arc<Config>,
	pub(crate) logger: L,
}

impl<L: Deref + Clone + Send + Sync> LSPS2ClientLiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) async fn lsps2_receive_to_jit_channel(
		&self, amount_msat: u64, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		max_total_lsp_fee_limit_msat: Option<u64>, payment_hash: Option<PaymentHash>,
		connection_manager: &ConnectionManager<L>,
	) -> Result<(Bolt11Invoice, u64, LspConfig), Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;

		// Connect to all candidate LSPs before querying fees.
		for lsp_node in &lsps2_nodes {
			if let Err(e) = connection_manager
				.connect_peer_if_necessary(lsp_node.node_id, lsp_node.address.clone())
				.await
			{
				log_warn!(
					self.logger,
					"Failed to connect to LSP {} for fee query: {}",
					lsp_node.node_id,
					e
				);
			}
		}

		let mut all_offers = Vec::new();
		for lsp_node in &lsps2_nodes {
			match self.lsps2_request_opening_fee_params(Some(&lsp_node.node_id)).await {
				Ok(fee_response) => all_offers.push((lsp_node.clone(), fee_response)),
				Err(e) => {
					log_warn!(
						self.logger,
						"Failed to get fees from LSP {}: {}",
						lsp_node.node_id,
						e
					);
					continue;
				},
			}
		}

		let (cheapest_lsp, min_total_fee_msat, min_opening_params) = all_offers
			.into_iter()
			.flat_map(|(lsp, resp)| {
				resp.opening_fee_params_menu
					.into_iter()
					.map(move |params| (lsp.clone(), params))
			})
			.filter_map(|(lsp, params)| {
				if amount_msat < params.min_payment_size_msat
					|| amount_msat > params.max_payment_size_msat
				{
					log_debug!(self.logger,
						"Skipping LSP {}'s JIT offer as the payment of {}msat doesn't meet LSP limits (min: {}msat, max: {}msat)",
						lsp.node_id,
						amount_msat,
						params.min_payment_size_msat,
						params.max_payment_size_msat
					);
					None
				} else {
					compute_opening_fee(amount_msat, params.min_fee_msat, params.proportional as u64)
						.map(|fee| (lsp, fee, params))
				}
			})
			.min_by_key(|(_, fee, _)| *fee)
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
			"Choosing cheapest liquidity offer from LSP {}, will pay {}msat in total LSP fees",
			cheapest_lsp.node_id,
			min_total_fee_msat
		);

		let buy_response = self
			.lsps2_send_buy_request(
				Some(amount_msat),
				min_opening_params,
				Some(&cheapest_lsp.node_id),
			)
			.await?;
		let invoice = self.lsps2_create_jit_invoice(
			buy_response,
			Some(amount_msat),
			description,
			expiry_secs,
			payment_hash,
			Some(&cheapest_lsp.node_id),
		)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, min_total_fee_msat, cheapest_lsp))
	}

	pub(crate) async fn lsps2_receive_variable_amount_to_jit_channel(
		&self, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>, payment_hash: Option<PaymentHash>,
		connection_manager: &ConnectionManager<L>,
	) -> Result<(Bolt11Invoice, u64, LspConfig), Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;

		// Connect to all candidate LSPs before querying fees.
		for lsp_node in &lsps2_nodes {
			if let Err(e) = connection_manager
				.connect_peer_if_necessary(lsp_node.node_id, lsp_node.address.clone())
				.await
			{
				log_warn!(
					self.logger,
					"Failed to connect to LSP {} for fee query: {}",
					lsp_node.node_id,
					e
				);
			}
		}

		let mut all_offers = Vec::new();
		for lsp_node in &lsps2_nodes {
			match self.lsps2_request_opening_fee_params(Some(&lsp_node.node_id)).await {
				Ok(fee_response) => all_offers.push((lsp_node.clone(), fee_response)),
				Err(e) => {
					log_warn!(
						self.logger,
						"Failed to get fees from LSP {}: {}",
						lsp_node.node_id,
						e
					);
					continue;
				},
			}
		}

		let (cheapest_lsp, min_prop_fee_ppm_msat, min_opening_params) = all_offers
			.into_iter()
			.flat_map(|(lsp, resp)| {
				resp.opening_fee_params_menu.into_iter().map(move |params| (lsp.clone(), params))
			})
			.map(|(lsp, params)| {
				let ppm = params.proportional as u64;
				(lsp, ppm, params)
			})
			.min_by_key(|(_, ppm, _)| *ppm)
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
			"Choosing cheapest liquidity offer from LSP {}, will pay {}ppm msat in proportional LSP fees",
			cheapest_lsp.node_id,
			min_prop_fee_ppm_msat
		);

		let buy_response = self
			.lsps2_send_buy_request(None, min_opening_params, Some(&cheapest_lsp.node_id))
			.await?;
		let invoice = self.lsps2_create_jit_invoice(
			buy_response,
			None,
			description,
			expiry_secs,
			payment_hash,
			Some(&cheapest_lsp.node_id),
		)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, min_prop_fee_ppm_msat, cheapest_lsp))
	}
}

impl<L: Deref> LSPS2ClientLiquiditySource<L>
where
	L::Target: LdkLogger,
{
	async fn lsps2_request_opening_fee_params(
		&self, node_id: Option<&PublicKey>,
	) -> Result<LSPS2FeeResponse, Error> {
		let lsps2_node = select_lsps_for_protocol(&self.lsp_nodes, 2, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps2_client_handler().ok_or_else(|| {
			log_error!(self.logger, "Liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (fee_request_sender, fee_request_receiver) = oneshot::channel();
		{
			let mut pending_fee_requests_lock =
				self.pending_lsps2_fee_requests.lock().expect("lock");
			let request_id =
				client_handler.request_opening_params(lsps2_node.node_id, lsps2_node.token.clone());
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
		&self, amount_msat: Option<u64>, opening_fee_params: LSPS2OpeningFeeParams,
		node_id: Option<&PublicKey>,
	) -> Result<LSPS2BuyResponse, Error> {
		let lsps2_node = select_lsps_for_protocol(&self.lsp_nodes, 2, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;

		let client_handler = self.liquidity_manager.lsps2_client_handler().ok_or_else(|| {
			log_error!(self.logger, "Liquidity client was not configured.",);
			Error::LiquiditySourceUnavailable
		})?;

		let (buy_request_sender, buy_request_receiver) = oneshot::channel();
		{
			let mut pending_buy_requests_lock =
				self.pending_lsps2_buy_requests.lock().expect("lock");
			let request_id = client_handler
				.select_opening_params(lsps2_node.node_id, amount_msat, opening_fee_params)
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
		payment_hash: Option<PaymentHash>, node_id: Option<&PublicKey>,
	) -> Result<Bolt11Invoice, Error> {
		let lsps2_node = select_lsps_for_protocol(&self.lsp_nodes, 2, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;

		// LSPS2 requires min_final_cltv_expiry_delta to be at least 2 more than usual.
		let min_final_cltv_expiry_delta = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;
		let (payment_hash, payment_secret) = match payment_hash {
			Some(payment_hash) => {
				let payment_secret = self
					.channel_manager
					.create_inbound_payment_for_hash(
						payment_hash,
						None,
						expiry_secs,
						Some(min_final_cltv_expiry_delta),
					)
					.map_err(|e| {
						log_error!(self.logger, "Failed to register inbound payment: {:?}", e);
						Error::InvoiceCreationFailed
					})?;
				(payment_hash, payment_secret)
			},
			None => self
				.channel_manager
				.create_inbound_payment(None, expiry_secs, Some(min_final_cltv_expiry_delta))
				.map_err(|e| {
					log_error!(self.logger, "Failed to register inbound payment: {:?}", e);
					Error::InvoiceCreationFailed
				})?,
		};

		let route_hint = RouteHint(vec![RouteHintHop {
			src_node_id: lsps2_node.node_id,
			short_channel_id: buy_response.intercept_scid,
			fees: RoutingFees { base_msat: 0, proportional_millionths: 0 },
			cltv_expiry_delta: buy_response.cltv_expiry_delta as u16,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}]);

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

	pub(crate) async fn handle_next_event(&self, event: LSPS2ClientEvent) {
		match event {
			LSPS2ClientEvent::OpeningParametersReady {
				request_id,
				counterparty_node_id,
				opening_fee_params_menu,
			} => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if let Some(sender) =
						self.pending_lsps2_fee_requests.lock().expect("lock").remove(&request_id)
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
			LSPS2ClientEvent::InvoiceParametersReady {
				request_id,
				counterparty_node_id,
				intercept_scid,
				cltv_expiry_delta,
				..
			} => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if let Some(sender) =
						self.pending_lsps2_buy_requests.lock().expect("lock").remove(&request_id)
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
			_ => {
				log_error!(self.logger, "Received unexpected LSPS2Client liquidity event!");
			},
		}
	}

	async fn get_lsps2_nodes(&self) -> Result<Vec<LspConfig>, Error> {
		let mut lsps2_nodes = select_all_lsps_for_protocol(&self.lsp_nodes, 2);
		if lsps2_nodes.is_empty() {
			// LSP protocol discovery may still be in flight, we wait briefly for it to finish, then re-check.
			let mut rx = self.discovery_done_rx.clone();
			if !*rx.borrow() {
				log_debug!(
					self.logger,
					"No LSPS2 node available yet, waiting for protocol discovery to complete."
				);
				let _ = tokio::time::timeout(
					Duration::from_secs(LSPS_DISCOVERY_WAIT_TIMEOUT_SECS),
					rx.wait_for(|done| *done),
				)
				.await;
			}
			lsps2_nodes = select_all_lsps_for_protocol(&self.lsp_nodes, 2);
		}

		if lsps2_nodes.is_empty() {
			log_error!(self.logger, "No LSPs available for LSPS2 protocol.");
			return Err(Error::LiquiditySourceUnavailable);
		};
		Ok(lsps2_nodes)
	}
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2FeeResponse {
	opening_fee_params_menu: Vec<LSPS2OpeningFeeParams>,
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2BuyResponse {
	intercept_scid: u64,
	cltv_expiry_delta: u32,
}
