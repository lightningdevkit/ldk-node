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
use lightning::blinded_path::message::OffersContext;
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::log_warn;
use lightning::offers::invoice_request::InvoiceRequest;
use lightning::offers::offer::OfferId;
use lightning::onion_message::messenger::Responder;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, InvoiceBuilder, RoutingFees};
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::router::PaymentParameters;
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
	now_secs, LSPS2LeaseState, LeaseRequestKey, PaymentLease, PaymentLeaseId, PaymentLeaseStore,
	PendingInvoiceRequestState, PendingLeaseRequestState, PendingOffer, PendingOfferAmount,
	PendingOfferId, PendingOfferState, PendingOfferStore,
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

const DEFAULT_BOLT12_INVOICE_EXPIRY_SECS: u32 = 2 * 60 * 60;

#[derive(Debug, PartialEq, Eq)]
enum JitInvoiceRequest {
	Fixed { amount_msat: u64, max_total_fee_msat: Option<u64> },
	Variable { amount_msat: u64, max_proportional_fee_ppm_msat: Option<u64> },
}

impl JitInvoiceRequest {
	fn allow_mpp(&self) -> bool {
		matches!(self, Self::Fixed { .. })
	}
}

fn jit_invoice_request(
	pending_offer: &PendingOffer, invoice_request: &InvoiceRequest,
) -> Result<JitInvoiceRequest, Error> {
	let amount_msat = invoice_request.amount_msats().ok_or(Error::InvalidAmount)?;
	match pending_offer.amount {
		PendingOfferAmount::Fixed { amount_msat: offer_amount_msat, max_total_fee_msat } => {
			if amount_msat < offer_amount_msat {
				return Err(Error::InvalidAmount);
			}
			Ok(JitInvoiceRequest::Fixed { amount_msat, max_total_fee_msat })
		},
		PendingOfferAmount::Variable { max_proportional_fee_ppm_msat } => {
			Ok(JitInvoiceRequest::Variable { amount_msat, max_proportional_fee_ppm_msat })
		},
	}
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
	pub(crate) pending_offer_store: Arc<PendingOfferStore<L>>,
	pub(crate) pending_offer_state: Mutex<PendingOfferState>,
	pub(crate) pending_invoice_request_state: Mutex<PendingInvoiceRequestState>,
	pub(crate) pending_lease_request_state: Mutex<PendingLeaseRequestState>,
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

	pub(crate) async fn prune_stale_pending_offers(&self) -> Result<(), Error> {
		let stale_ids = self.pending_offer_state.lock().expect("lock").prune(now_secs());
		for id in stale_ids {
			self.pending_offer_store.remove(&id).await?;
		}
		Ok(())
	}

	pub(crate) async fn register_pending_offer(&self, offer: PendingOffer) -> Result<(), Error> {
		self.pending_offer_store.insert_or_update(offer.clone()).await?;
		let evicted = self.pending_offer_state.lock().expect("lock").register(offer);
		if let Some(id) = evicted {
			self.pending_offer_store.remove(&id).await?;
		}
		Ok(())
	}

	pub(crate) async fn pending_offer(
		&self, offer_id: PendingOfferId,
	) -> Result<Option<PendingOffer>, Error> {
		let offer = self.pending_offer_state.lock().expect("lock").get(&offer_id, now_secs());
		if let Some(offer) = offer {
			self.pending_offer_store.insert_or_update(offer.clone()).await?;
			Ok(Some(offer))
		} else {
			self.pending_offer_store.remove(&offer_id).await?;
			Ok(None)
		}
	}

	pub(crate) async fn respond_to_invoice_request(
		self: Arc<Self>, offer_id: OfferId, invoice_request: InvoiceRequest,
		context: Option<OffersContext>, responder: Responder,
		connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		let pending_offer_id = offer_id.into();
		let is_jit_offer =
			self.pending_offer_state.lock().expect("lock").contains(&pending_offer_id);
		if !is_jit_offer {
			let payment_metadata = match context.as_ref() {
				Some(OffersContext::InvoiceRequest { payment_metadata, .. }) => {
					payment_metadata.clone()
				},
				_ => None,
			};
			return self
				.channel_manager
				.respond_to_invoice_request(
					invoice_request,
					context,
					responder,
					payment_metadata,
					DEFAULT_BOLT12_INVOICE_EXPIRY_SECS,
					true,
				)
				.map_err(|error| {
					log_error!(
						self.logger,
						"Failed responding to BOLT12 invoice request: {}",
						error
					);
					Error::InvoiceCreationFailed
				});
		}

		let request_lock =
			self.pending_invoice_request_state.lock().expect("lock").request_lock(pending_offer_id);
		let result = {
			let _guard = request_lock.lock().await;
			self.respond_to_jit_invoice_request(
				pending_offer_id,
				invoice_request,
				context,
				responder,
				&connection_manager,
			)
			.await
		};
		drop(request_lock);
		self.pending_invoice_request_state.lock().expect("lock").prune();
		result
	}

	async fn respond_to_jit_invoice_request(
		self: &Arc<Self>, pending_offer_id: PendingOfferId, invoice_request: InvoiceRequest,
		context: Option<OffersContext>, responder: Responder,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		let pending_offer =
			self.pending_offer(pending_offer_id).await?.ok_or(Error::InvalidOffer)?;
		let request = jit_invoice_request(&pending_offer, &invoice_request)?;
		let allow_mpp = request.allow_mpp();
		let (lease, fee_parameters) = match request {
			JitInvoiceRequest::Fixed { amount_msat, max_total_fee_msat } => {
				let (lease, total_fee_msat, _, _) = self
					.acquire_fixed_lease(amount_msat, max_total_fee_msat, connection_manager)
					.await?;
				(
					lease,
					LSPS2Parameters {
						max_total_opening_fee_msat: Some(total_fee_msat),
						max_proportional_opening_fee_ppm_msat: None,
					},
				)
			},
			JitInvoiceRequest::Variable { amount_msat: _, max_proportional_fee_ppm_msat } => {
				let (lease, proportional_fee, _, _) = self
					.acquire_variable_lease(max_proportional_fee_ppm_msat, connection_manager)
					.await?;
				(
					lease,
					LSPS2Parameters {
						max_total_opening_fee_msat: None,
						max_proportional_opening_fee_ppm_msat: Some(proportional_fee),
					},
				)
			},
		};

		let remaining_validity_secs = lease
			.valid_until
			.checked_sub(now_secs())
			.filter(|remaining| *remaining > 0)
			.ok_or(Error::LiquidityRequestFailed)?;
		let relative_expiry_secs = DEFAULT_BOLT12_INVOICE_EXPIRY_SECS
			.min(remaining_validity_secs.try_into().unwrap_or(u32::MAX));
		let lease_parameters = PaymentParameters {
			lsp_node_id: lease.id.lsp_node_id,
			intercept_scid: lease.id.intercept_scid,
			cltv_expiry_delta: lease
				.cltv_expiry_delta
				.try_into()
				.map_err(|_| Error::LiquidityRequestFailed)?,
			payment_size_msat: lease.payment_size_msat,
			valid_until: lease.valid_until,
		};
		let payment_metadata = PaymentMetadata {
			lsps2_parameters: Some(fee_parameters),
			lsps2_lease_parameters: Some(lease_parameters),
		}
		.encode_as_bolt12_payment_metadata();

		self.channel_manager
			.respond_to_invoice_request(
				invoice_request,
				context,
				responder,
				Some(payment_metadata),
				relative_expiry_secs,
				allow_mpp,
			)
			.map_err(|error| {
				log_error!(self.logger, "Failed responding to LSPS2 invoice request: {}", error);
				Error::InvoiceCreationFailed
			})
	}

	pub(crate) async fn lsps2_receive_to_jit_channel(
		self: Arc<Self>, amount_msat: u64, description: &Bolt11InvoiceDescription,
		expiry_secs: u32, max_total_lsp_fee_limit_msat: Option<u64>,
		payment_hash: Option<PaymentHash>, connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		let (lease, total_fee_msat, lsp, was_negotiated) = self
			.acquire_fixed_lease(amount_msat, max_total_lsp_fee_limit_msat, &connection_manager)
			.await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(total_fee_msat),
			max_proportional_opening_fee_ppm_msat: None,
		};

		let invoice = self.lsps2_create_jit_invoice(
			LSPS2BuyResponse::from(&lease),
			Some(amount_msat),
			description,
			expiry_secs,
			payment_hash,
			lsps2_parameters,
			Some(&lsp.node_id),
		)?;

		if was_negotiated {
			log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		}
		Ok((invoice, lsp))
	}

	pub(crate) async fn lsps2_receive_variable_amount_to_jit_channel(
		self: Arc<Self>, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>, payment_hash: Option<PaymentHash>,
		connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		let (lease, proportional_fee, lsp, was_negotiated) = self
			.acquire_variable_lease(max_proportional_lsp_fee_limit_ppm_msat, &connection_manager)
			.await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: None,
			max_proportional_opening_fee_ppm_msat: Some(proportional_fee),
		};
		let invoice = self.lsps2_create_jit_invoice(
			LSPS2BuyResponse::from(&lease),
			None,
			description,
			expiry_secs,
			payment_hash,
			lsps2_parameters,
			Some(&lsp.node_id),
		)?;

		if was_negotiated {
			log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		}
		Ok((invoice, lsp))
	}

	async fn acquire_fixed_lease(
		self: &Arc<Self>, amount_msat: u64, max_total_lsp_fee_limit_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig, bool), Error> {
		if let Some((lease, total_fee_msat, lsp)) =
			self.take_cached_fixed_lease(amount_msat, max_total_lsp_fee_limit_msat).await?
		{
			self.schedule_fixed_lease_refill(
				amount_msat,
				max_total_lsp_fee_limit_msat,
				connection_manager,
			);
			return Ok((lease, total_fee_msat, lsp, false));
		}
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseRequestKey::Fixed(amount_msat));
		let _request_guard = request_lock.lock().await;
		if let Some((lease, total_fee_msat, lsp)) =
			self.take_cached_fixed_lease(amount_msat, max_total_lsp_fee_limit_msat).await?
		{
			self.schedule_fixed_lease_refill(
				amount_msat,
				max_total_lsp_fee_limit_msat,
				connection_manager,
			);
			return Ok((lease, total_fee_msat, lsp, false));
		}

		let (negotiated_lease, min_total_fee_msat, cheapest_lsp) = self
			.negotiate_fixed_lease(amount_msat, max_total_lsp_fee_limit_msat, connection_manager)
			.await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		self.schedule_fixed_lease_refill(
			amount_msat,
			max_total_lsp_fee_limit_msat,
			connection_manager,
		);
		Ok((lease, min_total_fee_msat, cheapest_lsp, true))
	}

	async fn negotiate_fixed_lease(
		self: &Arc<Self>, amount_msat: u64, max_total_lsp_fee_limit_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig), Error> {
		let all_offers = self.gather_lsps2_offers(connection_manager).await?;
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

		let negotiated_lease = self
			.lsps2_send_buy_request(
				Some(amount_msat),
				min_opening_params,
				Some(&cheapest_lsp.node_id),
			)
			.await?;
		Ok((negotiated_lease, min_total_fee_msat, cheapest_lsp))
	}

	async fn acquire_variable_lease(
		self: &Arc<Self>, max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig, bool), Error> {
		if let Some((lease, proportional_fee, lsp)) =
			self.take_cached_variable_lease(max_proportional_lsp_fee_limit_ppm_msat).await?
		{
			self.schedule_variable_lease_refill(
				max_proportional_lsp_fee_limit_ppm_msat,
				connection_manager,
			);
			return Ok((lease, proportional_fee, lsp, false));
		}
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseRequestKey::Variable);
		let _request_guard = request_lock.lock().await;
		if let Some((lease, proportional_fee, lsp)) =
			self.take_cached_variable_lease(max_proportional_lsp_fee_limit_ppm_msat).await?
		{
			self.schedule_variable_lease_refill(
				max_proportional_lsp_fee_limit_ppm_msat,
				connection_manager,
			);
			return Ok((lease, proportional_fee, lsp, false));
		}

		let (negotiated_lease, min_prop_fee_ppm_msat, cheapest_lsp) = self
			.negotiate_variable_lease(max_proportional_lsp_fee_limit_ppm_msat, connection_manager)
			.await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		self.schedule_variable_lease_refill(
			max_proportional_lsp_fee_limit_ppm_msat,
			connection_manager,
		);
		Ok((lease, min_prop_fee_ppm_msat, cheapest_lsp, true))
	}

	async fn negotiate_variable_lease(
		self: &Arc<Self>, max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig), Error> {
		let all_offers = self.gather_lsps2_offers(connection_manager).await?;
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

		let negotiated_lease = self
			.lsps2_send_buy_request(None, min_opening_params, Some(&cheapest_lsp.node_id))
			.await?;
		Ok((negotiated_lease, min_prop_fee_ppm_msat, cheapest_lsp))
	}

	fn schedule_fixed_lease_refill(
		self: &Arc<Self>, amount_msat: u64, max_total_lsp_fee_limit_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) {
		let client = Arc::clone(self);
		let connection_manager = Arc::clone(connection_manager);
		tokio::spawn(async move {
			if let Err(error) = client
				.cache_fixed_lease(amount_msat, max_total_lsp_fee_limit_msat, &connection_manager)
				.await
			{
				log_warn!(client.logger, "Failed refilling LSPS2 payment lease: {}", error);
			}
		});
	}

	fn schedule_variable_lease_refill(
		self: &Arc<Self>, max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) {
		let client = Arc::clone(self);
		let connection_manager = Arc::clone(connection_manager);
		tokio::spawn(async move {
			if let Err(error) = client
				.cache_variable_lease(max_proportional_lsp_fee_limit_ppm_msat, &connection_manager)
				.await
			{
				log_warn!(client.logger, "Failed refilling LSPS2 payment lease: {}", error);
			}
		});
	}

	async fn cache_fixed_lease(
		self: &Arc<Self>, amount_msat: u64, max_total_lsp_fee_limit_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseRequestKey::Fixed(amount_msat));
		let _request_guard = request_lock.lock().await;
		if self
			.lease_state
			.lock()
			.expect("lock")
			.has_fixed_amount(amount_msat, max_total_lsp_fee_limit_msat)
		{
			return Ok(());
		}
		self.negotiate_fixed_lease(amount_msat, max_total_lsp_fee_limit_msat, connection_manager)
			.await?;
		Ok(())
	}

	async fn cache_variable_lease(
		self: &Arc<Self>, max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseRequestKey::Variable);
		let _request_guard = request_lock.lock().await;
		if self
			.lease_state
			.lock()
			.expect("lock")
			.has_variable_amount(max_proportional_lsp_fee_limit_ppm_msat)
		{
			return Ok(());
		}
		self.negotiate_variable_lease(max_proportional_lsp_fee_limit_ppm_msat, connection_manager)
			.await?;
		Ok(())
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
		&self, amount_msat: u64, max_fee_msat: Option<u64>,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		loop {
			let Some((lease, fee_msat)) =
				self.lease_state.lock().expect("lock").fixed_amount(amount_msat, max_fee_msat)
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
		&self, max_proportional_fee_ppm_msat: Option<u64>,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		loop {
			let Some((lease, proportional_fee)) = self
				.lease_state
				.lock()
				.expect("lock")
				.variable_amount(max_proportional_fee_ppm_msat)
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
	use std::num::NonZeroU64;

	use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
	use lightning::ln::channelmanager::PaymentId;
	use lightning::ln::inbound_payment::ExpandedKey;
	use lightning::offers::nonce::Nonce;
	use lightning::offers::offer::{OfferBuilder, Quantity};
	use lightning::sign::EntropySource;

	use super::*;
	use crate::liquidity::client::lsps2::state::PendingOfferAmount;

	struct FixedEntropy;

	impl EntropySource for FixedEntropy {
		fn get_secure_random_bytes(&self) -> [u8; 32] {
			[42; 32]
		}
	}

	fn recipient_pubkey() -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[43; 32]).unwrap())
	}

	#[test]
	fn fixed_offer_request_uses_quantity_resolved_amount() {
		let expanded_key = ExpandedKey::new([44; 32]);
		let nonce = Nonce::from_entropy_source(&FixedEntropy);
		let secp_ctx = Secp256k1::new();
		let offer = OfferBuilder::new(recipient_pubkey())
			.amount_msats(1_000)
			.supported_quantity(Quantity::Bounded(NonZeroU64::new(10).unwrap()))
			.build()
			.unwrap();
		let invoice_request = offer
			.request_invoice(&expanded_key, nonce, &secp_ctx, PaymentId([45; 32]))
			.unwrap()
			.quantity(3)
			.unwrap()
			.build_and_sign()
			.unwrap();
		let pending_offer = PendingOffer::new(
			offer.id(),
			PendingOfferAmount::Fixed { amount_msat: 1_000, max_total_fee_msat: Some(250) },
			None,
		);

		assert_eq!(
			jit_invoice_request(&pending_offer, &invoice_request).unwrap(),
			JitInvoiceRequest::Fixed { amount_msat: 3_000, max_total_fee_msat: Some(250) }
		);
	}

	#[test]
	fn variable_offer_request_disables_mpp() {
		let expanded_key = ExpandedKey::new([46; 32]);
		let nonce = Nonce::from_entropy_source(&FixedEntropy);
		let secp_ctx = Secp256k1::new();
		let offer = OfferBuilder::new(recipient_pubkey()).build().unwrap();
		let invoice_request = offer
			.request_invoice(&expanded_key, nonce, &secp_ctx, PaymentId([47; 32]))
			.unwrap()
			.amount_msats(2_500)
			.unwrap()
			.build_and_sign()
			.unwrap();
		let pending_offer = PendingOffer::new(
			offer.id(),
			PendingOfferAmount::Variable { max_proportional_fee_ppm_msat: Some(10_000) },
			None,
		);

		assert_eq!(
			jit_invoice_request(&pending_offer, &invoice_request).unwrap(),
			JitInvoiceRequest::Variable {
				amount_msat: 2_500,
				max_proportional_fee_ppm_msat: Some(10_000),
			}
		);
		assert!(!jit_invoice_request(&pending_offer, &invoice_request).unwrap().allow_mpp());
	}

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

pub(crate) mod state;
