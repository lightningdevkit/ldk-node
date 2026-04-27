// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects related to liquidity management.

pub(crate) mod client;
pub(crate) mod service;

pub use client::lsps1::LSPS1Liquidity;
pub use client::LSPS1OrderStatus;
pub use service::lsps2::LSPS2ServiceConfig;

use std::collections::hash_map::Entry;
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
use crate::connection::ConnectionManager;
use crate::liquidity::client::lsps1::LSPS1LiquiditySource;
use crate::liquidity::client::lsps2::LSPS2ClientLiquiditySource;
use crate::liquidity::service::lsps2::{LSPS2Service, LSPS2ServiceLiquiditySource};
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{Broadcaster, ChannelManager, DynStore, KeysManager, LiquidityManager, Wallet};
use crate::{Config, Error};

const LIQUIDITY_REQUEST_TIMEOUT_SECS: u64 = 5;
const LSPS_DISCOVERY_WAIT_TIMEOUT_SECS: u64 = 10;

fn select_lsps_for_protocol(
	lsp_nodes: &Arc<RwLock<Vec<LspNode>>>, protocol: u16, override_node_id: Option<&PublicKey>,
) -> Option<LspConfig> {
	lsp_nodes
		.read()
		.expect("lock")
		.iter()
		.find(|lsp_node| {
			if let Some(override_node_id) = override_node_id {
				lsp_node.node_id == *override_node_id
					&& lsp_node
						.supported_protocols
						.lock()
						.expect("lock")
						.as_ref()
						.map(|protocols| protocols.contains(&protocol))
						.unwrap_or(false)
			} else {
				lsp_node
					.supported_protocols
					.lock()
					.expect("lock")
					.as_ref()
					.map(|protocols| protocols.contains(&protocol))
					.unwrap_or(false)
			}
		})
		.map(|n| LspConfig {
			node_id: n.node_id,
			address: n.address.clone(),
			token: n.token.clone(),
			trust_peer_0conf: n.trust_peer_0conf,
		})
}

fn select_all_lsps_for_protocol(
	lsp_nodes: &Arc<RwLock<Vec<LspNode>>>, protocol: u16,
) -> Vec<LspConfig> {
	lsp_nodes
		.read()
		.expect("lock")
		.iter()
		.filter(|lsp_node| {
			lsp_node
				.supported_protocols
				.lock()
				.expect("lock")
				.as_ref()
				.map(|protocols| protocols.contains(&protocol))
				.unwrap_or(false)
		})
		.map(|n| LspConfig {
			node_id: n.node_id,
			address: n.address.clone(),
			token: n.token.clone(),
			trust_peer_0conf: n.trust_peer_0conf,
		})
		.collect()
}

/// A liquidity handler allowing to manage LSP connections and request channels.
///
/// Should be retrieved by calling [`Node::liquidity`].
///
/// [`Node::liquidity`]: crate::Node::liquidity
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct Liquidity {
	runtime: Arc<Runtime>,
	wallet: Arc<Wallet>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>,
	logger: Arc<Logger>,
}

impl Liquidity {
	pub(crate) fn new(
		runtime: Arc<Runtime>, wallet: Arc<Wallet>,
		connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
		liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>, logger: Arc<Logger>,
	) -> Self {
		Self { runtime, wallet, connection_manager, liquidity_source, logger }
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl Liquidity {
	/// Adds an LSP as an inbound liquidity source at runtime.
	///
	/// LSP nodes are automatically trusted for 0-confirmation channels.
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	pub fn add_liquidity_source(
		&self, node_id: PublicKey, address: SocketAddress, token: Option<String>,
		trust_peer_0conf: bool,
	) -> Result<(), Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		{
			let mut lsp_nodes = liquidity_source.lsp_nodes.write().expect("lock");
			if lsp_nodes.iter().any(|n| n.node_id == node_id) {
				log_error!(self.logger, "LSP node {} is already added.", node_id);
				return Err(Error::DuplicateLspNode);
			}

			lsp_nodes.push(LspNode {
				node_id,
				address: address.clone(),
				token: token.clone(),
				trust_peer_0conf,
				supported_protocols: Mutex::new(None),
			});
		}

		let con_cm = Arc::clone(&self.connection_manager);
		let connect_addr = address.clone();

		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(node_id, connect_addr).await
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}.", node_id, address);

		self.runtime
			.block_on(async move { liquidity_source.discover_lsp_protocols(&node_id).await })?;
		Ok(())
	}

	/// Returns a liquidity handler allowing to request channels via the [bLIP-51 / LSPS1] protocol.
	///
	/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
	pub fn lsps1(&self) -> LSPS1Liquidity {
		LSPS1Liquidity::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.wallet),
			Arc::clone(&self.connection_manager),
			self.liquidity_source.as_ref().map(|ls| ls.lsps1_client()),
			Arc::clone(&self.logger),
		)
	}
}

#[derive(Debug, Clone)]
pub(crate) struct LspConfig {
	pub node_id: PublicKey,
	pub address: SocketAddress,
	pub token: Option<String>,
	pub trust_peer_0conf: bool,
}

pub(crate) struct LspNode {
	node_id: PublicKey,
	address: SocketAddress,
	token: Option<String>,
	trust_peer_0conf: bool,
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

		let (discovery_done_tx, discovery_done_rx) = tokio::sync::watch::channel(false);

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
					trust_peer_0conf: cfg.trust_peer_0conf,
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
				discovery_done_rx: discovery_done_rx.clone(),
				liquidity_manager: Arc::clone(&liquidity_manager),
				logger: self.logger.clone(),
			}),
			lsps2_client: Arc::new(LSPS2ClientLiquiditySource {
				lsp_nodes: Arc::clone(&lsp_nodes),
				pending_lsps2_fee_requests: Mutex::new(HashMap::new()),
				pending_lsps2_buy_requests: Mutex::new(HashMap::new()),
				channel_manager: self.channel_manager.clone(),
				keys_manager: self.keys_manager.clone(),
				discovery_done_rx,
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
			discovery_done_tx,
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
	discovery_done_tx: tokio::sync::watch::Sender<bool>,
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
					if let Some(sender) = self
						.pending_lsps0_discovery
						.lock()
						.expect("lock")
						.remove(&counterparty_node_id)
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
		self.lsp_nodes.read().expect("lock").iter().any(|n| n.node_id == *node_id)
	}

	pub(crate) fn get_all_lsp_details(&self) -> Vec<(PublicKey, SocketAddress)> {
		self.lsp_nodes
			.read()
			.expect("lock")
			.iter()
			.map(|n| (n.node_id, n.address.clone()))
			.collect()
	}

	pub(crate) fn get_lsp_config(
		&self, node_id: &PublicKey, protocol: Option<u16>,
	) -> Option<LspConfig> {
		self.lsp_nodes
			.read()
			.expect("lock")
			.iter()
			.find(|lsp_node| {
				&lsp_node.node_id == node_id && {
					if let Some(protocol) = protocol {
						lsp_node
							.supported_protocols
							.lock()
							.expect("lock")
							.as_ref()
							.map(|protocols| protocols.contains(&protocol))
							.unwrap_or(false)
					} else {
						true
					}
				}
			})
			.map(|n| LspConfig {
				node_id: n.node_id,
				address: n.address.clone(),
				token: n.token.clone(),
				trust_peer_0conf: n.trust_peer_0conf,
			})
	}

	pub(crate) async fn discover_lsp_protocols(
		&self, node_id: &PublicKey,
	) -> Result<Vec<u16>, Error> {
		let lsps0_handler = self.liquidity_manager.lsps0_client_handler();

		let (sender, receiver) = oneshot::channel();
		{
			let mut pending_discovery = self.pending_lsps0_discovery.lock().expect("lock");
			match pending_discovery.entry(*node_id) {
				Entry::Occupied(_) => {
					log_error!(
						self.logger,
						"LSPS0 protocol discovery already in flight for {}",
						node_id
					);
					return Err(Error::LiquidityRequestFailed);
				},
				Entry::Vacant(v) => {
					v.insert(sender);
					lsps0_handler.list_protocols(node_id); // ← moved after insert
				},
			}
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
					self.pending_lsps0_discovery.lock().expect("lock").remove(node_id);
					Error::LiquidityRequestFailed
				})?
				.map_err(|e| {
					log_error!(
						self.logger,
						"Failed to handle LSPS0 discovery response from {}: {}",
						node_id,
						e
					);
					self.pending_lsps0_discovery.lock().expect("lock").remove(node_id);
					Error::LiquidityRequestFailed
				})?;

		if let Some(lsp_node) =
			self.lsp_nodes.read().expect("lock").iter().find(|n| &n.node_id == node_id)
		{
			*lsp_node.supported_protocols.lock().expect("lock") = Some(protocols.clone());
		}

		Ok(protocols)
	}

	pub(crate) fn mark_discovery_done(&self) {
		let _ = self.discovery_done_tx.send(true);
	}
}
