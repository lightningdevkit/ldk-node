// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::{Arc, RwLock};

use bitcoin::block::Header;
use bitcoin::constants::ChainHash;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{Block, BlockHash, Network, Txid};
use lightning::blinded_path::message::OffersContext;
use lightning::chain::transaction::TransactionData;
use lightning::chain::{BlockLocator, Confirm, Listen};
use lightning::offers::flow::OffersMessageFlow;
use lightning::onion_message::messenger::{
	MessageSendInstructions, Responder, ResponseInstruction,
};
use lightning::onion_message::offers::{OffersMessage, OffersMessageHandler};
use lightning::sign::{EntropySource, NodeSigner, Recipient};

use crate::logger::Logger;
use crate::types::{ChannelManager, KeysManager, MessageRouter};

/// Routes offers messages through node-local handling before falling back to the channel manager.
pub(crate) struct NodeOffersMessageHandler {
	channel_manager: Arc<ChannelManager>,
	flow: OffersMessageFlow<Arc<MessageRouter>, Arc<Logger>>,
	best_block: RwLock<BlockLocator>,
}

impl NodeOffersMessageHandler {
	pub(crate) fn new(
		network: Network, current_timestamp: u32, channel_manager: Arc<ChannelManager>,
		keys_manager: Arc<KeysManager>, message_router: Arc<MessageRouter>, logger: Arc<Logger>,
	) -> Self {
		let best_block = channel_manager.current_best_block();
		let mut secp_ctx = Secp256k1::new();
		secp_ctx.seeded_randomize(&keys_manager.get_secure_random_bytes());
		let flow = OffersMessageFlow::new(
			ChainHash::using_genesis_block(network),
			best_block,
			keys_manager.get_node_id(Recipient::Node).expect("node ID"),
			current_timestamp,
			keys_manager.get_expanded_key(),
			keys_manager.get_receive_auth_key(),
			secp_ctx,
			message_router,
			logger,
		);
		Self { channel_manager, flow, best_block: RwLock::new(best_block) }
	}

	pub(crate) fn current_best_block(&self) -> BlockLocator {
		*self.best_block.read().expect("lock")
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

impl Confirm for NodeOffersMessageHandler {
	fn transactions_confirmed(&self, _header: &Header, _txdata: &TransactionData, _height: u32) {}

	fn transaction_unconfirmed(&self, _txid: &Txid) {}

	fn best_block_updated(&self, header: &Header, height: u32) {
		let best_block = BlockLocator::new(header.block_hash(), height);
		*self.best_block.write().expect("lock") = best_block;
		self.flow.best_block_updated(header, height);
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		Vec::new()
	}
}

impl Listen for NodeOffersMessageHandler {
	fn filtered_block_connected(&self, header: &Header, _txdata: &TransactionData, height: u32) {
		self.best_block_updated(header, height);
	}

	fn block_connected(&self, block: &Block, height: u32) {
		self.best_block_updated(&block.header, height);
	}

	fn blocks_disconnected(&self, fork_point_block: BlockLocator) {
		*self.best_block.write().expect("lock") = fork_point_block;
	}
}
