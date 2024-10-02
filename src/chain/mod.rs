// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::logger::FilesystemLogger;

use esplora_client::AsyncClient as EsploraAsyncClient;

use std::sync::Arc;

// The default Esplora server we're using.
pub(crate) const DEFAULT_ESPLORA_SERVER_URL: &str = "https://blockstream.info/api";

// The default Esplora client timeout we're using.
pub(crate) const DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS: u64 = 10;

pub(crate) enum ChainSource {
	Esplora { esplora_client: EsploraAsyncClient, logger: Arc<FilesystemLogger> },
}

impl ChainSource {
	pub(crate) fn new_esplora(server_url: String, logger: Arc<FilesystemLogger>) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url.clone());
		client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);
		let esplora_client = client_builder.build_async().unwrap();
		Self::Esplora { esplora_client, logger }
	}
}
