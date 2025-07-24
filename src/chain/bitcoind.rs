// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::types::{ChainMonitor, ChannelManager, Sweeper, Wallet};

use lightning::chain::Listen;

use lightning_block_sync::gossip::UtxoSource;
use lightning_block_sync::http::{HttpEndpoint, JsonResponse};
use lightning_block_sync::poll::ValidatedBlockHeader;
use lightning_block_sync::rest::RestClient;
use lightning_block_sync::rpc::{RpcClient, RpcError};
use lightning_block_sync::{
	AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource, Cache,
};
use serde::Serialize;

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use bitcoin::{BlockHash, FeeRate, Transaction, Txid};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub enum BitcoindClient {
	Rpc {
		rpc_client: Arc<RpcClient>,
		latest_mempool_timestamp: AtomicU64,
		mempool_entries_cache: tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
	},
	Rest {
		rest_client: Arc<RestClient>,
		rpc_client: Arc<RpcClient>,
		latest_mempool_timestamp: AtomicU64,
		mempool_entries_cache: tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
	},
}

impl BitcoindClient {
	/// Creates a new RPC API client for the chain interactions with Bitcoin Core.
	pub(crate) fn new_rpc(host: String, port: u16, rpc_user: String, rpc_password: String) -> Self {
		let http_endpoint = endpoint(host, port);
		let rpc_credentials = rpc_credentials(rpc_user, rpc_password);

		let rpc_client = Arc::new(RpcClient::new(&rpc_credentials, http_endpoint));

		let latest_mempool_timestamp = AtomicU64::new(0);

		let mempool_entries_cache = tokio::sync::Mutex::new(HashMap::new());
		let mempool_txs_cache = tokio::sync::Mutex::new(HashMap::new());
		Self::Rpc { rpc_client, latest_mempool_timestamp, mempool_entries_cache, mempool_txs_cache }
	}

	/// Creates a new, primarily REST API client for the chain interactions
	/// with Bitcoin Core.
	///
	/// Aside the required REST host and port, we provide RPC configuration
	/// options for necessary calls not supported by the REST interface.
	pub(crate) fn new_rest(
		rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16, rpc_user: String,
		rpc_password: String,
	) -> Self {
		let rest_endpoint = endpoint(rest_host, rest_port).with_path("/rest".to_string());
		let rest_client = Arc::new(RestClient::new(rest_endpoint));

		let rpc_endpoint = endpoint(rpc_host, rpc_port);
		let rpc_credentials = rpc_credentials(rpc_user, rpc_password);
		let rpc_client = Arc::new(RpcClient::new(&rpc_credentials, rpc_endpoint));

		let latest_mempool_timestamp = AtomicU64::new(0);

		let mempool_entries_cache = tokio::sync::Mutex::new(HashMap::new());
		let mempool_txs_cache = tokio::sync::Mutex::new(HashMap::new());

		Self::Rest {
			rest_client,
			rpc_client,
			latest_mempool_timestamp,
			mempool_entries_cache,
			mempool_txs_cache,
		}
	}

	pub(crate) fn utxo_source(&self) -> Arc<dyn UtxoSource> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => Arc::clone(rpc_client) as Arc<dyn UtxoSource>,
			BitcoindClient::Rest { rest_client, .. } => {
				Arc::clone(rest_client) as Arc<dyn UtxoSource>
			},
		}
	}

	/// Broadcasts the provided transaction.
	pub(crate) async fn broadcast_transaction(&self, tx: &Transaction) -> std::io::Result<Txid> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::broadcast_transaction_inner(Arc::clone(rpc_client), tx).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// Bitcoin Core's REST interface does not support broadcasting transactions
				// so we use the RPC client.
				Self::broadcast_transaction_inner(Arc::clone(rpc_client), tx).await
			},
		}
	}

	async fn broadcast_transaction_inner(
		rpc_client: Arc<RpcClient>, tx: &Transaction,
	) -> std::io::Result<Txid> {
		let tx_serialized = bitcoin::consensus::encode::serialize_hex(tx);
		let tx_json = serde_json::json!(tx_serialized);
		rpc_client.call_method::<Txid>("sendrawtransaction", &[tx_json]).await
	}

	/// Retrieve the fee estimate needed for a transaction to begin
	/// confirmation within the provided `num_blocks`.
	pub(crate) async fn get_fee_estimate_for_target(
		&self, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_fee_estimate_for_target_inner(
					Arc::clone(rpc_client),
					num_blocks,
					estimation_mode,
				)
				.await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// We rely on the internal RPC client to make this call, as this
				// operation is not supported by Bitcoin Core's REST interface.
				Self::get_fee_estimate_for_target_inner(
					Arc::clone(rpc_client),
					num_blocks,
					estimation_mode,
				)
				.await
			},
		}
	}

	/// Estimate the fee rate for the provided target number of blocks.
	async fn get_fee_estimate_for_target_inner(
		rpc_client: Arc<RpcClient>, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		let num_blocks_json = serde_json::json!(num_blocks);
		let estimation_mode_json = serde_json::json!(estimation_mode);
		rpc_client
			.call_method::<FeeResponse>(
				"estimatesmartfee",
				&[num_blocks_json, estimation_mode_json],
			)
			.await
			.map(|resp| resp.0)
	}

	/// Gets the mempool minimum fee rate.
	pub(crate) async fn get_mempool_minimum_fee_rate(&self) -> std::io::Result<FeeRate> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_mempool_minimum_fee_rate_rpc(Arc::clone(rpc_client)).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_mempool_minimum_fee_rate_rest(Arc::clone(rest_client)).await
			},
		}
	}

	/// Get the mempool minimum fee rate via RPC interface.
	async fn get_mempool_minimum_fee_rate_rpc(
		rpc_client: Arc<RpcClient>,
	) -> std::io::Result<FeeRate> {
		rpc_client
			.call_method::<MempoolMinFeeResponse>("getmempoolinfo", &[])
			.await
			.map(|resp| resp.0)
	}

	/// Get the mempool minimum fee rate via REST interface.
	async fn get_mempool_minimum_fee_rate_rest(
		rest_client: Arc<RestClient>,
	) -> std::io::Result<FeeRate> {
		rest_client
			.request_resource::<JsonResponse, MempoolMinFeeResponse>("mempool/info.json")
			.await
			.map(|resp| resp.0)
	}

	/// Gets the raw transaction for the provided transaction ID. Returns `None` if not found.
	pub(crate) async fn get_raw_transaction(
		&self, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_raw_transaction_rpc(Arc::clone(rpc_client), txid).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_raw_transaction_rest(Arc::clone(rest_client), txid).await
			},
		}
	}

	/// Retrieve raw transaction for provided transaction ID via the RPC interface.
	async fn get_raw_transaction_rpc(
		rpc_client: Arc<RpcClient>, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = bitcoin::consensus::encode::serialize_hex(txid);
		let txid_json = serde_json::json!(txid_hex);
		match rpc_client
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

	/// Retrieve raw transaction for provided transaction ID via the REST interface.
	async fn get_raw_transaction_rest(
		rest_client: Arc<RestClient>, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = bitcoin::consensus::encode::serialize_hex(txid);
		let tx_path = format!("tx/{}.json", txid_hex);
		match rest_client
			.request_resource::<JsonResponse, GetRawTransactionResponse>(&tx_path)
			.await
		{
			Ok(resp) => Ok(Some(resp.0)),
			Err(e) => match e.kind() {
				std::io::ErrorKind::Other => {
					match e.into_inner() {
						Some(inner) => {
							let http_error_res: Result<Box<HttpError>, _> = inner.downcast();
							match http_error_res {
								Ok(http_error) => {
									// Check if it's the HTTP NOT_FOUND error code.
									if &http_error.status_code == "404" {
										Ok(None)
									} else {
										Err(std::io::Error::new(
											std::io::ErrorKind::Other,
											http_error,
										))
									}
								},
								Err(_) => {
									let error_msg =
										format!("Failed to process {} response.", tx_path);
									Err(std::io::Error::new(
										std::io::ErrorKind::Other,
										error_msg.as_str(),
									))
								},
							}
						},
						None => {
							let error_msg = format!("Failed to process {} response.", tx_path);
							Err(std::io::Error::new(std::io::ErrorKind::Other, error_msg.as_str()))
						},
					}
				},
				_ => {
					let error_msg = format!("Failed to process {} response.", tx_path);
					Err(std::io::Error::new(std::io::ErrorKind::Other, error_msg.as_str()))
				},
			},
		}
	}

	/// Retrieves the raw mempool.
	pub(crate) async fn get_raw_mempool(&self) -> std::io::Result<Vec<Txid>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_raw_mempool_rpc(Arc::clone(rpc_client)).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_raw_mempool_rest(Arc::clone(rest_client)).await
			},
		}
	}

	/// Retrieves the raw mempool via the RPC interface.
	async fn get_raw_mempool_rpc(rpc_client: Arc<RpcClient>) -> std::io::Result<Vec<Txid>> {
		let verbose_flag_json = serde_json::json!(false);
		rpc_client
			.call_method::<GetRawMempoolResponse>("getrawmempool", &[verbose_flag_json])
			.await
			.map(|resp| resp.0)
	}

	/// Retrieves the raw mempool via the REST interface.
	async fn get_raw_mempool_rest(rest_client: Arc<RestClient>) -> std::io::Result<Vec<Txid>> {
		rest_client
			.request_resource::<JsonResponse, GetRawMempoolResponse>(
				"mempool/contents.json?verbose=false",
			)
			.await
			.map(|resp| resp.0)
	}

	/// Retrieves an entry from the mempool if it exists, else return `None`.
	pub(crate) async fn get_mempool_entry(
		&self, txid: Txid,
	) -> std::io::Result<Option<MempoolEntry>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_mempool_entry_inner(Arc::clone(rpc_client), txid).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				Self::get_mempool_entry_inner(Arc::clone(rpc_client), txid).await
			},
		}
	}

	/// Retrieves the mempool entry of the provided transaction ID.
	async fn get_mempool_entry_inner(
		client: Arc<RpcClient>, txid: Txid,
	) -> std::io::Result<Option<MempoolEntry>> {
		let txid_hex = bitcoin::consensus::encode::serialize_hex(&txid);
		let txid_json = serde_json::json!(txid_hex);

		match client.call_method::<GetMempoolEntryResponse>("getmempoolentry", &[txid_json]).await {
			Ok(resp) => Ok(Some(MempoolEntry { txid, time: resp.time, height: resp.height })),
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
							"Failed to process getmempoolentry response",
						)),
					}
				},
				None => Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to process getmempoolentry response",
				)),
			},
		}
	}

	pub(crate) async fn update_mempool_entries_cache(&self) -> std::io::Result<()> {
		match self {
			BitcoindClient::Rpc { mempool_entries_cache, .. } => {
				self.update_mempool_entries_cache_inner(mempool_entries_cache).await
			},
			BitcoindClient::Rest { mempool_entries_cache, .. } => {
				self.update_mempool_entries_cache_inner(mempool_entries_cache).await
			},
		}
	}

	async fn update_mempool_entries_cache_inner(
		&self, mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
	) -> std::io::Result<()> {
		let mempool_txids = self.get_raw_mempool().await?;

		let mut mempool_entries_cache = mempool_entries_cache.lock().await;
		mempool_entries_cache.retain(|txid, _| mempool_txids.contains(txid));

		if let Some(difference) = mempool_txids.len().checked_sub(mempool_entries_cache.capacity())
		{
			mempool_entries_cache.reserve(difference)
		}

		for txid in mempool_txids {
			if mempool_entries_cache.contains_key(&txid) {
				continue;
			}

			if let Some(entry) = self.get_mempool_entry(txid).await? {
				mempool_entries_cache.insert(txid, entry.clone());
			}
		}

		mempool_entries_cache.shrink_to_fit();

		Ok(())
	}

	/// Returns two `Vec`s:
	/// - mempool transactions, alongside their first-seen unix timestamps.
	/// - transactions that have been evicted from the mempool, alongside the last time they were seen absent.
	pub(crate) async fn get_updated_mempool_transactions(
		&self, best_processed_height: u32, unconfirmed_txids: Vec<Txid>,
	) -> std::io::Result<(Vec<(Transaction, u64)>, Vec<(Txid, u64)>)> {
		let mempool_txs =
			self.get_mempool_transactions_and_timestamp_at_height(best_processed_height).await?;
		let evicted_txids = self.get_evicted_mempool_txids_and_timestamp(unconfirmed_txids).await?;
		Ok((mempool_txs, evicted_txids))
	}

	/// Get mempool transactions, alongside their first-seen unix timestamps.
	///
	/// This method is an adapted version of `bdk_bitcoind_rpc::Emitter::mempool`. It emits each
	/// transaction only once, unless we cannot assume the transaction's ancestors are already
	/// emitted.
	pub(crate) async fn get_mempool_transactions_and_timestamp_at_height(
		&self, best_processed_height: u32,
	) -> std::io::Result<Vec<(Transaction, u64)>> {
		match self {
			BitcoindClient::Rpc {
				latest_mempool_timestamp,
				mempool_entries_cache,
				mempool_txs_cache,
				..
			} => {
				self.get_mempool_transactions_and_timestamp_at_height_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					mempool_txs_cache,
					best_processed_height,
				)
				.await
			},
			BitcoindClient::Rest {
				latest_mempool_timestamp,
				mempool_entries_cache,
				mempool_txs_cache,
				..
			} => {
				self.get_mempool_transactions_and_timestamp_at_height_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					mempool_txs_cache,
					best_processed_height,
				)
				.await
			},
		}
	}

	async fn get_mempool_transactions_and_timestamp_at_height_inner(
		&self, latest_mempool_timestamp: &AtomicU64,
		mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: &tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
		best_processed_height: u32,
	) -> std::io::Result<Vec<(Transaction, u64)>> {
		let prev_mempool_time = latest_mempool_timestamp.load(Ordering::Relaxed);
		let mut latest_time = prev_mempool_time;

		self.update_mempool_entries_cache().await?;

		let mempool_entries_cache = mempool_entries_cache.lock().await;
		let mut mempool_txs_cache = mempool_txs_cache.lock().await;
		mempool_txs_cache.retain(|txid, _| mempool_entries_cache.contains_key(txid));

		if let Some(difference) =
			mempool_entries_cache.len().checked_sub(mempool_txs_cache.capacity())
		{
			mempool_txs_cache.reserve(difference)
		}

		let mut txs_to_emit = Vec::with_capacity(mempool_entries_cache.len());
		for (txid, entry) in mempool_entries_cache.iter() {
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

			if let Some((cached_tx, cached_time)) = mempool_txs_cache.get(txid) {
				txs_to_emit.push((cached_tx.clone(), *cached_time));
				continue;
			}

			match self.get_raw_transaction(&entry.txid).await {
				Ok(Some(tx)) => {
					mempool_txs_cache.insert(entry.txid, (tx.clone(), entry.time));
					txs_to_emit.push((tx, entry.time));
				},
				Ok(None) => {
					continue;
				},
				Err(e) => return Err(e),
			};
		}

		if !txs_to_emit.is_empty() {
			latest_mempool_timestamp.store(latest_time, Ordering::Release);
		}
		Ok(txs_to_emit)
	}

	// Retrieve a list of Txids that have been evicted from the mempool.
	//
	// To this end, we first update our local mempool_entries_cache and then return all unconfirmed
	// wallet `Txid`s that don't appear in the mempool still.
	async fn get_evicted_mempool_txids_and_timestamp(
		&self, unconfirmed_txids: Vec<Txid>,
	) -> std::io::Result<Vec<(Txid, u64)>> {
		match self {
			BitcoindClient::Rpc { latest_mempool_timestamp, mempool_entries_cache, .. } => {
				Self::get_evicted_mempool_txids_and_timestamp_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					unconfirmed_txids,
				)
				.await
			},
			BitcoindClient::Rest { latest_mempool_timestamp, mempool_entries_cache, .. } => {
				Self::get_evicted_mempool_txids_and_timestamp_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					unconfirmed_txids,
				)
				.await
			},
		}
	}

	async fn get_evicted_mempool_txids_and_timestamp_inner(
		latest_mempool_timestamp: &AtomicU64,
		mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		unconfirmed_txids: Vec<Txid>,
	) -> std::io::Result<Vec<(Txid, u64)>> {
		let latest_mempool_timestamp = latest_mempool_timestamp.load(Ordering::Relaxed);
		let mempool_entries_cache = mempool_entries_cache.lock().await;
		let evicted_txids = unconfirmed_txids
			.into_iter()
			.filter(|txid| mempool_entries_cache.contains_key(txid))
			.map(|txid| (txid, latest_mempool_timestamp))
			.collect();
		Ok(evicted_txids)
	}
}

impl BlockSource for BitcoindClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a bitcoin::BlockHash, height_hint: Option<u32>,
	) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Box::pin(async move { rpc_client.get_header(header_hash, height_hint).await })
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Box::pin(async move { rest_client.get_header(header_hash, height_hint).await })
			},
		}
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a bitcoin::BlockHash,
	) -> AsyncBlockSourceResult<'a, BlockData> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Box::pin(async move { rpc_client.get_block(header_hash).await })
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Box::pin(async move { rest_client.get_block(header_hash).await })
			},
		}
	}

	fn get_best_block(&self) -> AsyncBlockSourceResult<(bitcoin::BlockHash, Option<u32>)> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Box::pin(async move { rpc_client.get_best_block().await })
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Box::pin(async move { rest_client.get_best_block().await })
			},
		}
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

pub(crate) struct MempoolMinFeeResponse(pub FeeRate);

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

pub(crate) struct GetRawTransactionResponse(pub Transaction);

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

pub(crate) fn rpc_credentials(rpc_user: String, rpc_password: String) -> String {
	BASE64_STANDARD.encode(format!("{}:{}", rpc_user, rpc_password))
}

pub(crate) fn endpoint(host: String, port: u16) -> HttpEndpoint {
	HttpEndpoint::for_host(host).with_port(port)
}

#[derive(Debug)]
pub struct HttpError {
	pub(crate) status_code: String,
	pub(crate) contents: Vec<u8>,
}

impl std::error::Error for HttpError {}

impl std::fmt::Display for HttpError {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		let contents = String::from_utf8_lossy(&self.contents);
		write!(f, "status_code: {}, contents: {}", self.status_code, contents)
	}
}
