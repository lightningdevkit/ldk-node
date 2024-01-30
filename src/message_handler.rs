use crate::liquidity::LiquiditySource;

use lightning::ln::features::{InitFeatures, NodeFeatures};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::CustomMessageReader;
use lightning::util::logger::Logger;

use lightning_liquidity::lsps0::ser::RawLSPSMessage;

use bitcoin::secp256k1::PublicKey;

use std::ops::Deref;
use std::sync::Arc;

pub(crate) enum NodeCustomMessageHandler<L: Deref>
where
	L::Target: Logger,
{
	Ignoring,
	Liquidity { liquidity_source: Arc<LiquiditySource<L>> },
}

impl<L: Deref> NodeCustomMessageHandler<L>
where
	L::Target: Logger,
{
	pub(crate) fn new_liquidity(liquidity_source: Arc<LiquiditySource<L>>) -> Self {
		Self::Liquidity { liquidity_source }
	}

	pub(crate) fn new_ignoring() -> Self {
		Self::Ignoring
	}
}

impl<L: Deref> CustomMessageReader for NodeCustomMessageHandler<L>
where
	L::Target: Logger,
{
	type CustomMessage = RawLSPSMessage;

	fn read<RD: lightning::io::Read>(
		&self, message_type: u16, buffer: &mut RD,
	) -> Result<Option<Self::CustomMessage>, lightning::ln::msgs::DecodeError> {
		match self {
			Self::Ignoring => Ok(None),
			Self::Liquidity { liquidity_source, .. } => {
				liquidity_source.liquidity_manager().read(message_type, buffer)
			},
		}
	}
}

impl<L: Deref> CustomMessageHandler for NodeCustomMessageHandler<L>
where
	L::Target: Logger,
{
	fn handle_custom_message(
		&self, msg: Self::CustomMessage, sender_node_id: &PublicKey,
	) -> Result<(), lightning::ln::msgs::LightningError> {
		match self {
			Self::Ignoring => Ok(()), // Should be unreachable!() as the reader will return `None`
			Self::Liquidity { liquidity_source, .. } => {
				liquidity_source.liquidity_manager().handle_custom_message(msg, sender_node_id)
			},
		}
	}

	fn get_and_clear_pending_msg(&self) -> Vec<(PublicKey, Self::CustomMessage)> {
		match self {
			Self::Ignoring => Vec::new(),
			Self::Liquidity { liquidity_source, .. } => {
				liquidity_source.liquidity_manager().get_and_clear_pending_msg()
			},
		}
	}

	fn provided_node_features(&self) -> NodeFeatures {
		match self {
			Self::Ignoring => NodeFeatures::empty(),
			Self::Liquidity { liquidity_source, .. } => {
				liquidity_source.liquidity_manager().provided_node_features()
			},
		}
	}

	fn provided_init_features(&self, their_node_id: &PublicKey) -> InitFeatures {
		match self {
			Self::Ignoring => InitFeatures::empty(),
			Self::Liquidity { liquidity_source, .. } => {
				liquidity_source.liquidity_manager().provided_init_features(their_node_id)
			},
		}
	}
}
