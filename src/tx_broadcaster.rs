use crate::logger::{log_bytes, log_debug, log_error, log_trace, Logger};
use crate::payjoin_receiver::payjoin_receiver_request_headers;
use crate::payjoin_scheduler::PayjoinScheduler;

use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::log_info;
use lightning::util::ser::Writeable;

use esplora_client::AsyncClient as EsploraClient;

use bitcoin::Transaction;

use tokio::sync::mpsc;
use tokio::sync::Mutex;

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: Logger,
{
	queue_sender: mpsc::Sender<Vec<Transaction>>,
	queue_receiver: Mutex<mpsc::Receiver<Vec<Transaction>>>,
	esplora_client: EsploraClient,
	logger: L,
	payjoin_scheduler: Arc<Mutex<PayjoinScheduler>>,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(
		esplora_client: EsploraClient, logger: L, payjoin_scheduler: Arc<Mutex<PayjoinScheduler>>,
	) -> Self {
		let (queue_sender, queue_receiver) = mpsc::channel(BCAST_PACKAGE_QUEUE_SIZE);
		Self {
			queue_sender,
			queue_receiver: Mutex::new(queue_receiver),
			esplora_client,
			logger,
			payjoin_scheduler,
		}
	}

	pub(crate) async fn process_queue(&self) {
		let mut receiver = self.queue_receiver.lock().await;
		while let Some(next_package) = receiver.recv().await {
			for tx in &next_package {
				if tx.input.iter().any(|input| input.witness.is_empty()) {
					log_info!(
						self.logger,
						"Skipping broadcast of transaction {} with empty witness, checking for payjoin.",
						tx.txid()
					);
					dbg!("Skipping broadcast of transaction {} with empty witness, checking for payjoin.", tx.txid());
					let is_payjoin_channel =
						self.payjoin_scheduler.lock().await.set_funding_tx_signed(tx.clone());
					if let Some((url, body)) = is_payjoin_channel {
						log_info!(
							self.logger,
							"Detected payjoin channel transaction. Sending payjoin sender request for transaction {}",
							tx.txid()
						);
						dbg!("Detected payjoin channel transaction. Sending payjoin sender request for transaction {}", tx.txid());

						let headers = payjoin_receiver_request_headers();
						let client = match reqwest::Client::builder().build() {
							Ok(client) => client,
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to create reqwest client for payjoin receiver request: {}",
									e
								);
								continue;
							},
						};
						match client.post(url).body(body).headers(headers).send().await {
							Ok(res) => {
								if res.status().is_success() {
									log_info!(
										self.logger,
										"Successfully sent payjoin receiver request for transaction {}",
										tx.txid()
									);
									dbg!("Successfully sent payjoin receiver request for transaction {}", tx.txid());
								} else {
									dbg!("Failed to send payjoin receiver request for transaction {}: {}", tx.txid(), res.status());
									log_error!(
										self.logger,
										"Failed to send payjoin receiver request for transaction {}: {}",
										tx.txid(),
										res.status()
									);
								}
							},
							Err(e) => {
								dbg!("Failed to send payjoin receiver request for transaction {}: {}", tx.txid(), &e);
								log_error!(
									self.logger,
									"Failed to send payjoin receiver request for transaction {}: {}",
									tx.txid(),
									e
								);
							},
						}
						continue;
					}
				}
				match self.esplora_client.broadcast(tx).await {
					Ok(()) => {
						log_trace!(self.logger, "Successfully broadcast transaction {}", tx.txid());
					},
					Err(e) => match e {
						esplora_client::Error::Reqwest(_) => {
							// Wait 500 ms and retry in case we get a `Reqwest` error (typically
							// 429)
							tokio::time::sleep(Duration::from_millis(500)).await;
							log_error!(
								self.logger,
								"Sync failed due to HTTP connection error, retrying: {}",
								e
							);
							match self.esplora_client.broadcast(tx).await {
								Ok(()) => {
									log_debug!(
										self.logger,
										"Successfully broadcast transaction {}",
										tx.txid()
									);
								},
								Err(e) => {
									log_error!(
										self.logger,
										"Failed to broadcast transaction {}: {}",
										tx.txid(),
										e
									);
									log_trace!(
										self.logger,
										"Failed broadcast transaction bytes: {}",
										log_bytes!(tx.encode())
									);
								},
							}
						},
						_ => {
							log_error!(
								self.logger,
								"Failed to broadcast transaction {}: {}",
								tx.txid(),
								e
							);
							log_trace!(
								self.logger,
								"Failed broadcast transaction bytes: {}",
								log_bytes!(tx.encode())
							);
						},
					},
				}
			}
		}
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: Logger,
{
	fn broadcast_transactions(&self, txs: &[&Transaction]) {
		let package = txs.iter().map(|&t| t.clone()).collect::<Vec<Transaction>>();
		self.queue_sender.try_send(package).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}
