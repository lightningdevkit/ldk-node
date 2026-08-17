// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::sync::{Arc, RwLock, Weak};

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::Bolt11InvoiceParameters;
use lightning::offers::offer::Offer;
use lightning::util::persist::KVStore;
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use lightning_liquidity::lsps0::ser::{LSPSDateTime, LSPSRequestId};
use lightning_liquidity::lsps1::event::LSPS1ServiceEvent;
use lightning_liquidity::lsps1::msgs::{
	LSPS1Bolt11PaymentInfo, LSPS1Bolt12PaymentInfo, LSPS1ChannelInfo, LSPS1OrderId,
	LSPS1OrderParams, LSPS1PaymentInfo, LSPS1PaymentState,
};
use lightning_liquidity::lsps1::service::{
	LSPS1ServiceConfig as LdkLSPS1ServiceConfig, PaymentMethod,
};

use crate::error::Error;
use crate::logger::{log_error, LdkLogger};
use crate::types::{ChannelManager, DynStore, LiquidityManager, PeerManager};

/// Server-side configuration options for bLIP-51 / LSPS1 channel requests.
#[derive(Debug, Clone)]
pub(crate) struct LSPS1Service {
	pub(crate) ldk_service_config: LdkLSPS1ServiceConfig,
	pub(crate) service_config: LSPS1ServiceConfig,
}

pub(crate) struct LSPS1ServiceLiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	pub(crate) lsps1_service_config: Option<LSPS1Service>,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) peer_manager: RwLock<Option<Weak<PeerManager>>>,
	pub(crate) liquidity_manager: Arc<LiquidityManager>,
	pub(crate) kv_store: Arc<DynStore>,
	pub(crate) logger: L,
}

pub(crate) struct PendingLSPS1Order {
	pub request_id: LSPSRequestId,
	pub counterparty_node_id: PublicKey,
	pub order_params: LSPS1OrderParams,
	pub order_total_amount_sat: u64,
	pub channel_expiry_blocks: u32,
}

impl_writeable_tlv_based!(PendingLSPS1Order, {
	(0, request_id, required),
	(1, counterparty_node_id, required),
	(2, order_params, required),
	(3, order_total_amount_sat, required),
	(4, channel_expiry_blocks, required),
});

pub(crate) struct PendingLSPS1Channel {
	pub order_id: LSPS1OrderId,
	pub channel_expiry_blocks: u32,
}

impl_writeable_tlv_based!(PendingLSPS1Channel, {
	(0, order_id, required),
	(1, channel_expiry_blocks, required),
});

/// Represents the options supported by the LSP.
///
/// See [bLIP-51 / LSPS1] for more information.
///
/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LSPS1ServiceConfig {
	/// A token we may require to be sent by the clients.
	///
	/// If set, only requests matching this token will be accepted.
	pub require_token: Option<String>,
	/// The flat base fee charged for opening the channel, in millisatoshis.
	pub channel_fee_base_msat: u64,
	/// The proportional fee charged based on the requested LSP liquidity, in parts-per-million.
	pub channel_fee_proportional_ppm: u32,
	/// The datetime when the payment option expires.
	pub payment_option_expires_at: LSPSDateTime,
	/// The Bolt11 invoice expiration time.
	pub bolt11_invoice_expiry_secs: Option<u32>,
	/// The option that tells if BOLT11 payments is accepted.
	pub allow_bolt11_payment: bool,
	/// The option that tells if onchain payments is accepted.
	pub allow_onchain_payment: bool,
	/// The option that tells if BOLT12 payments is accepted.
	pub allow_bolt12_payment: bool,
}

impl<L: Deref> LSPS1ServiceLiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn set_peer_manager(&self, peer_manager: Weak<PeerManager>) {
		*self.peer_manager.write().expect("lock") = Some(peer_manager);
	}

	pub(crate) fn liquidity_manager(&self) -> Arc<LiquidityManager> {
		Arc::clone(&self.liquidity_manager)
	}

	pub(crate) async fn handle_event(&self, event: LSPS1ServiceEvent) {
		match event {
			LSPS1ServiceEvent::RequestForPaymentDetails {
				request_id,
				counterparty_node_id,
				order,
				refund_onchain_address,
			} => {
				let lsps1_service_handler = match self.liquidity_manager.lsps1_service_handler() {
					Some(handler) => handler,
					None => {
						log_error!(self.logger, "Failed to handle LSPS1ServiceEvent as LSPS1 liquidity service was not configured.",);
						return;
					},
				};

				let lsps1_service_config = match &self.lsps1_service_config {
					Some(config) => config,
					None => {
						log_error!(self.logger, "Failed to handle LSPS1ServiceEvent as LSPS1 liquidity service was not configured.",);
						return;
					},
				};

				if let Some(token) = &lsps1_service_config.service_config.require_token {
					if Some(token) != order.token.as_ref() {
						log_error!(
                            self.logger,
                            "Rejecting LSPS1 request {:?} from counterparty {} as the client provided an invalid token.",
                            request_id,
                            counterparty_node_id
                        );
						lsps1_service_handler.invalid_token_provided(counterparty_node_id, request_id.clone()).
                        unwrap_or_else(|e| {
                            debug_assert!(false, "Failed to reject LSPS1 request. This should never happen.");
                            log_error!(
                                self.logger,
                                "Failed to reject LSPS1 request {:?} from counterparty {} due to: {:?}. This should never happen.",
                                request_id,
                                counterparty_node_id,
                                e
                            );
                        });
						return;
					}
				}

				let mut payment_info =
					LSPS1PaymentInfo { bolt11: None, bolt12: None, onchain: None };
				let mut payment_hash_opt = None;
				let mut offer_id_opt = None;

				let (fee_total_sat, order_total_sat) = self.calculate_order_amounts(&order);

				if lsps1_service_config.service_config.allow_bolt11_payment {
					let invoice = match self.handle_bolt11_payment(
						&request_id,
						&order,
						&lsps1_service_config,
					) {
						Ok(inv) => inv,
						Err(e) => {
							log_error!(
								self.logger,
								"Failed to generate LSPS1 BOLT11 invoice: {:?}",
								e
							);
							return;
						},
					};

					payment_hash_opt = Some(invoice.payment_hash());

					payment_info.bolt11 = Some(LSPS1Bolt11PaymentInfo {
						state: LSPS1PaymentState::ExpectPayment,
						expires_at: lsps1_service_config.service_config.payment_option_expires_at,
						fee_total_sat,
						order_total_sat,
						invoice,
					})
				}

				if lsps1_service_config.service_config.allow_bolt12_payment {
					let offer = match self.handle_bolt12_offer(&request_id, &order) {
						Ok(offer) => offer,
						Err(e) => {
							log_error!(self.logger, "Failed to create offer: {:?}", e);
							return;
						},
					};

					offer_id_opt = Some(offer.id());

					payment_info.bolt12 = Some(LSPS1Bolt12PaymentInfo {
						state: LSPS1PaymentState::ExpectPayment,
						expires_at: lsps1_service_config.service_config.payment_option_expires_at,
						fee_total_sat,
						order_total_sat,
						offer,
					})
				}

				if refund_onchain_address.is_none()
					&& lsps1_service_config.service_config.allow_onchain_payment
				{
					lsps1_service_handler.onchain_payments_required(counterparty_node_id, request_id.clone()).unwrap_or_else(|e| {
                        debug_assert!(false, "Failed to reject LSPS1 request. This should never happen.");
                        log_error!(
                            self.logger,
                            "Failed to reject LSPS1 request {:?} from counterparty {} due to: {:?}. This should never happen.",
                            request_id,
                            counterparty_node_id,
                            e
                        );
                    });
				}

				// TODO: handle the onchain payment option here
				if refund_onchain_address.is_some()
					&& lsps1_service_config.service_config.allow_onchain_payment
				{
					payment_info.onchain = None;
				}

				if payment_hash_opt.is_none() && offer_id_opt.is_none() {
					log_error!(
						self.logger,
						"Failed to process LSPS1 request: No valid payment method was generated."
					);
					return;
				}

				let pending_order = PendingLSPS1Order {
					counterparty_node_id,
					request_id: request_id.clone(),
					order_params: order.clone(),
					order_total_amount_sat: order_total_sat,
					channel_expiry_blocks: lsps1_service_config
						.ldk_service_config
						.supported_options
						.max_channel_expiry_blocks,
				};

				let serialized_order = pending_order.encode();

				if let Some(payment_hash) = payment_hash_opt {
					if let Err(e) = self
						.kv_store
						.write(
							"lsps1_pending_orders",
							"",
							&payment_hash.to_string(),
							serialized_order.clone(),
						)
						.await
					{
						log_error!(
                            self.logger,
                            "Failed to persist pending LSPS1 order for payment hash {}: {:?}. Aborting request.",
                            payment_hash,
                            e
                        );
						return;
					}
				}

				if let Some(offer_id) = offer_id_opt {
					if let Err(e) = self
						.kv_store
						.write(
							"lsps1_pending_orders",
							"",
							&offer_id.to_string(),
							serialized_order.clone(),
						)
						.await
					{
						log_error!(
                            self.logger,
                            "Failed to persist pending LSPS1 order for offer ID {}: {:?}. Aborting request.",
                            offer_id,
                            e
                        );
						return;
					}
				}

				if let Err(e) = lsps1_service_handler
					.send_payment_details(request_id.clone(), counterparty_node_id, payment_info)
					.await
				{
					log_error!(
                        self.logger,
                        "Failed to send LSPS1 payment details {:?} to counterparty {} due to: {:?}. This should never happen.",
                        request_id,
                        counterparty_node_id,
                        e
                    );
				}
			},
		}
	}

	pub(crate) fn calculate_order_amounts(&self, order: &LSPS1OrderParams) -> (u64, u64) {
		let lsps1_service_config = match &self.lsps1_service_config {
			Some(cfg) => cfg,
			None => return (0, 0),
		};

		let lsp_balance_msat = order.lsp_balance_sat.saturating_mul(1000);
		let proportional_fee_msat = (lsp_balance_msat as u128).saturating_mul(
			lsps1_service_config.service_config.channel_fee_proportional_ppm as u128,
		) / 1_000_000;

		let service_fee_msat = lsps1_service_config
			.service_config
			.channel_fee_base_msat
			.saturating_add(proportional_fee_msat as u64);

		let fee_total_sat = service_fee_msat.saturating_add(999) / 1000;

		let order_total_sat = fee_total_sat.saturating_add(order.client_balance_sat);

		(fee_total_sat, order_total_sat)
	}

	fn handle_bolt11_payment(
		&self, request_id: &LSPSRequestId, order: &LSPS1OrderParams,
		lsps1_service_config: &LSPS1Service,
	) -> Result<Bolt11Invoice, Error> {
		let (_fee_total_sat, order_total_sat) = self.calculate_order_amounts(&order);

		let invoice_amount_msat = order_total_sat.saturating_mul(1000);

		let invoice_description = Bolt11InvoiceDescription::Direct(
			Description::new(format!("LSPS1 Order {}", request_id.0))
				.map_err(|_| Error::InvoiceCreationFailed)?,
		);

		let invoice_params = Bolt11InvoiceParameters {
			amount_msats: Some(invoice_amount_msat),
			description: invoice_description,
			invoice_expiry_delta_secs: lsps1_service_config
				.service_config
				.bolt11_invoice_expiry_secs,
			..Default::default()
		};

		let invoice = match self.channel_manager.create_bolt11_invoice(invoice_params) {
			Ok(invoice) => invoice,
			Err(e) => {
				log_error!(self.logger, "Failed to generate LSPS1 BOLT11 invoice: {:?}", e);
				return Err(Error::InvoiceCreationFailed);
			},
		};

		Ok(invoice)
	}

	fn handle_bolt12_offer(
		&self, request_id: &LSPSRequestId, order: &LSPS1OrderParams,
	) -> Result<Offer, Error> {
		let offer_builder = self.channel_manager.create_offer_builder().map_err(|e| {
			log_error!(self.logger, "Failed to create offer builder: {:?}", e);
			Error::OfferCreationFailed
		})?;

		let (_fee_total_sat, order_total_sat) = self.calculate_order_amounts(&order);

		let offer_amount_msat = order_total_sat.saturating_mul(1000);
		let description = format!("LSPS1 Order {}", request_id.0);

		let offer = offer_builder.amount_msats(offer_amount_msat).description(description);

		let finalized_offer = offer.build().map_err(|e| {
			log_error!(self.logger, "Failed to create offer: {:?}", e);
			Error::OfferCreationFailed
		})?;

		Ok(finalized_offer)
	}

	pub(crate) async fn handle_order_payment_received(
		&self, counterparty_node_id: PublicKey, order_id: LSPS1OrderId, method: PaymentMethod,
	) {
		if let Some(lsps1_service_handler) = self.liquidity_manager.lsps1_service_handler() {
			if let Err(e) = lsps1_service_handler
				.order_payment_received(counterparty_node_id, order_id.clone(), method)
				.await
			{
				log_error!(
					self.logger,
					"Failed to handle and mark the order {:?} as paid due to: {:?}.",
					order_id,
					e
				);
			}
		}
	}

	pub(crate) async fn handle_order_channel_opened(
		&self, counterparty_node_id: PublicKey, order_id: LSPS1OrderId,
		channel_info: LSPS1ChannelInfo,
	) {
		if let Some(lsps1_service_handler) = self.liquidity_manager.lsps1_service_handler() {
			if let Err(e) = lsps1_service_handler
				.order_channel_opened(counterparty_node_id, order_id.clone(), channel_info)
				.await
			{
				log_error!(
					self.logger,
					"Failed to handle and mark the order {:?} as completed due to: {:?}.",
					order_id,
					e
				);
			}
		}
	}

	pub(crate) async fn handle_order_failed_and_refunded(
		&self, counterparty_node_id: PublicKey, order_id: LSPS1OrderId,
	) {
		if let Some(lsps1_service_handler) = self.liquidity_manager.lsps1_service_handler() {
			if let Err(e) = lsps1_service_handler
				.order_failed_and_refunded(counterparty_node_id, order_id.clone())
				.await
			{
				log_error!(
					self.logger,
					"Failed to handle and mark the order {:?} as failed and refunded due to: {:?}",
					order_id,
					e
				);
			}
		}
	}
}
