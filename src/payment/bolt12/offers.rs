// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::Arc;

use lightning::blinded_path::message::OffersContext;
use lightning::onion_message::messenger::{
	MessageSendInstructions, Responder, ResponseInstruction,
};
use lightning::onion_message::offers::{OffersMessage, OffersMessageHandler};

use crate::types::ChannelManager;

/// Routes offers messages through node-local handling before falling back to the channel manager.
pub(crate) struct NodeOffersMessageHandler {
	channel_manager: Arc<ChannelManager>,
}

impl NodeOffersMessageHandler {
	pub(crate) fn new(channel_manager: Arc<ChannelManager>) -> Self {
		Self { channel_manager }
	}
}

impl OffersMessageHandler for NodeOffersMessageHandler {
	fn handle_message(
		&self, message: OffersMessage, context: Option<OffersContext>, responder: Option<Responder>,
	) -> Option<(OffersMessage, ResponseInstruction)> {
		self.channel_manager.handle_message(message, context, responder)
	}

	fn release_pending_messages(&self) -> Vec<(OffersMessage, MessageSendInstructions)> {
		self.channel_manager.release_pending_messages()
	}
}
