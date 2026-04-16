// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to liquidity management.

pub(crate) mod client;
pub(crate) mod service;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock, Weak};

use bitcoin::secp256k1::PublicKey;
use chrono::Utc;
use lightning::ln::msgs::SocketAddress;
use lightning::sign::EntropySource;
use lightning_liquidity::events::LiquidityEvent;
use lightning_liquidity::lsps0::ser::LSPSDateTime;
use lightning_liquidity::lsps1::client::LSPS1ClientConfig as LdkLSPS1ClientConfig;
use lightning_liquidity::lsps1::event::LSPS1ClientEvent;
use lightning_liquidity::lsps2::client::LSPS2ClientConfig as LdkLSPS2ClientConfig;
use lightning_liquidity::lsps2::event::{LSPS2ClientEvent, LSPS2ServiceEvent};
use lightning_liquidity::lsps2::msgs::LSPS2RawOpeningFeeParams;
use lightning_liquidity::lsps2::service::LSPS2ServiceConfig as LdkLSPS2ServiceConfig;
use lightning_liquidity::{LiquidityClientConfig, LiquidityServiceConfig};

use crate::builder::BuildError;
use crate::logger::{log_error, LdkLogger};
use crate::types::{
	Broadcaster, ChannelManager, DynStore, KeysManager, LiquidityManager, PeerManager, Wallet,
};
use crate::{total_anchor_channels_reserve_sats, Config};

pub(crate) use client::lsps1::{LSPS1Client, LSPS1ClientConfig, LSPS1OpeningParamsResponse};
pub use client::lsps1::{LSPS1Liquidity, LSPS1OrderStatus};
pub(crate) use client::lsps2::{
	LSPS2BuyResponse, LSPS2Client, LSPS2ClientConfig, LSPS2FeeResponse,
};
pub use service::lsps2::LSPS2ServiceConfig;
pub(crate) use service::lsps2::{
	LSPS2Service, LSPS2_CHANNEL_CLTV_EXPIRY_DELTA, LSPS2_GETINFO_REQUEST_EXPIRY,
};

pub(crate) const LIQUIDITY_REQUEST_TIMEOUT_SECS: u64 = 5;

pub(crate) struct LiquiditySourceBuilder<L: Deref>
where
	L::Target: LdkLogger,
{
	lsps1_client: Option<LSPS1Client>,
	lsps2_client: Option<LSPS2Client>,
	lsps2_service: Option<LSPS2Service>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	tx_broadcaster: Arc<Broadcaster>,
	kv_store: Arc<DynStore>,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref> LiquiditySourceBuilder<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>, logger: L,
	) -> Self {
		let lsps1_client = None;
		let lsps2_client = None;
		let lsps2_service = None;
		Self {
			lsps1_client,
			lsps2_client,
			lsps2_service,
			wallet,
			channel_manager,
			keys_manager,
			tx_broadcaster,
			kv_store,
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

	pub(crate) fn lsps2_service(
		&mut self, promise_secret: [u8; 32], service_config: LSPS2ServiceConfig,
	) -> &mut Self {
		let ldk_service_config = LdkLSPS2ServiceConfig { promise_secret };
		self.lsps2_service = Some(LSPS2Service { service_config, ldk_service_config });
		self
	}

	pub(crate) async fn build(self) -> Result<LiquiditySource<L>, BuildError> {
		let liquidity_service_config = self.lsps2_service.as_ref().map(|s| {
			let lsps2_service_config = Some(s.ldk_service_config.clone());
			let lsps5_service_config = None;
			let advertise_service = s.service_config.advertise_service;
			LiquidityServiceConfig {
				lsps1_service_config: None,
				lsps2_service_config,
				lsps5_service_config,
				advertise_service,
			}
		});

		let lsps1_client_config = self.lsps1_client.as_ref().map(|s| s.ldk_client_config.clone());
		let lsps2_client_config = self.lsps2_client.as_ref().map(|s| s.ldk_client_config.clone());
		let lsps5_client_config = None;
		let liquidity_client_config = Some(LiquidityClientConfig {
			lsps1_client_config,
			lsps2_client_config,
			lsps5_client_config,
		});

		let liquidity_manager = Arc::new(
			LiquidityManager::new(
				Arc::clone(&self.keys_manager),
				Arc::clone(&self.keys_manager),
				Arc::clone(&self.channel_manager),
				Arc::clone(&self.kv_store),
				Arc::clone(&self.tx_broadcaster),
				liquidity_service_config,
				liquidity_client_config,
			)
			.await
			.map_err(|_| BuildError::ReadFailed)?,
		);

		Ok(LiquiditySource {
			lsps1_client: self.lsps1_client,
			lsps2_client: self.lsps2_client,
			lsps2_service: self.lsps2_service,
			wallet: self.wallet,
			channel_manager: self.channel_manager,
			peer_manager: RwLock::new(None),
			keys_manager: self.keys_manager,
			liquidity_manager,
			config: self.config,
			logger: self.logger,
		})
	}
}

pub(crate) struct LiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	lsps1_client: Option<LSPS1Client>,
	lsps2_client: Option<LSPS2Client>,
	lsps2_service: Option<LSPS2Service>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	peer_manager: RwLock<Option<Weak<PeerManager>>>,
	keys_manager: Arc<KeysManager>,
	liquidity_manager: Arc<LiquidityManager>,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref> LiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn set_peer_manager(&self, peer_manager: Weak<PeerManager>) {
		*self.peer_manager.write().expect("lock") = Some(peer_manager);
	}

	pub(crate) fn liquidity_manager(&self) -> Arc<LiquidityManager> {
		Arc::clone(&self.liquidity_manager)
	}

	pub(crate) async fn handle_next_event(&self) {
		match self.liquidity_manager.next_event_async().await {
			LiquidityEvent::LSPS1Client(LSPS1ClientEvent::SupportedOptionsReady {
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
						.expect("lock")
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
			LiquidityEvent::LSPS1Client(LSPS1ClientEvent::OrderCreated {
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
						.expect("lock")
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
			LiquidityEvent::LSPS1Client(LSPS1ClientEvent::OrderStatus {
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
						.expect("lock")
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
			LiquidityEvent::LSPS2Service(LSPS2ServiceEvent::GetInfo {
				request_id,
				counterparty_node_id,
				token,
			}) => {
				if let Some(lsps2_service_handler) =
					self.liquidity_manager.lsps2_service_handler().as_ref()
				{
					let service_config = if let Some(service_config) =
						self.lsps2_service.as_ref().map(|s| s.service_config.clone())
					{
						service_config
					} else {
						log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
						return;
					};

					if let Some(required) = service_config.require_token {
						if token != Some(required) {
							log_error!(
								self.logger,
								"Rejecting LSPS2 request {:?} from counterparty {} as the client provided an invalid token.",
								request_id,
								counterparty_node_id
							);
							lsps2_service_handler.invalid_token_provided(&counterparty_node_id, request_id.clone()).unwrap_or_else(|e| {
								debug_assert!(false, "Failed to reject LSPS2 request. This should never happen.");
								log_error!(
									self.logger,
									"Failed to reject LSPS2 request {:?} from counterparty {} due to: {:?}. This should never happen.",
									request_id,
									counterparty_node_id,
									e
								);
							});
							return;
						}
					}

					let valid_until = LSPSDateTime(Utc::now() + LSPS2_GETINFO_REQUEST_EXPIRY);
					let opening_fee_params = LSPS2RawOpeningFeeParams {
						min_fee_msat: service_config.min_channel_opening_fee_msat,
						proportional: service_config.channel_opening_fee_ppm,
						valid_until,
						min_lifetime: service_config.min_channel_lifetime,
						max_client_to_self_delay: service_config.max_client_to_self_delay,
						min_payment_size_msat: service_config.min_payment_size_msat,
						max_payment_size_msat: service_config.max_payment_size_msat,
					};

					let opening_fee_params_menu = vec![opening_fee_params];

					if let Err(e) = lsps2_service_handler.opening_fee_params_generated(
						&counterparty_node_id,
						request_id,
						opening_fee_params_menu,
					) {
						log_error!(
							self.logger,
							"Failed to handle generated opening fee params: {:?}",
							e
						);
					}
				} else {
					log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
					return;
				}
			},
			LiquidityEvent::LSPS2Service(LSPS2ServiceEvent::BuyRequest {
				request_id,
				counterparty_node_id,
				opening_fee_params: _,
				payment_size_msat,
			}) => {
				if let Some(lsps2_service_handler) =
					self.liquidity_manager.lsps2_service_handler().as_ref()
				{
					let service_config = if let Some(service_config) =
						self.lsps2_service.as_ref().map(|s| s.service_config.clone())
					{
						service_config
					} else {
						log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
						return;
					};

					let user_channel_id: u128 = u128::from_ne_bytes(
						self.keys_manager.get_secure_random_bytes()[..16]
							.try_into()
							.expect("a 16-byte slice should convert into a [u8; 16]"),
					);
					let intercept_scid = self.channel_manager.get_intercept_scid();

					if let Some(payment_size_msat) = payment_size_msat {
						// We already check this in `lightning-liquidity`, but better safe than
						// sorry.
						//
						// TODO: We might want to eventually send back an error here, but we
						// currently can't and have to trust `lightning-liquidity` is doing the
						// right thing.
						//
						// TODO: Eventually we also might want to make sure that we have sufficient
						// liquidity for the channel opening here.
						if payment_size_msat > service_config.max_payment_size_msat
							|| payment_size_msat < service_config.min_payment_size_msat
						{
							log_error!(
								self.logger,
								"Rejecting to handle LSPS2 buy request {:?} from counterparty {} as the client requested an invalid payment size.",
								request_id,
								counterparty_node_id
							);
							return;
						}
					}

					match lsps2_service_handler
						.invoice_parameters_generated(
							&counterparty_node_id,
							request_id,
							intercept_scid,
							LSPS2_CHANNEL_CLTV_EXPIRY_DELTA,
							service_config.client_trusts_lsp,
							user_channel_id,
						)
						.await
					{
						Ok(()) => {},
						Err(e) => {
							log_error!(
								self.logger,
								"Failed to provide invoice parameters: {:?}",
								e
							);
							return;
						},
					}
				} else {
					log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
					return;
				}
			},
			LiquidityEvent::LSPS2Service(LSPS2ServiceEvent::OpenChannel {
				their_network_key,
				amt_to_forward_msat,
				opening_fee_msat: _,
				user_channel_id,
				intercept_scid: _,
			}) => {
				if self.liquidity_manager.lsps2_service_handler().is_none() {
					log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
					return;
				};

				let service_config = if let Some(service_config) =
					self.lsps2_service.as_ref().map(|s| s.service_config.clone())
				{
					service_config
				} else {
					log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
					return;
				};

				let init_features = if let Some(Some(peer_manager)) =
					self.peer_manager.read().expect("lock").as_ref().map(|weak| weak.upgrade())
				{
					// Fail if we're not connected to the prospective channel partner.
					if let Some(peer) = peer_manager.peer_by_node_id(&their_network_key) {
						peer.init_features
					} else {
						// TODO: We just silently fail here. Eventually we will need to remember
						// the pending requests and regularly retry opening the channel until we
						// succeed.
						log_error!(
							self.logger,
							"Failed to open LSPS2 channel to {} due to peer not being not connected.",
							their_network_key,
						);
						return;
					}
				} else {
					debug_assert!(false, "Failed to handle LSPS2ServiceEvent as peer manager isn't available. This should never happen.",);
					log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as peer manager isn't available. This should never happen.",);
					return;
				};

				// Fail if we have insufficient onchain funds available.
				let over_provisioning_msat = (amt_to_forward_msat
					* service_config.channel_over_provisioning_ppm as u64)
					/ 1_000_000;
				let channel_amount_sats = (amt_to_forward_msat + over_provisioning_msat) / 1000;
				let cur_anchor_reserve_sats =
					total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
				let spendable_amount_sats =
					self.wallet.get_spendable_amount_sats(cur_anchor_reserve_sats).unwrap_or(0);
				let required_funds_sats = channel_amount_sats
					+ self.config.anchor_channels_config.as_ref().map_or(0, |c| {
						if init_features.requires_anchors_zero_fee_htlc_tx()
							&& !c.trusted_peers_no_reserve.contains(&their_network_key)
						{
							c.per_channel_reserve_sats
						} else {
							0
						}
					});
				if spendable_amount_sats < required_funds_sats {
					log_error!(self.logger,
						"Unable to create channel due to insufficient funds. Available: {}sats, Required: {}sats",
						spendable_amount_sats, channel_amount_sats
					);
					// TODO: We just silently fail here. Eventually we will need to remember
					// the pending requests and regularly retry opening the channel until we
					// succeed.
					return;
				}

				let mut config = self.channel_manager.get_current_config().clone();

				// We set these LSP-specific values during Node building, here we're making sure it's actually set.
				debug_assert_eq!(
					config
						.channel_handshake_config
						.max_inbound_htlc_value_in_flight_percent_of_channel,
					100
				);
				debug_assert!(config.accept_forwards_to_priv_channels);

				// We set the forwarding fee to 0 for now as we're getting paid by the channel fee.
				//
				// TODO: revisit this decision eventually.
				config.channel_config.forwarding_fee_base_msat = 0;
				config.channel_config.forwarding_fee_proportional_millionths = 0;

				let result = if service_config.disable_client_reserve {
					self.channel_manager.create_channel_to_trusted_peer_0reserve(
						their_network_key,
						channel_amount_sats,
						0,
						user_channel_id,
						None,
						Some(config),
					)
				} else {
					self.channel_manager.create_channel(
						their_network_key,
						channel_amount_sats,
						0,
						user_channel_id,
						None,
						Some(config),
					)
				};

				match result {
					Ok(_) => {},
					Err(e) => {
						// TODO: We just silently fail here. Eventually we will need to remember
						// the pending requests and regularly retry opening the channel until we
						// succeed.
						let zero_reserve_string =
							if service_config.disable_client_reserve { "0reserve " } else { "" };
						log_error!(
							self.logger,
							"Failed to open LSPS2 {}channel to {}: {:?}",
							zero_reserve_string,
							their_network_key,
							e
						);
						return;
					},
				}
			},
			LiquidityEvent::LSPS2Client(LSPS2ClientEvent::OpeningParametersReady {
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
						lsps2_client.pending_fee_requests.lock().expect("lock").remove(&request_id)
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
			LiquidityEvent::LSPS2Client(LSPS2ClientEvent::InvoiceParametersReady {
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
						lsps2_client.pending_buy_requests.lock().expect("lock").remove(&request_id)
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
}
