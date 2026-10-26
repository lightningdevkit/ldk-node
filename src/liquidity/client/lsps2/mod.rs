// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! The client side of the LSPS2 just-in-time channel protocol.
//!
//! Receiving over a just-in-time channel requires negotiating a payment lease with an LSP, i.e.,
//! obtaining an intercept SCID for accepted opening fee parameters. See the [`state`] module for
//! how leases are tracked.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock, Weak};
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
	select_all_lsps_for_protocol, select_lsps_for_protocol, LspConfig, LspNode, PendingRequest,
	PendingRequestGuard, LIQUIDITY_REQUEST_TIMEOUT_SECS, LSPS_DISCOVERY_WAIT_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::payment::store::LSPS2Parameters;
use crate::payment::PaymentMetadata;
use crate::runtime::Runtime;
use crate::types::{ChannelManager, KeysManager, LiquidityManager};
use crate::{Config, Error};

use self::router::LSPS2LeaseParameters;
use self::state::{
	now_secs, LSPS2LeaseState, LeaseCacheTarget, LeaseCacheTargetId, LeaseCacheTargetStore,
	PaymentLease, PaymentLeaseId, PaymentLeaseStore, PendingLeaseRequestState,
};

// LSPS2 requires two blocks more than the usual final CLTV delta.
const LSPS2_MIN_FINAL_CLTV_EXPIRY_DELTA: u16 = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;

async fn consume_after_persisted_removal<T, E, RF, CF, Fut>(
	value: T, persist_removal: RF, restore: CF,
) -> Result<T, E>
where
	T: Clone,
	RF: FnOnce(T) -> Fut,
	CF: FnOnce(T),
	Fut: Future<Output = Result<(), E>>,
{
	match persist_removal(value.clone()).await {
		Ok(()) => Ok(value),
		Err(error) => {
			restore(value);
			Err(error)
		},
	}
}

const DEFAULT_BOLT12_INVOICE_EXPIRY_SECS: u32 = 2 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JitInvoiceRequest {
	Fixed { amount_msat: u64, absolute_expiry: Option<u64> },
	Variable { amount_msat: u64, absolute_expiry: Option<u64> },
}

impl JitInvoiceRequest {
	pub(crate) fn allow_mpp(&self) -> bool {
		matches!(self, Self::Fixed { .. })
	}

	fn cache_target(&self) -> (LeaseCacheTargetId, Option<u64>) {
		match *self {
			Self::Fixed { amount_msat, absolute_expiry } => {
				(LeaseCacheTargetId::Fixed { amount_msat }, absolute_expiry)
			},
			Self::Variable { absolute_expiry, .. } => {
				(LeaseCacheTargetId::Variable, absolute_expiry)
			},
		}
	}
}

pub(crate) struct LSPS2Client<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	pub(crate) pending_lsps2_fee_requests:
		Mutex<HashMap<LSPSRequestId, PendingRequest<LSPS2FeeResponse>>>,
	pub(crate) pending_buy_requests:
		Mutex<HashMap<LSPSRequestId, PendingRequest<LSPS2BuyResponse>>>,
	pub(crate) lease_store: Arc<PaymentLeaseStore<L>>,
	pub(crate) lease_state: Mutex<LSPS2LeaseState>,
	pub(crate) cache_target_store: Arc<LeaseCacheTargetStore<L>>,
	pub(crate) pending_lease_request_state: Mutex<PendingLeaseRequestState>,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) keys_manager: Arc<KeysManager>,
	pub(crate) discovery_done_rx: tokio::sync::watch::Receiver<bool>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	// Refill tasks are runtime-owned; keeping this weak avoids extending the runtime's lifetime.
	pub(crate) runtime: Weak<Runtime>,
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
			.await
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
		let removed_target_ids = self.cache_target_store.prune(now_secs()).await?;
		self.drop_cached_leases(&removed_target_ids).await
	}

	/// Drops the cached leases of targets we no longer track, as otherwise every lease negotiated
	/// for a since-removed target would remain stored until it expires.
	async fn drop_cached_leases(&self, target_ids: &[LeaseCacheTargetId]) -> Result<(), Error> {
		if target_ids.is_empty() {
			return Ok(());
		}
		let lease_ids = self.lease_state.lock().expect("lock").lease_ids_for_targets(target_ids);
		for id in lease_ids {
			self.lease_store.remove(&id).await?;
			self.lease_state.lock().expect("lock").remove(&id);
		}
		Ok(())
	}

	pub(crate) async fn register_cache_target(
		&self, id: LeaseCacheTargetId, absolute_expiry: Option<u64>,
	) -> Result<(), Error> {
		// Without an LSPS2 liquidity source, no lease could ever be negotiated for the target.
		if !self.may_have_lsps2_nodes() {
			return Ok(());
		}
		let target = LeaseCacheTarget::new(id, absolute_expiry);
		if let Some(evicted_id) = self.cache_target_store.register(target).await? {
			self.drop_cached_leases(&[evicted_id]).await?;
		}
		Ok(())
	}

	/// Returns whether any configured LSP supports LSPS2, or may support it once protocol
	/// discovery completes.
	fn may_have_lsps2_nodes(&self) -> bool {
		self.lsp_nodes.read().expect("lock").iter().any(|lsp_node| {
			lsp_node.supported_protocols.as_ref().map_or(true, |protocols| protocols.contains(&2))
		})
	}

	pub(crate) fn cache_targets(&self) -> Vec<LeaseCacheTarget> {
		self.cache_target_store.targets()
	}

	/// Records a cache target and starts negotiating a matching lease in the background, so that
	/// the first payer does not have to wait for the negotiation.
	pub(crate) async fn register_cache_target_and_refill(
		self: &Arc<Self>, id: LeaseCacheTargetId, absolute_expiry: Option<u64>,
		connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		if !self.may_have_lsps2_nodes() {
			return Ok(());
		}
		let res = self.register_cache_target(id, absolute_expiry).await;
		self.schedule_lease_refill(id, connection_manager);
		res
	}

	pub(crate) async fn commit_invoice_cache_target(
		self: &Arc<Self>, request: JitInvoiceRequest,
		connection_manager: &Arc<ConnectionManager<L>>,
	) {
		// Cache targets are only an optimization. A persistence failure must not make the current
		// invoice request unusable.
		let (target_id, absolute_expiry) = request.cache_target();
		if let Err(error) = self.register_cache_target(target_id, absolute_expiry).await {
			log_warn!(self.logger, "Failed recording LSPS2 lease cache target: {}", error);
		}
		self.schedule_lease_refill(target_id, connection_manager);
	}

	pub(crate) async fn prepare_invoice_response(
		self: Arc<Self>, request: JitInvoiceRequest, connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
		let (lease, fee_parameters) = match request {
			JitInvoiceRequest::Fixed { amount_msat, .. } => {
				let (lease, total_fee_msat, _, _) =
					self.acquire_fixed_lease(amount_msat, &connection_manager).await?;
				let min_opening_fee_msat = lease.params.min_fee_msat;
				(
					lease,
					LSPS2Parameters {
						max_total_opening_fee_msat: Some(total_fee_msat),
						max_proportional_opening_fee_ppm_msat: None,
						min_opening_fee_msat,
					},
				)
			},
			JitInvoiceRequest::Variable { .. } => {
				let (lease, proportional_fee, _, _) =
					self.acquire_variable_lease(&connection_manager).await?;
				let min_opening_fee_msat = lease.params.min_fee_msat;
				(
					lease,
					LSPS2Parameters {
						max_total_opening_fee_msat: self.config.lsps2_max_total_lsp_fee_limit_msat,
						max_proportional_opening_fee_ppm_msat: self
							.config
							.lsps2_max_total_lsp_fee_limit_msat
							.is_none()
							.then_some(proportional_fee),
						min_opening_fee_msat,
					},
				)
			},
		};

		// The offers flow currently uses its two-hour default invoice expiry. Never publish a JIT
		// path whose intercept SCID may expire before the invoice does.
		if lease.valid_until.saturating_sub(now_secs()) < DEFAULT_BOLT12_INVOICE_EXPIRY_SECS as u64
		{
			return Err(Error::LiquidityRequestFailed);
		}
		let lease_parameters = LSPS2LeaseParameters {
			lsp_node_id: lease.id.lsp_node_id,
			intercept_scid: lease.id.intercept_scid,
			cltv_expiry_delta: state::checked_cltv_expiry_delta(lease.cltv_expiry_delta)
				.expect("LSPS2 lease CLTV delta was validated before selection"),
			payment_size_msat: lease.payment_size_msat,
			valid_until: lease.valid_until,
		};
		let payment_metadata = PaymentMetadata {
			lsps2_parameters: Some(fee_parameters),
			lsps2_lease_parameters: Some(lease_parameters),
		}
		.encode_as_bolt12_payment_metadata();

		Ok(payment_metadata)
	}

	pub(crate) async fn lsps2_receive_to_jit_channel(
		self: Arc<Self>, amount_msat: u64, description: &Bolt11InvoiceDescription,
		expiry_secs: u32, payment_hash: Option<PaymentHash>,
		connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		let (lease, total_fee_msat, lsp, was_negotiated) =
			self.acquire_fixed_lease(amount_msat, &connection_manager).await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(total_fee_msat),
			max_proportional_opening_fee_ppm_msat: None,
			min_opening_fee_msat: lease.params.min_fee_msat,
		};

		let invoice = self.lsps2_create_jit_invoice(
			&lease,
			description,
			expiry_secs,
			payment_hash,
			lsps2_parameters,
		)?;
		self.schedule_lease_refill(LeaseCacheTargetId::Fixed { amount_msat }, &connection_manager);

		if was_negotiated {
			log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		}
		Ok((invoice, lsp))
	}

	pub(crate) async fn lsps2_receive_variable_amount_to_jit_channel(
		self: Arc<Self>, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		payment_hash: Option<PaymentHash>, connection_manager: Arc<ConnectionManager<L>>,
	) -> Result<(Bolt11Invoice, LspConfig), Error> {
		let (lease, proportional_fee, lsp, was_negotiated) =
			self.acquire_variable_lease(&connection_manager).await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: self.config.lsps2_max_total_lsp_fee_limit_msat,
			max_proportional_opening_fee_ppm_msat: Some(proportional_fee),
			min_opening_fee_msat: lease.params.min_fee_msat,
		};
		let invoice = self.lsps2_create_jit_invoice(
			&lease,
			description,
			expiry_secs,
			payment_hash,
			lsps2_parameters,
		)?;
		self.schedule_lease_refill(LeaseCacheTargetId::Variable, &connection_manager);

		if was_negotiated {
			log_info!(self.logger, "JIT-channel invoice created: {}", invoice);
		}
		Ok((invoice, lsp))
	}

	async fn acquire_fixed_lease(
		self: &Arc<Self>, amount_msat: u64, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig, bool), Error> {
		if let Some((lease, total_fee_msat, lsp)) =
			self.take_cached_fixed_lease(amount_msat).await?
		{
			return Ok((lease, total_fee_msat, lsp, false));
		}
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseCacheTargetId::Fixed { amount_msat });
		let _request_guard = request_lock.lock().await;
		if let Some((lease, total_fee_msat, lsp)) =
			self.take_cached_fixed_lease(amount_msat).await?
		{
			return Ok((lease, total_fee_msat, lsp, false));
		}

		let (negotiated_lease, min_total_fee_msat, cheapest_lsp) =
			self.negotiate_fixed_lease(amount_msat, connection_manager).await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		Ok((lease, min_total_fee_msat, cheapest_lsp, true))
	}

	async fn negotiate_fixed_lease(
		self: &Arc<Self>, amount_msat: u64, connection_manager: &Arc<ConnectionManager<L>>,
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

		let max_total_lsp_fee_limit_msat = self.max_fixed_amount_fee_msat(amount_msat);
		if min_total_fee_msat > max_total_lsp_fee_limit_msat {
			log_error!(self.logger,
				"Failed to request inbound JIT channel as LSP's requested total opening fee of {}msat exceeds our fee limit of {}msat",
				min_total_fee_msat, max_total_lsp_fee_limit_msat
			);
			return Err(Error::LiquidityFeeTooHigh);
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
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig, bool), Error> {
		if let Some((lease, proportional_fee, lsp)) = self.take_cached_variable_lease().await? {
			return Ok((lease, proportional_fee, lsp, false));
		}
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseCacheTargetId::Variable);
		let _request_guard = request_lock.lock().await;
		if let Some((lease, proportional_fee, lsp)) = self.take_cached_variable_lease().await? {
			return Ok((lease, proportional_fee, lsp, false));
		}

		let (negotiated_lease, min_prop_fee_ppm_msat, cheapest_lsp) =
			self.negotiate_variable_lease(connection_manager).await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		Ok((lease, min_prop_fee_ppm_msat, cheapest_lsp, true))
	}

	async fn negotiate_variable_lease(
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig), Error> {
		let all_offers = self.gather_lsps2_offers(connection_manager).await?;
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
		Ok((negotiated_lease, min_prop_fee_ppm_msat, cheapest_lsp))
	}

	fn schedule_lease_refill(
		self: &Arc<Self>, target_id: LeaseCacheTargetId,
		connection_manager: &Arc<ConnectionManager<L>>,
	) {
		let Some(runtime) = self.runtime.upgrade() else { return };
		let client = Arc::clone(self);
		let connection_manager = Arc::clone(connection_manager);
		runtime.spawn_cancellable_background_task(async move {
			let result = match target_id {
				LeaseCacheTargetId::Fixed { amount_msat } => {
					client.cache_fixed_lease(amount_msat, &connection_manager).await
				},
				LeaseCacheTargetId::Variable => {
					client.cache_variable_lease(&connection_manager).await
				},
			};
			if let Err(error) = result {
				log_warn!(client.logger, "Failed refilling LSPS2 payment lease: {}", error);
			}
		});
	}

	async fn cache_fixed_lease(
		self: &Arc<Self>, amount_msat: u64, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseCacheTargetId::Fixed { amount_msat });
		let _request_guard = request_lock.lock().await;
		let available_lsps =
			self.get_lsps2_nodes().await?.into_iter().map(|lsp| lsp.node_id).collect::<Vec<_>>();
		if self.lease_state.lock().expect("lock").has_fixed_amount(
			amount_msat,
			self.config.lsps2_max_total_lsp_fee_limit_msat,
			&available_lsps,
		) {
			return Ok(());
		}
		self.negotiate_fixed_lease(amount_msat, connection_manager).await?;
		Ok(())
	}

	async fn cache_variable_lease(
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseCacheTargetId::Variable);
		let _request_guard = request_lock.lock().await;
		let available_lsps =
			self.get_lsps2_nodes().await?.into_iter().map(|lsp| lsp.node_id).collect::<Vec<_>>();
		if self
			.lease_state
			.lock()
			.expect("lock")
			.has_variable_amount(self.config.lsps2_max_total_lsp_fee_limit_msat, &available_lsps)
		{
			return Ok(());
		}
		self.negotiate_variable_lease(connection_manager).await?;
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
					log_warn!(self.logger, "Failed to get fees from LSP {}: {}", lsp.node_id, e);
				},
				Err(e) => {
					log_warn!(self.logger, "Failed to get fees from LSP: {}", e);
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
		let _pending_request = {
			let mut pending_fee_requests_lock =
				self.pending_lsps2_fee_requests.lock().expect("lock");
			let request_id =
				client_handler.request_opening_params(lsps2_node.node_id, lsps2_node.token.clone());
			PendingRequestGuard::insert(
				&self.pending_lsps2_fee_requests,
				&mut pending_fee_requests_lock,
				request_id,
				fee_request_sender,
			)
		};

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
		let _pending_request = {
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
			PendingRequestGuard::insert(
				&self.pending_buy_requests,
				&mut pending_buy_requests_lock,
				request_id,
				buy_request_sender,
			)
		};

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
		let cltv_expiry_delta = state::checked_cltv_expiry_delta(buy_response.cltv_expiry_delta)
			.ok_or_else(|| {
				log_error!(
					self.logger,
					"LSP {} returned an unsupported CLTV expiry delta of {}",
					lsps2_node.node_id,
					buy_response.cltv_expiry_delta
				);
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
			cltv_expiry_delta: cltv_expiry_delta.into(),
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
			.take_valid(id)
			.ok_or(Error::LiquidityRequestFailed)?;
		self.consume_selected_lease(lease).await
	}

	async fn consume_selected_lease(&self, lease: PaymentLease) -> Result<PaymentLease, Error> {
		let lease_store = Arc::clone(&self.lease_store);
		// Selection has already removed the lease from the shared state so no other payment can
		// reuse its intercept SCID. Restore it only if the durable removal fails, before an invoice
		// containing the lease can be returned.
		consume_after_persisted_removal(
			lease,
			move |lease| async move { lease_store.remove(&lease.id).await },
			|lease| {
				self.lease_state.lock().expect("lock").insert(lease);
			},
		)
		.await
	}

	/// Fixed-amount opening fees are bounded by the payment amount even without a configured limit.
	fn max_fixed_amount_fee_msat(&self, amount_msat: u64) -> u64 {
		self.config.lsps2_max_total_lsp_fee_limit_msat.unwrap_or(amount_msat).min(amount_msat)
	}

	async fn take_cached_fixed_lease(
		&self, amount_msat: u64,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;
		let available_lsps = lsps2_nodes.iter().map(|lsp| lsp.node_id).collect::<Vec<_>>();
		let Some((lease, fee_msat)) = self.lease_state.lock().expect("lock").fixed_amount(
			amount_msat,
			Some(self.max_fixed_amount_fee_msat(amount_msat)),
			&available_lsps,
		) else {
			return Ok(None);
		};
		let lsp = lsps2_nodes
			.into_iter()
			.find(|lsp| lsp.node_id == lease.id.lsp_node_id)
			.expect("lease was selected from available LSPs");
		let lease = self.consume_selected_lease(lease).await?;
		Ok(Some((lease, fee_msat, lsp)))
	}

	async fn take_cached_variable_lease(
		&self,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;
		let available_lsps = lsps2_nodes.iter().map(|lsp| lsp.node_id).collect::<Vec<_>>();
		let Some((lease, proportional_fee)) = self
			.lease_state
			.lock()
			.expect("lock")
			.variable_amount(self.config.lsps2_max_total_lsp_fee_limit_msat, &available_lsps)
		else {
			return Ok(None);
		};
		let lsp = lsps2_nodes
			.into_iter()
			.find(|lsp| lsp.node_id == lease.id.lsp_node_id)
			.expect("lease was selected from available LSPs");
		let lease = self.consume_selected_lease(lease).await?;
		Ok(Some((lease, proportional_fee, lsp)))
	}

	fn lsps2_create_jit_invoice(
		&self, lease: &PaymentLease, description: &Bolt11InvoiceDescription, expiry_secs: u32,
		payment_hash: Option<PaymentHash>, lsps2_parameters: LSPS2Parameters,
	) -> Result<Bolt11Invoice, Error> {
		let lsps2_node = select_lsps_for_protocol(&self.lsp_nodes, 2, Some(&lease.id.lsp_node_id))
			.ok_or(Error::LiquiditySourceUnavailable)?;

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
						lease.payment_size_msat,
						expiry_secs,
						Some(LSPS2_MIN_FINAL_CLTV_EXPIRY_DELTA),
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
					lease.payment_size_msat,
					expiry_secs,
					Some(LSPS2_MIN_FINAL_CLTV_EXPIRY_DELTA),
					Some(encoded_payment_metadata),
				)
				.map_err(|e| {
					log_error!(self.logger, "Failed to register inbound payment: {:?}", e);
					Error::InvoiceCreationFailed
				})?,
		};

		let route_hint = RouteHint(vec![RouteHintHop {
			src_node_id: lsps2_node.node_id,
			short_channel_id: lease.id.intercept_scid,
			fees: RoutingFees { base_msat: 0, proportional_millionths: 0 },
			cltv_expiry_delta: state::checked_cltv_expiry_delta(lease.cltv_expiry_delta)
				.expect("LSPS2 lease CLTV delta was validated before invoice creation"),
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}]);

		let currency = self.config.network.into();
		let mut invoice_builder = InvoiceBuilder::new(currency)
			.invoice_description(description.clone())
			.payment_hash(payment_hash)
			.payment_secret(payment_secret)
			.current_timestamp()
			.min_final_cltv_expiry_delta(LSPS2_MIN_FINAL_CLTV_EXPIRY_DELTA.into())
			.expiry_time(Duration::from_secs(expiry_secs.into()))
			.private_route(route_hint);

		if let Some(amount_msat) = lease.payment_size_msat {
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
					if let Some(request) =
						self.pending_lsps2_fee_requests.lock().expect("lock").remove(&request_id)
					{
						let response = LSPS2FeeResponse { opening_fee_params_menu };

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
					if let Some(request) =
						self.pending_buy_requests.lock().expect("lock").remove(&request_id)
					{
						let response = LSPS2BuyResponse { intercept_scid, cltv_expiry_delta };

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

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn failed_persisted_removal_restores_value() {
		let restored = Arc::new(Mutex::new(None));
		let restored_ref = Arc::clone(&restored);
		let result = consume_after_persisted_removal(
			42,
			|_| async { Err(()) },
			move |value| *restored_ref.lock().unwrap() = Some(value),
		)
		.await;

		assert_eq!(result, Err(()));
		assert_eq!(*restored.lock().unwrap(), Some(42));
	}

	#[test]
	fn removed_cache_targets_drop_their_leases() {
		use crate::builder::NodeBuilder;
		use crate::entropy::NodeEntropy;
		use crate::io::test_utils::InMemoryStore;
		use crate::liquidity::LSPS2_LEASE_CACHE_TARGET_SIZE;

		let lsp_node_id: PublicKey =
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993".parse().unwrap();
		let mut builder = NodeBuilder::new();
		builder.set_log_facade_logger();
		builder.add_liquidity_source(
			lsp_node_id,
			lightning::ln::msgs::SocketAddress::OnionV2([0; 12]),
			None,
			false,
		);
		#[cfg(not(feature = "uniffi"))]
		let entropy = NodeEntropy::from_seed_bytes([42; 64]);
		#[cfg(feature = "uniffi")]
		let entropy = NodeEntropy::from_seed_bytes(vec![42; 64]).unwrap();
		let node = builder.build_with_store(entropy, InMemoryStore::new()).unwrap();
		let client = node.liquidity_source.lsps2_client();
		client.lsp_nodes.write().unwrap()[0].supported_protocols = Some(vec![2]);

		let valid_until = now_secs() + 2 * state::MIN_LEASE_REMAINING_SECS;
		let lease = |intercept_scid, amount_msat| PaymentLease {
			id: PaymentLeaseId { lsp_node_id, intercept_scid },
			params: LSPS2OpeningFeeParams {
				min_fee_msat: 1,
				proportional: 1,
				valid_until: chrono::DateTime::from_timestamp(valid_until as i64, 0)
					.unwrap()
					.to_rfc3339()
					.parse()
					.unwrap(),
				min_lifetime: 1,
				max_client_to_self_delay: 1,
				min_payment_size_msat: 1,
				max_payment_size_msat: 1_000_000,
				promise: String::new(),
			},
			cltv_expiry_delta: 144,
			payment_size_msat: Some(amount_msat),
			valid_until,
		};
		let (evicted, expired, retained) = (lease(1, 1_000), lease(2, 2_000), lease(3, 3_000));

		node.runtime.block_on(async {
			for lease in [&evicted, &expired, &retained] {
				client.lease_store.insert(lease.clone()).await.unwrap();
				client.lease_state.lock().unwrap().insert(lease.clone());
			}
			let fixed = |amount_msat| LeaseCacheTargetId::Fixed { amount_msat };
			client.register_cache_target(fixed(1_000), None).await.unwrap();
			client.register_cache_target(fixed(2_000), Some(now_secs())).await.unwrap();
			client.register_cache_target(fixed(3_000), None).await.unwrap();
			// Fill the cache so that registering one more target evicts the least recently used one.
			for amount_msat in 0..(LSPS2_LEASE_CACHE_TARGET_SIZE as u64 - 3) {
				client.register_cache_target(fixed(10_000 + amount_msat), None).await.unwrap();
			}
			client.register_cache_target(fixed(999_999), None).await.unwrap();
			client.prune_stale_cache_targets().await.unwrap();

			for (lease, expect_retained) in
				[(&evicted, false), (&expired, false), (&retained, true)]
			{
				let amount_msat = lease.payment_size_msat.unwrap();
				assert_eq!(
					client.lease_state.lock().unwrap().has_fixed_amount(
						amount_msat,
						None,
						&[lsp_node_id]
					),
					expect_retained,
					"unexpected cached lease state for {amount_msat} msat"
				);
				assert_eq!(
					client.lease_store.get(&lease.id).await.unwrap().is_some(),
					expect_retained,
					"unexpected persisted lease state for {amount_msat} msat"
				);
			}
		});
	}

	#[test]
	fn cache_targets_require_lsps2_liquidity_source() {
		use crate::builder::NodeBuilder;
		use crate::entropy::NodeEntropy;
		use crate::io::test_utils::InMemoryStore;

		let lsp_node_id: PublicKey =
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993".parse().unwrap();
		// Protocol discovery may not have completed yet, in which case the LSP may support LSPS2.
		for (lsp_protocols, expect_registered) in [
			(None, false),
			(Some(Some(vec![1])), false),
			(Some(None), true),
			(Some(Some(vec![1, 2])), true),
		] {
			let mut builder = NodeBuilder::new();
			builder.set_log_facade_logger();
			if lsp_protocols.is_some() {
				builder.add_liquidity_source(
					lsp_node_id,
					lightning::ln::msgs::SocketAddress::OnionV2([0; 12]),
					None,
					false,
				);
			}
			#[cfg(not(feature = "uniffi"))]
			let entropy = NodeEntropy::from_seed_bytes([42; 64]);
			#[cfg(feature = "uniffi")]
			let entropy = NodeEntropy::from_seed_bytes(vec![42; 64]).unwrap();
			let node = builder.build_with_store(entropy, InMemoryStore::new()).unwrap();
			let client = node.liquidity_source.lsps2_client();
			if let Some(protocols) = lsp_protocols.clone() {
				client.lsp_nodes.write().unwrap()[0].supported_protocols = protocols;
			}

			node.runtime
				.block_on(client.register_cache_target(LeaseCacheTargetId::Variable, None))
				.unwrap();
			assert_eq!(
				!client.cache_targets().is_empty(),
				expect_registered,
				"unexpected cache target registration with LSP protocols {lsp_protocols:?}"
			);
		}
	}
}

pub(crate) mod router;
pub(crate) mod state;
