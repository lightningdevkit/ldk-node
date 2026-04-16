// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Transaction;
use lightning::events::HTLCHandlingFailureType;
use lightning::ln::channelmanager::InterceptId;
use lightning::ln::types::ChannelId;
use lightning_liquidity::lsps2::service::LSPS2ServiceConfig as LdkLSPS2ServiceConfig;
use lightning_types::payment::PaymentHash;

use crate::logger::{log_error, LdkLogger};

use super::super::LiquiditySource;

pub(crate) const LSPS2_GETINFO_REQUEST_EXPIRY: Duration = Duration::from_secs(60 * 60 * 24);
pub(crate) const LSPS2_CHANNEL_CLTV_EXPIRY_DELTA: u32 = 72;

pub(crate) struct LSPS2Service {
	pub(crate) service_config: LSPS2ServiceConfig,
	pub(crate) ldk_service_config: LdkLSPS2ServiceConfig,
}

/// Represents the configuration of the LSPS2 service.
///
/// See [bLIP-52 / LSPS2] for more information.
///
/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LSPS2ServiceConfig {
	/// A token we may require to be sent by the clients.
	///
	/// If set, only requests matching this token will be accepted.
	pub require_token: Option<String>,
	/// Indicates whether the LSPS service will be announced via the gossip network.
	pub advertise_service: bool,
	/// The fee we withhold for the channel open from the initial payment.
	///
	/// This fee is proportional to the client-requested amount, in parts-per-million.
	pub channel_opening_fee_ppm: u32,
	/// The proportional overprovisioning for the channel.
	///
	/// This determines, in parts-per-million, how much value we'll provision on top of the amount
	/// we need to forward the payment to the client.
	///
	/// For example, setting this to `100_000` will result in a channel being opened that is 10%
	/// larger than then the to-be-forwarded amount (i.e., client-requested amount minus the
	/// channel opening fee fee).
	pub channel_over_provisioning_ppm: u32,
	/// The minimum fee required for opening a channel.
	pub min_channel_opening_fee_msat: u64,
	/// The minimum number of blocks after confirmation we promise to keep the channel open.
	pub min_channel_lifetime: u32,
	/// The maximum number of blocks that the client is allowed to set its `to_self_delay` parameter.
	pub max_client_to_self_delay: u32,
	/// The minimum payment size that we will accept when opening a channel.
	pub min_payment_size_msat: u64,
	/// The maximum payment size that we will accept when opening a channel.
	pub max_payment_size_msat: u64,
	/// Use the 'client-trusts-LSP' trust model.
	///
	/// When set, the service will delay *broadcasting* the JIT channel's funding transaction until
	/// the client claimed sufficient HTLC parts to pay for the channel open.
	///
	/// Note this will render the flow incompatible with clients utilizing the 'LSP-trust-client'
	/// trust model, i.e., in turn delay *claiming* any HTLCs until they see the funding
	/// transaction in the mempool.
	///
	/// Please refer to [`bLIP-52`] for more information.
	///
	/// [`bLIP-52`]: https://github.com/lightning/blips/blob/master/blip-0052.md#trust-models
	pub client_trusts_lsp: bool,
	/// When set, we will allow clients to spend their entire channel balance in the channels
	/// we open to them. This allows clients to try to steal your channel balance with
	/// no financial penalty, so this should only be set if you trust your clients.
	///
	/// See [`Node::open_0reserve_channel`] to manually open these channels.
	///
	/// [`Node::open_0reserve_channel`]: crate::Node::open_0reserve_channel
	pub disable_client_reserve: bool,
}

impl<L: Deref> LiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn lsps2_channel_needs_manual_broadcast(
		&self, counterparty_node_id: PublicKey, user_channel_id: u128,
	) -> bool {
		self.lsps2_service.as_ref().map_or(false, |lsps2_service| {
			lsps2_service.service_config.client_trusts_lsp
				&& self
					.liquidity_manager()
					.lsps2_service_handler()
					.and_then(|handler| {
						handler
							.channel_needs_manual_broadcast(user_channel_id, &counterparty_node_id)
							.ok()
					})
					.unwrap_or(false)
		})
	}

	pub(crate) fn lsps2_store_funding_transaction(
		&self, user_channel_id: u128, counterparty_node_id: PublicKey, funding_tx: Transaction,
	) {
		if self.lsps2_service.as_ref().map_or(false, |svc| !svc.service_config.client_trusts_lsp) {
			// Only necessary for client-trusts-LSP flow
			return;
		}

		let lsps2_service_handler = self.liquidity_manager.lsps2_service_handler();
		if let Some(handler) = lsps2_service_handler {
			handler
				.store_funding_transaction(user_channel_id, &counterparty_node_id, funding_tx)
				.unwrap_or_else(|e| {
					debug_assert!(false, "Failed to store funding transaction: {:?}", e);
					log_error!(self.logger, "Failed to store funding transaction: {:?}", e);
				});
		} else {
			log_error!(self.logger, "LSPS2 service handler is not available.");
		}
	}

	pub(crate) fn lsps2_funding_tx_broadcast_safe(
		&self, user_channel_id: u128, counterparty_node_id: PublicKey,
	) {
		if self.lsps2_service.as_ref().map_or(false, |svc| !svc.service_config.client_trusts_lsp) {
			// Only necessary for client-trusts-LSP flow
			return;
		}

		let lsps2_service_handler = self.liquidity_manager.lsps2_service_handler();
		if let Some(handler) = lsps2_service_handler {
			handler
				.set_funding_tx_broadcast_safe(user_channel_id, &counterparty_node_id)
				.unwrap_or_else(|e| {
					debug_assert!(
						false,
						"Failed to mark funding transaction safe to broadcast: {:?}",
						e
					);
					log_error!(
						self.logger,
						"Failed to mark funding transaction safe to broadcast: {:?}",
						e
					);
				});
		} else {
			log_error!(self.logger, "LSPS2 service handler is not available.");
		}
	}

	pub(crate) async fn handle_channel_ready(
		&self, user_channel_id: u128, channel_id: &ChannelId, counterparty_node_id: &PublicKey,
	) {
		if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
			if let Err(e) = lsps2_service_handler
				.channel_ready(user_channel_id, channel_id, counterparty_node_id)
				.await
			{
				log_error!(
					self.logger,
					"LSPS2 service failed to handle ChannelReady event: {:?}",
					e
				);
			}
		}
	}

	pub(crate) async fn handle_htlc_intercepted(
		&self, intercept_scid: u64, intercept_id: InterceptId, expected_outbound_amount_msat: u64,
		payment_hash: PaymentHash,
	) {
		if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
			if let Err(e) = lsps2_service_handler
				.htlc_intercepted(
					intercept_scid,
					intercept_id,
					expected_outbound_amount_msat,
					payment_hash,
				)
				.await
			{
				log_error!(
					self.logger,
					"LSPS2 service failed to handle HTLCIntercepted event: {:?}",
					e
				);
			}
		}
	}

	pub(crate) async fn handle_htlc_handling_failed(&self, failure_type: HTLCHandlingFailureType) {
		if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
			if let Err(e) = lsps2_service_handler.htlc_handling_failed(failure_type).await {
				log_error!(
					self.logger,
					"LSPS2 service failed to handle HTLCHandlingFailed event: {:?}",
					e
				);
			}
		}
	}

	pub(crate) async fn handle_payment_forwarded(
		&self, next_channel_id: Option<ChannelId>, skimmed_fee_msat: u64,
	) {
		if let Some(next_channel_id) = next_channel_id {
			if let Some(lsps2_service_handler) = self.liquidity_manager.lsps2_service_handler() {
				if let Err(e) =
					lsps2_service_handler.payment_forwarded(next_channel_id, skimmed_fee_msat).await
				{
					log_error!(
						self.logger,
						"LSPS2 service failed to handle PaymentForwarded: {:?}",
						e
					);
				}
			}
		}
	}
}
