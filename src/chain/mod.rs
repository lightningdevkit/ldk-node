// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::{
	BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS,
	LDK_WALLET_SYNC_TIMEOUT_SECS,
};
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::types::Wallet;
use crate::Error;

use lightning::chain::{Confirm, Filter};

use lightning_transaction_sync::EsploraSyncClient;

use bdk_esplora::EsploraAsyncExt;

use esplora_client::AsyncClient as EsploraAsyncClient;

use std::sync::{Arc, Mutex};
use std::time::Duration;

// The default Esplora server we're using.
pub(crate) const DEFAULT_ESPLORA_SERVER_URL: &str = "https://blockstream.info/api";

// The default Esplora client timeout we're using.
pub(crate) const DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS: u64 = 10;

pub(crate) enum WalletSyncStatus {
	Completed,
	InProgress { subscribers: tokio::sync::broadcast::Sender<Result<(), Error>> },
}

impl WalletSyncStatus {
	fn register_or_subscribe_pending_sync(
		&mut self,
	) -> Option<tokio::sync::broadcast::Receiver<Result<(), Error>>> {
		match self {
			WalletSyncStatus::Completed => {
				// We're first to register for a sync.
				let (tx, _) = tokio::sync::broadcast::channel(1);
				*self = WalletSyncStatus::InProgress { subscribers: tx };
				None
			},
			WalletSyncStatus::InProgress { subscribers } => {
				// A sync is in-progress, we subscribe.
				let rx = subscribers.subscribe();
				Some(rx)
			},
		}
	}

	fn propagate_result_to_subscribers(&mut self, res: Result<(), Error>) {
		// Send the notification to any other tasks that might be waiting on it by now.
		{
			match self {
				WalletSyncStatus::Completed => {
					// No sync in-progress, do nothing.
					return;
				},
				WalletSyncStatus::InProgress { subscribers } => {
					// A sync is in-progress, we notify subscribers.
					if subscribers.receiver_count() > 0 {
						match subscribers.send(res) {
							Ok(_) => (),
							Err(e) => {
								debug_assert!(
									false,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
							},
						}
					}
					*self = WalletSyncStatus::Completed;
				},
			}
		}
	}
}

pub(crate) enum ChainSource {
	Esplora {
		esplora_client: EsploraAsyncClient,
		onchain_wallet: Arc<Wallet>,
		onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
		tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
		lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
		logger: Arc<FilesystemLogger>,
	},
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, onchain_wallet: Arc<Wallet>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url.clone());
		client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);
		let esplora_client = client_builder.build_async().unwrap();
		let tx_sync =
			Arc::new(EsploraSyncClient::from_client(esplora_client.clone(), Arc::clone(&logger)));
		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self::Esplora {
			esplora_client,
			onchain_wallet,
			onchain_wallet_sync_status,
			tx_sync,
			lightning_wallet_sync_status,
			logger,
		}
	}

	pub(crate) async fn sync_onchain_wallet(&self) -> Result<(), Error> {
		match self {
			Self::Esplora {
				esplora_client,
				onchain_wallet,
				onchain_wallet_sync_status,
				logger,
				..
			} => {
				let receiver_res = {
					let mut status_lock = onchain_wallet_sync_status.lock().unwrap();
					status_lock.register_or_subscribe_pending_sync()
				};
				if let Some(mut sync_receiver) = receiver_res {
					log_info!(logger, "Sync in progress, skipping.");
					return sync_receiver.recv().await.map_err(|e| {
						debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
						log_error!(logger, "Failed to receive wallet sync result: {:?}", e);
						Error::WalletOperationFailed
					})?;
				}

				let res = {
					let full_scan_request = onchain_wallet.get_full_scan_request();

					let wallet_sync_timeout_fut = tokio::time::timeout(
						Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
						esplora_client.full_scan(
							full_scan_request,
							BDK_CLIENT_STOP_GAP,
							BDK_CLIENT_CONCURRENCY,
						),
					);

					match wallet_sync_timeout_fut.await {
						Ok(res) => match res {
							Ok(update) => onchain_wallet.apply_update(update),
							Err(e) => match *e {
								esplora_client::Error::Reqwest(he) => {
									log_error!(
										logger,
										"Sync failed due to HTTP connection error: {}",
										he
									);
									Err(Error::WalletOperationFailed)
								},
								_ => {
									log_error!(logger, "Sync failed due to Esplora error: {}", e);
									Err(Error::WalletOperationFailed)
								},
							},
						},
						Err(e) => {
							log_error!(logger, "On-chain wallet sync timed out: {}", e);
							Err(Error::WalletOperationTimeout)
						},
					}
				};

				onchain_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

				res
			},
		}
	}

	pub(crate) async fn sync_lightning_wallet(
		&self, confirmables: Vec<&(dyn Confirm + Send + Sync)>,
	) -> Result<(), Error> {
		match self {
			Self::Esplora { tx_sync, lightning_wallet_sync_status, logger, .. } => {
				let receiver_res = {
					let mut status_lock = lightning_wallet_sync_status.lock().unwrap();
					status_lock.register_or_subscribe_pending_sync()
				};
				if let Some(mut sync_receiver) = receiver_res {
					log_info!(logger, "Sync in progress, skipping.");
					return sync_receiver.recv().await.map_err(|e| {
						debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
						log_error!(logger, "Failed to receive wallet sync result: {:?}", e);
						Error::WalletOperationFailed
					})?;
				}
				let res = {
					let timeout_fut = tokio::time::timeout(
						Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS),
						tx_sync.sync(confirmables),
					);
					match timeout_fut.await {
						Ok(res) => res.map_err(|_| Error::TxSyncFailed),
						Err(e) => {
							log_error!(logger, "Lightning wallet sync timed out: {}", e);
							Err(Error::TxSyncTimeout)
						},
					}
				};

				lightning_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

				res
			},
		}
	}
}

impl Filter for ChainSource {
	fn register_tx(&self, txid: &bitcoin::Txid, script_pubkey: &bitcoin::Script) {
		match self {
			Self::Esplora { tx_sync, .. } => tx_sync.register_tx(txid, script_pubkey),
		}
	}
	fn register_output(&self, output: lightning::chain::WatchedOutput) {
		match self {
			Self::Esplora { tx_sync, .. } => tx_sync.register_output(output),
		}
	}
}
