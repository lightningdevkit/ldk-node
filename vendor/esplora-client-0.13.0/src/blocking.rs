// SPDX-License-Identifier: MIT OR Apache-2.0

//! # Blocking Esplora Client
//!
//! This module implements [`BlockingClient`], a synchronous HTTP client for
//! interacting with an [Esplora] server by way of [`bitreq`].
//!
//! Use this client from synchronous applications, command-line tools, tests,
//! or code paths where blocking the current thread is acceptable. Each method
//! sends the request immediately and returns only after the response body has
//! been read and decoded.
//!
//! The client is configured through [`Builder`], including the
//! base URL, proxy, socket timeout, custom headers, and retry count.
//!
//! # Example
//!
//! ```rust,no_run
//! # fn example() -> Result<(), esplora_client::Error> {
//!
//! let client = esplora_client::Builder::new("https://mempool.space/api").build_blocking();
//! let height = client.get_height()?;
//!
//! # Ok(())
//! # }
//! ```
//!
//! [Esplora]: https://github.com/Blockstream/esplora/blob/master/API.md

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use bitcoin::consensus::encode::serialize_hex;
use bitreq::{Method, Proxy, Request, Response};

use bitcoin::block::Header as BlockHeader;
use bitcoin::consensus::{deserialize, serialize, Decodable};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::{DisplayHex, FromHex};
use bitcoin::{Address, Amount, Block, BlockHash, FeeRate, MerkleBlock, Script, Transaction, Txid};

use crate::{
	duration_to_timeout_secs, is_retryable, is_success, sat_per_vbyte_to_feerate, AddressStats,
	BlockInfo, BlockStatus, Builder, Error, EsploraTx, MempoolRecentTx, MempoolStats, MerkleProof,
	OutputStatus, ScriptHashStats, SubmitPackageResult, TxStatus, Utxo, BASE_BACKOFF_MILLIS,
};

#[allow(deprecated)]
use crate::BlockSummary;

/// A synchronous client for interacting with an Esplora API server.
///
/// Use [`Builder`] to construct an instance of this client. The client stores
/// the server base URL and request configuration, then exposes convenience
/// methods for the transaction, block, address, scripthash, fee-estimate, and
/// mempool endpoints.
///
/// Each method blocks the current thread until the HTTP request completes and
/// the response is parsed into the requested type.
///
/// # Retries
///
/// Failed requests are automatically retried up to `max_retries` times
/// (configured via [`Builder`]) with exponential backoff, but only for
/// retryable HTTP status codes. See [`crate::RETRYABLE_ERROR_CODES`] for the
/// full list.
#[derive(Debug, Clone)]
pub struct BlockingClient {
	/// The URL of the Esplora server.
	url: String,
	/// The URL of the proxy host.
	///
	/// NOTE: The proxy is ignored when targeting `wasm32`.
	pub proxy: Option<String>,
	/// Per-request socket timeout.
	pub timeout: Option<Duration>,
	/// HTTP headers to set on every request made to the Esplora server.
	pub headers: HashMap<String, String>,
	/// Maximum number of retry attempts for retryable responses.
	pub max_retries: usize,
}

impl BlockingClient {
	/// Build a [`BlockingClient`] from a [`Builder`].
	///
	/// This consumes the builder configuration and stores it on the client.
	/// No network request is made until a client method is called.
	pub fn from_builder(builder: Builder) -> Self {
		Self {
			url: builder.base_url,
			proxy: builder.proxy,
			timeout: builder.timeout,
			headers: builder.headers,
			max_retries: builder.max_retries,
		}
	}

	/// Return the base URL of the Esplora server this client connects to.
	///
	/// The returned value is the exact string provided to [`Builder::new`].
	pub fn url(&self) -> &str {
		&self.url
	}

	/// Build a HTTP [`Request`] with given [`Method`] and URI `path`.
	///
	/// Configures the request with the proxy, timeout, and headers set on
	/// this client. Used internally by all other request helper methods.
	pub(crate) fn build_request(&self, method: Method, path: &str) -> Result<Request, Error> {
		let mut request = Request::new(method, format!("{}{}", self.url, path));

		if let Some(proxy) = &self.proxy {
			request = request.with_proxy(Proxy::new_http(proxy)?);
		}

		if let Some(timeout) = self.timeout {
			request = request.with_timeout(duration_to_timeout_secs(timeout));
		}

		if !self.headers.is_empty() {
			request = request.with_headers(&self.headers);
		}

		Ok(request)
	}

	/// Make an HTTP POST request to `path` with `body`.
	///
	/// Configures query parameters, if any, and returns the raw response after
	/// checking the HTTP status code.
	///
	/// # Errors
	///
	/// This function will return an error either from the HTTP client, or the
	/// response's [`serde_json`] deserialization.
	pub fn post_request<T: Into<Vec<u8>>>(
		&self, path: &str, body: T, query_params: Option<HashSet<(&str, String)>>,
	) -> Result<Response, Error> {
		let mut request = self.build_request(Method::Post, path)?.with_body(body);

		for (key, value) in query_params.unwrap_or_default() {
			request = request.with_param(key, value);
		}

		let response = request.send()?;

		if !is_success(&response) {
			let status = u16::try_from(response.status_code).map_err(Error::StatusCode)?;
			let message = response.as_str().unwrap_or_default().to_string();
			return Err(Error::HttpResponse { status, message });
		}

		Ok(response)
	}

	/// Sends a GET request to `url`, retrying on retryable status codes
	/// with exponential backoff until [`BlockingClient::max_retries`] is reached.
	fn get_with_retry(&self, url: &str) -> Result<Response, Error> {
		let mut delay = BASE_BACKOFF_MILLIS;
		let mut attempts = 0;

		loop {
			match self.build_request(Method::Get, url)?.send()? {
				resp if attempts < self.max_retries && is_retryable(&resp) => {
					thread::sleep(delay);
					attempts += 1;
					delay *= 2;
				},
				resp => return Ok(resp),
			}
		}
	}

	/// Makes a GET request to `path`, deserializing the response body as
	/// raw bytes into `T` using [`bitcoin::consensus::Decodable`].
	///
	/// Use this for endpoints that return raw binary Bitcoin data.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the request fails or deserialization fails.
	fn get_response<T: Decodable>(&self, path: &str) -> Result<T, Error> {
		let response = self.get_with_retry(path)?;

		if !is_success(&response) {
			let status = u16::try_from(response.status_code).map_err(Error::StatusCode)?;
			let message = response.as_str().unwrap_or_default().to_string();
			return Err(Error::HttpResponse { status, message });
		}

		Ok(deserialize::<T>(response.as_bytes())?)
	}

	/// Makes a GET request to `path`, returning `None` on a 404 response.
	///
	/// Delegates to [`Self::get_response`]. See its documentation for details.
	fn get_opt_response<T: Decodable>(&self, path: &str) -> Result<Option<T>, Error> {
		match self.get_response(path) {
			Ok(response) => Ok(Some(response)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Makes a GET request to `path`, deserializing the hex-encoded response
	/// body into `T` using [`bitcoin::consensus::Decodable`].
	///
	/// Use this for endpoints that return hex-encoded Bitcoin data.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the request fails, hex decoding fails,
	/// or consensus deserialization fails.
	fn get_response_hex<T: Decodable>(&self, path: &str) -> Result<T, Error> {
		let response = self.get_with_retry(path)?;

		if !is_success(&response) {
			let status = u16::try_from(response.status_code).map_err(Error::StatusCode)?;
			let message = response.as_str().unwrap_or_default().to_string();
			return Err(Error::HttpResponse { status, message });
		}

		let hex_str = response.as_str()?;
		deserialize(&Vec::from_hex(hex_str)?).map_err(Error::BitcoinEncoding)
	}

	/// Makes a GET request to `path`, returning `None` on a 404 response.
	///
	/// Delegates to [`Self::get_response_hex`]. See its documentation for details.
	fn get_opt_response_hex<T: Decodable>(&self, path: &str) -> Result<Option<T>, Error> {
		match self.get_response_hex(path) {
			Ok(res) => Ok(Some(res)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Makes a GET request to `path`, deserializing the response body as JSON
	/// into `T` using [`serde::de::DeserializeOwned`].
	///
	/// Use this for endpoints that return Esplora-specific JSON types, as
	/// defined in [`crate::api`].
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the request fails or JSON deserialization fails.
	fn get_response_json<'a, T: serde::de::DeserializeOwned>(
		&'a self, path: &'a str,
	) -> Result<T, Error> {
		let response = self.get_with_retry(path)?;

		if !is_success(&response) {
			let status = u16::try_from(response.status_code).map_err(Error::StatusCode)?;
			let message = response.as_str().unwrap_or_default().to_string();
			return Err(Error::HttpResponse { status, message });
		}

		response.json::<T>().map_err(Error::BitReq)
	}

	/// Makes a GET request to `path`, returning `None` on a 404 response.
	///
	/// Delegates to [`Self::get_response_json`]. See its documentation for details.
	fn get_opt_response_json<T: serde::de::DeserializeOwned>(
		&self, path: &str,
	) -> Result<Option<T>, Error> {
		match self.get_response_json(path) {
			Ok(res) => Ok(Some(res)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Makes a GET request to `path`, returning the response body as a [`String`].
	///
	/// Use this for endpoints that return plain text data that needs further
	/// parsing downstream.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the request fails.
	fn get_response_text(&self, path: &str) -> Result<String, Error> {
		let response = self.get_with_retry(path)?;

		if !is_success(&response) {
			let status = u16::try_from(response.status_code).map_err(Error::StatusCode)?;
			let message = response.as_str().unwrap_or_default().to_string();
			return Err(Error::HttpResponse { status, message });
		}

		Ok(response.as_str()?.to_string())
	}

	/// Makes a GET request to `path`, returning `None` on a 404 response.
	///
	/// Delegates to [`Self::get_response_text`]. See its documentation for details.
	fn get_opt_response_text(&self, path: &str) -> Result<Option<String>, Error> {
		match self.get_response_text(path) {
			Ok(s) => Ok(Some(s)),
			Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Get a raw [`Transaction`] given its [`Txid`].
	///
	/// Returns `None` if the transaction is not found.
	pub fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
		self.get_opt_response(&format!("/tx/{txid}/raw"))
	}

	/// Get a raw [`Transaction`] given its [`Txid`].
	///
	/// Returns an [`Error::TransactionNotFound`] if the transaction is not found.
	/// Prefer [`Self::get_tx`] if you want to handle the not-found case explicitly.
	pub fn get_tx_no_opt(&self, txid: &Txid) -> Result<Transaction, Error> {
		match self.get_tx(txid) {
			Ok(Some(tx)) => Ok(tx),
			Ok(None) => Err(Error::TransactionNotFound(*txid)),
			Err(e) => Err(e),
		}
	}

	/// Get the [`Txid`] of the transaction at position `index` within the
	/// block identified by `block_hash`.
	///
	/// Returns `None` if the block or index is not found.
	pub fn get_txid_at_block_index(
		&self, block_hash: &BlockHash, index: usize,
	) -> Result<Option<Txid>, Error> {
		match self.get_opt_response_text(&format!("/block/{block_hash}/txid/{index}"))? {
			Some(s) => Ok(Some(Txid::from_str(&s).map_err(Error::HexToArray)?)),
			None => Ok(None),
		}
	}

	/// Get the confirmation status of a [`Transaction`] given its [`Txid`].
	///
	/// Returns a [`TxStatus`] containing whether the transaction is confirmed,
	/// and if so, the block height, hash, and timestamp it was confirmed in.
	pub fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
		self.get_response_json(&format!("/tx/{txid}/status"))
	}

	/// Get an [`EsploraTx`] given its [`Txid`].
	///
	/// Unlike [`Self::get_tx`], returns the Esplora-specific [`EsploraTx`]
	/// type, which includes additional metadata such as confirmation status,
	/// fee, and weight. Returns `None` if the transaction is not found.
	pub fn get_tx_info(&self, txid: &Txid) -> Result<Option<EsploraTx>, Error> {
		self.get_opt_response_json(&format!("/tx/{txid}"))
	}

	/// Get the spend status of all outputs in a [`Transaction`], given its [`Txid`].
	///
	/// Returns a [`Vec`] of [`OutputStatus`], one per output, ordered as they
	/// appear in the [`Transaction`].
	pub fn get_tx_outspends(&self, txid: &Txid) -> Result<Vec<OutputStatus>, Error> {
		self.get_response_json(&format!("/tx/{txid}/outspends"))
	}

	/// Get the [`BlockHeader`] of a [`Block`] given its [`BlockHash`].
	pub fn get_header_by_hash(&self, block_hash: &BlockHash) -> Result<BlockHeader, Error> {
		self.get_response_hex(&format!("/block/{block_hash}/header"))
	}

	/// Get the [`BlockStatus`] of a [`Block`] given its [`BlockHash`].
	///
	/// Returns a [`BlockStatus`] indicating whether this [`Block`] is part of
	/// the best chain, its height, and the [`BlockHash`] of the next [`Block`],
	/// if any.
	pub fn get_block_status(&self, block_hash: &BlockHash) -> Result<BlockStatus, Error> {
		self.get_response_json(&format!("/block/{block_hash}/status"))
	}

	/// Get the full [`Block`] with the given [`BlockHash`].
	///
	/// Returns `None` if the [`Block`] is not found.
	pub fn get_block_by_hash(&self, block_hash: &BlockHash) -> Result<Option<Block>, Error> {
		self.get_opt_response(&format!("/block/{block_hash}/raw"))
	}

	/// Get a Merkle inclusion proof for a [`Transaction`] given its [`Txid`].
	///
	/// Returns a [`MerkleProof`] that can be used to verify the transaction's
	/// inclusion in a block. Returns `None` if the transaction is not found or
	/// is unconfirmed.
	pub fn get_merkle_proof(&self, txid: &Txid) -> Result<Option<MerkleProof>, Error> {
		self.get_opt_response_json(&format!("/tx/{txid}/merkle-proof"))
	}

	/// Get a [`MerkleBlock`] inclusion proof for a [`Transaction`] given its [`Txid`].
	///
	/// Returns `None` if the transaction is not found or is unconfirmed.
	pub fn get_merkle_block(&self, txid: &Txid) -> Result<Option<MerkleBlock>, Error> {
		self.get_opt_response_hex(&format!("/tx/{txid}/merkleblock-proof"))
	}

	/// Get the spend status of a specific output, identified by its [`Txid`]
	/// and output index.
	///
	/// Returns an [`OutputStatus`] indicating whether the output has been
	/// spent, and if so, by which transaction. Returns `None` if not found.
	pub fn get_output_status(
		&self, txid: &Txid, index: u64,
	) -> Result<Option<OutputStatus>, Error> {
		self.get_opt_response_json(&format!("/tx/{txid}/outspend/{index}"))
	}

	/// Broadcast a [`Transaction`] to the Esplora server.
	///
	/// The transaction is serialized and sent as a hex-encoded string.
	/// Returns the [`Txid`] of the broadcasted transaction.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the request fails or the server rejects the transaction.
	pub fn broadcast(&self, transaction: &Transaction) -> Result<Txid, Error> {
		let body = serialize::<Transaction>(transaction).to_lower_hex_string();

		let response = self.post_request("/tx", body, None)?;
		let txid = Txid::from_str(response.as_str()?).map_err(Error::HexToArray)?;

		Ok(txid)
	}

	/// Broadcast a package of [`Transaction`]s to the Esplora server.
	///
	/// Returns a [`SubmitPackageResult`] containing the result for each
	/// transaction in the package, keyed by [`bitcoin::Wtxid`].
	///
	/// Optionally, `maxfeerate` and `maxburnamount` can be provided to reject
	/// transactions that exceed these thresholds.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the request fails or the server rejects the package.
	pub fn submit_package(
		&self, transactions: &[Transaction], maxfeerate: Option<FeeRate>,
		maxburnamount: Option<Amount>,
	) -> Result<SubmitPackageResult, Error> {
		let serialized_txs = transactions.iter().map(|tx| serialize_hex(&tx)).collect::<Vec<_>>();

		let mut params = HashSet::<(&str, String)>::new();

		// Esplora expects `maxfeerate` in sats/vB.
		if let Some(maxfeerate) = maxfeerate {
			params.insert(("maxfeerate", maxfeerate.to_sat_per_vb_ceil().to_string()));
		}
		// Esplora expects `maxburnamount` in BTC.
		if let Some(maxburnamount) = maxburnamount {
			params.insert(("maxburnamount", maxburnamount.to_btc().to_string()));
		}

		let response = self.post_request(
			"/txs/package",
			serde_json::to_string(&serialized_txs).map_err(Error::SerdeJson)?,
			Some(params),
		)?;

		let result = response.json::<SubmitPackageResult>()?;

		Ok(result)
	}

	/// Get the block height of the current blockchain tip.
	pub fn get_height(&self) -> Result<u32, Error> {
		self.get_response_text("/blocks/tip/height")
			.map(|s| u32::from_str(s.as_str()).map_err(Error::Parsing))?
	}

	/// Get the [`BlockHash`] of the current blockchain tip.
	pub fn get_tip_hash(&self) -> Result<BlockHash, Error> {
		self.get_response_text("/blocks/tip/hash")
			.map(|s| BlockHash::from_str(s.as_str()).map_err(Error::HexToArray))?
	}

	/// Get the [`BlockHash`] of a [`Block`] given its height.
	pub fn get_block_hash(&self, block_height: u32) -> Result<BlockHash, Error> {
		self.get_response_text(&format!("/block-height/{block_height}"))
			.map(|s| BlockHash::from_str(s.as_str()).map_err(Error::HexToArray))?
	}

	/// Get global statistics about the mempool.
	///
	/// Returns a [`MempoolStats`] containing the transaction count, total
	/// virtual size, total fees, and fee rate histogram.
	pub fn get_mempool_stats(&self) -> Result<MempoolStats, Error> {
		self.get_response_json("/mempool")
	}

	/// Get the last 10 [`MempoolRecentTx`]s to enter the mempool.
	pub fn get_mempool_recent_txs(&self) -> Result<Vec<MempoolRecentTx>, Error> {
		self.get_response_json("/mempool/recent")
	}

	/// Get the full list of [`Txid`]s currently in the mempool.
	///
	/// The order of the returned [`Txid`]s is arbitrary.
	pub fn get_mempool_txids(&self) -> Result<Vec<Txid>, Error> {
		self.get_response_json("/mempool/txids")
	}

	/// Get fee estimates for a range of confirmation targets.
	///
	/// Returns a [`HashMap`] where the key is the confirmation target in blocks
	/// and the value is the estimated [`FeeRate`].
	pub fn get_fee_estimates(&self) -> Result<HashMap<u16, FeeRate>, Error> {
		let estimates_raw: HashMap<u16, f64> = self.get_response_json("/fee-estimates")?;
		let estimates = sat_per_vbyte_to_feerate(estimates_raw);

		Ok(estimates)
	}

	/// Get statistics about an [`Address`].
	///
	/// Returns an [`AddressStats`] containing confirmed and mempool transaction
	/// summaries for the given address, including funded and spent output
	/// counts and their total values.
	pub fn get_address_stats(&self, address: &Address) -> Result<AddressStats, Error> {
		let path = format!("/address/{address}");
		self.get_response_json(&path)
	}

	/// Get statistics about a [`Script`] hash's confirmed and mempool transactions.
	///
	/// Returns a [`ScriptHashStats`] containing transaction summaries for the
	/// SHA256 hash of the given [`Script`].
	pub fn get_scripthash_stats(&self, script: &Script) -> Result<ScriptHashStats, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = format!("/scripthash/{script_hash}");
		self.get_response_json(&path)
	}

	/// Get confirmed transaction history for an [`Address`], sorted newest first.
	///
	/// Returns up to 50 mempool transactions plus the first 25 confirmed transactions.
	/// To paginate, pass the [`Txid`] of the last transaction seen in the
	/// previous response as `last_seen`.
	pub fn get_address_txs(
		&self, address: &Address, last_seen: Option<Txid>,
	) -> Result<Vec<EsploraTx>, Error> {
		let path = match last_seen {
			Some(last_seen) => format!("/address/{address}/txs/chain/{last_seen}"),
			None => format!("/address/{address}/txs"),
		};

		self.get_response_json(&path)
	}

	/// Get unconfirmed mempool [`EsploraTx`]s for an [`Address`], sorted newest first.
	pub fn get_mempool_address_txs(&self, address: &Address) -> Result<Vec<EsploraTx>, Error> {
		let path = format!("/address/{address}/txs/mempool");

		self.get_response_json(&path)
	}

	/// Get confirmed transaction history for a [`Script`] hash, sorted newest first.
	///
	/// Returns 25 transactions per page. To paginate, pass the [`Txid`] of the
	/// last transaction seen in the previous response as `last_seen`.
	pub fn get_scripthash_txs(
		&self, script: &Script, last_seen: Option<Txid>,
	) -> Result<Vec<EsploraTx>, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = match last_seen {
			Some(last_seen) => format!("/scripthash/{script_hash:x}/txs/chain/{last_seen}"),
			None => format!("/scripthash/{script_hash:x}/txs"),
		};
		self.get_response_json(&path)
	}

	/// Get unconfirmed mempool [`EsploraTx`]s for a [`Script`] hash, sorted newest first.
	pub fn get_mempool_scripthash_txs(&self, script: &Script) -> Result<Vec<EsploraTx>, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = format!("/scripthash/{script_hash:x}/txs/mempool");

		self.get_response_json(&path)
	}

	/// Get a [`BlockInfo`] summary for the [`Block`] with the given [`BlockHash`].
	///
	/// [`BlockInfo`] includes metadata such as the height, timestamp,
	/// [`Transaction`] count, size, and [`Weight`](bitcoin::Weight).
	///
	/// This method does not return the full [`Block`].
	pub fn get_block_info(&self, blockhash: &BlockHash) -> Result<BlockInfo, Error> {
		let path = format!("/block/{blockhash}");

		self.get_response_json(&path)
	}

	/// Get all [`Txid`]s of [`Transaction`]s included in the [`Block`] with the
	/// given [`BlockHash`].
	pub fn get_block_txids(&self, blockhash: &BlockHash) -> Result<Vec<Txid>, Error> {
		let path = format!("/block/{blockhash}/txids");

		self.get_response_json(&path)
	}

	/// Get up to 25 [`EsploraTx`]s from the [`Block`] with the given
	/// [`BlockHash`], starting at `start_index`.
	///
	/// If `start_index` is `None`, starts from the first transaction (index 0).
	///
	/// Note that `start_index` must be a multiple of 25, otherwise the server
	/// will return an error.
	pub fn get_block_txs(
		&self, blockhash: &BlockHash, start_index: Option<u32>,
	) -> Result<Vec<EsploraTx>, Error> {
		let path = match start_index {
			None => format!("/block/{blockhash}/txs"),
			Some(start_index) => format!("/block/{blockhash}/txs/{start_index}"),
		};

		self.get_response_json(&path)
	}

	/// Get [`BlockInfo`] summaries for recent [`Block`]s.
	///
	/// If `height` is `Some(h)`, returns blocks starting from height `h`.
	/// If `height` is `None`, returns blocks starting from the current tip.
	///
	/// The maximum number of summaries returned depends on the backend itself:
	/// Esplora returns `10` while [mempool.space](https://mempool.space/docs/api) returns `15`.
	#[allow(deprecated)]
	#[deprecated(since = "0.13.0", note = "use `get_block_infos` instead")]
	pub fn get_blocks(&self, height: Option<u32>) -> Result<Vec<BlockSummary>, Error> {
		let path = match height {
			Some(height) => format!("/blocks/{height}"),
			None => "/blocks".to_string(),
		};
		let blocks: Vec<BlockSummary> = self.get_response_json(&path)?;
		if blocks.is_empty() {
			return Err(Error::InvalidResponse);
		}
		Ok(blocks)
	}

	/// Get [`BlockInfo`] summaries for recent [`Block`]s.
	///
	/// If `height` is `Some(h)`, returns blocks starting from height `h`.
	/// If `height` is `None`, returns blocks starting from the current tip.
	///
	/// The maximum number of summaries returned depends on the backend itself:
	/// Esplora returns `10` while [mempool.space](https://mempool.space/docs/api) returns `15`.
	///
	/// # Errors
	///
	/// Returns [`Error::InvalidResponse`] if the server returns an empty list.
	///
	/// This method does not return the full [`Block`].
	pub fn get_block_infos(&self, height: Option<u32>) -> Result<Vec<BlockInfo>, Error> {
		let path = match height {
			Some(height) => format!("/blocks/{height}"),
			None => "/blocks".to_string(),
		};
		let blocks: Vec<BlockInfo> = self.get_response_json(&path)?;
		if blocks.is_empty() {
			return Err(Error::InvalidResponse);
		}
		Ok(blocks)
	}

	/// Get all confirmed [`Utxo`]s locked to the given [`Address`].
	pub fn get_address_utxos(&self, address: &Address) -> Result<Vec<Utxo>, Error> {
		let path = format!("/address/{address}/utxo");

		self.get_response_json(&path)
	}

	/// Get all confirmed [`Utxo`]s locked to the given [`Script`].
	pub fn get_scripthash_utxos(&self, script: &Script) -> Result<Vec<Utxo>, Error> {
		let script_hash = sha256::Hash::hash(script.as_bytes());
		let path = format!("/scripthash/{script_hash}/utxo");

		self.get_response_json(&path)
	}
}
