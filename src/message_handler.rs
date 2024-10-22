// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::liquidity::LiquiditySource;
use crate::StableChannelsMessage;

use lightning::ln::features::{InitFeatures, NodeFeatures};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::{CustomMessageReader, Type};
use lightning::log_info;
use lightning::util::logger::Logger;

use lightning::util::ser::{Readable, Writeable, Writer};
use lightning_liquidity::lsps0::ser::RawLSPSMessage;

use bitcoin::secp256k1::PublicKey;

use std::ops::Deref;
use std::sync::Arc;
use lightning::ln::msgs::DecodeError;
use lightning::ln::msgs::LightningError;

#[derive(Debug)]
pub enum NodeCustomMessage {
    RawLSPSMessage(RawLSPSMessage),
    StableChannelsMessage(StableChannelsMessage),
}

impl Writeable for NodeCustomMessage {
    fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
        match self {
            NodeCustomMessage::RawLSPSMessage(msg) => msg.write(writer),
            NodeCustomMessage::StableChannelsMessage(msg) => msg.write(writer),
        }
    }
}

impl Type for NodeCustomMessage {
    fn type_id(&self) -> u16 {
        match self {
            NodeCustomMessage::RawLSPSMessage(msg) => msg.type_id(),
            NodeCustomMessage::StableChannelsMessage(msg) => msg.type_id(),
        }
    }
}

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
    type CustomMessage = NodeCustomMessage;

    fn read<RD: lightning::io::Read>(
        &self,
        message_type: u16,
        buffer: &mut RD,
    ) -> Result<Option<Self::CustomMessage>, DecodeError> {
        if message_type == StableChannelsMessage::TYPE_ID {
            let msg = StableChannelsMessage::read(buffer)?;
            Ok(Some(NodeCustomMessage::StableChannelsMessage(msg)))
        } else {
            match self {
                Self::Ignoring => Ok(None),
                Self::Liquidity { liquidity_source, .. } => {
                    let res = liquidity_source
                        .liquidity_manager()
                        .read(message_type, buffer)?;
                    Ok(res.map(NodeCustomMessage::RawLSPSMessage))
                }
            }
        }
    }
}
impl<L: Deref> CustomMessageHandler for NodeCustomMessageHandler<L>
where
    L::Target: Logger,
{
    fn handle_custom_message(
        &self,
        msg: Self::CustomMessage,
        sender_node_id: &PublicKey,
    ) -> Result<(), LightningError> {
        match msg {
            NodeCustomMessage::StableChannelsMessage(msg) => {
				println!("Received StableChannelsMessage");
                // Handle the StableChannelsMessage here
                // For example, log the message
                // log_info!(
                //     self.logger,
                //     "Received StableChannelsMessage from {}: {}",
                //     sender_node_id,
                //     msg.message
                // );
                Ok(())
            }
            NodeCustomMessage::RawLSPSMessage(raw_msg) => match self {
                Self::Ignoring => Ok(()),
                Self::Liquidity { liquidity_source, .. } => {
                    liquidity_source
                        .liquidity_manager()
                        .handle_custom_message(raw_msg, sender_node_id)
                }
            },
        }
    }

    fn get_and_clear_pending_msg(&self) -> Vec<(PublicKey, Self::CustomMessage)> {
        let mut pending_msgs = Vec::new();

        // Collect pending messages from the liquidity manager if applicable
        if let Self::Liquidity { liquidity_source, .. } = self {
            let liquidity_msgs = liquidity_source
                .liquidity_manager()
                .get_and_clear_pending_msg()
                .into_iter()
                .map(|(pk, msg)| (pk, NodeCustomMessage::RawLSPSMessage(msg)));
            pending_msgs.extend(liquidity_msgs);
        }

        // If you have any pending StableChannelsMessages to send, add them here
        // For now, we'll assume there are none
        pending_msgs
    }

    fn provided_node_features(&self) -> NodeFeatures {
        let mut features = NodeFeatures::empty();
        // Add any features required by StableChannelsMessage
        // For now, we assume none are needed
        if let Self::Liquidity { liquidity_source, .. } = self {
            features |= liquidity_source
                .liquidity_manager()
                .provided_node_features();
        }
        features
    }

    fn provided_init_features(&self, their_node_id: &PublicKey) -> InitFeatures {
        let mut features = InitFeatures::empty();
        // Add any features required by StableChannelsMessage
        // For now, we assume none are needed
        if let Self::Liquidity { liquidity_source, .. } = self {
            features |= liquidity_source
                .liquidity_manager()
                .provided_init_features(their_node_id);
        }
        features
    }

    fn peer_connected(
        &self,
        their_node_id: &PublicKey,
        msg: &lightning::ln::msgs::Init,
        inbound: bool,
    ) -> Result<(), ()> {
        match self {
            Self::Ignoring => Ok(()),
            Self::Liquidity { liquidity_source, .. } => liquidity_source
                .liquidity_manager()
                .peer_connected(their_node_id, msg, inbound),
        }
    }

    fn peer_disconnected(&self, their_node_id: &PublicKey) {
        if let Self::Liquidity { liquidity_source, .. } = self {
            liquidity_source
                .liquidity_manager()
                .peer_disconnected(their_node_id);
        }
    }
}