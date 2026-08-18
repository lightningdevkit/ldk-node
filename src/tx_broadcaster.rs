// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::{Mutex as StdMutex, Weak};

use bitcoin::{Transaction, Txid};
use lightning::chain::chaininterface::{
	BroadcasterInterface, TransactionType as LdkTransactionType,
};
use tokio::sync::{mpsc, Mutex, MutexGuard};
use tokio::time::Instant;

use crate::logger::{log_error, LdkLogger};
use crate::types::Wallet;
use crate::Error;

const BCAST_PACKAGE_QUEUE_SIZE: usize = 256;

/// The most droppable packages [`RetryQueue`] holds. Claims and sweeps re-enter the broadcast
/// queue on LDK's periodic rebroadcast timers, so one dropped here resurfaces on its own once
/// the store recovers. Packages nothing re-broadcasts — fundings and cooperative closes —
/// don't count against the bound: they are finite — one per negotiated funding candidate and
/// one per closing channel, since a copy of a waiting package is never queued twice.
const MAX_QUEUED_RETRIES: usize = BCAST_PACKAGE_QUEUE_SIZE;

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
	pub(crate) fn into_sorted_transactions(self) -> SortedTransactions {
		let txs = self.0.into_iter().map(|(tx, _)| tx).collect();
		SortedTransactions::sort_parents_child_package_topologically(txs)
	}

	/// The packaged transactions' txids in sorted order, identifying the package's effect on
	/// chain: two packages with the same txids broadcast the same transactions.
	pub(crate) fn sorted_txids(&self) -> Vec<Txid> {
		let mut txids: Vec<Txid> = self.0.iter().map(|(tx, _)| tx.compute_txid()).collect();
		txids.sort_unstable();
		txids
	}

	/// Whether the package may be dropped to keep [`RetryQueue`] within its bound: every
	/// transaction in it is re-broadcast by its originator, so a dropped package resurfaces on
	/// its own. LDK re-hands claims, anchor bumps, and force-close commitments to the
	/// broadcaster periodically, and the sweeper regenerates sweeps once per block. Nothing
	/// re-broadcasts a funding transaction (a channel open or splice, whose classification
	/// writes the payment record tracking the funding) or a cooperative close (whose channel is
	/// gone from the `ChannelManager` by broadcast time), so a package containing either is
	/// never dropped.
	fn is_droppable(&self) -> bool {
		self.0.iter().all(|(_, tx_type)| match tx_type {
			Some(
				LdkTransactionType::Funding { .. }
				| LdkTransactionType::InteractiveFunding { .. }
				| LdkTransactionType::CooperativeClose { .. },
			) => false,
			Some(
				LdkTransactionType::UnilateralClose { .. }
				| LdkTransactionType::AnchorBump { .. }
				| LdkTransactionType::Claim { .. }
				| LdkTransactionType::Sweep { .. },
			) => true,
			// Wallet-originated: re-submitted on chain tip changes. Never queued anyway, since
			// classification of an untyped package is a no-op that can't fail.
			None => true,
		})
	}
}

/// What [`RetryQueue::schedule`] did with a package, so the caller can log the cases in which
/// the package won't be retried as-is.
pub(crate) enum ScheduleOutcome {
	/// The package waits for its retry deadline. When the bound was reached, the oldest waiting
	/// droppable package was dropped to make room and is returned — its transactions resurface
	/// with LDK's next periodic rebroadcast.
	Scheduled { dropped: Option<BroadcastPackage> },
	/// A package broadcasting the same transactions already waits, and its retry covers this
	/// one: the incoming package is dropped and returned.
	AlreadyQueued(BroadcastPackage),
	/// The bound was reached and every waiting package is one that must not be dropped (a
	/// funding or a cooperative close): the incoming package is refused and returned.
	Refused(BroadcastPackage),
}

/// Packages whose classification failed, each waiting out a retry delay before its next attempt.
/// Deduplicated and bounded: LDK re-broadcasts pending claims every 30 seconds (and sweeps once
/// per block) until they confirm, so while the store is unavailable, copies would otherwise
/// accumulate without bound and replay as a burst on recovery. An identical copy is never queued
/// twice — the waiting entry and its deadline stand; fee-bumped rebroadcast variants carry new
/// txids, so the bound — not the dedup — is what limits their accumulation.
pub(crate) struct RetryQueue(VecDeque<(Instant, Vec<Txid>, BroadcastPackage)>);

impl RetryQueue {
	pub(crate) fn new() -> Self {
		Self(VecDeque::new())
	}

	/// The deadline of the next retry, if a package is waiting. Packages are scheduled with a fixed
	/// delay, so the front entry is always the next to retry.
	pub(crate) fn next_retry_at(&self) -> Option<Instant> {
		self.0.front().map(|(deadline, _, _)| *deadline)
	}

	/// Removes and returns the package scheduled to retry first.
	pub(crate) fn pop_next(&mut self) -> Option<BroadcastPackage> {
		self.0.pop_front().map(|(_, _, package)| package)
	}

	/// Schedules a package to retry at `retry_at`, unless a package with the same transactions already
	/// waits or accepting it would exceed [`MAX_QUEUED_RETRIES`] with no droppable package to
	/// make room with; see [`ScheduleOutcome`].
	pub(crate) fn schedule(
		&mut self, package: BroadcastPackage, retry_at: Instant,
	) -> ScheduleOutcome {
		let txids = package.sorted_txids();
		if self.0.iter().any(|(_, waiting, _)| *waiting == txids) {
			// Same transactions, same classification outcome: keep the waiting entry and its
			// earlier deadline. The one same-txid package with a *different* type is LDK's
			// re-typed generic-funding rebroadcast of a promoted 0conf splice, which always
			// arrives after the interactive-funding original (the zero-conf rebroadcast canary
			// tests assert that ordering), so the entry kept is the richer of the two — and its
			// classification declines the downgrade anyway.
			return ScheduleOutcome::AlreadyQueued(package);
		}

		let mut dropped = None;
		if package.is_droppable() && self.0.len() >= MAX_QUEUED_RETRIES {
			// Drop the oldest droppable package: its transactions are re-broadcast
			// periodically, while the incoming package may carry a fresher fee-bumped variant.
			// A funding package is never dropped — nothing would re-broadcast it, and losing it
			// leaves its transaction confirming without a recorded candidate. Neither is a
			// cooperative close, whose queued package may hold the only copy of the signed
			// closing transaction.
			match self.0.iter().position(|(_, _, waiting)| waiting.is_droppable()) {
				Some(oldest) => dropped = self.0.remove(oldest).map(|(_, _, package)| package),
				None => return ScheduleOutcome::Refused(package),
			}
		}
		self.0.push_back((retry_at, txids, package));
		ScheduleOutcome::Scheduled { dropped }
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

	/// Classifies a queued package into payment records. Returns `Err` if any classification
	/// fails; callers must not broadcast the package in that case, since a crash would leave the
	/// transaction on-chain without a record — but must retry it later rather than drop it.
	pub(crate) async fn classify_package(&self, package: &BroadcastPackage) -> Result<(), Error> {
		let wallet_opt = self.wallet.lock().expect("lock").as_ref().and_then(Weak::upgrade);
		if let Some(wallet) = wallet_opt {
			for (tx, tx_type) in package.transactions() {
				if let Some(tx_type) = tx_type {
					wallet.classify_broadcast(tx, tx_type).await?;
				}
			}
		}
		Ok(())
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

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

	use super::{
		BroadcastPackage, LdkTransactionType, RetryQueue, ScheduleOutcome, SortedTransactions,
		MAX_QUEUED_RETRIES,
	};

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

	fn funding_package(tx: &Transaction) -> BroadcastPackage {
		BroadcastPackage::new(&[(tx, LdkTransactionType::Funding { channels: vec![] })])
	}

	fn test_counterparty_node_id() -> bitcoin::secp256k1::PublicKey {
		use std::str::FromStr;
		bitcoin::secp256k1::PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap()
	}

	fn coop_close_package(tx: &Transaction) -> BroadcastPackage {
		BroadcastPackage::new(&[(
			tx,
			LdkTransactionType::CooperativeClose {
				counterparty_node_id: test_counterparty_node_id(),
				channel_id: lightning::ln::types::ChannelId([13u8; 32]),
			},
		)])
	}

	fn claim_package(tx: &Transaction) -> BroadcastPackage {
		BroadcastPackage::new(&[(
			tx,
			LdkTransactionType::Claim {
				counterparty_node_id: test_counterparty_node_id(),
				channel_id: lightning::ln::types::ChannelId([13u8; 32]),
			},
		)])
	}

	fn deadline(secs: u64) -> tokio::time::Instant {
		tokio::time::Instant::now() + std::time::Duration::from_secs(secs)
	}

	/// A re-broadcast of the same transactions is not queued again: the waiting entry keeps its
	/// earlier deadline and its package — the first arrival carries the richer classification
	/// when LDK later re-types a rebroadcast.
	#[tokio::test]
	async fn retry_queue_queues_identical_transactions_once() {
		let tx = parent_tx(1);
		let mut retries = RetryQueue::new();

		let first_deadline = deadline(2);
		assert!(matches!(
			retries.schedule(funding_package(&tx), first_deadline),
			ScheduleOutcome::Scheduled { dropped: None }
		));
		assert!(matches!(
			retries.schedule(BroadcastPackage::unclassified(tx.clone()), deadline(4)),
			ScheduleOutcome::AlreadyQueued(_)
		));

		assert_eq!(retries.next_retry_at(), Some(first_deadline));
		let kept = retries.pop_next().expect("the first package is kept");
		assert!(
			matches!(kept.transactions()[0].1, Some(LdkTransactionType::Funding { .. })),
			"the first-scheduled package must be kept"
		);
		assert!(retries.pop_next().is_none());
	}

	#[tokio::test]
	async fn retry_queue_retries_in_schedule_order() {
		let (tx_a, tx_b) = (parent_tx(1), parent_tx(2));
		let mut retries = RetryQueue::new();

		assert!(matches!(
			retries.schedule(BroadcastPackage::unclassified(tx_a.clone()), deadline(2)),
			ScheduleOutcome::Scheduled { dropped: None }
		));
		assert!(matches!(
			retries.schedule(BroadcastPackage::unclassified(tx_b.clone()), deadline(2)),
			ScheduleOutcome::Scheduled { dropped: None }
		));

		let popped = retries.pop_next().expect("first package");
		assert_eq!(popped.sorted_txids(), vec![tx_a.compute_txid()]);
		let popped = retries.pop_next().expect("second package");
		assert_eq!(popped.sorted_txids(), vec![tx_b.compute_txid()]);
	}

	/// Distinct transactions (e.g. fee-bumped claim variants during a store outage) are held to
	/// the bound: the oldest droppable package is dropped for an incoming one, never a funding
	/// package.
	#[tokio::test]
	async fn retry_queue_drops_the_oldest_droppable_package_at_the_bound() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([7u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let mut retries = RetryQueue::new();
		let funding_tx = numbered_tx(0);
		assert!(matches!(
			retries.schedule(funding_package(&funding_tx), deadline(2)),
			ScheduleOutcome::Scheduled { dropped: None }
		));
		let oldest_claim = numbered_tx(1);
		for n in 1..(MAX_QUEUED_RETRIES as u32) {
			assert!(matches!(
				retries.schedule(BroadcastPackage::unclassified(numbered_tx(n)), deadline(2)),
				ScheduleOutcome::Scheduled { dropped: None }
			));
		}

		// At the bound, an incoming droppable package drops the oldest waiting one — not the
		// older funding package.
		let new_claim = numbered_tx(MAX_QUEUED_RETRIES as u32);
		match retries.schedule(BroadcastPackage::unclassified(new_claim.clone()), deadline(2)) {
			ScheduleOutcome::Scheduled { dropped: Some(dropped) } => {
				assert_eq!(dropped.sorted_txids(), vec![oldest_claim.compute_txid()]);
			},
			_ => panic!("the incoming claim must be scheduled by dropping the oldest one"),
		}

		// An incoming funding package is never dropped for the bound.
		let new_funding_tx = numbered_tx(MAX_QUEUED_RETRIES as u32 + 1);
		assert!(matches!(
			retries.schedule(funding_package(&new_funding_tx), deadline(2)),
			ScheduleOutcome::Scheduled { dropped: None }
		));

		let mut remaining = Vec::new();
		while let Some(package) = retries.pop_next() {
			remaining.extend(package.sorted_txids());
		}
		assert!(remaining.contains(&funding_tx.compute_txid()), "funding is never dropped");
		assert!(remaining.contains(&new_claim.compute_txid()));
		assert!(!remaining.contains(&oldest_claim.compute_txid()));
	}

	/// When only funding packages wait at the bound, an incoming droppable package is refused:
	/// LDK re-broadcasts claims and sweeps periodically, while a dropped funding package would
	/// leave its transaction confirming without a recorded candidate.
	#[tokio::test]
	async fn retry_queue_refuses_a_droppable_package_over_waiting_funding_packages() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([8u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let mut retries = RetryQueue::new();
		for n in 0..(MAX_QUEUED_RETRIES as u32) {
			assert!(matches!(
				retries.schedule(funding_package(&numbered_tx(n)), deadline(2)),
				ScheduleOutcome::Scheduled { dropped: None }
			));
		}

		let claim = numbered_tx(MAX_QUEUED_RETRIES as u32);
		assert!(matches!(
			retries.schedule(BroadcastPackage::unclassified(claim), deadline(2)),
			ScheduleOutcome::Refused(_)
		));
	}

	/// A cooperative close is never dropped at the bound: nothing re-broadcasts it, and the
	/// queued package may hold the only copy of the signed closing transaction.
	#[tokio::test]
	async fn retry_queue_never_drops_a_cooperative_close_at_the_bound() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([9u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let mut retries = RetryQueue::new();
		let coop_close_tx = numbered_tx(0);
		assert!(matches!(
			retries.schedule(coop_close_package(&coop_close_tx), deadline(2)),
			ScheduleOutcome::Scheduled { dropped: None }
		));
		let oldest_claim = numbered_tx(1);
		for n in 1..(MAX_QUEUED_RETRIES as u32) {
			assert!(matches!(
				retries.schedule(claim_package(&numbered_tx(n)), deadline(2)),
				ScheduleOutcome::Scheduled { dropped: None }
			));
		}

		// At the bound, an incoming claim drops the oldest waiting claim — not the older
		// cooperative close.
		let new_claim = numbered_tx(MAX_QUEUED_RETRIES as u32);
		match retries.schedule(claim_package(&new_claim), deadline(2)) {
			ScheduleOutcome::Scheduled { dropped: Some(dropped) } => {
				assert_eq!(dropped.sorted_txids(), vec![oldest_claim.compute_txid()]);
			},
			_ => panic!("the incoming claim must be scheduled by dropping the oldest one"),
		}

		// An incoming cooperative close is never dropped for the bound either.
		let new_coop_close_tx = numbered_tx(MAX_QUEUED_RETRIES as u32 + 1);
		assert!(matches!(
			retries.schedule(coop_close_package(&new_coop_close_tx), deadline(2)),
			ScheduleOutcome::Scheduled { dropped: None }
		));

		let mut remaining = Vec::new();
		while let Some(package) = retries.pop_next() {
			remaining.extend(package.sorted_txids());
		}
		assert!(
			remaining.contains(&coop_close_tx.compute_txid()),
			"a cooperative close is never dropped"
		);
		assert!(remaining.contains(&new_coop_close_tx.compute_txid()));
		assert!(!remaining.contains(&oldest_claim.compute_txid()));
	}

	/// When only cooperative closes wait at the bound, an incoming claim is refused: LDK
	/// re-broadcasts the claim periodically, while a dropped close would lose the only copy of
	/// its signed closing transaction.
	#[tokio::test]
	async fn retry_queue_refuses_a_claim_over_waiting_cooperative_closes() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([10u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let mut retries = RetryQueue::new();
		for n in 0..(MAX_QUEUED_RETRIES as u32) {
			assert!(matches!(
				retries.schedule(coop_close_package(&numbered_tx(n)), deadline(2)),
				ScheduleOutcome::Scheduled { dropped: None }
			));
		}

		let claim = numbered_tx(MAX_QUEUED_RETRIES as u32);
		assert!(matches!(
			retries.schedule(claim_package(&claim), deadline(2)),
			ScheduleOutcome::Refused(_)
		));
	}
}
