// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::service::Service;
use hyper::{Request, Response, StatusCode};
use ldk_node::bitcoin::hashes::hmac::{Hmac, HmacEngine};
use ldk_node::bitcoin::hashes::{sha256, Hash, HashEngine};
use ldk_node::Node;
use ldk_server_protos::endpoints::{
	BOLT11_RECEIVE_PATH, BOLT11_SEND_PATH, BOLT12_RECEIVE_PATH, BOLT12_SEND_PATH,
	CLOSE_CHANNEL_PATH, CONNECT_PEER_PATH, FORCE_CLOSE_CHANNEL_PATH, GET_BALANCES_PATH,
	GET_NODE_INFO_PATH, GET_PAYMENT_DETAILS_PATH, LIST_CHANNELS_PATH, LIST_FORWARDED_PAYMENTS_PATH,
	LIST_PAYMENTS_PATH, ONCHAIN_RECEIVE_PATH, ONCHAIN_SEND_PATH, OPEN_CHANNEL_PATH, SPLICE_IN_PATH,
	SPLICE_OUT_PATH, UPDATE_CHANNEL_CONFIG_PATH,
};
use prost::Message;

use crate::api::bolt11_receive::handle_bolt11_receive_request;
use crate::api::bolt11_send::handle_bolt11_send_request;
use crate::api::bolt12_receive::handle_bolt12_receive_request;
use crate::api::bolt12_send::handle_bolt12_send_request;
use crate::api::close_channel::{handle_close_channel_request, handle_force_close_channel_request};
use crate::api::connect_peer::handle_connect_peer;
use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::{AuthError, InvalidRequestError};
use crate::api::get_balances::handle_get_balances_request;
use crate::api::get_node_info::handle_get_node_info_request;
use crate::api::get_payment_details::handle_get_payment_details_request;
use crate::api::list_channels::handle_list_channels_request;
use crate::api::list_forwarded_payments::handle_list_forwarded_payments_request;
use crate::api::list_payments::handle_list_payments_request;
use crate::api::onchain_receive::handle_onchain_receive_request;
use crate::api::onchain_send::handle_onchain_send_request;
use crate::api::open_channel::handle_open_channel;
use crate::api::splice_channel::{handle_splice_in_request, handle_splice_out_request};
use crate::api::update_channel_config::handle_update_channel_config_request;
use crate::io::persist::paginated_kv_store::PaginatedKVStore;
use crate::util::proto_adapter::to_error_response;

// Maximum request body size: 10 MB
// This prevents memory exhaustion from large requests
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct NodeService {
	node: Arc<Node>,
	paginated_kv_store: Arc<dyn PaginatedKVStore>,
	api_key: String,
}

impl NodeService {
	pub(crate) fn new(
		node: Arc<Node>, paginated_kv_store: Arc<dyn PaginatedKVStore>, api_key: String,
	) -> Self {
		Self { node, paginated_kv_store, api_key }
	}
}

// Maximum allowed time difference between client timestamp and server time (1 minute)
const AUTH_TIMESTAMP_TOLERANCE_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub(crate) struct AuthParams {
	timestamp: u64,
	hmac_hex: String,
}

/// Extracts authentication parameters from request headers.
/// Returns (timestamp, hmac_hex) if valid format, or error.
fn extract_auth_params<B>(req: &Request<B>) -> Result<AuthParams, LdkServerError> {
	let auth_header = req
		.headers()
		.get("X-Auth")
		.and_then(|v| v.to_str().ok())
		.ok_or_else(|| LdkServerError::new(AuthError, "Missing X-Auth header"))?;

	// Format: "HMAC <timestamp>:<hmac_hex>"
	let auth_data = auth_header
		.strip_prefix("HMAC ")
		.ok_or_else(|| LdkServerError::new(AuthError, "Invalid X-Auth header format"))?;

	let (timestamp_str, hmac_hex) = auth_data
		.split_once(':')
		.ok_or_else(|| LdkServerError::new(AuthError, "Invalid X-Auth header format"))?;

	let timestamp = timestamp_str
		.parse::<u64>()
		.map_err(|_| LdkServerError::new(AuthError, "Invalid timestamp in X-Auth header"))?;

	// validate hmac_hex is valid hex
	if hmac_hex.len() != 64 || !hmac_hex.chars().all(|c| c.is_ascii_hexdigit()) {
		return Err(LdkServerError::new(AuthError, "Invalid HMAC in X-Auth header"));
	}

	Ok(AuthParams { timestamp, hmac_hex: hmac_hex.to_string() })
}

/// Validates the HMAC authentication after the request body has been read.
fn validate_hmac_auth(
	timestamp: u64, provided_hmac_hex: &str, body: &[u8], api_key: &str,
) -> Result<(), LdkServerError> {
	// Validate timestamp is within acceptable window
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_err(|_| LdkServerError::new(AuthError, "System time error"))?
		.as_secs();

	let time_diff = now.abs_diff(timestamp);
	if time_diff > AUTH_TIMESTAMP_TOLERANCE_SECS {
		return Err(LdkServerError::new(AuthError, "Request timestamp expired"));
	}

	// Compute expected HMAC: HMAC-SHA256(api_key, timestamp_bytes || body)
	let mut hmac_engine: HmacEngine<sha256::Hash> = HmacEngine::new(api_key.as_bytes());
	hmac_engine.input(&timestamp.to_be_bytes());
	hmac_engine.input(body);
	let expected_hmac = Hmac::<sha256::Hash>::from_engine(hmac_engine);

	// Compare HMACs (constant-time comparison via Hash equality)
	let expected_hex = expected_hmac.to_string();
	if expected_hex != provided_hmac_hex {
		return Err(LdkServerError::new(AuthError, "Invalid credentials"));
	}

	Ok(())
}

pub(crate) struct Context {
	pub(crate) node: Arc<Node>,
	pub(crate) paginated_kv_store: Arc<dyn PaginatedKVStore>,
}

impl Service<Request<Incoming>> for NodeService {
	type Response = Response<Full<Bytes>>;
	type Error = hyper::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn call(&self, req: Request<Incoming>) -> Self::Future {
		// Extract auth params from headers (validation happens after body is read)
		let auth_params = match extract_auth_params(&req) {
			Ok(params) => params,
			Err(e) => {
				let (error_response, status_code) = to_error_response(e);
				return Box::pin(async move {
					Ok(Response::builder()
						.status(status_code)
						.body(Full::new(Bytes::from(error_response.encode_to_vec())))
						// unwrap safety: body only errors when previous chained calls failed.
						.unwrap())
				});
			},
		};

		let context = Context {
			node: Arc::clone(&self.node),
			paginated_kv_store: Arc::clone(&self.paginated_kv_store),
		};
		let api_key = self.api_key.clone();

		// Exclude '/' from path pattern matching.
		match &req.uri().path()[1..] {
			GET_NODE_INFO_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_get_node_info_request,
			)),
			GET_BALANCES_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_get_balances_request,
			)),
			ONCHAIN_RECEIVE_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_onchain_receive_request,
			)),
			ONCHAIN_SEND_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_onchain_send_request,
			)),
			BOLT11_RECEIVE_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_bolt11_receive_request,
			)),
			BOLT11_SEND_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_bolt11_send_request,
			)),
			BOLT12_RECEIVE_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_bolt12_receive_request,
			)),
			BOLT12_SEND_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_bolt12_send_request,
			)),
			OPEN_CHANNEL_PATH => {
				Box::pin(handle_request(context, req, auth_params, api_key, handle_open_channel))
			},
			SPLICE_IN_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_splice_in_request,
			)),
			SPLICE_OUT_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_splice_out_request,
			)),
			CLOSE_CHANNEL_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_close_channel_request,
			)),
			FORCE_CLOSE_CHANNEL_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_force_close_channel_request,
			)),
			LIST_CHANNELS_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_list_channels_request,
			)),
			UPDATE_CHANNEL_CONFIG_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_update_channel_config_request,
			)),
			GET_PAYMENT_DETAILS_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_get_payment_details_request,
			)),
			LIST_PAYMENTS_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_list_payments_request,
			)),
			LIST_FORWARDED_PAYMENTS_PATH => Box::pin(handle_request(
				context,
				req,
				auth_params,
				api_key,
				handle_list_forwarded_payments_request,
			)),
			CONNECT_PEER_PATH => {
				Box::pin(handle_request(context, req, auth_params, api_key, handle_connect_peer))
			},
			path => {
				let error = format!("Unknown request: {}", path).into_bytes();
				Box::pin(async {
					Ok(Response::builder()
						.status(StatusCode::BAD_REQUEST)
						.body(Full::new(Bytes::from(error)))
						// unwrap safety: body only errors when previous chained calls failed.
						.unwrap())
				})
			},
		}
	}
}

async fn handle_request<
	T: Message + Default,
	R: Message,
	F: Fn(Context, T) -> Result<R, LdkServerError>,
>(
	context: Context, request: Request<Incoming>, auth_params: AuthParams, api_key: String,
	handler: F,
) -> Result<<NodeService as Service<Request<Incoming>>>::Response, hyper::Error> {
	// Limit the size of the request body to prevent abuse
	let limited_body = Limited::new(request.into_body(), MAX_BODY_SIZE);
	let bytes = match limited_body.collect().await {
		Ok(collected) => collected.to_bytes(),
		Err(_) => {
			let (error_response, status_code) = to_error_response(LdkServerError::new(
				InvalidRequestError,
				"Request body too large or failed to read.",
			));
			return Ok(Response::builder()
				.status(status_code)
				.body(Full::new(Bytes::from(error_response.encode_to_vec())))
				// unwrap safety: body only errors when previous chained calls failed.
				.unwrap());
		},
	};

	// Validate HMAC authentication with the request body
	if let Err(e) =
		validate_hmac_auth(auth_params.timestamp, &auth_params.hmac_hex, &bytes, &api_key)
	{
		let (error_response, status_code) = to_error_response(e);
		return Ok(Response::builder()
			.status(status_code)
			.body(Full::new(Bytes::from(error_response.encode_to_vec())))
			// unwrap safety: body only errors when previous chained calls failed.
			.unwrap());
	}

	match T::decode(bytes) {
		Ok(request) => match handler(context, request) {
			Ok(response) => Ok(Response::builder()
				.body(Full::new(Bytes::from(response.encode_to_vec())))
				// unwrap safety: body only errors when previous chained calls failed.
				.unwrap()),
			Err(e) => {
				let (error_response, status_code) = to_error_response(e);
				Ok(Response::builder()
					.status(status_code)
					.body(Full::new(Bytes::from(error_response.encode_to_vec())))
					// unwrap safety: body only errors when previous chained calls failed.
					.unwrap())
			},
		},
		Err(_) => {
			let (error_response, status_code) =
				to_error_response(LdkServerError::new(InvalidRequestError, "Malformed request."));
			Ok(Response::builder()
				.status(status_code)
				.body(Full::new(Bytes::from(error_response.encode_to_vec())))
				// unwrap safety: body only errors when previous chained calls failed.
				.unwrap())
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn compute_hmac(api_key: &str, timestamp: u64, body: &[u8]) -> String {
		let mut hmac_engine: HmacEngine<sha256::Hash> = HmacEngine::new(api_key.as_bytes());
		hmac_engine.input(&timestamp.to_be_bytes());
		hmac_engine.input(body);
		Hmac::<sha256::Hash>::from_engine(hmac_engine).to_string()
	}

	fn create_test_request(auth_header: Option<String>) -> Request<()> {
		let mut builder = Request::builder();
		if let Some(header) = auth_header {
			builder = builder.header("X-Auth", header);
		}
		builder.body(()).unwrap()
	}

	#[test]
	fn test_extract_auth_params_success() {
		let timestamp =
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		let hmac = "8f5a33c2c68fb253899a588308fd13dcaf162d2788966a1fb6cc3aa2e0c51a93";
		let auth_header = format!("HMAC {timestamp}:{hmac}");

		let req = create_test_request(Some(auth_header));

		let result = extract_auth_params(&req);
		assert!(result.is_ok());
		let AuthParams { timestamp: ts, hmac_hex } = result.unwrap();
		assert_eq!(ts, timestamp);
		assert_eq!(hmac_hex, hmac);
	}

	#[test]
	fn test_extract_auth_params_missing_header() {
		let req = create_test_request(None);

		let result = extract_auth_params(&req);
		assert!(result.is_err());
		assert_eq!(result.unwrap_err().error_code, AuthError);
	}

	#[test]
	fn test_extract_auth_params_invalid_format() {
		// Missing "HMAC " prefix
		let req = create_test_request(Some("12345:deadbeef".to_string()));

		let result = extract_auth_params(&req);
		assert!(result.is_err());
		assert_eq!(result.unwrap_err().error_code, AuthError);
	}

	#[test]
	fn test_validate_hmac_auth_success() {
		let api_key = "test_api_key".to_string();
		let body = b"test request body";
		let timestamp =
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		let hmac = compute_hmac(&api_key, timestamp, body);

		let result = validate_hmac_auth(timestamp, &hmac, body, &api_key);
		assert!(result.is_ok());
	}

	#[test]
	fn test_validate_hmac_auth_wrong_key() {
		let api_key = "test_api_key".to_string();
		let body = b"test request body";
		let timestamp =
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		// Compute HMAC with wrong key
		let hmac = compute_hmac("wrong_key", timestamp, body);

		let result = validate_hmac_auth(timestamp, &hmac, body, &api_key);
		assert!(result.is_err());
		assert_eq!(result.unwrap_err().error_code, AuthError);
	}

	#[test]
	fn test_validate_hmac_auth_expired_timestamp() {
		let api_key = "test_api_key".to_string();
		let body = b"test request body";
		// Use a timestamp from 10 minutes ago
		let timestamp =
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
				- 600;
		let hmac = compute_hmac(&api_key, timestamp, body);

		let result = validate_hmac_auth(timestamp, &hmac, body, &api_key);
		assert!(result.is_err());
		assert_eq!(result.unwrap_err().error_code, AuthError);
	}

	#[test]
	fn test_validate_hmac_auth_tampered_body() {
		let api_key = "test_api_key".to_string();
		let original_body = b"test request body";
		let tampered_body = b"tampered body";
		let timestamp =
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
		// Compute HMAC with original body
		let hmac = compute_hmac(&api_key, timestamp, original_body);

		// Try to validate with tampered body
		let result = validate_hmac_auth(timestamp, &hmac, tampered_body, &api_key);
		assert!(result.is_err());
		assert_eq!(result.unwrap_err().error_code, AuthError);
	}
}
