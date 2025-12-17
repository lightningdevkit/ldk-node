// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use prost::Message;

use crate::error::LdkServerError;
use crate::error::LdkServerErrorCode::{
	AuthError, InternalError, InternalServerError, InvalidRequestError, LightningError,
};
use ldk_server_protos::api::{
	Bolt11ReceiveRequest, Bolt11ReceiveResponse, Bolt11SendRequest, Bolt11SendResponse,
	Bolt12ReceiveRequest, Bolt12ReceiveResponse, Bolt12SendRequest, Bolt12SendResponse,
	CloseChannelRequest, CloseChannelResponse, ForceCloseChannelRequest, ForceCloseChannelResponse,
	GetBalancesRequest, GetBalancesResponse, GetNodeInfoRequest, GetNodeInfoResponse,
	ListChannelsRequest, ListChannelsResponse, ListPaymentsRequest, ListPaymentsResponse,
	OnchainReceiveRequest, OnchainReceiveResponse, OnchainSendRequest, OnchainSendResponse,
	OpenChannelRequest, OpenChannelResponse, SpliceInRequest, SpliceInResponse, SpliceOutRequest,
	SpliceOutResponse, UpdateChannelConfigRequest, UpdateChannelConfigResponse,
};
use ldk_server_protos::endpoints::{
	BOLT11_RECEIVE_PATH, BOLT11_SEND_PATH, BOLT12_RECEIVE_PATH, BOLT12_SEND_PATH,
	CLOSE_CHANNEL_PATH, FORCE_CLOSE_CHANNEL_PATH, GET_BALANCES_PATH, GET_NODE_INFO_PATH,
	LIST_CHANNELS_PATH, LIST_PAYMENTS_PATH, ONCHAIN_RECEIVE_PATH, ONCHAIN_SEND_PATH,
	OPEN_CHANNEL_PATH, SPLICE_IN_PATH, SPLICE_OUT_PATH, UPDATE_CHANNEL_CONFIG_PATH,
};
use ldk_server_protos::error::{ErrorCode, ErrorResponse};
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;

const APPLICATION_OCTET_STREAM: &str = "application/octet-stream";

/// Client to access a hosted instance of LDK Server.
#[derive(Clone)]
pub struct LdkServerClient {
	base_url: String,
	client: Client,
}

impl LdkServerClient {
	/// Constructs a [`LdkServerClient`] using `base_url` as the ldk-server endpoint.
	pub fn new(base_url: String) -> Self {
		Self { base_url, client: Client::new() }
	}

	/// Retrieve the latest node info like `node_id`, `current_best_block` etc.
	/// For API contract/usage, refer to docs for [`GetNodeInfoRequest`] and [`GetNodeInfoResponse`].
	pub async fn get_node_info(
		&self, request: GetNodeInfoRequest,
	) -> Result<GetNodeInfoResponse, LdkServerError> {
		let url = format!("http://{}/{GET_NODE_INFO_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Retrieves an overview of all known balances.
	/// For API contract/usage, refer to docs for [`GetBalancesRequest`] and [`GetBalancesResponse`].
	pub async fn get_balances(
		&self, request: GetBalancesRequest,
	) -> Result<GetBalancesResponse, LdkServerError> {
		let url = format!("http://{}/{GET_BALANCES_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Retrieve a new on-chain funding address.
	/// For API contract/usage, refer to docs for [`OnchainReceiveRequest`] and [`OnchainReceiveResponse`].
	pub async fn onchain_receive(
		&self, request: OnchainReceiveRequest,
	) -> Result<OnchainReceiveResponse, LdkServerError> {
		let url = format!("http://{}/{ONCHAIN_RECEIVE_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Send an on-chain payment to the given address.
	/// For API contract/usage, refer to docs for [`OnchainSendRequest`] and [`OnchainSendResponse`].
	pub async fn onchain_send(
		&self, request: OnchainSendRequest,
	) -> Result<OnchainSendResponse, LdkServerError> {
		let url = format!("http://{}/{ONCHAIN_SEND_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Retrieve a new BOLT11 payable invoice.
	/// For API contract/usage, refer to docs for [`Bolt11ReceiveRequest`] and [`Bolt11ReceiveResponse`].
	pub async fn bolt11_receive(
		&self, request: Bolt11ReceiveRequest,
	) -> Result<Bolt11ReceiveResponse, LdkServerError> {
		let url = format!("http://{}/{BOLT11_RECEIVE_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Send a payment for a BOLT11 invoice.
	/// For API contract/usage, refer to docs for [`Bolt11SendRequest`] and [`Bolt11SendResponse`].
	pub async fn bolt11_send(
		&self, request: Bolt11SendRequest,
	) -> Result<Bolt11SendResponse, LdkServerError> {
		let url = format!("http://{}/{BOLT11_SEND_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Retrieve a new BOLT11 payable offer.
	/// For API contract/usage, refer to docs for [`Bolt12ReceiveRequest`] and [`Bolt12ReceiveResponse`].
	pub async fn bolt12_receive(
		&self, request: Bolt12ReceiveRequest,
	) -> Result<Bolt12ReceiveResponse, LdkServerError> {
		let url = format!("http://{}/{BOLT12_RECEIVE_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Send a payment for a BOLT12 offer.
	/// For API contract/usage, refer to docs for [`Bolt12SendRequest`] and [`Bolt12SendResponse`].
	pub async fn bolt12_send(
		&self, request: Bolt12SendRequest,
	) -> Result<Bolt12SendResponse, LdkServerError> {
		let url = format!("http://{}/{BOLT12_SEND_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Creates a new outbound channel.
	/// For API contract/usage, refer to docs for [`OpenChannelRequest`] and [`OpenChannelResponse`].
	pub async fn open_channel(
		&self, request: OpenChannelRequest,
	) -> Result<OpenChannelResponse, LdkServerError> {
		let url = format!("http://{}/{OPEN_CHANNEL_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Splices funds into the channel specified by given request.
	/// For API contract/usage, refer to docs for [`SpliceInRequest`] and [`SpliceInResponse`].
	pub async fn splice_in(
		&self, request: SpliceInRequest,
	) -> Result<SpliceInResponse, LdkServerError> {
		let url = format!("http://{}/{SPLICE_IN_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Splices funds out of the channel specified by given request.
	/// For API contract/usage, refer to docs for [`SpliceOutRequest`] and [`SpliceOutResponse`].
	pub async fn splice_out(
		&self, request: SpliceOutRequest,
	) -> Result<SpliceOutResponse, LdkServerError> {
		let url = format!("http://{}/{SPLICE_OUT_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Closes the channel specified by given request.
	/// For API contract/usage, refer to docs for [`CloseChannelRequest`] and [`CloseChannelResponse`].
	pub async fn close_channel(
		&self, request: CloseChannelRequest,
	) -> Result<CloseChannelResponse, LdkServerError> {
		let url = format!("http://{}/{CLOSE_CHANNEL_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Force closes the channel specified by given request.
	/// For API contract/usage, refer to docs for [`ForceCloseChannelRequest`] and [`ForceCloseChannelResponse`].
	pub async fn force_close_channel(
		&self, request: ForceCloseChannelRequest,
	) -> Result<ForceCloseChannelResponse, LdkServerError> {
		let url = format!("http://{}/{FORCE_CLOSE_CHANNEL_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Retrieves list of known channels.
	/// For API contract/usage, refer to docs for [`ListChannelsRequest`] and [`ListChannelsResponse`].
	pub async fn list_channels(
		&self, request: ListChannelsRequest,
	) -> Result<ListChannelsResponse, LdkServerError> {
		let url = format!("http://{}/{LIST_CHANNELS_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Retrieves list of all payments sent or received by us.
	/// For API contract/usage, refer to docs for [`ListPaymentsRequest`] and [`ListPaymentsResponse`].
	pub async fn list_payments(
		&self, request: ListPaymentsRequest,
	) -> Result<ListPaymentsResponse, LdkServerError> {
		let url = format!("http://{}/{LIST_PAYMENTS_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	/// Updates the config for a previously opened channel.
	/// For API contract/usage, refer to docs for [`UpdateChannelConfigRequest`] and [`UpdateChannelConfigResponse`].
	pub async fn update_channel_config(
		&self, request: UpdateChannelConfigRequest,
	) -> Result<UpdateChannelConfigResponse, LdkServerError> {
		let url = format!("http://{}/{UPDATE_CHANNEL_CONFIG_PATH}", self.base_url);
		self.post_request(&request, &url).await
	}

	async fn post_request<Rq: Message, Rs: Message + Default>(
		&self, request: &Rq, url: &str,
	) -> Result<Rs, LdkServerError> {
		let request_body = request.encode_to_vec();
		let response_raw = self
			.client
			.post(url)
			.header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
			.body(request_body)
			.send()
			.await
			.map_err(|e| {
				LdkServerError::new(InternalError, format!("HTTP request failed: {}", e))
			})?;

		let status = response_raw.status();
		let payload = response_raw.bytes().await.map_err(|e| {
			LdkServerError::new(InternalError, format!("Failed to read response body: {}", e))
		})?;

		if status.is_success() {
			Ok(Rs::decode(&payload[..]).map_err(|e| {
				LdkServerError::new(
					InternalError,
					format!("Failed to decode success response: {}", e),
				)
			})?)
		} else {
			let error_response = ErrorResponse::decode(&payload[..]).map_err(|e| {
				LdkServerError::new(
					InternalError,
					format!("Failed to decode error response (status {}): {}", status, e),
				)
			})?;

			let error_code = match ErrorCode::from_i32(error_response.error_code) {
				Some(ErrorCode::InvalidRequestError) => InvalidRequestError,
				Some(ErrorCode::AuthError) => AuthError,
				Some(ErrorCode::LightningError) => LightningError,
				Some(ErrorCode::InternalServerError) => InternalServerError,
				Some(ErrorCode::UnknownError) | None => InternalError,
			};

			Err(LdkServerError::new(error_code, error_response.message))
		}
	}
}
