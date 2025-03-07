// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::TX_BROADCAST_TIMEOUT_SECS;
use crate::error::Error;
use crate::logger::{log_bytes, log_error, log_trace, LdkLogger, Logger};

use lightning::chain::{Filter, WatchedOutput};
use lightning::util::ser::Writeable;
use lightning_transaction_sync::ElectrumSyncClient;

use bdk_electrum::BdkElectrumClient;

use electrum_client::Client as ElectrumClient;
use electrum_client::ConfigBuilder as ElectrumConfigBuilder;
use electrum_client::ElectrumApi;

use bitcoin::{Script, Transaction, Txid};

use std::sync::Arc;
use std::time::Duration;

const ELECTRUM_CLIENT_NUM_RETRIES: u8 = 3;
const ELECTRUM_CLIENT_TIMEOUT_SECS: u8 = 20;

pub(crate) struct ElectrumRuntimeClient {
	electrum_client: Arc<ElectrumClient>,
	bdk_electrum_client: Arc<BdkElectrumClient<ElectrumClient>>,
	tx_sync: Arc<ElectrumSyncClient<Arc<Logger>>>,
	runtime: Arc<tokio::runtime::Runtime>,
	logger: Arc<Logger>,
}

impl ElectrumRuntimeClient {
	pub(crate) fn new(
		server_url: String, runtime: Arc<tokio::runtime::Runtime>, logger: Arc<Logger>,
	) -> Result<Self, Error> {
		let electrum_config = ElectrumConfigBuilder::new()
			.retry(ELECTRUM_CLIENT_NUM_RETRIES)
			.timeout(Some(ELECTRUM_CLIENT_TIMEOUT_SECS))
			.build();

		let electrum_client = Arc::new(
			ElectrumClient::from_config(&server_url, electrum_config.clone()).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?,
		);
		let electrum_client_2 =
			ElectrumClient::from_config(&server_url, electrum_config).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?;
		let bdk_electrum_client = Arc::new(BdkElectrumClient::new(electrum_client_2));
		let tx_sync = Arc::new(
			ElectrumSyncClient::new(server_url.clone(), Arc::clone(&logger)).map_err(|e| {
				log_error!(logger, "Failed to connect to electrum server: {}", e);
				Error::ConnectionFailed
			})?,
		);
		Ok(Self { electrum_client, bdk_electrum_client, tx_sync, runtime, logger })
	}

	pub(crate) async fn broadcast(&self, tx: Transaction) {
		let electrum_client = Arc::clone(&self.electrum_client);

		let txid = tx.compute_txid();
		let tx_bytes = tx.encode();

		let spawn_fut =
			self.runtime.spawn_blocking(move || electrum_client.transaction_broadcast(&tx));

		let timeout_fut =
			tokio::time::timeout(Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS), spawn_fut);

		match timeout_fut.await {
			Ok(res) => match res {
				Ok(_) => {
					log_trace!(self.logger, "Successfully broadcast transaction {}", txid);
				},
				Err(e) => {
					log_error!(self.logger, "Failed to broadcast transaction {}: {}", txid, e);
					log_trace!(
						self.logger,
						"Failed broadcast transaction bytes: {}",
						log_bytes!(tx_bytes)
					);
				},
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to broadcast transaction due to timeout {}: {}",
					txid,
					e
				);
				log_trace!(
					self.logger,
					"Failed broadcast transaction bytes: {}",
					log_bytes!(tx_bytes)
				);
			},
		}
	}
}

impl Filter for ElectrumRuntimeClient {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.tx_sync.register_tx(txid, script_pubkey)
	}
	fn register_output(&self, output: WatchedOutput) {
		self.tx_sync.register_output(output)
	}
}
