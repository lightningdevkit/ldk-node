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

use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::{PublicKey, Secp256k1};
use chrono::Utc;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::log_warn;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning::util::persist::KVStore;
use lightning::util::ser::{Readable, Writeable};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, InvoiceBuilder, RoutingFees};
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;
use lightning_types::payment::PaymentHash;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::connection::ConnectionManager;
use crate::io::{LSPS2_GET_INFO_CACHE_PRIMARY_NAMESPACE, LSPS2_GET_INFO_CACHE_SECONDARY_NAMESPACE};
use crate::liquidity::{
	select_all_lsps_for_protocol, select_lsps_for_protocol, LspConfig, LspNode,
	LIQUIDITY_REQUEST_TIMEOUT_SECS, LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::payment::store::LSPS2Parameters;
use crate::payment::PaymentMetadata;
use crate::types::{ChannelManager, DynStore, KeysManager, LiquidityManager};
use crate::{Config, Error};

pub(crate) struct LSPS2Client<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_lsps2_fee_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS2FeeResponse>>>,
	pub(crate) pending_buy_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS2BuyResponse>>>,
	pub(crate) fee_response_cache: LSPS2FeeResponseCache,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) keys_manager: Arc<KeysManager>,
	pub(crate) discovery_done_rx: tokio::sync::watch::Receiver<bool>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) config: Arc<Config>,
	pub(crate) logger: L,
}

impl<L: Deref + Clone + Send + Sync + 'static> LSPS2Client<L>
where
	L::Target: LdkLogger,
{
	pub(crate) async fn lsps2_receive_to_jit_channel(
		self: Arc<Self>, amount_msat: u64, description: &Bolt11InvoiceDescription,
		expiry_secs: u32, max_total_lsp_fee_limit_msat: Option<u64>,
		payment_hash: Option<PaymentHash>, connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		// Connect to all candidate LSPs before querying fees.
		let all_offers = self.gather_lsps2_offers(&connection_manager, expiry_secs).await?;
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
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(min_total_fee_msat),
			max_proportional_opening_fee_ppm_msat: None,
		};

		let invoice = self.lsps2_create_jit_invoice(
			buy_response,
			Some(amount_msat),
			description,
			expiry_secs,
			payment_hash,
			lsps2_parameters,
			Some(&cheapest_lsp.node_id),
		)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, cheapest_lsp))
	}

	pub(crate) async fn lsps2_receive_variable_amount_to_jit_channel(
		self: Arc<Self>, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>, payment_hash: Option<PaymentHash>,
		connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		// Connect to all candidate LSPs before querying fees.
		let all_offers = self.gather_lsps2_offers(&connection_manager, expiry_secs).await?;
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
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: None,
			max_proportional_opening_fee_ppm_msat: Some(min_prop_fee_ppm_msat),
		};
		let invoice = self.lsps2_create_jit_invoice(
			buy_response,
			None,
			description,
			expiry_secs,
			payment_hash,
			lsps2_parameters,
			Some(&cheapest_lsp.node_id),
		)?;

		log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		Ok((invoice, cheapest_lsp))
	}

	async fn gather_lsps2_offers(
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>, expiry_secs: u32,
	) -> Result<Vec<(LspConfig, LSPS2FeeResponse)>, Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;

		// Connect to all candidate LSPs in parallel.
		let mut connect_set = JoinSet::new();
		for lsp_node in &lsps2_nodes {
			let cm = Arc::clone(connection_manager);
			let node_id = lsp_node.node_id;
			let addr = lsp_node.address.clone();
			let logger = self.logger.clone();
			connect_set.spawn(async move {
				if let Err(e) = cm.connect_peer_if_necessary(node_id, addr).await {
					log_warn!(logger, "Failed to connect to LSP {} for fee query: {}", node_id, e);
				}
			});
		}
		while connect_set.join_next().await.is_some() {}

		let mut all_offers: Vec<(LspConfig, LSPS2FeeResponse)> =
			Vec::with_capacity(lsps2_nodes.len());
		let mut fee_set: JoinSet<(LspConfig, Result<LSPS2FeeResponse, Error>)> = JoinSet::new();
		for lsp_node in &lsps2_nodes {
			let lsp = lsp_node.clone();
			let client = Arc::clone(self);
			fee_set.spawn(async move {
				let res =
					client.lsps2_request_opening_fee_params(Some(&lsp.node_id), expiry_secs).await;
				(lsp, res)
			});
		}
		while let Some(join_result) = fee_set.join_next().await {
			match join_result {
				Ok((lsp, Ok(fees))) => all_offers.push((lsp, fees)),
				Ok((lsp, Err(e))) => {
					log_warn!(self.logger, "Failed to get fees from LSP {}: {}", lsp.node_id, e)
				},
				Err(e) => {
					log_warn!(self.logger, "Failed to get fees from LSP: {}", e)
				},
			}
		}

		Ok(all_offers)
	}
}

impl<L: Deref> LSPS2Client<L>
where
	L::Target: LdkLogger,
{
	async fn lsps2_request_opening_fee_params(
		&self, node_id: Option<&PublicKey>, invoice_expiry_secs: u32,
	) -> Result<LSPS2FeeResponse, Error> {
		let lsps2_node = select_lsps_for_protocol(&self.lsp_nodes, 2, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;
		let required_validity = Duration::from_secs(
			u64::from(invoice_expiry_secs).saturating_add(LSPS2_GET_INFO_CACHE_EXPIRY_BUFFER_SECS),
		);
		match self
			.fee_response_cache
			.get(&lsps2_node.node_id, lsps2_node.token.as_deref(), required_validity)
			.await
		{
			Ok(Some(response)) => {
				log_debug!(
					self.logger,
					"Using cached LSPS2 fee response from {}",
					lsps2_node.node_id
				);
				return Ok(response);
			},
			Ok(None) => {},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to read cached LSPS2 fee response from {}: {}",
					lsps2_node.node_id,
					e
				);
			},
		}

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

		let response = tokio::time::timeout(
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
		})?;

		if let Err(e) = self
			.fee_response_cache
			.put(lsps2_node.node_id, lsps2_node.token.as_deref(), &response)
			.await
		{
			log_error!(
				self.logger,
				"Failed to persist LSPS2 fee response from {}: {}",
				lsps2_node.node_id,
				e
			);
		}

		filter_valid_offers(&response.opening_fee_params_menu, required_validity).ok_or_else(|| {
			log_error!(
				self.logger,
				"LSPS2 fee response from {} has no offers valid for the requested invoice expiry",
				lsps2_node.node_id
			);
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
			let mut pending_buy_requests_lock = self.pending_buy_requests.lock().expect("lock");
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

		let buy_result = tokio::time::timeout(
			Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS),
			buy_request_receiver,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Liquidity request timed out: {}", e);
			Error::LiquidityRequestFailed
		})
		.and_then(|res| {
			res.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to handle response from liquidity service: {:?}",
					e
				);
				Error::LiquidityRequestFailed
			})
		});

		if buy_result.is_err() {
			// The buy request might have failed because our cached opening fee params became
			// invalid, e.g., if the LSP rotated its promise secret. Evict the cache entry so
			// that fresh params will be requested on retry.
			if let Err(e) = self.fee_response_cache.remove(&lsps2_node.node_id).await {
				log_error!(
					self.logger,
					"Failed to evict cached LSPS2 fee response for {}: {}",
					lsps2_node.node_id,
					e
				);
			}
		}

		buy_result
	}

	fn lsps2_create_jit_invoice(
		&self, buy_response: LSPS2BuyResponse, amount_msat: Option<u64>,
		description: &Bolt11InvoiceDescription, expiry_secs: u32,
		payment_hash: Option<PaymentHash>, lsps2_parameters: LSPS2Parameters,
		node_id: Option<&PublicKey>,
	) -> Result<Bolt11Invoice, Error> {
		let lsps2_node = select_lsps_for_protocol(&self.lsp_nodes, 2, node_id)
			.ok_or(Error::LiquiditySourceUnavailable)?;

		// LSPS2 requires min_final_cltv_expiry_delta to be at least 2 more than usual.
		let min_final_cltv_expiry_delta = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;
		let encoded_payment_metadata =
			PaymentMetadata { lsps2_parameters: Some(lsps2_parameters) }.encode();
		let (payment_hash, payment_secret, payment_metadata) = match payment_hash {
			Some(payment_hash) => {
				let (payment_secret, payment_metadata) = self
					.channel_manager
					.create_inbound_payment_for_hash(
						payment_hash,
						None,
						expiry_secs,
						Some(min_final_cltv_expiry_delta),
						Some(encoded_payment_metadata),
					)
					.map_err(|e| {
						log_error!(self.logger, "Failed to register inbound payment: {:?}", e);
						Error::InvoiceCreationFailed
					})?;
				(payment_hash, payment_secret, payment_metadata)
			},
			None => self
				.channel_manager
				.create_inbound_payment(
					None,
					expiry_secs,
					Some(min_final_cltv_expiry_delta),
					Some(encoded_payment_metadata),
				)
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

		let invoice = if let Some(payment_metadata) = payment_metadata {
			invoice_builder.payment_metadata(payment_metadata).build_signed(|hash| {
				Secp256k1::new()
					.sign_ecdsa_recoverable(hash, &self.keys_manager.get_node_secret_key())
			})
		} else {
			invoice_builder.build_signed(|hash| {
				Secp256k1::new()
					.sign_ecdsa_recoverable(hash, &self.keys_manager.get_node_secret_key())
			})
		};
		invoice.map_err(|e| {
			log_error!(self.logger, "Failed to build and sign invoice: {}", e);
			Error::InvoiceCreationFailed
		})
	}

	pub(crate) async fn handle_event(&self, event: LSPS2ClientEvent) {
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
						self.pending_buy_requests.lock().expect("lock").remove(&request_id)
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
			LSPS2ClientEvent::GetInfoFailed { request_id, counterparty_node_id, error } => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if self
						.pending_lsps2_fee_requests
						.lock()
						.expect("lock")
						.remove(&request_id)
						.is_some()
					{
						log_error!(
							self.logger,
							"LSPS2 get_info request {:?} failed: {:?}",
							request_id,
							error
						);
					} else {
						debug_assert!(
							false,
							"Received failure from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received failure from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(
						self.logger,
						"Received unexpected LSPS2Client::GetInfoFailed event!"
					);
				}
			},
			LSPS2ClientEvent::BuyRequestFailed { request_id, counterparty_node_id, error } => {
				if self
					.lsp_nodes
					.read()
					.expect("lock")
					.iter()
					.any(|n| n.node_id == counterparty_node_id)
				{
					if self.pending_buy_requests.lock().expect("lock").remove(&request_id).is_some()
					{
						log_error!(
							self.logger,
							"LSPS2 buy request {:?} failed: {:?}",
							request_id,
							error
						);
					} else {
						debug_assert!(
							false,
							"Received failure from liquidity service for unknown request."
						);
						log_error!(
							self.logger,
							"Received failure from liquidity service for unknown request."
						);
					}
				} else {
					log_error!(
						self.logger,
						"Received unexpected LSPS2Client::BuyRequestFailed event!"
					);
				}
			},
		}
	}

	async fn get_lsps2_nodes(&self) -> Result<Vec<LspConfig>, Error> {
		let has_undiscovered_protocol =
			self.lsp_nodes.read().expect("lock").iter().any(|n| n.supported_protocols.is_none());

		if has_undiscovered_protocol {
			// LSP protocol discovery is still in flight, we wait briefly for it to finish, then re-check.
			let mut rx = self.discovery_done_rx.clone();
			if !*rx.borrow() {
				log_debug!(
					self.logger,
					"Waiting for LSP protocol discovery to complete before selecting LSPS2 nodes."
				);
				let _ = tokio::time::timeout(
					Duration::from_secs(LSPS_DISCOVERY_WAIT_TIMEOUT_SECS),
					rx.wait_for(|done| *done),
				)
				.await;
			}
		}

		let lsps2_nodes = select_all_lsps_for_protocol(&self.lsp_nodes, 2);
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

const LSPS2_GET_INFO_CACHE_EXPIRY_BUFFER_SECS: u64 = 60;

#[derive(Clone)]
struct PersistedLSPS2FeeResponse {
	node_id: PublicKey,
	token_hash: Option<[u8; 32]>,
	opening_fee_params_menu: Vec<LSPS2OpeningFeeParams>,
}

impl_writeable_tlv_based!(PersistedLSPS2FeeResponse, {
	(0, node_id, required),
	(2, token_hash, option),
	(4, opening_fee_params_menu, required_vec),
});

impl PersistedLSPS2FeeResponse {
	fn new(node_id: PublicKey, token_hash: Option<[u8; 32]>, response: &LSPS2FeeResponse) -> Self {
		Self {
			node_id,
			token_hash,
			opening_fee_params_menu: response.opening_fee_params_menu.clone(),
		}
	}

	fn matches_request(&self, node_id: &PublicKey, token_hash: Option<[u8; 32]>) -> bool {
		self.node_id == *node_id && self.token_hash == token_hash
	}

	fn response_valid_for(&self, required_validity: Duration) -> Option<LSPS2FeeResponse> {
		filter_valid_offers(&self.opening_fee_params_menu, required_validity)
	}
}

fn filter_valid_offers(
	opening_fee_params_menu: &[LSPS2OpeningFeeParams], required_validity: Duration,
) -> Option<LSPS2FeeResponse> {
	let minimum_valid_until = Utc::now() + required_validity;
	let opening_fee_params_menu = opening_fee_params_menu
		.iter()
		.filter(|params| params.valid_until.0 > minimum_valid_until)
		.cloned()
		.collect::<Vec<_>>();
	if opening_fee_params_menu.is_empty() {
		None
	} else {
		Some(LSPS2FeeResponse { opening_fee_params_menu })
	}
}

#[derive(Debug)]
enum LSPS2FeeResponseCacheError {
	Store(bitcoin::io::Error),
	Decode(lightning::ln::msgs::DecodeError),
}

impl std::fmt::Display for LSPS2FeeResponseCacheError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Store(e) => write!(f, "store error: {e}"),
			Self::Decode(e) => write!(f, "decode error: {e}"),
		}
	}
}

pub(crate) struct LSPS2FeeResponseCache {
	entries: Mutex<HashMap<PublicKey, PersistedLSPS2FeeResponse>>,
	kv_store: Arc<DynStore>,
	token_hash_key: [u8; 32],
}

impl LSPS2FeeResponseCache {
	pub(crate) fn new(kv_store: Arc<DynStore>, node_secret: [u8; 32]) -> Self {
		let mut engine = HmacEngine::<sha256::Hash>::new(&node_secret);
		engine.input(b"LDK Node LSPS2 get_info cache token hash");
		let token_hash_key = Hmac::<sha256::Hash>::from_engine(engine).to_byte_array();
		Self { entries: Mutex::new(HashMap::new()), kv_store, token_hash_key }
	}

	fn hash_token(&self, token: Option<&str>) -> Option<[u8; 32]> {
		token.map(|token| {
			let mut engine = HmacEngine::<sha256::Hash>::new(&self.token_hash_key);
			engine.input(token.as_bytes());
			Hmac::<sha256::Hash>::from_engine(engine).to_byte_array()
		})
	}

	async fn get(
		&self, node_id: &PublicKey, token: Option<&str>, required_validity: Duration,
	) -> Result<Option<LSPS2FeeResponse>, LSPS2FeeResponseCacheError> {
		let token_hash = self.hash_token(token);
		let cached_entry = self.entries.lock().expect("lock").get(node_id).cloned();
		if let Some(entry) = cached_entry {
			return Ok(if entry.matches_request(node_id, token_hash) {
				entry.response_valid_for(required_validity)
			} else {
				None
			});
		}

		let key = node_id.to_string();
		let bytes = match KVStore::read(
			&*self.kv_store,
			LSPS2_GET_INFO_CACHE_PRIMARY_NAMESPACE,
			LSPS2_GET_INFO_CACHE_SECONDARY_NAMESPACE,
			&key,
		)
		.await
		{
			Ok(bytes) => bytes,
			Err(e) if e.kind() == bitcoin::io::ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(LSPS2FeeResponseCacheError::Store(e)),
		};
		let entry = PersistedLSPS2FeeResponse::read(&mut &*bytes)
			.map_err(LSPS2FeeResponseCacheError::Decode)?;
		let response = if entry.matches_request(node_id, token_hash) {
			entry.response_valid_for(required_validity)
		} else {
			None
		};
		self.entries.lock().expect("lock").insert(*node_id, entry);
		Ok(response)
	}

	async fn remove(&self, node_id: &PublicKey) -> Result<(), LSPS2FeeResponseCacheError> {
		self.entries.lock().expect("lock").remove(node_id);
		KVStore::remove(
			&*self.kv_store,
			LSPS2_GET_INFO_CACHE_PRIMARY_NAMESPACE,
			LSPS2_GET_INFO_CACHE_SECONDARY_NAMESPACE,
			&node_id.to_string(),
			false,
		)
		.await
		.map_err(LSPS2FeeResponseCacheError::Store)
	}

	async fn put(
		&self, node_id: PublicKey, token: Option<&str>, response: &LSPS2FeeResponse,
	) -> Result<(), LSPS2FeeResponseCacheError> {
		let mut entry = PersistedLSPS2FeeResponse::new(node_id, self.hash_token(token), response);
		let minimum_valid_until =
			Utc::now() + Duration::from_secs(LSPS2_GET_INFO_CACHE_EXPIRY_BUFFER_SECS);
		entry.opening_fee_params_menu.retain(|params| params.valid_until.0 > minimum_valid_until);
		let key = node_id.to_string();

		if entry.opening_fee_params_menu.is_empty() {
			return self.remove(&node_id).await;
		}

		let encoded_entry = entry.encode();
		self.entries.lock().expect("lock").insert(node_id, entry);
		KVStore::write(
			&*self.kv_store,
			LSPS2_GET_INFO_CACHE_PRIMARY_NAMESPACE,
			LSPS2_GET_INFO_CACHE_SECONDARY_NAMESPACE,
			&key,
			encoded_entry,
		)
		.await
		.map_err(LSPS2FeeResponseCacheError::Store)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use bitcoin::secp256k1::SecretKey;
	use lightning::io;
	use lightning::util::persist::{PageToken, PaginatedKVStore, PaginatedListResponse};
	use lightning_liquidity::lsps0::ser::LSPSDateTime;

	use crate::io::test_utils::InMemoryStore;
	use crate::types::DynStoreWrapper;

	fn test_node_id() -> PublicKey {
		let secret_key = SecretKey::from_slice(&[42; 32]).unwrap();
		PublicKey::from_secret_key(&Secp256k1::new(), &secret_key)
	}

	fn fee_params(min_fee_msat: u64, valid_for: Duration) -> LSPS2OpeningFeeParams {
		LSPS2OpeningFeeParams {
			min_fee_msat,
			proportional: 100,
			valid_until: LSPSDateTime(Utc::now() + valid_for),
			min_lifetime: 144,
			max_client_to_self_delay: 144,
			min_payment_size_msat: 1,
			max_payment_size_msat: 1_000_000,
			promise: "promise".to_string(),
		}
	}

	fn test_store() -> Arc<DynStore> {
		Arc::new(DynStoreWrapper(InMemoryStore::new()))
	}

	fn test_cache(store: Arc<DynStore>) -> LSPS2FeeResponseCache {
		LSPS2FeeResponseCache::new(store, [43; 32])
	}

	struct FailingStore;

	#[allow(clippy::manual_async_fn)]
	impl KVStore for FailingStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str,
		) -> impl std::future::Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "read failed")) }
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "write failed")) }
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> impl std::future::Future<Output = Result<(), io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "remove failed")) }
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> impl std::future::Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
			async { Err(io::Error::new(io::ErrorKind::Other, "list failed")) }
		}
	}

	#[allow(clippy::manual_async_fn)]
	impl PaginatedKVStore for FailingStore {
		fn list_paginated(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
			_page_token: Option<PageToken>,
		) -> impl std::future::Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send
		{
			async { Err(io::Error::new(io::ErrorKind::Other, "list_paginated failed")) }
		}
	}

	#[tokio::test]
	async fn persists_and_restores_get_info_response() {
		let node_id = test_node_id();
		let store = test_store();
		let cache = test_cache(Arc::clone(&store));
		let response = LSPS2FeeResponse {
			opening_fee_params_menu: vec![
				fee_params(1_000, Duration::from_secs(30)),
				fee_params(2_000, Duration::from_secs(7_200)),
			],
		};

		cache.put(node_id, Some("token"), &response).await.unwrap();

		let restored_cache = test_cache(store);
		let restored = restored_cache
			.get(&node_id, Some("token"), Duration::from_secs(3_600))
			.await
			.unwrap()
			.unwrap();
		assert_eq!(restored.opening_fee_params_menu.len(), 1);
		assert_eq!(restored.opening_fee_params_menu[0].min_fee_msat, 2_000);
	}

	#[tokio::test]
	async fn rejects_cache_for_changed_token_or_insufficient_validity() {
		let node_id = test_node_id();
		let store = test_store();
		let cache = test_cache(Arc::clone(&store));
		let response = LSPS2FeeResponse {
			opening_fee_params_menu: vec![fee_params(1_000, Duration::from_secs(7_200))],
		};
		cache.put(node_id, Some("token"), &response).await.unwrap();

		let restored_cache = test_cache(store);
		assert!(restored_cache
			.get(&node_id, Some("other-token"), Duration::from_secs(60))
			.await
			.unwrap()
			.is_none());
		assert!(restored_cache
			.get(&node_id, Some("token"), Duration::from_secs(10_800))
			.await
			.unwrap()
			.is_none());
	}

	#[tokio::test]
	async fn removes_cache_without_reusable_offers() {
		let node_id = test_node_id();
		let store = test_store();
		let cache = test_cache(Arc::clone(&store));
		let response = LSPS2FeeResponse {
			opening_fee_params_menu: vec![fee_params(1_000, Duration::from_secs(7_200))],
		};
		cache.put(node_id, None, &response).await.unwrap();

		let empty_response = LSPS2FeeResponse { opening_fee_params_menu: Vec::new() };
		cache.put(node_id, None, &empty_response).await.unwrap();

		let restored_cache = test_cache(store);
		assert!(restored_cache
			.get(&node_id, None, Duration::from_secs(60))
			.await
			.unwrap()
			.is_none());
	}

	#[tokio::test]
	async fn remove_evicts_persisted_and_in_memory_entries() {
		let node_id = test_node_id();
		let store = test_store();
		let cache = test_cache(Arc::clone(&store));
		let response = LSPS2FeeResponse {
			opening_fee_params_menu: vec![fee_params(1_000, Duration::from_secs(7_200))],
		};
		cache.put(node_id, None, &response).await.unwrap();
		assert!(cache.get(&node_id, None, Duration::from_secs(60)).await.unwrap().is_some());

		cache.remove(&node_id).await.unwrap();

		assert!(cache.get(&node_id, None, Duration::from_secs(60)).await.unwrap().is_none());
		let restored_cache = test_cache(store);
		assert!(restored_cache
			.get(&node_id, None, Duration::from_secs(60))
			.await
			.unwrap()
			.is_none());
	}

	#[tokio::test]
	async fn reports_corrupt_persisted_response() {
		let node_id = test_node_id();
		let store = test_store();
		KVStore::write(
			&*store,
			LSPS2_GET_INFO_CACHE_PRIMARY_NAMESPACE,
			LSPS2_GET_INFO_CACHE_SECONDARY_NAMESPACE,
			&node_id.to_string(),
			vec![0xff],
		)
		.await
		.unwrap();

		let cache = test_cache(store);
		assert!(matches!(
			cache.get(&node_id, None, Duration::from_secs(60)).await,
			Err(LSPS2FeeResponseCacheError::Decode(_))
		));
	}

	#[tokio::test]
	async fn keeps_in_memory_response_when_persistence_fails() {
		let node_id = test_node_id();
		let store: Arc<DynStore> = Arc::new(DynStoreWrapper(FailingStore));
		let cache = test_cache(store);
		let response = LSPS2FeeResponse {
			opening_fee_params_menu: vec![fee_params(1_000, Duration::from_secs(7_200))],
		};

		assert!(matches!(
			cache.put(node_id, None, &response).await,
			Err(LSPS2FeeResponseCacheError::Store(_))
		));
		assert!(cache.get(&node_id, None, Duration::from_secs(60)).await.unwrap().is_some());
	}
}
