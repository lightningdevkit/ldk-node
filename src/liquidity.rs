use crate::logger::log_error;
use crate::types::{ChannelManager, KeysManager, LiquidityManager, PeerManager};
use crate::Config;

use lightning::ln::msgs::SocketAddress;
use lightning::util::logger::Logger;
use lightning::util::persist::KVStore;
use lightning_liquidity::events::Event;
use lightning_liquidity::lsps0::msgs::RequestId;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::OpeningFeeParams;

use bitcoin::secp256k1::PublicKey;

use tokio::sync::oneshot;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

struct LSPS2Service {
	address: SocketAddress,
	node_id: PublicKey,
	token: Option<String>,
	pending_fee_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS2FeeResponse>>>,
	pending_buy_requests: Mutex<HashMap<RequestId, oneshot::Sender<LSPS2BuyResponse>>>,
}

pub(crate) struct LiquiditySource<K: KVStore + Sync + Send + 'static, L: Deref>
where
	L::Target: Logger,
{
	lsps2_service: Option<LSPS2Service>,
	channel_manager: Arc<ChannelManager<K>>,
	keys_manager: Arc<KeysManager>,
	liquidity_manager: Arc<LiquidityManager<K>>,
	config: Arc<Config>,
	logger: L,
}

impl<K: KVStore + Sync + Send, L: Deref> LiquiditySource<K, L>
where
	L::Target: Logger,
{
	pub(crate) fn new_lsps2(
		address: SocketAddress, node_id: PublicKey, token: Option<String>,
		channel_manager: Arc<ChannelManager<K>>, keys_manager: Arc<KeysManager>,
		liquidity_manager: Arc<LiquidityManager<K>>, config: Arc<Config>, logger: L,
	) -> Self {
		let pending_fee_requests = Mutex::new(HashMap::new());
		let pending_buy_requests = Mutex::new(HashMap::new());
		let lsps2_service = Some(LSPS2Service {
			address,
			node_id,
			token,
			pending_fee_requests,
			pending_buy_requests,
		});
		Self { lsps2_service, channel_manager, keys_manager, liquidity_manager, config, logger }
	}

	pub(crate) fn set_peer_manager(&self, peer_manager: Arc<PeerManager<K>>) {
		let process_msgs_callback = move || peer_manager.process_events();
		self.liquidity_manager.set_process_msgs_callback(process_msgs_callback);
	}

	pub(crate) fn liquidity_manager(&self) -> &LiquidityManager<K> {
		self.liquidity_manager.as_ref()
	}

	pub(crate) async fn handle_next_event(&self) {
		match self.liquidity_manager.next_event_async().await {
			Event::LSPS2Client(LSPS2ClientEvent::OpeningParametersReady {
				request_id,
				counterparty_node_id,
				opening_fee_params_menu,
			}) => {
				if let Some(lsps2_service) = self.lsps2_service.as_ref() {
					if counterparty_node_id != lsps2_service.node_id {
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
						lsps2_service.pending_fee_requests.lock().unwrap().remove(&request_id)
					{
						let response = LSPS2FeeResponse {
							opening_fee_params_menu,
						};

						match sender.send(response) {
							Ok(()) => (),
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to handle response from liquidity service: {:?}",
									e
								);
							}
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
			}
			Event::LSPS2Client(LSPS2ClientEvent::InvoiceParametersReady {
				request_id,
				counterparty_node_id,
				intercept_scid,
				cltv_expiry_delta,
				..
			}) => {
				if let Some(lsps2_service) = self.lsps2_service.as_ref() {
					if counterparty_node_id != lsps2_service.node_id {
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
						lsps2_service.pending_buy_requests.lock().unwrap().remove(&request_id)
					{
						let response = LSPS2BuyResponse { intercept_scid, cltv_expiry_delta };

						match sender.send(response) {
							Ok(()) => (),
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to handle response from liquidity service: {:?}",
									e
								);
							}
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
			}
			e => {
				log_error!(self.logger, "Received unexpected liquidity event: {:?}", e);
			}
		}
	}
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2FeeResponse {
	opening_fee_params_menu: Vec<OpeningFeeParams>,
}

#[derive(Debug, Clone)]
pub(crate) struct LSPS2BuyResponse {
	intercept_scid: u64,
	cltv_expiry_delta: u32,
}
