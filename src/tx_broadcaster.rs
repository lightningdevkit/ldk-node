// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeSet, VecDeque};
use std::ops::Deref;
use std::sync::{Mutex as StdMutex, Weak};

use bitcoin::{Transaction, Txid};
use lightning::chain::chaininterface::{
	BroadcasterInterface, TransactionType as LdkTransactionType,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::logger::{log_debug, log_error, LdkLogger};
use crate::types::Wallet;
use crate::Error;

/// The most packages [`BroadcastQueue`] holds, fresh and awaiting a retry together. Claims and
/// sweeps re-enter the queue on LDK's periodic rebroadcast timers, so one dropped at the bound
/// resurfaces on its own once the store recovers. Packages nothing re-broadcasts — fundings and
/// cooperative closes — are never dropped or refused for the bound, though they count toward it:
/// what LDK hands over of them is finite — one per negotiated funding candidate and one per
/// closing channel — and a copy of a package awaiting a retry is never queued twice.
const MAX_QUEUED_PACKAGES: usize = 256;

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

	/// The txids of the packaged transactions, identifying the package's effect on chain: two
	/// packages with the same txids broadcast the same transactions.
	pub(crate) fn txids(&self) -> BTreeSet<Txid> {
		self.0.iter().map(|(tx, _)| tx.compute_txid()).collect()
	}

	/// Whether the package may be dropped to keep [`BroadcastQueue`] within its bound: every
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
			// Wallet-originated: the wallet re-submits its unconfirmed transactions on each chain
			// tip change. Classification of an untyped package is a no-op that can't fail, so one
			// never awaits a retry.
			None => true,
		})
	}
}

/// What [`BroadcastQueue`] did with a package, so the caller can log the cases in which the
/// package won't be classified and broadcast as-is.
pub(crate) enum QueueOutcome {
	/// The package is queued. When the bound was reached, the oldest droppable package was
	/// dropped to make room and is returned — its transactions resurface with LDK's next
	/// periodic rebroadcast.
	Queued { dropped: Option<BroadcastPackage> },
	/// A package broadcasting the same transactions already awaits a classification retry, and
	/// that retry covers this one: the incoming package is dropped and returned.
	AlreadyQueued(BroadcastPackage),
	/// The bound was reached and every queued package is one that must not be dropped (a
	/// funding or a cooperative close): the incoming package is refused and returned.
	Refused(BroadcastPackage),
}

/// The packages handed to the broadcaster, waiting for the background task to classify and
/// broadcast them: fresh packages in arrival order, and packages whose classification failed,
/// each waiting out a retry delay. One queue holds both, so one bound and one rule for what may
/// be dropped at it cover fresh packages and retries alike, and a re-broadcast of a package
/// awaiting a retry is recognized as it is queued rather than after one more failed attempt.
///
/// Deduplicated and bounded: LDK re-broadcasts pending claims every 30 seconds (and sweeps once
/// per block) until they confirm, so while the store is unavailable, copies would otherwise
/// accumulate without bound and replay as a burst on recovery. An identical copy is never queued
/// while one awaits a retry — the waiting entry and its deadline stand; fee-bumped rebroadcast
/// variants carry new txids, so the bound — not the dedup — is what limits their accumulation.
///
/// The queue belongs to the broadcaster and outlives the task draining it: what is queued when
/// the node stops, fresh or awaiting a retry, is classified and broadcast after the next start.
pub(crate) struct BroadcastQueue {
	state: StdMutex<QueueState>,
	/// Wakes the draining task when a package is queued.
	notify: Notify,
}

struct QueueState {
	/// Packages not yet attempted, in arrival order.
	fresh: VecDeque<BroadcastPackage>,
	/// Packages whose classification failed, with their txids and retry deadlines. Retries are
	/// scheduled with a fixed delay, so the front entry is always the next to fall due.
	retries: VecDeque<(Instant, BTreeSet<Txid>, BroadcastPackage)>,
}

impl BroadcastQueue {
	pub(crate) fn new() -> Self {
		let state = QueueState { fresh: VecDeque::new(), retries: VecDeque::new() };
		Self { state: StdMutex::new(state), notify: Notify::new() }
	}

	/// Queues a fresh package, unless a package with the same transactions already awaits a
	/// retry or accepting it would exceed [`MAX_QUEUED_PACKAGES`] with no droppable package to
	/// make room with; see [`QueueOutcome`].
	pub(crate) fn push(&self, package: BroadcastPackage) -> QueueOutcome {
		self.admit(package, None)
	}

	/// Queues a package whose classification failed, to be attempted again at `retry_at`, under
	/// the same conditions as [`Self::push`].
	pub(crate) fn retry(&self, package: BroadcastPackage, retry_at: Instant) -> QueueOutcome {
		self.admit(package, Some(retry_at))
	}

	fn admit(&self, package: BroadcastPackage, retry_at: Option<Instant>) -> QueueOutcome {
		let outcome = self.state.lock().expect("lock").admit(package, retry_at);
		if matches!(outcome, QueueOutcome::Queued { .. }) {
			self.notify.notify_one();
		}
		outcome
	}

	/// The next package to classify and broadcast: a fresh package if any is queued, otherwise
	/// the retry whose deadline has passed, waiting for one or the other when neither is ready.
	/// Fresh packages go first so broadcasts arriving during a store outage are never held back
	/// by the outage's retries. A retry keeps its delay regardless: without it, an otherwise idle
	/// queue would retry a fast-failing store back to back, logging an error each time.
	///
	/// Safe to drop before completion: a package leaves the queue only as the future completes.
	pub(crate) async fn next(&self) -> BroadcastPackage {
		loop {
			let next_deadline = {
				let mut state = self.state.lock().expect("lock");
				if let Some(package) = state.fresh.pop_front() {
					return package;
				}
				match state.retries.front() {
					Some((deadline, _, _)) if *deadline <= Instant::now() => {
						let (_, _, package) = state.retries.pop_front().expect("front entry");
						return package;
					},
					Some((deadline, _, _)) => Some(*deadline),
					None => None,
				}
			};
			// A package queued between the check above and the wait below is not missed: with
			// no task waiting, `notify_one` stores a permit that completes the next `notified`.
			match next_deadline {
				Some(deadline) => {
					tokio::select! {
						_ = self.notify.notified() => {},
						_ = tokio::time::sleep_until(deadline) => {},
					}
				},
				None => self.notify.notified().await,
			}
		}
	}
}

impl QueueState {
	fn admit(&mut self, package: BroadcastPackage, retry_at: Option<Instant>) -> QueueOutcome {
		let txids = package.txids();
		if self.retries.iter().any(|(_, waiting, _)| *waiting == txids) {
			// Same transactions, same classification outcome: keep the waiting entry and its
			// earlier deadline. The one same-txid package with a *different* type is LDK's
			// re-typed generic-funding rebroadcast of a promoted 0conf splice, which always
			// arrives after the interactive-funding original (the zero-conf rebroadcast canary
			// tests assert that ordering), so the entry kept is the richer of the two — and its
			// classification declines the downgrade anyway.
			return QueueOutcome::AlreadyQueued(package);
		}

		let mut dropped = None;
		if package.is_droppable() && self.fresh.len() + self.retries.len() >= MAX_QUEUED_PACKAGES {
			// Drop the oldest droppable package, a waiting retry before a fresh package: its
			// transactions are re-broadcast periodically, while the incoming package may carry
			// a fresher fee-bumped variant. A funding package is never dropped — nothing would
			// re-broadcast it, and losing it leaves its transaction confirming without a
			// recorded candidate. Neither is a cooperative close, whose queued package may hold
			// the only copy of the signed closing transaction.
			dropped = self.drop_oldest_droppable();
			if dropped.is_none() {
				return QueueOutcome::Refused(package);
			}
		}
		match retry_at {
			Some(retry_at) => self.retries.push_back((retry_at, txids, package)),
			None => self.fresh.push_back(package),
		}
		QueueOutcome::Queued { dropped }
	}

	fn drop_oldest_droppable(&mut self) -> Option<BroadcastPackage> {
		if let Some(oldest) = self.retries.iter().position(|(_, _, waiting)| waiting.is_droppable())
		{
			return self.retries.remove(oldest).map(|(_, _, package)| package);
		}
		let oldest = self.fresh.iter().position(|waiting| waiting.is_droppable())?;
		self.fresh.remove(oldest)
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
	/// Weak handle to the [`Wallet`] that classifies funding broadcasts (channel opens and
	/// splices) into payment records. Remains `None` while the builder is wiring the node up,
	/// during which broadcasts are queued but no payment record is written.
	/// [`Self::set_wallet`] installs the handle once the [`Wallet`] exists.
	wallet: StdMutex<Option<Weak<Wallet>>>,
	logger: L,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(logger: L) -> Self {
		Self { queue: BroadcastQueue::new(), wallet: StdMutex::new(None), logger }
	}

	/// Installs the [`Wallet`] handle used to classify funding broadcasts (channel opens and
	/// splices) into payment records. Called once the builder has constructed both the
	/// broadcaster and the wallet.
	pub(crate) fn set_wallet(&self, wallet: Weak<Wallet>) {
		*self.wallet.lock().expect("lock") = Some(wallet);
	}

	/// The next queued package to classify and broadcast, waiting for one when none is ready;
	/// see [`BroadcastQueue::next`].
	pub(crate) async fn next_package(&self) -> BroadcastPackage {
		self.queue.next().await
	}

	/// Queues a package whose classification failed, to be attempted again at `retry_at`.
	pub(crate) fn retry_package(&self, package: BroadcastPackage, retry_at: Instant) {
		self.log_dropped(self.queue.retry(package, retry_at));
	}

	fn queue_package(&self, package: BroadcastPackage) {
		self.log_dropped(self.queue.push(package));
	}

	fn log_dropped(&self, outcome: QueueOutcome) {
		match outcome {
			QueueOutcome::Queued { dropped: None } => {},
			QueueOutcome::Queued { dropped: Some(dropped) } => {
				log_error!(
					self.logger,
					"Dropped the oldest queued package to make room; its transactions are re-broadcast periodically: {:?}",
					dropped.txids(),
				);
			},
			QueueOutcome::AlreadyQueued(duplicate) => {
				log_debug!(
					self.logger,
					"Dropped a re-broadcast package; an identical one already awaits a classification retry: {:?}",
					duplicate.txids(),
				);
			},
			QueueOutcome::Refused(package) => {
				log_error!(
					self.logger,
					"Dropped a package; too many packages await classification and broadcast: {:?}",
					package.txids(),
				);
			},
		}
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
		self.queue_package(BroadcastPackage::unclassified(tx));
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[(&Transaction, LdkTransactionType)]) {
		self.queue_package(BroadcastPackage::new(txs));
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use bitcoin::hashes::Hash;
	use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

	use super::{
		BroadcastPackage, BroadcastQueue, LdkTransactionType, QueueOutcome, SortedTransactions,
		MAX_QUEUED_PACKAGES,
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

	/// A due retry: `next` hands out a retry once its deadline has passed.
	fn due() -> tokio::time::Instant {
		tokio::time::Instant::now()
	}

	/// Everything `next` hands out before the queue goes quiet, in order.
	async fn drain(queue: &BroadcastQueue) -> Vec<Txid> {
		let mut txids = Vec::new();
		while let Ok(package) =
			tokio::time::timeout(std::time::Duration::from_millis(200), queue.next()).await
		{
			txids.extend(package.txids());
		}
		txids
	}

	/// While a package awaits a retry, another with the same transactions is not queued, whether
	/// it arrives as a retry or fresh: the waiting entry keeps its deadline and its package.
	#[tokio::test]
	async fn identical_transactions_are_queued_once_while_a_retry_waits() {
		let tx = parent_tx(1);
		let queue = BroadcastQueue::new();

		assert!(matches!(
			queue.retry(funding_package(&tx), due()),
			QueueOutcome::Queued { dropped: None }
		));
		assert!(matches!(
			queue.retry(BroadcastPackage::unclassified(tx.clone()), deadline(4)),
			QueueOutcome::AlreadyQueued(_)
		));
		assert!(matches!(
			queue.push(BroadcastPackage::unclassified(tx.clone())),
			QueueOutcome::AlreadyQueued(_)
		));

		// The kept entry is due now; the duplicate's later deadline must not have replaced it.
		let kept = tokio::time::timeout(std::time::Duration::from_secs(1), queue.next())
			.await
			.expect("the waiting entry keeps its earlier deadline");
		assert!(
			matches!(kept.transactions()[0].1, Some(LdkTransactionType::Funding { .. })),
			"the first-scheduled package must be kept"
		);
		assert!(drain(&queue).await.is_empty());
	}

	/// Fresh packages are not deduplicated against each other: two arrivals of the same
	/// transactions before either is attempted are both classified, as with the channel before.
	#[tokio::test]
	async fn fresh_packages_are_not_deduplicated_against_each_other() {
		let tx = parent_tx(1);
		let queue = BroadcastQueue::new();

		assert!(matches!(queue.push(funding_package(&tx)), QueueOutcome::Queued { dropped: None }));
		assert!(matches!(
			queue.push(BroadcastPackage::unclassified(tx.clone())),
			QueueOutcome::Queued { dropped: None }
		));
		assert_eq!(drain(&queue).await, vec![tx.compute_txid(), tx.compute_txid()]);
	}

	/// Fresh packages go before due retries, each group in arrival order.
	#[tokio::test]
	async fn fresh_packages_come_before_due_retries() {
		let (tx_a, tx_b, tx_c, tx_d) = (parent_tx(1), parent_tx(2), parent_tx(3), parent_tx(4));
		let queue = BroadcastQueue::new();

		assert!(matches!(
			queue.retry(BroadcastPackage::unclassified(tx_a.clone()), due()),
			QueueOutcome::Queued { dropped: None }
		));
		assert!(matches!(
			queue.retry(BroadcastPackage::unclassified(tx_b.clone()), due()),
			QueueOutcome::Queued { dropped: None }
		));
		assert!(matches!(
			queue.push(BroadcastPackage::unclassified(tx_c.clone())),
			QueueOutcome::Queued { dropped: None }
		));
		assert!(matches!(
			queue.push(BroadcastPackage::unclassified(tx_d.clone())),
			QueueOutcome::Queued { dropped: None }
		));

		assert_eq!(
			drain(&queue).await,
			vec![
				tx_c.compute_txid(),
				tx_d.compute_txid(),
				tx_a.compute_txid(),
				tx_b.compute_txid()
			]
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

		let (pushed, next) = tokio::join!(
			async {
				tokio::time::sleep(std::time::Duration::from_millis(50)).await;
				queue.push(BroadcastPackage::unclassified(tx.clone()))
			},
			tokio::time::timeout(std::time::Duration::from_secs(5), queue.next()),
		);
		assert!(matches!(pushed, QueueOutcome::Queued { dropped: None }));
		assert_eq!(next.expect("woken by the push").txids(), BTreeSet::from([tx.compute_txid()]));
	}

	/// A fresh package pushed while `next` waits out a retry's delay is handed out at once; the
	/// retry keeps waiting.
	#[tokio::test]
	async fn next_wakes_on_a_push_while_a_retry_waits() {
		let (retry_tx, fresh_tx) = (parent_tx(1), parent_tx(2));
		let queue = BroadcastQueue::new();

		assert!(matches!(
			queue.retry(BroadcastPackage::unclassified(retry_tx.clone()), deadline(5)),
			QueueOutcome::Queued { dropped: None }
		));
		let (pushed, next) = tokio::join!(
			async {
				tokio::time::sleep(std::time::Duration::from_millis(50)).await;
				queue.push(BroadcastPackage::unclassified(fresh_tx.clone()))
			},
			tokio::time::timeout(std::time::Duration::from_secs(2), queue.next()),
		);
		assert!(matches!(pushed, QueueOutcome::Queued { dropped: None }));
		assert_eq!(
			next.expect("woken by the push before the retry falls due").txids(),
			BTreeSet::from([fresh_tx.compute_txid()])
		);
		assert!(drain(&queue).await.is_empty(), "the retry was handed out before its deadline");
	}

	/// `next` holds a retry back until its deadline, then hands it out on its own.
	#[tokio::test]
	async fn next_waits_for_a_retry_deadline() {
		let tx = parent_tx(1);
		let queue = BroadcastQueue::new();

		let retry_at = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
		assert!(matches!(
			queue.retry(BroadcastPackage::unclassified(tx.clone()), retry_at),
			QueueOutcome::Queued { dropped: None }
		));
		assert!(tokio::time::timeout(std::time::Duration::from_millis(500), queue.next())
			.await
			.is_err());

		let next = tokio::time::timeout(std::time::Duration::from_secs(5), queue.next()).await;
		assert!(tokio::time::Instant::now() >= retry_at, "the retry was handed out early");
		assert_eq!(next.expect("due retry").txids(), BTreeSet::from([tx.compute_txid()]));
	}

	/// Distinct transactions (e.g. fee-bumped claim variants during a store outage) are held to
	/// the bound across fresh and waiting packages: the oldest droppable package is dropped for
	/// an incoming one, a waiting retry before a fresh package and never a funding package.
	#[tokio::test]
	async fn bound_drops_the_oldest_droppable_retry_before_a_fresh_package() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([7u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let queue = BroadcastQueue::new();
		let funding_tx = numbered_tx(0);
		assert!(matches!(
			queue.retry(funding_package(&funding_tx), due()),
			QueueOutcome::Queued { dropped: None }
		));
		let oldest_claim = numbered_tx(1);
		let retried = MAX_QUEUED_PACKAGES as u32 / 2;
		for n in 1..retried {
			assert!(matches!(
				queue.retry(BroadcastPackage::unclassified(numbered_tx(n)), due()),
				QueueOutcome::Queued { dropped: None }
			));
		}
		let oldest_fresh = numbered_tx(retried);
		for n in retried..(MAX_QUEUED_PACKAGES as u32) {
			assert!(matches!(
				queue.push(BroadcastPackage::unclassified(numbered_tx(n))),
				QueueOutcome::Queued { dropped: None }
			));
		}

		// At the bound, an incoming droppable package drops the oldest waiting retry — not the
		// older funding package, and not a fresh package.
		let new_claim = numbered_tx(MAX_QUEUED_PACKAGES as u32);
		match queue.push(BroadcastPackage::unclassified(new_claim.clone())) {
			QueueOutcome::Queued { dropped: Some(dropped) } => {
				assert_eq!(dropped.txids(), BTreeSet::from([oldest_claim.compute_txid()]));
			},
			_ => panic!("the incoming claim must be queued by dropping the oldest one"),
		}

		// An incoming funding package is never dropped for the bound.
		let new_funding_tx = numbered_tx(MAX_QUEUED_PACKAGES as u32 + 1);
		assert!(matches!(
			queue.push(funding_package(&new_funding_tx)),
			QueueOutcome::Queued { dropped: None }
		));

		let remaining = drain(&queue).await;
		assert_eq!(remaining.len(), MAX_QUEUED_PACKAGES + 1);
		assert!(remaining.contains(&funding_tx.compute_txid()), "funding is never dropped");
		assert!(remaining.contains(&oldest_fresh.compute_txid()), "a retry is dropped first");
		assert!(remaining.contains(&new_claim.compute_txid()));
		assert!(remaining.contains(&new_funding_tx.compute_txid()));
		assert!(!remaining.contains(&oldest_claim.compute_txid()));
	}

	/// With no retry waiting, the bound falls on the fresh packages: the oldest droppable one
	/// is dropped for an incoming one.
	#[tokio::test]
	async fn bound_drops_the_oldest_droppable_fresh_package() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([11u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let queue = BroadcastQueue::new();
		let oldest = numbered_tx(0);
		for n in 0..(MAX_QUEUED_PACKAGES as u32) {
			assert!(matches!(
				queue.push(claim_package(&numbered_tx(n))),
				QueueOutcome::Queued { dropped: None }
			));
		}

		let new_claim = numbered_tx(MAX_QUEUED_PACKAGES as u32);
		match queue.push(claim_package(&new_claim)) {
			QueueOutcome::Queued { dropped: Some(dropped) } => {
				assert_eq!(dropped.txids(), BTreeSet::from([oldest.compute_txid()]));
			},
			_ => panic!("the incoming claim must be queued by dropping the oldest one"),
		}

		let remaining = drain(&queue).await;
		assert_eq!(remaining.len(), MAX_QUEUED_PACKAGES);
		assert!(!remaining.contains(&oldest.compute_txid()));
		assert_eq!(remaining.last(), Some(&new_claim.compute_txid()));
	}

	/// When only funding packages are queued at the bound, an incoming droppable package is
	/// refused, fresh or retried: LDK re-broadcasts claims and sweeps periodically, while a
	/// dropped funding package would leave its transaction confirming without a recorded
	/// candidate.
	#[tokio::test]
	async fn bound_refuses_a_droppable_package_over_queued_funding_packages() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([8u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let queue = BroadcastQueue::new();
		for n in 0..(MAX_QUEUED_PACKAGES as u32) {
			assert!(matches!(
				queue.retry(funding_package(&numbered_tx(n)), deadline(60)),
				QueueOutcome::Queued { dropped: None }
			));
		}

		let claim = numbered_tx(MAX_QUEUED_PACKAGES as u32);
		assert!(matches!(
			queue.push(BroadcastPackage::unclassified(claim.clone())),
			QueueOutcome::Refused(_)
		));
		assert!(matches!(
			queue.retry(BroadcastPackage::unclassified(claim), deadline(60)),
			QueueOutcome::Refused(_)
		));
	}

	/// A cooperative close is never dropped at the bound: nothing re-broadcasts it, and the
	/// queued package may hold the only copy of the signed closing transaction.
	#[tokio::test]
	async fn bound_never_drops_a_cooperative_close() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([9u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let queue = BroadcastQueue::new();
		let coop_close_tx = numbered_tx(0);
		assert!(matches!(
			queue.retry(coop_close_package(&coop_close_tx), due()),
			QueueOutcome::Queued { dropped: None }
		));
		let oldest_claim = numbered_tx(1);
		for n in 1..(MAX_QUEUED_PACKAGES as u32) {
			assert!(matches!(
				queue.retry(claim_package(&numbered_tx(n)), due()),
				QueueOutcome::Queued { dropped: None }
			));
		}

		// At the bound, an incoming claim drops the oldest waiting claim — not the older
		// cooperative close.
		let new_claim = numbered_tx(MAX_QUEUED_PACKAGES as u32);
		match queue.retry(claim_package(&new_claim), due()) {
			QueueOutcome::Queued { dropped: Some(dropped) } => {
				assert_eq!(dropped.txids(), BTreeSet::from([oldest_claim.compute_txid()]));
			},
			_ => panic!("the incoming claim must be queued by dropping the oldest one"),
		}

		// An incoming cooperative close is never dropped for the bound either.
		let new_coop_close_tx = numbered_tx(MAX_QUEUED_PACKAGES as u32 + 1);
		assert!(matches!(
			queue.push(coop_close_package(&new_coop_close_tx)),
			QueueOutcome::Queued { dropped: None }
		));

		let remaining = drain(&queue).await;
		assert!(
			remaining.contains(&coop_close_tx.compute_txid()),
			"a cooperative close is never dropped"
		);
		assert!(remaining.contains(&new_coop_close_tx.compute_txid()));
		assert!(!remaining.contains(&oldest_claim.compute_txid()));
	}

	/// When only cooperative closes are queued at the bound, an incoming claim is refused: LDK
	/// re-broadcasts the claim periodically, while a dropped close would lose the only copy of
	/// its signed closing transaction.
	#[tokio::test]
	async fn bound_refuses_a_claim_over_queued_cooperative_closes() {
		fn numbered_tx(n: u32) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version::TWO,
				lock_time: bitcoin::absolute::LockTime::ZERO,
				input: vec![txin(Txid::from_byte_array([10u8; 32]), n)],
				output: vec![txout(1_000)],
			}
		}

		let queue = BroadcastQueue::new();
		for n in 0..(MAX_QUEUED_PACKAGES as u32) {
			assert!(matches!(
				queue.push(coop_close_package(&numbered_tx(n))),
				QueueOutcome::Queued { dropped: None }
			));
		}

		let claim = numbered_tx(MAX_QUEUED_PACKAGES as u32);
		assert!(matches!(
			queue.retry(claim_package(&claim), deadline(60)),
			QueueOutcome::Refused(_)
		));
	}
}
