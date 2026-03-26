// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to liquidity management.

pub(crate) mod client;
pub(crate) mod service;

pub(crate) use client::lsps1::LSPS1Liquidity;
pub use client::LSPS1OrderStatus;
pub use service::lsps2::LSPS2ServiceConfig;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::msgs::SocketAddress;
use lightning_liquidity::events::LiquidityEvent;
use lightning_liquidity::lsps0::event::LSPS0ClientEvent;
use lightning_liquidity::lsps1::client::LSPS1ClientConfig as LdkLSPS1ClientConfig;
use lightning_liquidity::lsps2::client::LSPS2ClientConfig as LdkLSPS2ClientConfig;
use lightning_liquidity::lsps2::service::LSPS2ServiceConfig as LdkLSPS2ServiceConfig;
use lightning_liquidity::{LiquidityClientConfig, LiquidityServiceConfig};
use tokio::sync::oneshot;

use crate::builder::BuildError;
use crate::liquidity::client::lsps1::LSPS1LiquiditySource;
use crate::liquidity::client::lsps2::LSPS2ClientLiquiditySource;
use crate::liquidity::service::lsps2::{LSPS2Service, LSPS2ServiceLiquiditySource};
use crate::logger::{log_error, log_info, LdkLogger};
use crate::types::{Broadcaster, ChannelManager, DynStore, KeysManager, LiquidityManager, Wallet};
use crate::{Config, Error};

const LIQUIDITY_REQUEST_TIMEOUT_SECS: u64 = 5;

fn select_lsps_for_protocol(
	lsp_nodes: &Arc<RwLock<Vec<LspNode>>>, protocol: u16, override_node_id: Option<&PublicKey>,
) -> Option<LspConfig> {
	lsp_nodes
		.read()
		.unwrap()
		.iter()
		.find(|lsp_node| {
			if let Some(override_node_id) = override_node_id {
				lsp_node.node_id == *override_node_id
					&& lsp_node
						.supported_protocols
						.lock()
						.unwrap()
						.as_ref()
						.map(|protocols| protocols.contains(&protocol))
						.unwrap_or(false)
			} else {
				lsp_node
					.supported_protocols
					.lock()
					.unwrap()
					.as_ref()
					.map(|protocols| protocols.contains(&protocol))
					.unwrap_or(false)
			}
		})
		.map(|n| LspConfig {
			node_id: n.node_id,
			address: n.address.clone(),
			token: n.token.clone(),
		})
}

fn select_all_lsps_for_protocol(
	lsp_nodes: &Arc<RwLock<Vec<LspNode>>>, protocol: u16,
) -> Vec<LspConfig> {
	lsp_nodes
		.read()
		.unwrap()
		.iter()
		.filter(|lsp_node| {
			lsp_node
				.supported_protocols
				.lock()
				.unwrap()
				.as_ref()
				.map(|protocols| protocols.contains(&protocol))
				.unwrap_or(false)
		})
		.map(|n| LspConfig {
			node_id: n.node_id,
			address: n.address.clone(),
			token: n.token.clone(),
		})
		.collect()
}

fn is_lsps_node(lsp_nodes: &Arc<RwLock<Vec<LspNode>>>, node_id: &PublicKey) -> bool {
	lsp_nodes.read().unwrap().iter().any(|n| n.node_id == *node_id)
}

#[derive(Debug, Clone)]
pub(crate) struct LspConfig {
	pub node_id: PublicKey,
	pub address: SocketAddress,
	pub token: Option<String>,
}

pub(crate) struct LspNode {
	node_id: PublicKey,
	address: SocketAddress,
	token: Option<String>,
	// Protocol numbers discovered via LSPS0 (e.g., 1 = LSPS1, 2 = LSPS2, 5 = LSPS5).
	supported_protocols: Mutex<Option<Vec<u16>>>,
}

pub(crate) struct LiquiditySourceBuilder<L: Deref>
where
	L::Target: LdkLogger,
{
	lsp_nodes: Vec<LspConfig>,
	lsps2_service: Option<LSPS2Service>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	tx_broadcaster: Arc<Broadcaster>,
	kv_store: Arc<DynStore>,
	config: Arc<Config>,
	logger: L,
}

impl<L: Deref + Clone> LiquiditySourceBuilder<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>, keys_manager: Arc<KeysManager>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>, logger: L,
	) -> Self {
		let lsp_nodes = Vec::new();
		let lsps2_service = None;
		Self {
			lsp_nodes,
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

	pub(crate) fn set_lsp_nodes(&mut self, lsp_nodes: Vec<LspConfig>) -> &mut Self {
		self.lsp_nodes = lsp_nodes;
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

		// Adding LSPS at runtime is now supported, so we create the client
		// config regardless of whether LSPs exist at build time
		let liquidity_client_config = Some(LiquidityClientConfig {
			lsps1_client_config: Some(LdkLSPS1ClientConfig { max_channel_fees_msat: None }),
			lsps2_client_config: Some(LdkLSPS2ClientConfig {}),
			lsps5_client_config: None,
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

		let lsp_nodes = Arc::new(RwLock::new(
			self.lsp_nodes
				.into_iter()
				.map(|cfg| LspNode {
					node_id: cfg.node_id,
					address: cfg.address,
					token: cfg.token,
					supported_protocols: Mutex::new(None),
				})
				.collect(),
		));

		Ok(LiquiditySource {
			lsp_nodes: Arc::clone(&lsp_nodes),
			lsps1_client: Arc::new(LSPS1LiquiditySource {
				lsp_nodes: Arc::clone(&lsp_nodes),
				pending_lsps1_opening_params_requests: Mutex::new(HashMap::new()),
				pending_lsps1_create_order_requests: Mutex::new(HashMap::new()),
				pending_lsps1_check_order_status_requests: Mutex::new(HashMap::new()),
				lsps1_order_lsp_map: Mutex::new(HashMap::new()),
				liquidity_manager: Arc::clone(&liquidity_manager),
				logger: self.logger.clone(),
			}),
			lsps2_client: Arc::new(LSPS2ClientLiquiditySource {
				lsp_nodes: Arc::clone(&lsp_nodes),
				pending_lsps2_fee_requests: Mutex::new(HashMap::new()),
				pending_lsps2_buy_requests: Mutex::new(HashMap::new()),
				channel_manager: self.channel_manager.clone(),
				keys_manager: self.keys_manager.clone(),
				liquidity_manager: Arc::clone(&liquidity_manager),
				config: self.config.clone(),
				logger: self.logger.clone(),
			}),
			lsps2_service: Arc::new(LSPS2ServiceLiquiditySource {
				lsps2_service: self.lsps2_service,
				wallet: self.wallet,
				channel_manager: self.channel_manager,
				peer_manager: RwLock::new(None),
				keys_manager: self.keys_manager,
				liquidity_manager: Arc::clone(&liquidity_manager),
				config: self.config.clone(),
				logger: self.logger.clone(),
			}),
			pending_lsps0_discovery: Mutex::new(HashMap::new()),
			liquidity_manager,
			logger: self.logger,
		})
	}
}

pub(crate) struct LiquiditySource<L: Deref>
where
	L::Target: LdkLogger,
{
	lsp_nodes: Arc<RwLock<Vec<LspNode>>>,
	lsps1_client: Arc<LSPS1LiquiditySource<L>>,
	lsps2_client: Arc<LSPS2ClientLiquiditySource<L>>,
	lsps2_service: Arc<LSPS2ServiceLiquiditySource<L>>,
	pending_lsps0_discovery: Mutex<HashMap<PublicKey, oneshot::Sender<Vec<u16>>>>,
	liquidity_manager: Arc<LiquidityManager>,
	logger: L,
}

impl<L: Deref> LiquiditySource<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn liquidity_manager(&self) -> Arc<LiquidityManager> {
		Arc::clone(&self.liquidity_manager)
	}

	pub(crate) fn lsps1_client(&self) -> Arc<LSPS1LiquiditySource<L>> {
		Arc::clone(&self.lsps1_client)
	}

	pub(crate) fn lsps2_client(&self) -> Arc<LSPS2ClientLiquiditySource<L>> {
		Arc::clone(&self.lsps2_client)
	}

	pub(crate) fn lsps2_service(&self) -> Arc<LSPS2ServiceLiquiditySource<L>> {
		Arc::clone(&self.lsps2_service)
	}

	pub(crate) async fn handle_next_event(&self) {
		match self.liquidity_manager.next_event_async().await {
			LiquidityEvent::LSPS1Client(event) => self.lsps1_client.handle_next_event(event).await,
			LiquidityEvent::LSPS2Client(event) => self.lsps2_client.handle_next_event(event).await,
			LiquidityEvent::LSPS2Service(event) => {
				self.lsps2_service.handle_next_event(event).await
			},

			LiquidityEvent::LSPS0Client(LSPS0ClientEvent::ListProtocolsResponse {
				counterparty_node_id,
				protocols,
			}) => {
				if self.is_lsps_node(&counterparty_node_id) {
					if let Some(sender) =
						self.pending_lsps0_discovery.lock().unwrap().remove(&counterparty_node_id)
					{
						match sender.send(protocols) {
							Ok(()) => (),
							Err(_) => {
								log_error!(
									self.logger,
									"Failed to handle response for request {:?} from liquidity service",
									counterparty_node_id
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
						"Received LSPS0 ListProtocolsResponse from unexpected counterparty {}.",
						counterparty_node_id
					);
				}
			},
			e => {
				log_error!(self.logger, "Received unexpected liquidity event: {:?}", e);
			},
		}
	}

	pub(crate) fn is_lsps_node(&self, node_id: &PublicKey) -> bool {
		is_lsps_node(&self.lsp_nodes, node_id)
	}

	pub(crate) fn get_all_lsp_details(&self) -> Vec<(PublicKey, SocketAddress)> {
		self.lsp_nodes.read().unwrap().iter().map(|n| (n.node_id, n.address.clone())).collect()
	}

	pub(crate) fn add_lsp_node(&self, lsp_node: LspConfig) -> Result<(), Error> {
		self.lsp_nodes.write().unwrap().push(LspNode {
			node_id: lsp_node.node_id,
			address: lsp_node.address.clone(),
			token: lsp_node.token.clone(),
			supported_protocols: Mutex::new(None),
		});

		Ok(())
	}

	pub(crate) async fn discover_lsp_protocols(
		&self, node_id: &PublicKey,
	) -> Result<Vec<u16>, Error> {
		let lsps0_handler = self.liquidity_manager.lsps0_client_handler();

		let (sender, receiver) = oneshot::channel();
		{
			let mut pending_discovery = self.pending_lsps0_discovery.lock().unwrap();
			lsps0_handler.list_protocols(node_id);
			pending_discovery.insert(*node_id, sender);
		}

		let protocols =
			tokio::time::timeout(Duration::from_secs(LIQUIDITY_REQUEST_TIMEOUT_SECS), receiver)
				.await
				.map_err(|e| {
					log_error!(
						self.logger,
						"LSPS0 discovery request timed out for {}: {}",
						node_id,
						e
					);
					Error::LiquidityRequestFailed
				})?
				.map_err(|e| {
					log_error!(
						self.logger,
						"Failed to handle LSPS0 discovery response from {}: {}",
						node_id,
						e
					);
					Error::LiquidityRequestFailed
				})?;

		if let Some(lsp_node) =
			self.lsp_nodes.read().unwrap().iter().find(|n| &n.node_id == node_id)
		{
			*lsp_node.supported_protocols.lock().unwrap() = Some(protocols.clone());
		}

		Ok(protocols)
	}

	pub(crate) async fn discover_all_lsp_protocols(&self) {
		let node_ids: Vec<PublicKey> =
			self.lsp_nodes.read().unwrap().iter().map(|n| n.node_id).collect();
		for node_id in &node_ids {
			match self.discover_lsp_protocols(node_id).await {
				Ok(protocols) => {
					log_info!(
						self.logger,
						"Discovered protocols for LSP {}: {:?}",
						node_id,
						protocols
					);
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to discover protocols for LSP {}: {:?}",
						node_id,
						e
					);
				},
			}
		}
	}
}
