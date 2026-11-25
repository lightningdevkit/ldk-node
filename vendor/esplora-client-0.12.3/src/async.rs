// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2025 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Esplora by way of `reqwest` HTTP client.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::block::Header as BlockHeader;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::consensus::{deserialize, serialize, Decodable};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::{DisplayHex, FromHex};
use bitcoin::{Address, Block, BlockHash, MerkleBlock, Script, Transaction, Txid};

#[allow(unused_imports)]
use log::{debug, error, info, trace};

use reqwest::{header, Body, Client, Response};

use crate::{
	AddressStats, BlockInfo, BlockStatus, BlockSummary, Builder, Error, HeaderProvider,
	MempoolRecentTx, MempoolStats, MerkleProof, OutputStatus, ScriptHashStats, SubmitPackageResult,
	Tx, TxStatus, Utxo, BASE_BACKOFF_MILLIS, RETRYABLE_ERROR_CODES,
};

/// An async client for interacting with an Esplora API server.
#[derive(Clone)]
pub struct AsyncClient<S = DefaultSleeper> {
	/// The URL of the Esplora Server.
	url: String,
	/// The inner [`reqwest::Client`] to make HTTP requests.
	client: Client,
	/// Number of times to retry a request
	max_retries: usize,
	header_provider: Option<HeaderProvider>,
	/// Marker for the type of sleeper used
	marker: PhantomData<S>,
}

impl<S: Sleeper> AsyncClient<S> {
	/// Build an [`AsyncClient`] from a [`Builder`].
	pub fn from_builder(builder: Builder) -> Result<Self, Error> {
		let mut client_builder = Client::builder();

		#[cfg(not(target_arch = "wasm32"))]
		if let Some(proxy) = &builder.proxy {
			client_builder = client_builder.proxy(reqwest::Proxy::all(proxy)?);
		}

		#[cfg(not(target_arch = "wasm32"))]
		if let Some(timeout) = builder.timeout {
			client_builder = client_builder.timeout(core::time::Duration::from_secs(timeout));
		}

		if !builder.headers.is_empty() {
			let mut headers = header::HeaderMap::new();
			for (k, v) in builder.headers {
				let header_name = header::HeaderName::from_lowercase(k.to_lowercase().as_bytes())
					.map_err(|_| Error::InvalidHttpHeaderName(k))?;
				let header_value = header::HeaderValue::from_str(&v)
					.map_err(|_| Error::InvalidHttpHeaderValue(v))?;
				headers.insert(header_name, header_value);
			}
			client_builder = client_builder.default_headers(headers);
		}

		Ok(AsyncClient {
			url: builder.base_url,
			client: client_builder.build()?,
			max_retries: builder.max_retries,
			header_provider: None,
			marker: PhantomData,
		})
	}

	/// Build an [`AsyncClient`] that obtains additional headers for each request.
	pub fn from_builder_with_header_provider(
		builder: Builder, header_provider: HeaderProvider,
	) -> Result<Self, Error> {
		let mut client = Self::from_builder(builder)?;
		client.header_provider = Some(header_provider);
		Ok(client)
	}

	/// Build an [`AsyncClient`] from a [`Client`].
	pub fn from_client(url: String, client: Client) -> Self {
		AsyncClient {
			url,
			client,
			max_retries: crate::DEFAULT_MAX_RETRIES,
			header_provider: None,
			marker: PhantomData,
		}
	}

	/// Make an HTTP GET request to given URL, deserializing to any `T` that
	/// implement [`bitcoin::consensus::Decodable`].
	///
	/// It should be used when requesting Esplora endpoints that can be directly
	/// deserialized to native `rust-bitcoin` types, which implements
	/// [`bitcoin::consensus::Decodable`] from `&[u8]`.
	///
	/// # Errors
	///
	/// This function will return an error either from the HTTP client, or the
	/// [`bitcoin::consensus::Decodable`] deserialization.
	async fn get_response<T: Decodable>(&self, path: &str) -> Result<T, Error> {
		let url = format!("{}{}", self.url, path);
		let response = self.get_with_retry(&url).await?;

		if !response.status().is_success() {
			return Err(Error::HttpResponse {
				status: response.status().as_u16(),
				message: response.text().await?,
			});
		}

		Ok(deserialize::<T>(&response.bytes().await?)?)
	}

	/// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
	///
	/// It uses [`AsyncEsploraClient::get_response`] internally.
	///
	/// See [`AsyncEsploraClient::get_response`] above for full documentation.
	async fn get_opt_response<T: Decodable>(&self, path: &str) -> Result<Option<T>, Error> {
		match self.get_response::<T>(path).await {
			Ok(res) => Ok(Some(res)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Make an HTTP GET request to given URL, deserializing to any `T` that
	/// implements [`serde::de::DeserializeOwned`].
	///
	/// It should be used when requesting Esplora endpoints that have a specific
	/// defined API, mostly defined in [`crate::api`].
	///
	/// # Errors
	///
	/// This function will return an error either from the HTTP client, or the
	/// [`serde::de::DeserializeOwned`] deserialization.
	async fn get_response_json<T: serde::de::DeserializeOwned>(
		&self, path: &str,
	) -> Result<T, Error> {
		let url = format!("{}{}", self.url, path);
		let response = self.get_with_retry(&url).await?;

		if !response.status().is_success() {
			return Err(Error::HttpResponse {
				status: response.status().as_u16(),
				message: response.text().await?,
			});
		}

		response.json::<T>().await.map_err(Error::Reqwest)
	}

	/// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
	///
	/// It uses [`AsyncEsploraClient::get_response_json`] internally.
	///
	/// See [`AsyncEsploraClient::get_response_json`] above for full
	/// documentation.
	async fn get_opt_response_json<T: serde::de::DeserializeOwned>(
		&self, url: &str,
	) -> Result<Option<T>, Error> {
		match self.get_response_json(url).await {
			Ok(res) => Ok(Some(res)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Make an HTTP GET request to given URL, deserializing to any `T` that
	/// implements [`bitcoin::consensus::Decodable`].
	///
	/// It should be used when requesting Esplora endpoints that are expected
	/// to return a hex string decodable to native `rust-bitcoin` types which
	/// implement [`bitcoin::consensus::Decodable`] from `&[u8]`.
	///
	/// # Errors
	///
	/// This function will return an error either from the HTTP client, or the
	/// [`bitcoin::consensus::Decodable`] deserialization.
	async fn get_response_hex<T: Decodable>(&self, path: &str) -> Result<T, Error> {
		let url = format!("{}{}", self.url, path);
		let response = self.get_with_retry(&url).await?;

		if !response.status().is_success() {
			return Err(Error::HttpResponse {
				status: response.status().as_u16(),
				message: response.text().await?,
			});
		}

		let hex_str = response.text().await?;
		Ok(deserialize(&Vec::from_hex(&hex_str)?)?)
	}

	/// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
	///
	/// It uses [`AsyncEsploraClient::get_response_hex`] internally.
	///
	/// See [`AsyncEsploraClient::get_response_hex`] above for full
	/// documentation.
	async fn get_opt_response_hex<T: Decodable>(&self, path: &str) -> Result<Option<T>, Error> {
		match self.get_response_hex(path).await {
			Ok(res) => Ok(Some(res)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Make an HTTP GET request to given URL, deserializing to `String`.
	///
	/// It should be used when requesting Esplora endpoints that can return
	/// `String` formatted data that can be parsed downstream.
	///
	/// # Errors
	///
	/// This function will return an error either from the HTTP client.
	async fn get_response_text(&self, path: &str) -> Result<String, Error> {
		let url = format!("{}{}", self.url, path);
		let response = self.get_with_retry(&url).await?;

		if !response.status().is_success() {
			return Err(Error::HttpResponse {
				status: response.status().as_u16(),
				message: response.text().await?,
			});
		}

		Ok(response.text().await?)
	}

	/// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
	///
	/// It uses [`AsyncEsploraClient::get_response_text`] internally.
	///
	/// See [`AsyncEsploraClient::get_response_text`] above for full
	/// documentation.
	async fn get_opt_response_text(&self, path: &str) -> Result<Option<String>, Error> {
		match self.get_response_text(path).await {
			Ok(s) => Ok(Some(s)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Make an HTTP POST request to given URL, converting any `T` that
	/// implement [`Into<Body>`] and setting query parameters, if any.
	///
	/// # Errors
	///
	/// This function will return an error either from the HTTP client, or the
	/// response's [`serde_json`] deserialization.
	async fn post_request_bytes<T: Into<Body>>(
		&self, path: &str, body: T, query_params: Option<HashSet<(&str, String)>>,
	) -> Result<Response, Error> {
		let url: String = format!("{}{}", self.url, path);
		let mut request = self.client.post(url).body(body);

		if let Some(header_provider) = &self.header_provider {
			let headers = header_provider().await.map_err(Error::HeaderProvider)?;
			let mut header_map = reqwest::header::HeaderMap::new();
			for (name, value) in headers {
				let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
					.map_err(|_| Error::InvalidHttpHeaderName(name.clone()))?;
				let header_value = reqwest::header::HeaderValue::from_str(&value)
					.map_err(|_| Error::InvalidHttpHeaderValue(value.clone()))?;
				header_map.insert(header_name, header_value);
			}
			request = request.headers(header_map);
		}

		for param in query_params.unwrap_or_default() {
			request = request.query(&param);
		}

		let response = request.send().await?;

		if !response.status().is_success() {
			return Err(Error::HttpResponse {
				status: response.status().as_u16(),
				message: response.text().await?,
			});
		}

		Ok(response)
	}

	/// Get a [`Transaction`] option given its [`Txid`]
	pub async fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
		self.get_opt_response(&format!("/tx/{txid}/raw")).await
	}

	/// Get a [`Transaction`] given its [`Txid`].
	pub async fn get_tx_no_opt(&self, txid: &Txid) -> Result<Transaction, Error> {
		match self.get_tx(txid).await {
			Ok(Some(tx)) => Ok(tx),
			Ok(None) => Err(Error::TransactionNotFound(*txid)),
			Err(e) => Err(e),
		}
	}

	/// Get a [`Txid`] of a transaction given its index in a block with a given
	/// hash.
	pub async fn get_txid_at_block_index(
		&self, block_hash: &BlockHash, index: usize,
	) -> Result<Option<Txid>, Error> {
		match self.get_opt_response_text(&format!("/block/{block_hash}/txid/{index}")).await? {
			Some(s) => Ok(Some(Txid::from_str(&s).map_err(Error::HexToArray)?)),
			None => Ok(None),
		}
	}

	/// Get the status of a [`Transaction`] given its [`Txid`].
	pub async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
		self.get_response_json(&format!("/tx/{txid}/status")).await
	}

	/// Get transaction info given its [`Txid`].
	pub async fn get_tx_info(&self, txid: &Txid) -> Result<Option<Tx>, Error> {
		self.get_opt_response_json(&format!("/tx/{txid}")).await
	}

	/// Get the spend status of a [`Transaction`]'s outputs, given its [`Txid`].
	pub async fn get_tx_outspends(&self, txid: &Txid) -> Result<Vec<OutputStatus>, Error> {
		self.get_response_json(&format!("/tx/{txid}/outspends")).await
	}

	/// Get a [`BlockHeader`] given a particular block hash.
	pub async fn get_header_by_hash(&self, block_hash: &BlockHash) -> Result<BlockHeader, Error> {
		self.get_response_hex(&format!("/block/{block_hash}/header")).await
	}

	/// Get the [`BlockStatus`] given a particular [`BlockHash`].
	pub async fn get_block_status(&self, block_hash: &BlockHash) -> Result<BlockStatus, Error> {
		self.get_response_json(&format!("/block/{block_hash}/status")).await
	}

	/// Get a [`Block`] given a particular [`BlockHash`].
	pub async fn get_block_by_hash(&self, block_hash: &BlockHash) -> Result<Option<Block>, Error> {
		self.get_opt_response(&format!("/block/{block_hash}/raw")).await
	}

	/// Get a merkle inclusion proof for a [`Transaction`] with the given
	/// [`Txid`].
	pub async fn get_merkle_proof(&self, tx_hash: &Txid) -> Result<Option<MerkleProof>, Error> {
		self.get_opt_response_json(&format!("/tx/{tx_hash}/merkle-proof")).await
	}

	/// Get a [`MerkleBlock`] inclusion proof for a [`Transaction`] with the
	/// given [`Txid`].
	pub async fn get_merkle_block(&self, tx_hash: &Txid) -> Result<Option<MerkleBlock>, Error> {
		self.get_opt_response_hex(&format!("/tx/{tx_hash}/merkleblock-proof")).await
	}

	/// Get the spending status of an output given a [`Txid`] and the output
	/// index.
	pub async fn get_output_status(
		&self, txid: &Txid, index: u64,
	) -> Result<Option<OutputStatus>, Error> {
		self.get_opt_response_json(&format!("/tx/{txid}/outspend/{index}")).await
	}

	/// Broadcast a [`Transaction`] to Esplora.
	pub async fn broadcast(&self, transaction: &Transaction) -> Result<(), Error> {
		let body = serialize::<Transaction>(transaction).to_lower_hex_string();
		match self.post_request_bytes("/tx", body, None).await {
			Ok(_resp) => Ok(()),
			Err(e) => Err(e),
		}
	}

	/// Broadcast a package of [`Transaction`]s to Esplora.
	///
	/// If `maxfeerate` is provided, any transaction whose
	/// fee is higher will be rejected.
	///
	/// If `maxburnamount` is provided, any transaction
	/// with higher provably unspendable outputs amount
	/// will be rejected.
	pub async fn submit_package(
		&self, transactions: &[Transaction], maxfeerate: Option<f64>, maxburnamount: Option<f64>,
	) -> Result<SubmitPackageResult, Error> {
		let mut queryparams = HashSet::<(&str, String)>::new();
		if let Some(maxfeerate) = maxfeerate {
			queryparams.insert(("maxfeerate", maxfeerate.to_string()));
		}
		if let Some(maxburnamount) = maxburnamount {
			queryparams.insert(("maxburnamount", maxburnamount.to_string()));
		}

		let serialized_txs = transactions.iter().map(|tx| serialize_hex(&tx)).collect::<Vec<_>>();

		let response = self
			.post_request_bytes(
				"/txs/package",
				serde_json::to_string(&serialized_txs).unwrap_or_default(),
				Some(queryparams),
			)
			.await?;

		Ok(response.json::<SubmitPackageResult>().await?)
	}

	/// Get the current height of the blockchain tip
	pub async fn get_height(&self) -> Result<u32, Error> {
		self.get_response_text("/blocks/tip/height")
			.await
			.map(|height| u32::from_str(&height).map_err(Error::Parsing))?
	}

	/// Get the [`BlockHash`] of the current blockchain tip.
	pub async fn get_tip_hash(&self) -> Result<BlockHash, Error> {
		self.get_response_text("/blocks/tip/hash")
			.await
			.map(|block_hash| BlockHash::from_str(&block_hash).map_err(Error::HexToArray))?
	}

	/// Get the [`BlockHash`] of a specific block height
	pub async fn get_block_hash(&self, block_height: u32) -> Result<BlockHash, Error> {
		self.get_response_text(&format!("/block-height/{block_height}"))
			.await
			.map(|block_hash| BlockHash::from_str(&block_hash).map_err(Error::HexToArray))?
	}

	/// Get information about a specific address, includes confirmed balance and transactions in
	/// the mempool.
	pub async fn get_address_stats(&self, address: &Address) -> Result<AddressStats, Error> {
		let path = format!("/address/{address}");
		self.get_response_json(&path).await
	}

	/// Get statistics about a particular [`Script`] hash's confirmed and mempool transactions.
	pub async fn get_scripthash_stats(&self, script: &Script) -> Result<ScriptHashStats, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = format!("/scripthash/{script_hash}");
		self.get_response_json(&path).await
	}

	/// Get transaction history for the specified address, sorted with newest first.
	///
	/// Returns up to 50 mempool transactions plus the first 25 confirmed transactions.
	/// More can be requested by specifying the last txid seen by the previous query.
	pub async fn get_address_txs(
		&self, address: &Address, last_seen: Option<Txid>,
	) -> Result<Vec<Tx>, Error> {
		let path = match last_seen {
			Some(last_seen) => format!("/address/{address}/txs/chain/{last_seen}"),
			None => format!("/address/{address}/txs"),
		};

		self.get_response_json(&path).await
	}

	/// Get mempool [`Transaction`]s for the specified [`Address`], sorted with newest first.
	pub async fn get_mempool_address_txs(&self, address: &Address) -> Result<Vec<Tx>, Error> {
		let path = format!("/address/{address}/txs/mempool");

		self.get_response_json(&path).await
	}

	/// Get transaction history for the specified address/scripthash,
	/// sorted with newest first. Returns 25 transactions per page.
	/// More can be requested by specifying the last txid seen by the previous
	/// query.
	pub async fn scripthash_txs(
		&self, script: &Script, last_seen: Option<Txid>,
	) -> Result<Vec<Tx>, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = match last_seen {
			Some(last_seen) => format!("/scripthash/{script_hash:x}/txs/chain/{last_seen}"),
			None => format!("/scripthash/{script_hash:x}/txs"),
		};

		self.get_response_json(&path).await
	}

	/// Get mempool [`Transaction`] history for the
	/// specified [`Script`] hash, sorted with newest first.
	pub async fn get_mempool_scripthash_txs(&self, script: &Script) -> Result<Vec<Tx>, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = format!("/scripthash/{script_hash:x}/txs/mempool");

		self.get_response_json(&path).await
	}

	/// Get statistics about the mempool.
	pub async fn get_mempool_stats(&self) -> Result<MempoolStats, Error> {
		self.get_response_json("/mempool").await
	}

	/// Get a list of the last 10 [`Transaction`]s to enter the mempool.
	pub async fn get_mempool_recent_txs(&self) -> Result<Vec<MempoolRecentTx>, Error> {
		self.get_response_json("/mempool/recent").await
	}

	/// Get the full list of [`Txid`]s in the mempool.
	///
	/// The order of the [`Txid`]s is arbitrary.
	pub async fn get_mempool_txids(&self) -> Result<Vec<Txid>, Error> {
		self.get_response_json("/mempool/txids").await
	}

	/// Get a map where the key is the confirmation target (in number of
	/// blocks) and the value is the estimated feerate (in sat/vB).
	pub async fn get_fee_estimates(&self) -> Result<HashMap<u16, f64>, Error> {
		self.get_response_json("/fee-estimates").await
	}

	/// Get a summary about a [`Block`], given its [`BlockHash`].
	pub async fn get_block_info(&self, blockhash: &BlockHash) -> Result<BlockInfo, Error> {
		let path = format!("/block/{blockhash}");

		self.get_response_json(&path).await
	}

	/// Get all [`Txid`]s that belong to a [`Block`] identified by it's [`BlockHash`].
	pub async fn get_block_txids(&self, blockhash: &BlockHash) -> Result<Vec<Txid>, Error> {
		let path = format!("/block/{blockhash}/txids");

		self.get_response_json(&path).await
	}

	/// Get up to 25 [`Transaction`]s from a [`Block`], given its [`BlockHash`],
	/// beginning at `start_index` (starts from 0 if `start_index` is `None`).
	///
	/// The `start_index` value MUST be a multiple of 25,
	/// else an error will be returned by Esplora.
	pub async fn get_block_txs(
		&self, blockhash: &BlockHash, start_index: Option<u32>,
	) -> Result<Vec<Tx>, Error> {
		let path = match start_index {
			None => format!("/block/{blockhash}/txs"),
			Some(start_index) => format!("/block/{blockhash}/txs/{start_index}"),
		};

		self.get_response_json(&path).await
	}

	/// Gets some recent block summaries starting at the tip or at `height` if
	/// provided.
	///
	/// The maximum number of summaries returned depends on the backend itself:
	/// esplora returns `10` while [mempool.space](https://mempool.space/docs/api) returns `15`.
	#[deprecated(since = "0.12.3", note = "use `get_block_infos` instead")]
	pub async fn get_blocks(&self, height: Option<u32>) -> Result<Vec<BlockSummary>, Error> {
		let path = match height {
			Some(height) => format!("/blocks/{height}"),
			None => "/blocks".to_string(),
		};
		let blocks: Vec<BlockSummary> = self.get_response_json(&path).await?;
		if blocks.is_empty() {
			return Err(Error::InvalidResponse);
		}
		Ok(blocks)
	}

	/// Get summaries about recent blocks as [`BlockInfo`]s,
	/// starting at the tip, or at `height`, if provided.
	///
	/// The maximum number of elements returned depends on the backend itself:
	/// - Esplora returns `10`
	/// - [mempool.space](https://mempool.space/docs/api) returns `15`
	pub async fn get_block_infos(&self, height: Option<u32>) -> Result<Vec<BlockInfo>, Error> {
		let path = match height {
			Some(height) => format!("/blocks/{height}"),
			None => "/blocks".to_string(),
		};
		let blocks: Vec<BlockInfo> = self.get_response_json(&path).await?;
		if blocks.is_empty() {
			return Err(Error::InvalidResponse);
		}
		Ok(blocks)
	}

	/// Get all UTXOs locked to an address.
	pub async fn get_address_utxos(&self, address: &Address) -> Result<Vec<Utxo>, Error> {
		let path = format!("/address/{address}/utxo");

		self.get_response_json(&path).await
	}

	/// Get all [`Utxo`]s locked to a [`Script`].
	pub async fn get_scripthash_utxos(&self, script: &Script) -> Result<Vec<Utxo>, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = format!("/scripthash/{script_hash}/utxo");

		self.get_response_json(&path).await
	}

	/// Get the underlying base URL.
	pub fn url(&self) -> &str {
		&self.url
	}

	/// Get the underlying [`Client`].
	pub fn client(&self) -> &Client {
		&self.client
	}

	/// Sends a GET request to the given `url`, retrying failed attempts
	/// for retryable error codes until max retries hit.
	async fn get_with_retry(&self, url: &str) -> Result<Response, Error> {
		let mut delay = BASE_BACKOFF_MILLIS;
		let mut attempts = 0;

		loop {
			let mut request = self.client.get(url);
			if let Some(header_provider) = &self.header_provider {
				let headers = header_provider().await.map_err(Error::HeaderProvider)?;
				let mut header_map = reqwest::header::HeaderMap::new();
				for (name, value) in headers {
					let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
						.map_err(|_| Error::InvalidHttpHeaderName(name.clone()))?;
					let header_value = reqwest::header::HeaderValue::from_str(&value)
						.map_err(|_| Error::InvalidHttpHeaderValue(value.clone()))?;
					header_map.insert(header_name, header_value);
				}
				request = request.headers(header_map);
			}
			match request.send().await? {
				resp if attempts < self.max_retries && is_status_retryable(resp.status()) => {
					S::sleep(delay).await;
					attempts += 1;
					delay *= 2;
				},
				resp => return Ok(resp),
			}
		}
	}
}

impl<S> fmt::Debug for AsyncClient<S> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("AsyncClient")
			.field("url", &self.url)
			.field("max_retries", &self.max_retries)
			.finish_non_exhaustive()
	}
}

fn is_status_retryable(status: reqwest::StatusCode) -> bool {
	RETRYABLE_ERROR_CODES.contains(&status.as_u16())
}

/// Sleeper trait that allows any async runtime to be used.
pub trait Sleeper: 'static {
	/// The `Future` type returned by the sleep function.
	type Sleep: std::future::Future<Output = ()>;
	/// Create a `Future` that completes after the specified [`Duration`].
	fn sleep(dur: Duration) -> Self::Sleep;
}

/// The default `Sleeper` implementation using the underlying async runtime.
#[derive(Debug, Clone, Copy)]
pub struct DefaultSleeper;

#[cfg(any(test, feature = "tokio"))]
impl Sleeper for DefaultSleeper {
	type Sleep = tokio::time::Sleep;

	fn sleep(dur: std::time::Duration) -> Self::Sleep {
		tokio::time::sleep(dur)
	}
}
