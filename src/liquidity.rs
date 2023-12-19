use crate::types::{ChannelManager, KeysManager, LiquidityManager, PeerManager};
use crate::Config;

use lightning::ln::msgs::SocketAddress;
use lightning::util::logger::Logger;
use lightning::util::persist::KVStore;
use lightning_liquidity::events::Event;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;

use bitcoin::secp256k1::PublicKey;

use std::ops::Deref;
use std::sync::Arc;

struct LSPS2Service {
	address: SocketAddress,
	node_id: PublicKey,
	token: Option<String>,
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
		let lsps2_service = Some(LSPS2Service { address, node_id, token });
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
				counterparty_node_id: _,
				opening_fee_params_menu: _,
				min_payment_size_msat: _,
				max_payment_size_msat: _,
			}) => {}
			Event::LSPS2Client(LSPS2ClientEvent::InvoiceParametersReady {
				counterparty_node_id: _,
				intercept_scid: _,
				cltv_expiry_delta: _,
				payment_size_msat: _,
				user_channel_id: _,
			}) => {}
			_ => {}
		}
	}
}
