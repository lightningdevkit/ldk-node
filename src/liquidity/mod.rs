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
use lightning::ln::msgs::SocketAddress;
use lightning_liquidity::events::LiquidityEvent;
use lightning_liquidity::lsps1::client::LSPS1ClientConfig as LdkLSPS1ClientConfig;
use lightning_liquidity::lsps2::client::LSPS2ClientConfig as LdkLSPS2ClientConfig;
use lightning_liquidity::lsps2::service::LSPS2ServiceConfig as LdkLSPS2ServiceConfig;
use lightning_liquidity::{LiquidityClientConfig, LiquidityServiceConfig};

use crate::builder::BuildError;
use crate::logger::{log_error, LdkLogger};
use crate::types::{
	Broadcaster, ChannelManager, DynStore, KeysManager, LiquidityManager, PeerManager, Wallet,
};
use crate::Config;

pub(crate) use client::lsps1::{LSPS1Client, LSPS1ClientConfig};
pub use client::lsps1::{LSPS1Liquidity, LSPS1OrderStatus};
pub(crate) use client::lsps2::{LSPS2Client, LSPS2ClientConfig};
pub(crate) use service::lsps2::LSPS2Service;
pub use service::lsps2::LSPS2ServiceConfig;

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
			LiquidityEvent::LSPS1Client(event) => {
				if let Some(lsps1_client) = self.lsps1_client.as_ref() {
					lsps1_client.handle_event(event, &self.logger).await;
				} else {
					log_error!(self.logger, "Received unexpected LSPS1Client event!");
				}
			},
			LiquidityEvent::LSPS2Service(event) => {
				if let Some(lsps2_service) = self.lsps2_service.as_ref() {
					lsps2_service
						.handle_event(
							event,
							&self.liquidity_manager,
							&self.channel_manager,
							&self.keys_manager,
							&self.peer_manager,
							&self.wallet,
							&self.config,
							&self.logger,
						)
						.await;
				} else {
					log_error!(self.logger, "Failed to handle LSPS2ServiceEvent as LSPS2 liquidity service was not configured.",);
				}
			},
			LiquidityEvent::LSPS2Client(event) => {
				if let Some(lsps2_client) = self.lsps2_client.as_ref() {
					lsps2_client.handle_event(event, &self.logger).await;
				} else {
					log_error!(self.logger, "Received unexpected LSPS2Client event!");
				}
			},
			e => {
				log_error!(self.logger, "Received unexpected liquidity event: {:?}", e);
			},
		}
	}
}
