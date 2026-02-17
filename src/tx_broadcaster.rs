// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::ops::Deref;

use bitcoin::{Transaction, Txid};
use lightning::chain::chaininterface::{
	BroadcasterInterface, TransactionType as LdkTransactionType,
};
use tokio::sync::{mpsc, Mutex, MutexGuard};

use crate::logger::{log_error, LdkLogger};
use crate::payment::store::TransactionType;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: LdkLogger,
{
	queue_sender: mpsc::Sender<Vec<Transaction>>,
	queue_receiver: Mutex<mpsc::Receiver<Vec<Transaction>>>,
	tx_types: std::sync::Mutex<HashMap<Txid, TransactionType>>,
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
			tx_types: std::sync::Mutex::new(HashMap::new()),
			logger,
		}
	}

	pub(crate) async fn get_broadcast_queue(
		&self,
	) -> MutexGuard<'_, mpsc::Receiver<Vec<Transaction>>> {
		self.queue_receiver.lock().await
	}

	pub(crate) fn broadcast_with_type(&self, tx: Transaction, tx_type: TransactionType) {
		let txid = tx.compute_txid();
		self.tx_types.lock().unwrap().insert(txid, tx_type);

		let package = vec![tx];
		self.queue_sender.try_send(package).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}

	pub(crate) fn remove_tx_type(&self, txid: &Txid) -> Option<TransactionType> {
		self.tx_types.lock().unwrap().remove(txid)
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, LdkTransactionType)]) {
		for (tx, ldk_type) in txs {
			let txid = tx.compute_txid();
			self.tx_types.lock().unwrap().insert(txid, TransactionType::from(ldk_type.clone()));
		}

		let package = txs.iter().map(|(t, _)| (*t).clone()).collect::<Vec<Transaction>>();
		self.queue_sender.try_send(package).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}
