// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::types::{ChainMonitor, ChannelManager, Sweeper, Wallet};

use lightning::chain::Listen;

use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::poll::ValidatedBlockHeader;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{
	AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource, Cache,
};

use bitcoin::BlockHash;

use base64::prelude::{Engine, BASE64_STANDARD};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

pub struct BitcoindRpcClient {
	rpc_client: Arc<RpcClient>,
}

impl BitcoindRpcClient {
	pub(crate) fn new(host: String, port: u16, rpc_user: String, rpc_password: String) -> Self {
		let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
		let rpc_credentials =
			BASE64_STANDARD.encode(format!("{}:{}", rpc_user.clone(), rpc_password.clone()));

		let rpc_client = Arc::new(
			RpcClient::new(&rpc_credentials, http_endpoint)
				.expect("RpcClient::new is actually infallible"),
		);

		Self { rpc_client }
	}
}

impl BlockSource for BitcoindRpcClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a BlockHash, height_hint: Option<u32>,
	) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
		Box::pin(async move { self.rpc_client.get_header(header_hash, height_hint).await })
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a BlockHash,
	) -> AsyncBlockSourceResult<'a, BlockData> {
		Box::pin(async move { self.rpc_client.get_block(header_hash).await })
	}

	fn get_best_block<'a>(&'a self) -> AsyncBlockSourceResult<(BlockHash, Option<u32>)> {
		Box::pin(async move { self.rpc_client.get_best_block().await })
	}
}

const MAX_HEADER_CACHE_ENTRIES: usize = 100;

pub(crate) struct BoundedHeaderCache {
	header_map: HashMap<BlockHash, ValidatedBlockHeader>,
	recently_seen: VecDeque<BlockHash>,
}

impl BoundedHeaderCache {
	pub(crate) fn new() -> Self {
		let header_map = HashMap::new();
		let recently_seen = VecDeque::new();
		Self { header_map, recently_seen }
	}
}

impl Cache for BoundedHeaderCache {
	fn look_up(&self, block_hash: &BlockHash) -> Option<&ValidatedBlockHeader> {
		self.header_map.get(block_hash)
	}

	fn block_connected(&mut self, block_hash: BlockHash, block_header: ValidatedBlockHeader) {
		self.recently_seen.push_back(block_hash);
		self.header_map.insert(block_hash, block_header);

		if self.header_map.len() >= MAX_HEADER_CACHE_ENTRIES {
			// Keep dropping old entries until we've actually removed a header entry.
			while let Some(oldest_entry) = self.recently_seen.pop_front() {
				if self.header_map.remove(&oldest_entry).is_some() {
					break;
				}
			}
		}
	}

	fn block_disconnected(&mut self, block_hash: &BlockHash) -> Option<ValidatedBlockHeader> {
		self.recently_seen.retain(|e| e != block_hash);
		self.header_map.remove(block_hash)
	}
}

pub(crate) struct ChainListener {
	pub(crate) onchain_wallet: Arc<Wallet>,
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) chain_monitor: Arc<ChainMonitor>,
	pub(crate) output_sweeper: Arc<Sweeper>,
}

impl Listen for ChainListener {
	fn filtered_block_connected(
		&self, header: &bitcoin::block::Header,
		txdata: &lightning::chain::transaction::TransactionData, height: u32,
	) {
		self.onchain_wallet.filtered_block_connected(header, txdata, height);
		self.channel_manager.filtered_block_connected(header, txdata, height);
		self.chain_monitor.filtered_block_connected(header, txdata, height);
		self.output_sweeper.filtered_block_connected(header, txdata, height);
	}
	fn block_connected(&self, block: &bitcoin::Block, height: u32) {
		self.onchain_wallet.block_connected(block, height);
		self.channel_manager.block_connected(block, height);
		self.chain_monitor.block_connected(block, height);
		self.output_sweeper.block_connected(block, height);
	}

	fn block_disconnected(&self, header: &bitcoin::block::Header, height: u32) {
		self.onchain_wallet.block_disconnected(header, height);
		self.channel_manager.block_disconnected(header, height);
		self.chain_monitor.block_disconnected(header, height);
		self.output_sweeper.block_disconnected(header, height);
	}
}
