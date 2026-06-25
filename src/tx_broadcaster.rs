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

/// A package of transactions that LDK handed to the broadcaster in one `broadcast_transactions`
/// call, along with each transaction's type. Queued until the background task classifies and
/// broadcasts it. Built only via [`BroadcastPackage::new`] from such a call, so unrelated
/// transactions can't be grouped into one package by accident.
pub(crate) struct BroadcastPackage(Vec<(Transaction, TransactionType)>);

impl BroadcastPackage {
	/// Builds a package from the transactions of a single `broadcast_transactions` call.
	fn new(txs: &[(&Transaction, TransactionType)]) -> Self {
		Self(txs.iter().map(|(tx, tx_type)| ((*tx).clone(), tx_type.clone())).collect())
	}

	/// The packaged transactions and their types, for classification.
	fn transactions(&self) -> &[(Transaction, TransactionType)] {
		&self.0
	}

	/// Consumes the package into its transactions, ready for the chain client.
	pub(crate) fn into_sorted_transactions(self) -> SortedTransactions {
		let txs = self.0.into_iter().map(|(tx, _)| tx).collect();
		SortedTransactions::sort_parents_child_package_topologically(txs)
	}
}

pub(crate) struct SortedTransactions(Vec<Transaction>);

impl SortedTransactions {
	pub(crate) fn sort_parents_child_package_topologically(
		mut txs: Vec<Transaction>,
	) -> SortedTransactions {
		if txs.len() == 0 || txs.len() == 1 {
			return SortedTransactions(txs);
		}
		let txids: Vec<_> = txs.iter().map(|tx| tx.compute_txid()).collect();
		let any_spends_from_package = |tx: &Transaction| -> bool {
			tx.input.iter().any(|input| txids.contains(&input.previous_output.txid))
		};
		txs.sort_by_key(any_spends_from_package);

		#[cfg(debug_assertions)]
		{
			let child = txs.last().expect("txs is not empty");
			let child_input_txids: Vec<_> =
				child.input.iter().map(|input| input.previous_output.txid).collect();
			let parents = &txs[..txs.len() - 1];
			let parent_txids: Vec<_> = parents.iter().map(|parent| parent.compute_txid()).collect();
			// Make sure all the parent txids are parents of the child transaction
			debug_assert!(parent_txids.iter().all(|txid| child_input_txids.contains(&txid)));
			// Make sure there are no grandparents
			debug_assert_eq!(txs.iter().filter(|tx| any_spends_from_package(tx)).count(), 1);
		}

		SortedTransactions(txs)
	}

	pub(crate) fn try_into_single_tx(mut self) -> Result<Transaction, ()> {
		if self.0.len() == 1 {
			Ok(self.0.pop().expect("The length is 1"))
		} else {
			Err(())
		}
	}
}

impl Deref for SortedTransactions {
	type Target = Vec<Transaction>;
	fn deref(&self) -> &Self::Target {
		&self.0
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
				wallet.classify_broadcast(tx, tx_type).await?;
			}
		}
		Ok(package)
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, TransactionType)]) {
		self.queue_sender.try_send(BroadcastPackage::new(txs)).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

	use super::SortedTransactions;

	fn txin(txid: Txid, vout: u32) -> TxIn {
		TxIn {
			previous_output: OutPoint { txid, vout },
			script_sig: ScriptBuf::new(),
			sequence: Sequence::MAX,
			witness: Witness::new(),
		}
	}

	fn txout(value_sat: u64) -> TxOut {
		TxOut { value: Amount::from_sat(value_sat), script_pubkey: ScriptBuf::new() }
	}

	fn parent_tx(seed: u8) -> Transaction {
		Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: bitcoin::absolute::LockTime::ZERO,
			input: vec![txin(Txid::from_byte_array([seed; 32]), 0)],
			output: vec![txout(1_000 + u64::from(seed))],
		}
	}

	fn child_tx(parents: &[&Transaction]) -> Transaction {
		Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: bitcoin::absolute::LockTime::ZERO,
			input: parents
				.iter()
				.enumerate()
				.map(|(idx, parent)| txin(parent.compute_txid(), idx as u32))
				.collect(),
			output: vec![txout(1_000)],
		}
	}

	fn assert_parents_before_child(
		txs: &[Transaction], expected_child: Txid, expected_parents: &[Txid],
	) {
		assert_eq!(txs.last().map(Transaction::compute_txid), Some(expected_child));
		assert_eq!(txs.len(), expected_parents.len() + 1);

		let parent_txids =
			txs[..txs.len() - 1].iter().map(Transaction::compute_txid).collect::<Vec<_>>();
		for expected_parent in expected_parents {
			assert!(parent_txids.contains(expected_parent));
		}
	}

	#[test]
	fn topological_sort_leaves_sorted_package_unchanged() {
		let parent_a = parent_tx(1);
		let parent_b = parent_tx(2);
		let child = child_tx(&[&parent_a, &parent_b]);

		let original_txids =
			[parent_a.compute_txid(), parent_b.compute_txid(), child.compute_txid()];
		let txs = vec![parent_a, parent_b, child];

		let package = SortedTransactions::sort_parents_child_package_topologically(txs);

		assert_eq!(
			package.iter().map(Transaction::compute_txid).collect::<Vec<_>>(),
			original_txids
		);
	}

	#[test]
	fn topological_sort_moves_single_parent_child_from_front_to_end() {
		let parent = parent_tx(1);
		let child = child_tx(&[&parent]);
		let parent_txids = [parent.compute_txid()];
		let child_txid = child.compute_txid();
		let txs = vec![child, parent];

		let package = SortedTransactions::sort_parents_child_package_topologically(txs);

		assert_parents_before_child(&package, child_txid, &parent_txids);
	}

	#[test]
	fn topological_sort_moves_child_from_front_to_end() {
		let parent_a = parent_tx(1);
		let parent_b = parent_tx(2);
		let child = child_tx(&[&parent_a, &parent_b]);
		let parent_txids = [parent_a.compute_txid(), parent_b.compute_txid()];
		let child_txid = child.compute_txid();
		let txs = vec![child, parent_a, parent_b];

		let package = SortedTransactions::sort_parents_child_package_topologically(txs);

		assert_parents_before_child(&package, child_txid, &parent_txids);
	}

	#[test]
	fn topological_sort_moves_child_from_front_with_multiple_parents_to_end() {
		let parent_a = parent_tx(1);
		let parent_b = parent_tx(2);
		let parent_c = parent_tx(3);
		let child = child_tx(&[&parent_a, &parent_b, &parent_c]);
		let parent_txids =
			[parent_a.compute_txid(), parent_b.compute_txid(), parent_c.compute_txid()];
		let child_txid = child.compute_txid();
		let txs = vec![child, parent_a, parent_b, parent_c];

		let package = SortedTransactions::sort_parents_child_package_topologically(txs);

		assert_parents_before_child(&package, child_txid, &parent_txids);
	}

	#[test]
	fn topological_sort_moves_child_from_middle_to_end() {
		let parent_a = parent_tx(1);
		let parent_b = parent_tx(2);
		let child = child_tx(&[&parent_a, &parent_b]);
		let parent_txids = [parent_a.compute_txid(), parent_b.compute_txid()];
		let child_txid = child.compute_txid();
		let txs = vec![parent_a, child, parent_b];

		let package = SortedTransactions::sort_parents_child_package_topologically(txs);

		assert_parents_before_child(&package, child_txid, &parent_txids);
	}

	#[test]
	fn topological_sort_leaves_single_transaction_package_unchanged() {
		let parent = parent_tx(1);
		let parent_txid = parent.compute_txid();
		let txs = vec![parent];

		let package = SortedTransactions::sort_parents_child_package_topologically(txs);

		assert_eq!(package.len(), 1);
		assert_eq!(package[0].compute_txid(), parent_txid);
	}

	#[test]
	fn topological_sort_accepts_empty_vec() {
		SortedTransactions::sort_parents_child_package_topologically(Vec::new());
	}
}
