// SPDX-License-Identifier: MIT OR Apache-2.0

//! # Esplora Client
//!
//! A client library for querying [Esplora] HTTP APIs from Rust.
//!
//! The crate exposes a shared set of Esplora response types in [`api`], plus
//! optional blocking and async clients built on [`bitreq`]. Both clients use the
//! same [`Builder`] configuration for the base URL, proxy, timeout, custom
//! headers, and retry policy.
//!
//! # Client Modes
//!
//! Enable the `blocking` feature to use `BlockingClient`, whose methods block
//! the current thread until each request completes. Enable the `async` feature
//! to use `AsyncClient`, whose methods return futures and require an async
//! runtime. The default async sleeper is backed by Tokio when the `tokio`
//! feature is enabled; custom runtimes can supply their own `Sleeper`.
//!
//! # Examples
//!
//! Create a blocking client:
//!
//! ```rust,ignore
//! use esplora_client::Builder;
//!
//! fn main() -> Result<(), esplora_client::Error> {
//!     let builder = Builder::new("https://blockstream.info/testnet/api");
//!     let blocking_client = builder.build_blocking();
//!     let height = blocking_client.get_height()?;
//!
//!     Ok(())
//! }
//! ```
//!
//! Create an async client:
//!
//! ```rust,ignore
//! use esplora_client::Builder;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), esplora_client::Error> {
//! let builder = Builder::new("https://blockstream.info/testnet/api");
//! let async_client = builder.build_async()?;
//! let height = async_client.get_height().await?;
//!
//! Ok(())
//! }
//! ```
//!
//! # Retries
//!
//! Both clients retry responses with status codes listed in
//! [`RETRYABLE_ERROR_CODES`]. Retry attempts use exponential backoff starting
//! at 256 milliseconds and are controlled by [`Builder::max_retries`].
//!
//! # Features
//!
//! By default the crate enables all features. To select only the pieces you
//! need, set `default-features = false` in `Cargo.toml` and list the desired
//! features explicitly:
//!
//! ```toml
//! esplora-client = { version = "*", default-features = false, features = ["blocking"] }
//! ```
//!
//! * `blocking` enables [`bitreq`], the blocking client with proxy.
//! * `blocking-https` enables [`bitreq`], the blocking client with proxy and TLS (SSL) capabilities
//!   using the default [`bitreq`] backend.
//! * `blocking-https-rustls` enables [`bitreq`], the blocking client with proxy and TLS (SSL)
//!   capabilities using the `rustls` backend.
//! * `blocking-https-native` enables [`bitreq`], the blocking client with proxy and TLS (SSL)
//!   capabilities using the platform's native TLS backend (likely OpenSSL).
//! * `blocking-https-rustls-probe` enables [`bitreq`], the blocking client with proxy and TLS (SSL)
//!   capabilities using `rustls` and probed system roots.
//! * `async` enables [`bitreq`], the async client with proxy capabilities.
//! * `async-https` enables [`bitreq`], the async client with support for proxying and TLS (SSL)
//!   using the default [`bitreq`] TLS backend.
//! * `async-https-native` enables [`bitreq`], the async client with support for proxying and TLS
//!   (SSL) using the platform's native TLS backend (likely OpenSSL).
//! * `async-https-rustls` enables [`bitreq`], the async client with support for proxying and TLS
//!   (SSL) using the `rustls` TLS backend.
//! * `async-https-rustls-probe` enables [`bitreq`], the async client with support for proxying and
//!   TLS (SSL) using `rustls` and probed system roots.
//! * `tokio` enables the default async sleeper used by [`Builder::build_async`].
//!
//! [Esplora]: https://github.com/Blockstream/esplora/blob/master/API.md
//! [`bitreq`]: https://docs.rs/bitreq
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]
#![allow(deprecated)]

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::num::TryFromIntError;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "async")]
pub use r#async::Sleeper;

pub mod api;
#[cfg(feature = "async")]
pub mod r#async;
#[cfg(feature = "blocking")]
pub mod blocking;

pub use api::*;
#[cfg(any(feature = "blocking", feature = "async"))]
use bitreq::Response;
#[cfg(feature = "blocking")]
pub use blocking::BlockingClient;
#[cfg(feature = "async")]
pub use r#async::AsyncClient;

/// Future returned by a dynamic Esplora header provider.
#[cfg(feature = "async")]
pub type HeaderProviderFuture =
	Pin<Box<dyn Future<Output = Result<HashMap<String, String>, String>> + Send>>;

/// Callback used to obtain HTTP headers for each Esplora request.
#[cfg(feature = "async")]
pub type HeaderProvider = Arc<dyn Fn() -> HeaderProviderFuture + Send + Sync>;

/// HTTP response status codes for which a request may be retried.
pub const RETRYABLE_ERROR_CODES: [u16; 3] = [
	429, // TOO_MANY_REQUESTS
	500, // INTERNAL_SERVER_ERROR
	503, // SERVICE_UNAVAILABLE
];

/// Base delay used by the exponential retry backoff.
#[cfg(any(feature = "blocking", feature = "async"))]
const BASE_BACKOFF_MILLIS: Duration = Duration::from_millis(256);

/// Default maximum number of retry attempts per request.
const DEFAULT_MAX_RETRIES: usize = 6;

/// Default maximum number of cached connections for the async client.
#[cfg(feature = "async")]
const DEFAULT_MAX_CONNECTIONS: usize = 10;

/// Check if the [`Response`] status code is informational (100-199).
#[allow(unused)]
#[cfg(any(feature = "blocking", feature = "async"))]
fn is_informational(response: &Response) -> bool {
	(100..200).contains(&response.status_code)
}

/// Check if the [`Response`] status code is successful (200-299).
#[cfg(any(feature = "blocking", feature = "async"))]
fn is_success(response: &Response) -> bool {
	(200..300).contains(&response.status_code)
}

/// Check if the [`Response`] status code is a redirection (300-399).
#[allow(unused)]
#[cfg(any(feature = "blocking", feature = "async"))]
fn is_redirection(response: &Response) -> bool {
	(300..400).contains(&response.status_code)
}

/// Check if the [`Response`] status code is a client error (400-499).
#[allow(unused)]
#[cfg(any(feature = "blocking", feature = "async"))]
fn is_client_error(response: &Response) -> bool {
	(400..500).contains(&response.status_code)
}

/// Check if the [`Response`] status code is a server error (500-599).
#[allow(unused)]
#[cfg(any(feature = "blocking", feature = "async"))]
fn is_server_error(response: &Response) -> bool {
	(500..600).contains(&response.status_code)
}

/// Check if the [`Response`] status code is retryable (429, 500, 503).
#[cfg(any(feature = "blocking", feature = "async"))]
fn is_retryable(response: &Response) -> bool {
	RETRYABLE_ERROR_CODES.contains(&(response.status_code as u16))
}

/// Convert a [`Duration`] to whole timeout seconds for `bitreq`.
#[cfg(any(feature = "blocking", feature = "async"))]
fn duration_to_timeout_secs(duration: Duration) -> u64 {
	if duration.subsec_nanos() == 0 {
		duration.as_secs()
	} else {
		duration.as_secs().saturating_add(1)
	}
}

/// Return the [`FeeRate`] for the given confirmation target in blocks.
///
/// Selects the highest confirmation target from `estimates` that is at or
/// below `target_blocks`, and returns its [`FeeRate`]. Returns `None` if no
/// matching estimate is found.
pub fn convert_fee_rate(target_blocks: usize, estimates: HashMap<u16, FeeRate>) -> Option<FeeRate> {
	estimates
		.into_iter()
		.filter(|(k, _)| *k as usize <= target_blocks)
		.max_by_key(|(k, _)| *k)
		.map(|(_, feerate)| feerate)
}

/// Convert a [`HashMap`] of fee estimates expressed as sat/vB ([`f64`]) into [`FeeRate`]s.
pub fn sat_per_vbyte_to_feerate(estimates: HashMap<u16, f64>) -> HashMap<u16, FeeRate> {
	estimates
		.into_iter()
		.map(|(k, v)| (k, FeeRate::from_sat_per_kwu((v * 250.0).round() as u64)))
		.collect()
}

/// Shared configuration for `BlockingClient` and `AsyncClient`.
///
/// Start with [`Builder::new`] and then chain optional configuration methods
/// before calling `Builder::build_blocking`, `Builder::build_async`, or
/// `Builder::build_async_with_sleeper`.
///
/// # Example
///
/// ```no_run
/// use std::time::Duration;
///
/// # #[cfg(feature = "blocking")]
/// # {
/// let client = esplora_client::Builder::new("https://mempool.space/testnet/api")
///     .timeout(Duration::from_secs(30))
///     .max_retries(4)
///     .header("user-agent", "my-wallet/0.1")
///     .build_blocking();
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Builder {
	/// The base URL of the Esplora server.
	///
	/// This should include the API prefix expected by the server, for example
	/// `https://mempool.space/api` or `https://blockstream.info/testnet/api`.
	pub base_url: String,
	/// Optional proxy URL used for requests to the Esplora server.
	///
	/// The string should be formatted as:
	/// `<protocol>://<user>:<password>@host:<port>`.
	///
	/// Note that the format of this value and the supported protocols change
	/// slightly by target and enabled transport features. See [bitreq]'s
	/// proxy documentation for the accepted schemes.
	///
	/// The proxy is ignored when targeting `wasm32`.
	pub proxy: Option<String>,
	/// Per-request socket timeout.
	pub timeout: Option<Duration>,
	/// HTTP headers to set on every request made to the Esplora server.
	pub headers: HashMap<String, String>,
	/// Maximum number of retry attempts for retryable HTTP responses.
	pub max_retries: usize,
	/// Maximum number of cached connections for the async client.
	#[cfg(feature = "async")]
	pub max_connections: usize,
}

impl Builder {
	/// Create a [`Builder`] for an Esplora server base URL.
	///
	/// The URL is stored exactly as provided and request paths are appended to
	/// it. Do not include a trailing slash unless your server expects one.
	pub fn new(base_url: &str) -> Self {
		Builder {
			base_url: base_url.to_string(),
			proxy: None,
			timeout: None,
			headers: HashMap::new(),
			max_retries: DEFAULT_MAX_RETRIES,
			#[cfg(feature = "async")]
			max_connections: DEFAULT_MAX_CONNECTIONS,
		}
	}

	/// Set the proxy URL used for requests.
	///
	/// The proxy is ignored when targeting `wasm32`.
	pub fn proxy(mut self, proxy: &str) -> Self {
		self.proxy = Some(proxy.to_string());
		self
	}

	/// Set the per-request socket timeout.
	pub fn timeout(mut self, timeout: Duration) -> Self {
		self.timeout = Some(timeout);
		self
	}

	/// Add or replace an HTTP header sent with every request.
	pub fn header(mut self, key: &str, value: &str) -> Self {
		self.headers.insert(key.to_string(), value.to_string());
		self
	}

	/// Set the maximum number of retry attempts for retryable responses.
	///
	/// Only responses whose status code is listed in
	/// [`RETRYABLE_ERROR_CODES`] are retried.
	pub fn max_retries(mut self, count: usize) -> Self {
		self.max_retries = count;
		self
	}

	/// Set the maximum number of cached connections in the async client.
	#[cfg(feature = "async")]
	pub fn max_connections(mut self, count: usize) -> Self {
		self.max_connections = count;
		self
	}

	/// Build a [`BlockingClient`] from this configuration.
	#[cfg(feature = "blocking")]
	pub fn build_blocking(self) -> BlockingClient {
		BlockingClient::from_builder(self)
	}

	/// Build an [`AsyncClient`] from this configuration.
	///
	/// This uses `DefaultSleeper`, which is backed by Tokio.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the async HTTP client cannot be constructed.
	#[cfg(all(feature = "async", feature = "tokio"))]
	pub fn build_async(self) -> Result<AsyncClient, Error> {
		AsyncClient::from_builder(self)
	}

	/// Build an [`AsyncClient`] that fetches additional HTTP headers for each request.
	#[cfg(all(feature = "async", feature = "tokio"))]
	pub fn build_async_with_header_provider(
		self, header_provider: HeaderProvider,
	) -> Result<AsyncClient, Error> {
		AsyncClient::from_builder_with_header_provider(self, header_provider)
	}

	/// Build an [`AsyncClient`] with a user-defined [`Sleeper`].
	///
	/// Use this when integrating with an async runtime other than Tokio or when
	/// tests need a custom sleep implementation.
	///
	/// # Errors
	///
	/// Returns an [`Error`] if the async HTTP client cannot be constructed.
	#[cfg(feature = "async")]
	pub fn build_async_with_sleeper<S: Sleeper>(self) -> Result<AsyncClient<S>, Error> {
		AsyncClient::from_builder(self)
	}
}

/// Errors that can occur while building clients, sending requests, or decoding responses.
#[derive(Debug)]
pub enum Error {
	/// Error during a [`bitreq`] HTTP request.
	#[cfg(any(feature = "blocking", feature = "async"))]
	BitReq(bitreq::Error),
	/// Error during JSON serialization or deserialization.
	SerdeJson(serde_json::Error),
	/// Non-successful HTTP response from the Esplora server.
	HttpResponse {
		/// The HTTP status code returned by the server.
		status: u16,
		/// The response body returned by the server.
		message: String,
	},
	/// Invalid integer returned by the server.
	Parsing(std::num::ParseIntError),
	/// Invalid status code, unable to convert to `u16`.
	StatusCode(TryFromIntError),
	/// Invalid Bitcoin consensus data returned by the server.
	BitcoinEncoding(bitcoin::consensus::encode::Error),
	/// Invalid fixed-size hex data returned by the server.
	HexToArray(bitcoin::hex::HexToArrayError),
	/// Invalid variable-length hex data returned by the server.
	HexToBytes(bitcoin::hex::HexToBytesError),
	/// Transaction not found.
	TransactionNotFound(Txid),
	/// Block header height not found.
	HeaderHeightNotFound(u32),
	/// Block header hash not found.
	HeaderHashNotFound(BlockHash),
	/// Invalid HTTP header name specified in [`Builder::header`].
	InvalidHttpHeaderName(String),
	/// Invalid HTTP header value specified in [`Builder::header`].
	InvalidHttpHeaderValue(String),
	/// The dynamic HTTP header provider failed.
	#[cfg(feature = "async")]
	HeaderProvider(String),
	/// The server sent an invalid response.
	InvalidResponse,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			#[cfg(any(feature = "blocking", feature = "async"))]
			Error::BitReq(e) => write!(f, "Bitreq HTTP error: {e}"),
			Error::SerdeJson(e) => write!(f, "JSON (de)serialization error: {e}"),
			Error::HttpResponse { status, message } => {
				write!(f, "HTTP error {status}: {message}")
			},
			Error::Parsing(e) => write!(f, "Failed to parse invalid number: {e}"),
			Error::StatusCode(e) => write!(f, "Invalid status code: {e}"),
			Error::BitcoinEncoding(e) => write!(f, "Invalid Bitcoin data: {e}"),
			Error::HexToArray(e) => write!(f, "Invalid hex to array conversion: {e}"),
			Error::HexToBytes(e) => write!(f, "Invalid hex to bytes conversion: {e}"),
			Error::TransactionNotFound(txid) => {
				write!(f, "Transaction not found: {txid}")
			},
			Error::HeaderHeightNotFound(height) => {
				write!(f, "Block header at height {height} not found")
			},
			Error::HeaderHashNotFound(hash) => {
				write!(f, "Block header with hash {hash} not found")
			},
			Error::InvalidHttpHeaderName(name) => {
				write!(f, "Invalid HTTP header name: {name}")
			},
			Error::InvalidHttpHeaderValue(value) => {
				write!(f, "Invalid HTTP header value: {value}")
			},
			#[cfg(feature = "async")]
			Error::HeaderProvider(error) => write!(f, "HTTP header provider failed: {error}"),
			Error::InvalidResponse => write!(f, "The server sent an invalid response"),
		}
	}
}

impl std::error::Error for Error {}

macro_rules! impl_error {
	( $from:ty, $to:ident ) => {
		impl_error!($from, $to, Error);
	};
	( $from:ty, $to:ident, $impl_for:ty ) => {
		impl std::convert::From<$from> for $impl_for {
			fn from(err: $from) -> Self {
				<$impl_for>::$to(err)
			}
		}
	};
}

#[cfg(any(feature = "blocking", feature = "async"))]
impl_error!(::bitreq::Error, BitReq, Error);
impl_error!(serde_json::Error, SerdeJson, Error);
impl_error!(std::num::ParseIntError, Parsing, Error);
impl_error!(bitcoin::consensus::encode::Error, BitcoinEncoding, Error);
impl_error!(bitcoin::hex::HexToArrayError, HexToArray, Error);
impl_error!(bitcoin::hex::HexToBytesError, HexToBytes, Error);
