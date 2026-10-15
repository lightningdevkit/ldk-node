// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::Mutex as StdMutex;

use bitcoin::{Transaction, Txid};
use lightning::chain::chaininterface::{
	BroadcasterInterface, TransactionType as LdkTransactionType,
};
use tokio::sync::Notify;

use crate::logger::{log_trace, LdkLogger};

/// A package of transactions to broadcast together: everything LDK handed over in one
/// `broadcast_transactions` call, or a single transaction the wallet broadcasts itself. Queued
/// until the background task sends it. Built only from one such source, so unrelated transactions
/// can't be grouped into one package by accident.
pub(crate) struct BroadcastPackage(Vec<Transaction>);

impl BroadcastPackage {
	/// The txids of the packaged transactions, identifying the package's effect on chain.
	fn txids(&self) -> Vec<Txid> {
		self.0.iter().map(Transaction::compute_txid).collect()
	}

	/// Consumes the package into its transactions, ready for the chain client.
	pub(crate) fn into_sorted_transactions(self) -> SortedTransactions {
		SortedTransactions::sort_parents_child_package_topologically(self.0)
	}
}

/// The packages handed to the broadcaster, waiting in arrival order for the background task to
/// send them.
///
/// The queue belongs to the broadcaster and outlives the task draining it: what is queued when the
/// node stops is broadcast after the next start.
pub(crate) struct BroadcastQueue {
	packages: StdMutex<VecDeque<BroadcastPackage>>,
	/// Wakes the draining task when a package is queued.
	notify: Notify,
}

impl BroadcastQueue {
	pub(crate) fn new() -> Self {
		Self { packages: StdMutex::new(VecDeque::new()), notify: Notify::new() }
	}

	/// Queues a package to broadcast.
	pub(crate) fn push(&self, package: BroadcastPackage) {
		self.packages.lock().expect("lock").push_back(package);
		self.notify.notify_one();
	}

	/// The next package to broadcast, waiting for one while the queue is empty.
	///
	/// Safe to drop before completion: a package leaves the queue only as the future completes.
	pub(crate) async fn next(&self) -> BroadcastPackage {
		loop {
			if let Some(package) = self.packages.lock().expect("lock").pop_front() {
				return package;
			}
			// A package queued between the check above and the wait below is not missed: with
			// no task waiting, `notify_one` stores a permit that completes the next `notified`.
			self.notify.notified().await;
		}
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

	pub(crate) fn into_inner(self) -> Vec<Transaction> {
		self.0
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
	queue: BroadcastQueue,
	logger: L,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(logger: L) -> Self {
		Self { queue: BroadcastQueue::new(), logger }
	}

	/// The next queued package to broadcast, waiting for one when none is queued.
	pub(crate) async fn next_package(&self) -> BroadcastPackage {
		self.queue.next().await
	}

	/// Queues a transaction the wallet broadcasts on its own behalf.
	pub(crate) fn broadcast(&self, tx: Transaction) {
		self.queue_package(BroadcastPackage(vec![tx]));
	}

	fn queue_package(&self, package: BroadcastPackage) {
		log_trace!(self.logger, "Queuing package for broadcast: {:?}", package.txids());
		self.queue.push(package);
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, LdkTransactionType)]) {
		self.queue_package(BroadcastPackage(txs.iter().map(|(tx, _)| (*tx).clone()).collect()));
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

	use super::{BroadcastPackage, BroadcastQueue, SortedTransactions};

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

	/// Everything `next` hands out before the queue goes quiet, in order.
	async fn drain(queue: &BroadcastQueue) -> Vec<Txid> {
		let mut txids = Vec::new();
		while let Ok(package) =
			tokio::time::timeout(std::time::Duration::from_millis(200), queue.next()).await
		{
			txids.extend(package.into_sorted_transactions().iter().map(Transaction::compute_txid));
		}
		txids
	}

	/// Every queued package is handed out, in arrival order, however often the same transaction
	/// arrives.
	#[tokio::test]
	async fn packages_are_handed_out_in_arrival_order() {
		let (tx_a, tx_b) = (parent_tx(1), parent_tx(2));
		let queue = BroadcastQueue::new();

		queue.push(BroadcastPackage(vec![tx_a.clone()]));
		queue.push(BroadcastPackage(vec![tx_b.clone()]));
		queue.push(BroadcastPackage(vec![tx_a.clone()]));

		assert_eq!(
			drain(&queue).await,
			vec![tx_a.compute_txid(), tx_b.compute_txid(), tx_a.compute_txid()]
		);
	}

	/// `next` waits for a package when none is queued and wakes when one is pushed.
	#[tokio::test]
	async fn next_wakes_on_a_push() {
		let tx = parent_tx(1);
		let queue = BroadcastQueue::new();

		assert!(tokio::time::timeout(std::time::Duration::from_millis(100), queue.next())
			.await
			.is_err());

		let (_, next) = tokio::join!(
			async {
				tokio::time::sleep(std::time::Duration::from_millis(50)).await;
				queue.push(BroadcastPackage(vec![tx.clone()]));
			},
			tokio::time::timeout(std::time::Duration::from_secs(5), queue.next()),
		);
		let handed_out = next.expect("woken by the push").into_sorted_transactions();
		assert_eq!(
			handed_out.iter().map(Transaction::compute_txid).collect::<Vec<_>>(),
			vec![tx.compute_txid()],
		);
	}
}
