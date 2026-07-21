// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::log_warn;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, InvoiceBuilder, RoutingFees};
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;
use lightning_types::payment::PaymentHash;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::connection::ConnectionManager;
use crate::liquidity::{
	select_all_lsps_for_protocol, select_lsps_for_protocol, LspConfig, LspNode,
	LIQUIDITY_REQUEST_TIMEOUT_SECS, LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::payment::store::LSPS2Parameters;
use crate::payment::PaymentMetadata;
use crate::types::{ChannelManager, KeysManager, LiquidityManager};
use crate::{Config, Error};

use self::state::{
	now_secs, LSPS2LeaseState, LeaseCacheTarget, LeaseCacheTargetId, LeaseCacheTargetStore,
	PaymentLease, PaymentLeaseId, PaymentLeaseStore,
};

async fn consume_after_persisted_removal<T, E, RF, CF, Fut>(
	value: T, persist_removal: RF, consume: CF,
) -> Result<T, E>
where
	T: Clone,
	RF: FnOnce(T) -> Fut,
	CF: FnOnce(&T),
	Fut: Future<Output = Result<(), E>>,
{
	persist_removal(value.clone()).await?;
	consume(&value);
	Ok(value)
}

pub(crate) struct LSPS2Client<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_lsps2_fee_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS2FeeResponse>>>,
	pub(crate) pending_buy_requests:
		Mutex<HashMap<LSPSRequestId, oneshot::Sender<LSPS2BuyResponse>>>,
	pub(crate) lease_store: Arc<PaymentLeaseStore<L>>,
	pub(crate) lease_state: Mutex<LSPS2LeaseState>,
	pub(crate) cache_target_store: Arc<LeaseCacheTargetStore<L>>,
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
	pub(crate) async fn prune_stale_leases(&self) -> Result<(), Error> {
		let stale_ids = self
			.lease_store
			.list_filter(|lease| !state::is_lease_usable(lease))
			.into_iter()
			.map(|lease| lease.id)
			.collect::<Vec<_>>();
		for id in stale_ids {
			self.lease_store.remove(&id).await?;
		}
		self.lease_state.lock().expect("lock").prune();
		Ok(())
	}

	pub(crate) async fn prune_stale_cache_targets(&self) -> Result<(), Error> {
		self.cache_target_store.prune(now_secs()).await
	}

	pub(crate) async fn register_cache_target(
		&self, id: LeaseCacheTargetId, absolute_expiry: Option<u64>,
	) -> Result<(), Error> {
		let target = LeaseCacheTarget::new(id, absolute_expiry);
		self.cache_target_store.register(target).await
	}

	pub(crate) fn cache_targets(&self) -> Vec<LeaseCacheTarget> {
		self.cache_target_store.targets()
	}

	pub(crate) async fn lsps2_receive_to_jit_channel(
		self: Arc<Self>, amount_msat: u64, description: &Bolt11InvoiceDescription,
		expiry_secs: u32, payment_hash: Option<PaymentHash>,
		connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		if let Some((lease, total_fee_msat, lsp)) =
			self.take_cached_fixed_lease(amount_msat).await?
		{
			let invoice = self.lsps2_create_jit_invoice(
				LSPS2BuyResponse::from(&lease),
				Some(amount_msat),
				description,
				expiry_secs,
				payment_hash,
				LSPS2Parameters {
					max_total_opening_fee_msat: Some(total_fee_msat),
					max_proportional_opening_fee_ppm_msat: None,
				},
				Some(&lsp.node_id),
			)?;
			return Ok((invoice, lsp));
		}

		// Connect to all candidate LSPs before querying fees.
		let all_offers = self.gather_lsps2_offers(&connection_manager).await?;
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

		if let Some(max_total_lsp_fee_limit_msat) = self.config.lsps2_max_total_lsp_fee_limit_msat {
			if min_total_fee_msat > max_total_lsp_fee_limit_msat {
				log_error!(self.logger,
					"Failed to request inbound JIT channel as LSP's requested total opening fee of {}msat exceeds our configured fee limit of {}msat",
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

		let negotiated_lease = self
			.lsps2_send_buy_request(
				Some(amount_msat),
				min_opening_params,
				Some(&cheapest_lsp.node_id),
			)
			.await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(min_total_fee_msat),
			max_proportional_opening_fee_ppm_msat: None,
		};

		let invoice = self.lsps2_create_jit_invoice(
			LSPS2BuyResponse::from(&lease),
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
		payment_hash: Option<PaymentHash>, connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		if let Some((lease, proportional_fee, lsp)) = self.take_cached_variable_lease().await? {
			let invoice = self.lsps2_create_jit_invoice(
				LSPS2BuyResponse::from(&lease),
				None,
				description,
				expiry_secs,
				payment_hash,
				LSPS2Parameters {
					max_total_opening_fee_msat: self.config.lsps2_max_total_lsp_fee_limit_msat,
					max_proportional_opening_fee_ppm_msat: self
						.config
						.lsps2_max_total_lsp_fee_limit_msat
						.is_none()
						.then_some(proportional_fee),
				},
				Some(&lsp.node_id),
			)?;
			return Ok((invoice, lsp));
		}

		// Connect to all candidate LSPs before querying fees.
		let all_offers = self.gather_lsps2_offers(&connection_manager).await?;
		let mut rejected_for_fee = false;
		let (cheapest_lsp, min_prop_fee_ppm_msat, min_opening_params) = all_offers
			.into_iter()
			.flat_map(|(lsp, resp)| {
				resp.opening_fee_params_menu.into_iter().map(move |params| (lsp.clone(), params))
			})
			.map(|(lsp, params)| {
				let ppm = params.proportional as u64;
				(lsp, ppm, params)
			})
			.filter(|(_, _, params)| {
				let allowed = self
					.config
					.lsps2_max_total_lsp_fee_limit_msat
					.map_or(true, |limit| params.min_fee_msat <= limit);
				rejected_for_fee |= !allowed;
				allowed
			})
			.min_by_key(|(_, ppm, _)| *ppm)
			.ok_or_else(|| {
				if rejected_for_fee {
					log_error!(
						self.logger,
						"Failed to request inbound JIT channel as all LSP offers exceed our configured fee limit"
					);
					return Error::LiquidityFeeTooHigh;
				}
				log_error!(self.logger, "Failed to handle response from liquidity service",);
				Error::LiquidityRequestFailed
			})?;
		log_debug!(
			self.logger,
			"Choosing cheapest liquidity offer from LSP {}, will pay {}ppm msat in proportional LSP fees",
			cheapest_lsp.node_id,
			min_prop_fee_ppm_msat
		);

		let negotiated_lease = self
			.lsps2_send_buy_request(None, min_opening_params, Some(&cheapest_lsp.node_id))
			.await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: self.config.lsps2_max_total_lsp_fee_limit_msat,
			max_proportional_opening_fee_ppm_msat: self
				.config
				.lsps2_max_total_lsp_fee_limit_msat
				.is_none()
				.then_some(min_prop_fee_ppm_msat),
		};
		let invoice = self.lsps2_create_jit_invoice(
			LSPS2BuyResponse::from(&lease),
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
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>,
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
				let res = client.lsps2_request_opening_fee_params(Some(&lsp.node_id)).await;
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
	) -> Result<PaymentLease, Error> {
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
				.select_opening_params(lsps2_node.node_id, amount_msat, opening_fee_params.clone())
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

		let valid_until = opening_fee_params
			.valid_until
			.0
			.timestamp()
			.try_into()
			.map_err(|_| Error::LiquidityRequestFailed)?;
		let lease = PaymentLease {
			id: PaymentLeaseId {
				lsp_node_id: lsps2_node.node_id,
				intercept_scid: buy_response.intercept_scid,
			},
			params: opening_fee_params,
			cltv_expiry_delta: buy_response.cltv_expiry_delta,
			payment_size_msat: amount_msat,
			valid_until,
		};
		self.lease_store.insert(lease.clone()).await?;
		self.lease_state.lock().expect("lock").insert(lease.clone());
		Ok(lease)
	}

	async fn consume_lease(&self, id: &PaymentLeaseId) -> Result<PaymentLease, Error> {
		let lease = self
			.lease_state
			.lock()
			.expect("lock")
			.valid(id)
			.ok_or(Error::LiquidityRequestFailed)?;
		self.consume_selected_lease(lease).await
	}

	async fn consume_selected_lease(&self, lease: PaymentLease) -> Result<PaymentLease, Error> {
		let lease_store = Arc::clone(&self.lease_store);
		consume_after_persisted_removal(
			lease,
			move |lease| async move { lease_store.remove(&lease.id).await },
			|lease| {
				self.lease_state.lock().expect("lock").remove(&lease.id);
			},
		)
		.await
	}

	async fn take_cached_fixed_lease(
		&self, amount_msat: u64,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		loop {
			let Some((lease, fee_msat)) = self
				.lease_state
				.lock()
				.expect("lock")
				.fixed_amount(amount_msat, self.config.lsps2_max_total_lsp_fee_limit_msat)
			else {
				return Ok(None);
			};
			let lease = self.consume_selected_lease(lease).await?;
			if let Some(lsp) =
				select_lsps_for_protocol(&self.lsp_nodes, 2, Some(&lease.id.lsp_node_id))
			{
				return Ok(Some((lease, fee_msat, lsp)));
			}
		}
	}

	async fn take_cached_variable_lease(
		&self,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		loop {
			let Some((lease, proportional_fee)) = self
				.lease_state
				.lock()
				.expect("lock")
				.variable_amount(self.config.lsps2_max_total_lsp_fee_limit_msat)
			else {
				return Ok(None);
			};
			let lease = self.consume_selected_lease(lease).await?;
			if let Some(lsp) =
				select_lsps_for_protocol(&self.lsp_nodes, 2, Some(&lease.id.lsp_node_id))
			{
				return Ok(Some((lease, proportional_fee, lsp)));
			}
		}
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
		let encoded_payment_metadata = PaymentMetadata {
			lsps2_parameters: Some(lsps2_parameters),
			lsps2_lease_parameters: None,
		}
		.encode();
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
			_ => {
				log_error!(self.logger, "Received unexpected LSPS2Client liquidity event!");
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

impl From<&PaymentLease> for LSPS2BuyResponse {
	fn from(lease: &PaymentLease) -> Self {
		Self { intercept_scid: lease.id.intercept_scid, cltv_expiry_delta: lease.cltv_expiry_delta }
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn failed_persisted_removal_does_not_consume() {
		let consumed = Arc::new(Mutex::new(false));
		let consumed_ref = Arc::clone(&consumed);
		let result = consume_after_persisted_removal(
			42,
			|_| async { Err(()) },
			move |_| *consumed_ref.lock().unwrap() = true,
		)
		.await;

		assert_eq!(result, Err(()));
		assert!(!*consumed.lock().unwrap());
	}
}

pub(crate) mod router;
pub(crate) mod state;
