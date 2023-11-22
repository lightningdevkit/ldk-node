use crate::logger::{log_error, log_trace, Logger};

use lightning::chain::chaininterface::BroadcasterInterface;

use esplora_client::AsyncClient as EsploraClient;

use bitcoin::Transaction;

use tokio::sync::mpsc;
use tokio::sync::Mutex;

use std::ops::Deref;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: Logger,
{
	queue_sender: mpsc::Sender<Vec<Transaction>>,
	queue_receiver: Mutex<mpsc::Receiver<Vec<Transaction>>>,
	esplora_client: EsploraClient,
	logger: L,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(esplora_client: EsploraClient, logger: L) -> Self {
		let (queue_sender, queue_receiver) = mpsc::channel(BCAST_PACKAGE_QUEUE_SIZE);
		Self { queue_sender, queue_receiver: Mutex::new(queue_receiver), esplora_client, logger }
	}

	pub(crate) async fn process_queue(&self) {
		let mut receiver = self.queue_receiver.lock().await;
		while let Some(next_package) = receiver.recv().await {
			for tx in &next_package {
				match self.esplora_client.broadcast(tx).await {
					Ok(()) => {
						log_trace!(self.logger, "Successfully broadcast transaction {}", tx.txid());
					}
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to broadcast transaction {}: {}",
							tx.txid(),
							e
						);
					}
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
