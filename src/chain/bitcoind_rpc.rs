// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::types::{ChainMonitor, ChannelManager, Sweeper, Wallet};

use lightning::chain::Listen;

use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::http::JsonResponse;
use lightning_block_sync::poll::ValidatedBlockHeader;
use lightning_block_sync::rpc::{RpcClient, RpcError};
use lightning_block_sync::{
	AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource, Cache,
};

use serde::Serialize;

use bitcoin::{BlockHash, FeeRate, Transaction, Txid};

use base64::prelude::{Engine, BASE64_STANDARD};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct BitcoindRpcClient {
	rpc_client: Arc<RpcClient>,
	latest_mempool_timestamp: AtomicU64,
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

		let latest_mempool_timestamp = AtomicU64::new(0);

		Self { rpc_client, latest_mempool_timestamp }
	}

	pub(crate) async fn broadcast_transaction(&self, tx: &Transaction) -> std::io::Result<Txid> {
		let tx_serialized = bitcoin::consensus::encode::serialize_hex(tx);
		let tx_json = serde_json::json!(tx_serialized);
		self.rpc_client.call_method::<Txid>("sendrawtransaction", &[tx_json]).await
	}

	pub(crate) async fn get_fee_estimate_for_target(
		&self, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		let num_blocks_json = serde_json::json!(num_blocks);
		let estimation_mode_json = serde_json::json!(estimation_mode);
		self.rpc_client
			.call_method::<FeeResponse>(
				"estimatesmartfee",
				&[num_blocks_json, estimation_mode_json],
			)
			.await
			.map(|resp| resp.0)
	}

	pub(crate) async fn get_mempool_minimum_fee_rate(&self) -> std::io::Result<FeeRate> {
		self.rpc_client
			.call_method::<MempoolMinFeeResponse>("getmempoolinfo", &[])
			.await
			.map(|resp| resp.0)
	}

	pub(crate) async fn get_raw_transaction(
		&self, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = bitcoin::consensus::encode::serialize_hex(txid);
		let txid_json = serde_json::json!(txid_hex);
		match self
			.rpc_client
			.call_method::<GetRawTransactionResponse>("getrawtransaction", &[txid_json])
			.await
		{
			Ok(resp) => Ok(Some(resp.0)),
			Err(e) => match e.into_inner() {
				Some(inner) => {
					let rpc_error_res: Result<Box<RpcError>, _> = inner.downcast();

					match rpc_error_res {
						Ok(rpc_error) => {
							// Check if it's the 'not found' error code.
							if rpc_error.code == -5 {
								Ok(None)
							} else {
								Err(std::io::Error::new(std::io::ErrorKind::Other, rpc_error))
							}
						},
						Err(_) => Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to process getrawtransaction response",
						)),
					}
				},
				None => Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to process getrawtransaction response",
				)),
			},
		}
	}

	pub(crate) async fn get_raw_mempool(&self) -> std::io::Result<Vec<Txid>> {
		let verbose_flag_json = serde_json::json!(false);
		self.rpc_client
			.call_method::<GetRawMempoolResponse>("getrawmempool", &[verbose_flag_json])
			.await
			.map(|resp| resp.0)
	}

	pub(crate) async fn get_mempool_entry(&self, txid: Txid) -> std::io::Result<MempoolEntry> {
		let txid_hex = bitcoin::consensus::encode::serialize_hex(&txid);
		let txid_json = serde_json::json!(txid_hex);
		self.rpc_client
			.call_method::<GetMempoolEntryResponse>("getmempoolentry", &[txid_json])
			.await
			.map(|resp| MempoolEntry { txid, height: resp.height, time: resp.time })
	}

	pub(crate) async fn get_mempool_entries(&self) -> std::io::Result<Vec<MempoolEntry>> {
		let mempool_txids = self.get_raw_mempool().await?;
		let mut mempool_entries = Vec::with_capacity(mempool_txids.len());
		for txid in mempool_txids {
			let entry = self.get_mempool_entry(txid).await?;
			mempool_entries.push(entry);
		}
		Ok(mempool_entries)
	}

	/// Get mempool transactions, alongside their first-seen unix timestamps.
	///
	/// This method is an adapted version of `bdk_bitcoind_rpc::Emitter::mempool`. It emits each
	/// transaction only once, unless we cannot assume the transaction's ancestors are already
	/// emitted.
	pub(crate) async fn get_mempool_transactions_and_timestamp_at_height(
		&self, best_processed_height: u32,
	) -> std::io::Result<Vec<(Transaction, u64)>> {
		let prev_mempool_time = self.latest_mempool_timestamp.load(Ordering::Relaxed);
		let mut latest_time = prev_mempool_time;

		let mempool_entries = self.get_mempool_entries().await?;
		let mut txs_to_emit = Vec::new();

		for entry in mempool_entries {
			if entry.time > latest_time {
				latest_time = entry.time;
			}

			// Avoid emitting transactions that are already emitted if we can guarantee
			// blocks containing ancestors are already emitted. The bitcoind rpc interface
			// provides us with the block height that the tx is introduced to the mempool.
			// If we have already emitted the block of height, we can assume that all
			// ancestor txs have been processed by the receiver.
			let ancestor_within_height = entry.height <= best_processed_height;
			let is_already_emitted = entry.time <= prev_mempool_time;
			if is_already_emitted && ancestor_within_height {
				continue;
			}

			match self.get_raw_transaction(&entry.txid).await {
				Ok(Some(tx)) => {
					txs_to_emit.push((tx, entry.time));
				},
				Ok(None) => {
					continue;
				},
				Err(e) => return Err(e),
			};
		}

		if !txs_to_emit.is_empty() {
			self.latest_mempool_timestamp.store(latest_time, Ordering::Release);
		}
		Ok(txs_to_emit)
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

pub(crate) struct FeeResponse(pub FeeRate);

impl TryInto<FeeResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<FeeResponse> {
		if !self.0["errors"].is_null() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Other,
				self.0["errors"].to_string(),
			));
		}
		let fee_rate_btc_per_kvbyte = self.0["feerate"]
			.as_f64()
			.ok_or(std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse fee rate"))?;
		// Bitcoin Core gives us a feerate in BTC/KvB.
		// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
		let fee_rate = {
			let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvbyte * 25_000_000.0).round() as u64;
			FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
		};
		Ok(FeeResponse(fee_rate))
	}
}

pub struct MempoolMinFeeResponse(pub FeeRate);

impl TryInto<MempoolMinFeeResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<MempoolMinFeeResponse> {
		let fee_rate_btc_per_kvbyte = self.0["mempoolminfee"]
			.as_f64()
			.ok_or(std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse fee rate"))?;
		// Bitcoin Core gives us a feerate in BTC/KvB.
		// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
		let fee_rate = {
			let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvbyte * 25_000_000.0).round() as u64;
			FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
		};
		Ok(MempoolMinFeeResponse(fee_rate))
	}
}

pub struct GetRawTransactionResponse(pub Transaction);

impl TryInto<GetRawTransactionResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetRawTransactionResponse> {
		let tx = self
			.0
			.as_str()
			.ok_or(std::io::Error::new(
				std::io::ErrorKind::Other,
				"Failed to parse getrawtransaction response",
			))
			.and_then(|s| {
				bitcoin::consensus::encode::deserialize_hex(s).map_err(|_| {
					std::io::Error::new(
						std::io::ErrorKind::Other,
						"Failed to parse getrawtransaction response",
					)
				})
			})?;

		Ok(GetRawTransactionResponse(tx))
	}
}

pub struct GetRawMempoolResponse(Vec<Txid>);

impl TryInto<GetRawMempoolResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetRawMempoolResponse> {
		let res = self.0.as_array().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse getrawmempool response",
		))?;

		let mut mempool_transactions = Vec::with_capacity(res.len());

		for hex in res {
			let txid = if let Some(hex_str) = hex.as_str() {
				match bitcoin::consensus::encode::deserialize_hex(hex_str) {
					Ok(txid) => txid,
					Err(_) => {
						return Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to parse getrawmempool response",
						));
					},
				}
			} else {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getrawmempool response",
				));
			};

			mempool_transactions.push(txid);
		}

		Ok(GetRawMempoolResponse(mempool_transactions))
	}
}

pub struct GetMempoolEntryResponse {
	time: u64,
	height: u32,
}

impl TryInto<GetMempoolEntryResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetMempoolEntryResponse> {
		let res = self.0.as_object().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse getmempoolentry response",
		))?;

		let time = match res["time"].as_u64() {
			Some(time) => time,
			None => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getmempoolentry response",
				));
			},
		};

		let height = match res["height"].as_u64().and_then(|h| h.try_into().ok()) {
			Some(height) => height,
			None => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getmempoolentry response",
				));
			},
		};

		Ok(GetMempoolEntryResponse { time, height })
	}
}

#[derive(Debug, Clone)]
pub(crate) struct MempoolEntry {
	/// The transaction id
	txid: Txid,
	/// Local time transaction entered pool in seconds since 1 Jan 1970 GMT
	time: u64,
	/// Block height when transaction entered pool
	height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum FeeRateEstimationMode {
	Economical,
	Conservative,
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
