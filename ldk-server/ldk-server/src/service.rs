use ldk_node::Node;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::Service;
use hyper::{Request, Response, StatusCode};

use prost::Message;

use crate::api::bolt11_receive::{handle_bolt11_receive_request, BOLT11_RECEIVE_PATH};
use crate::api::bolt11_send::{handle_bolt11_send_request, BOLT11_SEND_PATH};
use crate::api::bolt12_receive::{handle_bolt12_receive_request, BOLT12_RECEIVE_PATH};
use crate::api::bolt12_send::{handle_bolt12_send_request, BOLT12_SEND_PATH};
use crate::api::close_channel::{handle_close_channel_request, CLOSE_CHANNEL_PATH};
use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InvalidRequestError;
use crate::api::get_balances::{handle_get_balances_request, GET_BALANCES};
use crate::api::get_node_info::{handle_get_node_info_request, GET_NODE_INFO};
use crate::api::get_payment_details::{
	handle_get_payment_details_request, GET_PAYMENT_DETAILS_PATH,
};
use crate::api::list_channels::{handle_list_channels_request, LIST_CHANNELS_PATH};
use crate::api::list_forwarded_payments::{
	handle_list_forwarded_payments_request, LIST_FORWARDED_PAYMENTS_PATH,
};
use crate::api::list_payments::{handle_list_payments_request, LIST_PAYMENTS_PATH};
use crate::api::onchain_receive::{handle_onchain_receive_request, ONCHAIN_RECEIVE_PATH};
use crate::api::onchain_send::{handle_onchain_send_request, ONCHAIN_SEND_PATH};
use crate::api::open_channel::{handle_open_channel, OPEN_CHANNEL_PATH};
use crate::api::update_channel_config::{
	handle_update_channel_config_request, UPDATE_CHANNEL_CONFIG_PATH,
};
use crate::io::paginated_kv_store::PaginatedKVStore;
use crate::util::proto_adapter::to_error_response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Clone)]
pub struct NodeService {
	node: Arc<Node>,
	paginated_kv_store: Arc<dyn PaginatedKVStore + Send + Sync>,
}

impl NodeService {
	pub(crate) fn new(
		node: Arc<Node>, paginated_kv_store: Arc<dyn PaginatedKVStore + Send + Sync>,
	) -> Self {
		Self { node, paginated_kv_store }
	}
}

pub(crate) struct Context {
	pub(crate) node: Arc<Node>,
	pub(crate) paginated_kv_store: Arc<dyn PaginatedKVStore + Send + Sync>,
}

impl Service<Request<Incoming>> for NodeService {
	type Response = Response<Full<Bytes>>;
	type Error = hyper::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn call(&self, req: Request<Incoming>) -> Self::Future {
		let context = Context {
			node: Arc::clone(&self.node),
			paginated_kv_store: Arc::clone(&self.paginated_kv_store),
		};
		// Exclude '/' from path pattern matching.
		match &req.uri().path()[1..] {
			GET_NODE_INFO => Box::pin(handle_request(context, req, handle_get_node_info_request)),
			GET_BALANCES => Box::pin(handle_request(context, req, handle_get_balances_request)),
			ONCHAIN_RECEIVE_PATH => {
				Box::pin(handle_request(context, req, handle_onchain_receive_request))
			},
			ONCHAIN_SEND_PATH => {
				Box::pin(handle_request(context, req, handle_onchain_send_request))
			},
			BOLT11_RECEIVE_PATH => {
				Box::pin(handle_request(context, req, handle_bolt11_receive_request))
			},
			BOLT11_SEND_PATH => Box::pin(handle_request(context, req, handle_bolt11_send_request)),
			BOLT12_RECEIVE_PATH => {
				Box::pin(handle_request(context, req, handle_bolt12_receive_request))
			},
			BOLT12_SEND_PATH => Box::pin(handle_request(context, req, handle_bolt12_send_request)),
			OPEN_CHANNEL_PATH => Box::pin(handle_request(context, req, handle_open_channel)),
			CLOSE_CHANNEL_PATH => {
				Box::pin(handle_request(context, req, handle_close_channel_request))
			},
			LIST_CHANNELS_PATH => {
				Box::pin(handle_request(context, req, handle_list_channels_request))
			},
			UPDATE_CHANNEL_CONFIG_PATH => {
				Box::pin(handle_request(context, req, handle_update_channel_config_request))
			},
			GET_PAYMENT_DETAILS_PATH => {
				Box::pin(handle_request(context, req, handle_get_payment_details_request))
			},
			LIST_PAYMENTS_PATH => {
				Box::pin(handle_request(context, req, handle_list_payments_request))
			},
			LIST_FORWARDED_PAYMENTS_PATH => {
				Box::pin(handle_request(context, req, handle_list_forwarded_payments_request))
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
	context: Context, request: Request<Incoming>, handler: F,
) -> Result<<NodeService as Service<Request<Incoming>>>::Response, hyper::Error> {
	// TODO: we should bound the amount of data we read to avoid allocating too much memory.
	let bytes = request.into_body().collect().await?.to_bytes();
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
