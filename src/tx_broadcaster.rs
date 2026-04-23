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
use crate::Error;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

/// A package of transactions that LDK handed to the broadcaster in one
/// `broadcast_transactions` call, along with each transaction's type. Queued until the
/// background task classifies and broadcasts it.
pub(crate) type BroadcastPackage = Vec<(Transaction, TransactionType)>;

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: LdkLogger,
{
	queue_sender: mpsc::Sender<BroadcastPackage>,
	queue_receiver: Mutex<mpsc::Receiver<BroadcastPackage>>,
	/// Weak handle to the [`Wallet`] that performs classification of funding broadcasts
	/// (channel opens and splices) into payment records. Remains `None` while the
	/// builder is wiring the node up, during which broadcasts are forwarded to the
	/// queue but no payment record is written. [`Self::set_wallet`] installs the handle
	/// once the [`Wallet`] exists.
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
	) -> MutexGuard<'_, mpsc::Receiver<BroadcastPackage>> {
		self.queue_receiver.lock().await
	}

	/// Classifies a queued package into payment records and returns the raw
	/// transactions ready for the chain client. Returns `Err` if any classification
	/// fails; callers must not broadcast the package in that case, since a crash would
	/// leave the tx on-chain without a record.
	pub(crate) async fn classify_package(
		&self, package: BroadcastPackage,
	) -> Result<Vec<Transaction>, Error> {
		let wallet_opt = self.wallet.lock().expect("lock").as_ref().and_then(Weak::upgrade);
		if let Some(wallet) = wallet_opt {
			let package = tokio::task::spawn_blocking(move || {
				for (tx, tx_type) in &package {
					wallet.classify_broadcast(tx, tx_type)?;
				}
				Ok::<_, Error>(package)
			})
			.await
			.map_err(|_| Error::PersistenceFailed)??;
			Ok(package.into_iter().map(|(tx, _)| tx).collect())
		} else {
			Ok(package.into_iter().map(|(tx, _)| tx).collect())
		}
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, TransactionType)]) {
		let package: BroadcastPackage =
			txs.iter().map(|(tx, tx_type)| ((*tx).clone(), tx_type.clone())).collect();
		self.queue_sender.try_send(package).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}
