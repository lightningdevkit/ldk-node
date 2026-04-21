// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::sync::{Mutex as StdMutex, Weak};

use bitcoin::Transaction;
use lightning::chain::chaininterface::{BroadcasterInterface, TransactionType};
use tokio::sync::{mpsc, Mutex, MutexGuard};

use crate::logger::{log_error, LdkLogger};
use crate::types::Wallet;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: LdkLogger,
{
	queue_sender: mpsc::Sender<Vec<Transaction>>,
	queue_receiver: Mutex<mpsc::Receiver<Vec<Transaction>>>,
	/// Weak handle to the [`Wallet`] that performs classification of funding broadcasts
	/// (channel opens and splices) into payment records. Remains `None` while the
	/// builder is wiring the node up, during which broadcasts are still forwarded to
	/// the queue but no payment record is written. [`Self::set_wallet`] installs the
	/// handle once the [`Wallet`] exists.
	wallet: StdMutex<Option<Weak<Wallet>>>,
	logger: L,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(logger: L) -> Self {
		let (queue_sender, queue_receiver) = mpsc::channel(BCAST_PACKAGE_QUEUE_SIZE);
		Self {
			queue_sender,
			queue_receiver: Mutex::new(queue_receiver),
			wallet: StdMutex::new(None),
			logger,
		}
	}

	/// Installs the [`Wallet`] handle used to classify funding broadcasts (channel
	/// opens and splices) into payment records. Called once the builder has constructed
	/// both the broadcaster and the wallet.
	pub(crate) fn set_wallet(&self, wallet: Weak<Wallet>) {
		*self.wallet.lock().expect("lock") = Some(wallet);
	}

	pub(crate) async fn get_broadcast_queue(
		&self,
	) -> MutexGuard<'_, mpsc::Receiver<Vec<Transaction>>> {
		self.queue_receiver.lock().await
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, TransactionType)]) {
		let wallet = self.wallet.lock().expect("lock").as_ref().and_then(Weak::upgrade);
		if let Some(wallet) = wallet {
			for (tx, tx_type) in txs {
				if let Err(e) = wallet.classify_broadcast(tx, tx_type) {
					log_error!(
						self.logger,
						"Failed to classify broadcast tx {}: {:?}",
						tx.compute_txid(),
						e,
					);
				}
			}
		}
		let package = txs.iter().map(|(t, _)| (*t).clone()).collect::<Vec<Transaction>>();
		self.queue_sender.try_send(package).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}
