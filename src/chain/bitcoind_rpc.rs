// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource};

use bitcoin::BlockHash;

use base64::prelude::{Engine, BASE64_STANDARD};

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
