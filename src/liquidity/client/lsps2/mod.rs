// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::log_warn;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, InvoiceBuilder, RoutingFees};
use lightning_liquidity::lsps0::ser::{LSPSRequestId, LSPSResponseError};
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
const LEASE_NEGOTIATION_MAX_ATTEMPTS: usize = 3;

#[derive(Debug, PartialEq, Eq)]
enum LeaseNegotiationError {
	Local(Error),
	Rejected(LSPSResponseError),
}

impl LeaseNegotiationError {
	fn is_retryable(&self) -> bool {
		match self {
			Self::Local(error) => *error == Error::LiquidityRequestFailed,
			// bLIP-52: INVALID_OPENING_FEE_PARAMS. Fetch fresh parameters before buying again.
			Self::Rejected(error) => error.code == 201,
		}
	}
}

impl From<Error> for LeaseNegotiationError {
	fn from(error: Error) -> Self {
		Self::Local(error)
	}
}

impl From<LeaseNegotiationError> for Error {
	fn from(error: LeaseNegotiationError) -> Self {
		match error {
			LeaseNegotiationError::Local(error) => error,
			LeaseNegotiationError::Rejected(_) => Self::LiquidityRequestFailed,
		}
	}
}

impl std::fmt::Display for LeaseNegotiationError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Local(error) => error.fmt(f),
			Self::Rejected(error) => {
				write!(f, "LSPS2 request rejected ({}): {}", error.code, error.message)
			},
		}
	}
}

fn should_retry_lease_negotiation(error: &LeaseNegotiationError, attempt: usize) -> bool {
	error.is_retryable() && attempt < LEASE_NEGOTIATION_MAX_ATTEMPTS
}

fn record_negotiation_failure(
	previous: &mut Option<LeaseNegotiationError>, error: LeaseNegotiationError,
) {
	// One LSP's permanent rejection must not suppress retries for another LSP's transient failure.
	if previous.as_ref().map_or(true, |error| !error.is_retryable()) {
		*previous = Some(error);
	}
}

async fn try_lease_candidates<T, K, R, KF, AF, Fut>(
	candidates: Vec<T>, candidate_key: KF, mut attempt: AF,
) -> Result<R, LeaseNegotiationError>
where
	K: Copy + Eq + Hash,
	KF: Fn(&T) -> K,
	AF: FnMut(T) -> Fut,
	Fut: Future<Output = Result<R, LeaseNegotiationError>>,
{
	let mut failed_candidates = HashSet::new();
	let mut last_error = None;
	for candidate in candidates {
		let key = candidate_key(&candidate);
		if failed_candidates.contains(&key) {
			continue;
		}
		match attempt(candidate).await {
			Ok(result) => return Ok(result),
			Err(error) => {
				failed_candidates.insert(key);
				record_negotiation_failure(&mut last_error, error);
			},
		}
	}
	Err(last_error.expect("lease candidates are non-empty"))
}

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
		Mutex<HashMap<LSPSRequestId, PendingRequest<Result<LSPS2FeeResponse, LSPSResponseError>>>>,
	pub(crate) pending_buy_requests:
		Mutex<HashMap<LSPSRequestId, PendingRequest<Result<LSPS2BuyResponse, LSPSResponseError>>>>,
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

	pub(crate) async fn refill_cached_leases(
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(), Error> {
		self.prune_stale_cache_targets().await?;
		for target in self.cache_targets() {
			self.schedule_lease_refill(target.id, connection_manager);
		}
		Ok(())
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
		let (lease, total_fee_msat, _, _) = match request {
			JitInvoiceRequest::Fixed { amount_msat, .. } => {
				self.acquire_fixed_lease(amount_msat, &connection_manager).await?
			},
			JitInvoiceRequest::Variable { amount_msat, .. } => {
				// A BOLT12 invoice request has already resolved the payment amount. Use it to avoid
				// consuming a cached variable lease outside its advertised range, and record the exact
				// fee for that amount so payment validation need not accept the broader node-wide cap.
				self.acquire_variable_lease(Some(amount_msat), &connection_manager).await?
			},
		};
		let fee_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(total_fee_msat),
			max_proportional_opening_fee_ppm_msat: None,
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
			self.acquire_variable_lease(None, &connection_manager).await?;
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: self.config.lsps2_max_total_lsp_fee_limit_msat,
			max_proportional_opening_fee_ppm_msat: Some(proportional_fee),
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
		let mut attempt = 1;
		loop {
			let result = self.negotiate_fixed_lease_once(amount_msat, connection_manager).await;
			match result {
				Err(error) if should_retry_lease_negotiation(&error, attempt) => {
					log_warn!(
						self.logger,
						"LSPS2 lease negotiation attempt {} failed, retrying: {}",
						attempt,
						error
					);
					attempt += 1;
				},
				result => return result.map_err(Error::from),
			}
		}
	}

	async fn negotiate_fixed_lease_once(
		self: &Arc<Self>, amount_msat: u64, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig), LeaseNegotiationError> {
		let all_offers = self.gather_lsps2_offers(connection_manager).await?;
		let mut candidates = all_offers
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
			.collect::<Vec<_>>();
		candidates.sort_unstable_by_key(|(_, fee, _)| *fee);
		let min_total_fee_msat = candidates.first().map(|(_, fee, _)| *fee).ok_or_else(|| {
			log_error!(self.logger, "Failed to handle response from liquidity service",);
			Error::LiquidityRequestFailed
		})?;

		if let Some(max_total_lsp_fee_limit_msat) = self.config.lsps2_max_total_lsp_fee_limit_msat {
			if min_total_fee_msat > max_total_lsp_fee_limit_msat {
				log_error!(self.logger,
					"Failed to request inbound JIT channel as LSP's requested total opening fee of {}msat exceeds our configured fee limit of {}msat",
					min_total_fee_msat, max_total_lsp_fee_limit_msat
				);
				return Err(Error::LiquidityFeeTooHigh.into());
			}
			candidates.retain(|(_, fee, _)| *fee <= max_total_lsp_fee_limit_msat);
		}

		try_lease_candidates(
			candidates,
			|(lsp, _, _)| lsp.node_id,
			|(lsp, total_fee_msat, opening_params)| {
				let client = Arc::clone(self);
				async move {
					log_debug!(
						client.logger,
						"Choosing liquidity offer from LSP {}, will pay {}msat in total LSP fees",
						lsp.node_id,
						total_fee_msat
					);
					match client
						.lsps2_send_buy_request(
							Some(amount_msat),
							opening_params,
							Some(&lsp.node_id),
						)
						.await
					{
						Ok(lease) => Ok((lease, total_fee_msat, lsp)),
						Err(error) => {
							log_warn!(
								client.logger,
								"Failed negotiating LSPS2 payment lease with LSP {}, trying the next candidate: {}",
								lsp.node_id,
								error
							);
							Err(error)
						},
					}
				}
			},
		)
		.await
	}

	async fn acquire_variable_lease(
		self: &Arc<Self>, amount_msat: Option<u64>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig, bool), Error> {
		if let Some((lease, fee, lsp)) = self.take_cached_variable_lease(amount_msat).await? {
			return Ok((lease, fee, lsp, false));
		}
		let request_lock = self
			.pending_lease_request_state
			.lock()
			.expect("lock")
			.request_lock(LeaseCacheTargetId::Variable);
		let _request_guard = request_lock.lock().await;
		if let Some((lease, fee, lsp)) = self.take_cached_variable_lease(amount_msat).await? {
			return Ok((lease, fee, lsp, false));
		}

		let (negotiated_lease, fee, cheapest_lsp) =
			self.negotiate_variable_lease(amount_msat, connection_manager).await?;
		let lease = self.consume_lease(&negotiated_lease.id).await?;
		Ok((lease, fee, cheapest_lsp, true))
	}

	async fn negotiate_variable_lease(
		self: &Arc<Self>, amount_msat: Option<u64>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig), Error> {
		let mut attempt = 1;
		loop {
			let result = self.negotiate_variable_lease_once(amount_msat, connection_manager).await;
			match result {
				Err(error) if should_retry_lease_negotiation(&error, attempt) => {
					log_warn!(
						self.logger,
						"LSPS2 lease negotiation attempt {} failed, retrying: {}",
						attempt,
						error
					);
					attempt += 1;
				},
				result => return result.map_err(Error::from),
			}
		}
	}

	async fn negotiate_variable_lease_once(
		self: &Arc<Self>, amount_msat: Option<u64>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<(PaymentLease, u64, LspConfig), LeaseNegotiationError> {
		let all_offers = self.gather_lsps2_offers(connection_manager).await?;
		let mut rejected_for_fee = false;
		let mut candidates = all_offers
			.into_iter()
			.flat_map(|(lsp, resp)| {
				resp.opening_fee_params_menu.into_iter().map(move |params| (lsp.clone(), params))
			})
			.filter_map(|(lsp, params)| {
				let (selection_fee, fee_for_limit) =
					state::variable_lease_fees(&params, amount_msat)?;
				let fee_allowed = self
					.config
					.lsps2_max_total_lsp_fee_limit_msat
					.map_or(true, |limit| fee_for_limit <= limit);
				rejected_for_fee |= !fee_allowed;
				fee_allowed.then_some((lsp, selection_fee, params))
			})
			.collect::<Vec<_>>();
		candidates.sort_unstable_by_key(|(_, fee, _)| *fee);
		if candidates.is_empty() {
			return Err(if rejected_for_fee {
				log_error!(
					self.logger,
					"Failed to request inbound JIT channel as all LSP offers exceed our configured fee limit"
				);
				Error::LiquidityFeeTooHigh.into()
			} else {
				log_error!(self.logger, "Failed to handle response from liquidity service",);
				Error::LiquidityRequestFailed.into()
			});
		}

		try_lease_candidates(
			candidates,
			|(lsp, _, _)| lsp.node_id,
			|(lsp, fee, opening_params)| {
				let client = Arc::clone(self);
				async move {
					if amount_msat.is_some() {
						log_debug!(
							client.logger,
							"Choosing liquidity offer from LSP {}, will pay {}msat in total LSP fees",
							lsp.node_id,
							fee
						);
					} else {
						log_debug!(
							client.logger,
							"Choosing liquidity offer from LSP {}, will pay {}ppm msat in proportional LSP fees",
							lsp.node_id,
							fee
						);
					}
					match client
						.lsps2_send_buy_request(None, opening_params, Some(&lsp.node_id))
						.await
					{
						Ok(lease) => Ok((lease, fee, lsp)),
						Err(error) => {
							log_warn!(
								client.logger,
								"Failed negotiating LSPS2 payment lease with LSP {}, trying the next candidate: {}",
								lsp.node_id,
								error
							);
							Err(error)
						},
					}
				}
			},
		)
		.await
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
		self.negotiate_variable_lease(None, connection_manager).await?;
		Ok(())
	}

	async fn gather_lsps2_offers(
		self: &Arc<Self>, connection_manager: &Arc<ConnectionManager<L>>,
	) -> Result<Vec<(LspConfig, LSPS2FeeResponse)>, LeaseNegotiationError> {
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
		let mut failure = None;
		let mut fee_set = JoinSet::new();
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
					record_negotiation_failure(&mut failure, e);
				},
				Err(e) => {
					log_warn!(self.logger, "Failed to get fees from LSP: {}", e);
					record_negotiation_failure(&mut failure, Error::LiquidityRequestFailed.into());
				},
			}
		}

		if all_offers.is_empty() {
			if let Some(error) = failure {
				return Err(error);
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
	) -> Result<LSPS2FeeResponse, LeaseNegotiationError> {
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
		})?
		.map_err(LeaseNegotiationError::Rejected)
	}

	async fn lsps2_send_buy_request(
		&self, amount_msat: Option<u64>, opening_fee_params: LSPS2OpeningFeeParams,
		node_id: Option<&PublicKey>,
	) -> Result<PaymentLease, LeaseNegotiationError> {
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
		})?
		.map_err(LeaseNegotiationError::Rejected)?;
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

	async fn take_cached_fixed_lease(
		&self, amount_msat: u64,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;
		let available_lsps = lsps2_nodes.iter().map(|lsp| lsp.node_id).collect::<Vec<_>>();
		let Some((lease, fee_msat)) = self.lease_state.lock().expect("lock").fixed_amount(
			amount_msat,
			self.config.lsps2_max_total_lsp_fee_limit_msat,
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
		&self, amount_msat: Option<u64>,
	) -> Result<Option<(PaymentLease, u64, LspConfig)>, Error> {
		let lsps2_nodes = self.get_lsps2_nodes().await?;
		let available_lsps = lsps2_nodes.iter().map(|lsp| lsp.node_id).collect::<Vec<_>>();
		let Some((lease, fee)) = self.lease_state.lock().expect("lock").variable_amount(
			amount_msat,
			self.config.lsps2_max_total_lsp_fee_limit_msat,
			&available_lsps,
		) else {
			return Ok(None);
		};
		let lsp = lsps2_nodes
			.into_iter()
			.find(|lsp| lsp.node_id == lease.id.lsp_node_id)
			.expect("lease was selected from available LSPs");
		let lease = self.consume_selected_lease(lease).await?;
		Ok(Some((lease, fee, lsp)))
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

						match request.sender.send(Ok(response)) {
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

						match request.sender.send(Ok(response)) {
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
			LSPS2ClientEvent::GetInfoFailed { request_id, counterparty_node_id, error } => {
				self.handle_request_rejection(
					&self.pending_lsps2_fee_requests,
					request_id,
					counterparty_node_id,
					error,
				);
			},
			LSPS2ClientEvent::BuyRequestFailed { request_id, counterparty_node_id, error } => {
				self.handle_request_rejection(
					&self.pending_buy_requests,
					request_id,
					counterparty_node_id,
					error,
				);
			},
		}
	}

	fn handle_request_rejection<T>(
		&self,
		pending: &Mutex<HashMap<LSPSRequestId, PendingRequest<Result<T, LSPSResponseError>>>>,
		request_id: LSPSRequestId, counterparty_node_id: PublicKey, error: LSPSResponseError,
	) {
		if !self.lsp_nodes.read().expect("lock").iter().any(|n| n.node_id == counterparty_node_id) {
			log_error!(
				self.logger,
				"Received LSPS2 rejection from unexpected peer {}",
				counterparty_node_id
			);
			return;
		}
		if let Some(request) = pending.lock().expect("lock").remove(&request_id) {
			if request.sender.send(Err(error)).is_err() {
				log_error!(
					self.logger,
					"Failed to handle rejection for liquidity request {:?}",
					request_id
				);
			}
		} else {
			log_error!(
				self.logger,
				"Received rejection from liquidity service for unknown request."
			);
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
	use std::future::poll_fn;
	use std::task::Poll;

	use crate::builder::NodeBuilder;
	use crate::entropy::NodeEntropy;
	use crate::io::test_utils::InMemoryStore;

	fn rejection_test_node() -> (crate::Node, PublicKey) {
		let node_id =
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993".parse().unwrap();
		let mut builder = NodeBuilder::new();
		builder.set_log_facade_logger();
		// An unsupported address makes connection attempts fail without network I/O.
		builder.add_liquidity_source(
			node_id,
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
		(node, node_id)
	}

	fn check_rejection_completes_request(is_buy: bool, code: i32) {
		let (node, node_id) = rejection_test_node();
		let client = node.liquidity_source.lsps2_client();
		node.runtime.block_on(async {
			let params = LSPS2OpeningFeeParams {
				min_fee_msat: 1,
				proportional: 1,
				valid_until: (chrono::Utc::now() + chrono::Duration::days(2))
					.to_rfc3339()
					.parse()
					.unwrap(),
				min_lifetime: 1,
				max_client_to_self_delay: 1,
				min_payment_size_msat: 1,
				max_payment_size_msat: 2_000,
				promise: String::new(),
			};
			let request = async {
				if is_buy {
					client
						.lsps2_send_buy_request(Some(1_000), params, Some(&node_id))
						.await
						.map(|_| ())
				} else {
					client.lsps2_request_opening_fee_params(Some(&node_id)).await.map(|_| ())
				}
			};
			tokio::pin!(request);
			poll_fn(|cx| {
				assert!(request.as_mut().poll(cx).is_pending());
				Poll::Ready(())
			})
			.await;
			let error = LSPSResponseError { code, message: "request rejected".into(), data: None };
			let expected_error = error.clone();
			let event = if is_buy {
				let request_id =
					client.pending_buy_requests.lock().unwrap().keys().next().unwrap().clone();
				LSPS2ClientEvent::BuyRequestFailed {
					request_id,
					counterparty_node_id: node_id,
					error,
				}
			} else {
				let request_id = client
					.pending_lsps2_fee_requests
					.lock()
					.unwrap()
					.keys()
					.next()
					.unwrap()
					.clone();
				LSPS2ClientEvent::GetInfoFailed { request_id, counterparty_node_id: node_id, error }
			};
			client.handle_event(event).await;
			let result = poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
			let Poll::Ready(Err(error)) = result else {
				panic!("rejection must complete the request immediately: {result:?}");
			};
			assert_eq!(error, LeaseNegotiationError::Rejected(expected_error));
			assert_eq!(should_retry_lease_negotiation(&error, 1), code == 201);
			assert!(!should_retry_lease_negotiation(&error, 3));
			assert!(client.pending_buy_requests.lock().unwrap().is_empty());
			assert!(client.pending_lsps2_fee_requests.lock().unwrap().is_empty());
		});
	}

	#[test]
	fn get_info_rejection_completes_request() {
		for code in [1, 200, -32602] {
			check_rejection_completes_request(false, code);
		}
	}

	#[test]
	fn buy_rejection_completes_request() {
		for code in [1, 201, 202, 203, -32602] {
			check_rejection_completes_request(true, code);
		}
	}

	#[test]
	fn rejected_fee_queries_stop_lease_negotiation() {
		let (node, node_id) = rejection_test_node();
		let client = node.liquidity_source.lsps2_client();
		node.runtime.block_on(async {
			for fixed_amount in [true, false] {
				let mut rejection_count = 0;
				tokio::time::timeout(Duration::from_secs(1), async {
					let negotiation = async {
						if fixed_amount {
							client.negotiate_fixed_lease(1_000, &node.connection_manager).await
						} else {
							client.negotiate_variable_lease(Some(1_000), &node.connection_manager).await
						}
					};
					let rejections = async {
						loop {
							let request_id = client.pending_lsps2_fee_requests.lock().unwrap().keys().next().cloned();
							if let Some(request_id) = request_id {
								rejection_count += 1;
								assert_eq!(rejection_count, 1, "a permanent fee query rejection must not trigger another negotiation round");
								client.handle_event(LSPS2ClientEvent::GetInfoFailed {
									request_id, counterparty_node_id: node_id,
									error: LSPSResponseError { code: 200, message: "unrecognized token".into(), data: None },
								}).await;
							} else {
								tokio::task::yield_now().await;
							}
						}
					};
					tokio::select! {
						result = negotiation => assert!(matches!(result, Err(Error::LiquidityRequestFailed))),
						_ = rejections => unreachable!(),
					}
				}).await.expect("rejected fee queries must finish without waiting for request timeouts");
				assert_eq!(rejection_count, 1);
				assert!(client.pending_lsps2_fee_requests.lock().unwrap().is_empty());
			}
		});
	}

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
	fn lease_negotiation_retries_only_transient_failures() {
		assert!(should_retry_lease_negotiation(&Error::LiquidityRequestFailed.into(), 1));
		assert!(should_retry_lease_negotiation(&Error::LiquidityRequestFailed.into(), 2));
		assert!(!should_retry_lease_negotiation(&Error::LiquidityRequestFailed.into(), 3));
		assert!(!should_retry_lease_negotiation(&Error::LiquidityFeeTooHigh.into(), 1));
		assert!(!should_retry_lease_negotiation(&Error::LiquiditySourceUnavailable.into(), 1));
	}

	#[tokio::test]
	async fn lease_negotiation_preserves_retryable_failures() {
		let candidates =
			vec![(1, Error::LiquidityRequestFailed), (2, Error::LiquiditySourceUnavailable)];
		for candidates in [candidates.clone(), candidates.into_iter().rev().collect()] {
			let result: Result<(), _> = try_lease_candidates(
				candidates,
				|candidate| candidate.0,
				|(_, error)| async move { Err(error.into()) },
			)
			.await;
			assert!(
				should_retry_lease_negotiation(&result.unwrap_err(), 1),
				"a permanent failure must not suppress another LSP's retryable failure"
			);
		}
	}

	#[tokio::test]
	async fn lease_negotiation_fails_over_between_lsps() {
		let candidates = vec![(1, 10), (1, 20), (2, 30)];
		let attempted_lsps = Arc::new(Mutex::new(Vec::new()));
		let attempted_lsps_ref = Arc::clone(&attempted_lsps);
		let result = try_lease_candidates(
			candidates,
			|candidate| candidate.0,
			move |candidate| {
				let attempted_lsps = Arc::clone(&attempted_lsps_ref);
				async move {
					attempted_lsps.lock().unwrap().push(candidate.0);
					if candidate.0 == 1 {
						Err(Error::LiquiditySourceUnavailable.into())
					} else {
						Ok(candidate.1)
					}
				}
			},
		)
		.await;

		assert_eq!(result, Ok(30));
		assert_eq!(*attempted_lsps.lock().unwrap(), vec![1, 2]);
	}
}

pub(crate) mod router;
pub(crate) mod state;
