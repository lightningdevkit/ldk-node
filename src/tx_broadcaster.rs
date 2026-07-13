// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::sync::{Mutex as StdMutex, Weak};

use bitcoin::Transaction;
use lightning::chain::chaininterface::{
	BroadcasterInterface, TransactionType as LdkTransactionType,
};
use tokio::sync::{mpsc, Mutex, MutexGuard};

use crate::logger::{log_error, LdkLogger};
use crate::types::Wallet;
use crate::Error;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

/// A package of transactions that LDK handed to the broadcaster in one `broadcast_transactions`
/// call, along with each transaction's type. Queued until the background task classifies and
/// broadcasts it. Built only via [`BroadcastPackage::new`] from such a call, so unrelated
/// transactions can't be grouped into one package by accident.
pub(crate) struct BroadcastPackage(Vec<(Transaction, Option<LdkTransactionType>)>);

impl BroadcastPackage {
	/// Builds a package from the transactions of a single `broadcast_transactions` call.
	fn new(txs: &[(&Transaction, LdkTransactionType)]) -> Self {
		Self(txs.iter().map(|(tx, tx_type)| ((*tx).clone(), Some(tx_type.clone()))).collect())
	}

	/// Builds a package for wallet-originated broadcasts that have no LDK classification.
	fn unclassified(tx: Transaction) -> Self {
		Self(vec![(tx, None)])
	}

	/// The packaged transactions and their types, for classification.
	fn transactions(&self) -> &[(Transaction, Option<LdkTransactionType>)] {
		&self.0
	}

	/// Consumes the package into its transactions, ready for the chain client.
	pub(crate) fn into_transactions(self) -> Vec<Transaction> {
		self.0.into_iter().map(|(tx, _)| tx).collect()
	}
}

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: LdkLogger,
{
	queue_sender: mpsc::Sender<BroadcastPackage>,
	queue_receiver: Mutex<mpsc::Receiver<BroadcastPackage>>,
	/// Weak handle to the [`Wallet`] that classifies funding broadcasts (channel opens and
	/// splices) into payment records. Remains `None` while the builder is wiring the node up,
	/// during which broadcasts are forwarded to the queue but no payment record is written.
	/// [`Self::set_wallet`] installs the handle once the [`Wallet`] exists.
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

	/// Installs the [`Wallet`] handle used to classify funding broadcasts (channel opens and
	/// splices) into payment records. Called once the builder has constructed both the
	/// broadcaster and the wallet.
	pub(crate) fn set_wallet(&self, wallet: Weak<Wallet>) {
		*self.wallet.lock().expect("lock") = Some(wallet);
	}

	pub(crate) async fn get_broadcast_queue(
		&self,
	) -> MutexGuard<'_, mpsc::Receiver<BroadcastPackage>> {
		self.queue_receiver.lock().await
	}

	/// Classifies a queued package into payment records and returns the package ready for the
	/// chain client. Returns `Err` if any classification fails; callers must not broadcast the
	/// package in that case, since a crash would leave the transaction on-chain without a record.
	pub(crate) async fn classify_package(
		&self, package: BroadcastPackage,
	) -> Result<BroadcastPackage, Error> {
		let wallet_opt = self.wallet.lock().expect("lock").as_ref().and_then(Weak::upgrade);
		if let Some(wallet) = wallet_opt {
			for (tx, tx_type) in package.transactions() {
				if let Some(tx_type) = tx_type {
					wallet.classify_broadcast(tx, tx_type).await?;
				}
			}
		}
		Ok(package)
	}

	pub(crate) fn broadcast_unclassified_transaction(&self, tx: Transaction) {
		self.queue_sender.try_send(BroadcastPackage::unclassified(tx)).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, LdkTransactionType)]) {
		self.queue_sender.try_send(BroadcastPackage::new(txs)).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}
