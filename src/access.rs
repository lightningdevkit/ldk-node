use crate::error::LdkLiteError as Error;
#[allow(unused_imports)]
use crate::logger::{
	log_error, log_given_level, log_info, log_internal, log_trace, log_warn, FilesystemLogger,
	Logger,
};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator, FEERATE_FLOOR_SATS_PER_KW};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};

use bdk::blockchain::{Blockchain, EsploraBlockchain};
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{SignOptions, SyncOptions};

use bitcoin::{BlockHash, BlockHeader, Script, Transaction, Txid};

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub struct LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	// A BDK blockchain used for wallet sync.
	blockchain: EsploraBlockchain,
	// A BDK on-chain wallet.
	wallet: Mutex<bdk::Wallet<D>>,
	// Transactions that were registered via the `Filter` interface and have to be processed.
	queued_transactions: Mutex<HashSet<Txid>>,
	// Transactions that were previously processed, but must not be forgotten yet.
	watched_transactions: Mutex<HashSet<Txid>>,
	// Outputs that were registered via the `Filter` interface and have to be processed.
	queued_outputs: Mutex<HashSet<WatchedOutput>>,
	// Outputs that were previously processed, but must not be forgotten yet.
	watched_outputs: Mutex<HashSet<WatchedOutput>>,
	// The tip hash observed during our last sync.
	last_sync_hash: tokio::sync::Mutex<Option<BlockHash>>,
	logger: Arc<FilesystemLogger>,
}

impl<D> LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	pub(crate) fn new(
		blockchain: EsploraBlockchain, wallet: bdk::Wallet<D>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let wallet = Mutex::new(wallet);
		let watched_transactions = Mutex::new(HashSet::new());
		let queued_transactions = Mutex::new(HashSet::new());
		let watched_outputs = Mutex::new(HashSet::new());
		let queued_outputs = Mutex::new(HashSet::new());
		let last_sync_hash = tokio::sync::Mutex::new(None);
		Self {
			blockchain,
			wallet,
			queued_transactions,
			watched_transactions,
			queued_outputs,
			watched_outputs,
			last_sync_hash,
			logger,
		}
	}

	pub(crate) async fn sync_wallet(&self) -> Result<(), Error> {
		let sync_options = SyncOptions { progress: None };

		self.wallet
			.lock()
			.unwrap()
			.sync(&self.blockchain, sync_options)
			.map_err(|e| Error::Bdk(e))?
	}

	/// Syncs the LDK wallet via the `Confirm` interface. We run in a loop until we completed a
	/// full iteration without
	pub(crate) async fn sync(&self, confirmables: Vec<&(dyn Confirm + Sync)>) -> Result<(), Error> {
		// This lock makes sure we're syncing once at a time.
		let mut locked_last_sync_hash = self.last_sync_hash.lock().await;

		let client = &*self.blockchain;

		let mut tip_hash = client.get_tip_hash().await?;

		loop {
			let pending_registrations = self.process_queues();
			let new_tip = Some(tip_hash) != *locked_last_sync_hash;

			// We loop until any registered transactions have been processed at least once, or the
			// tip hasn't been updated during the last iteration.
			if pending_registrations || new_tip {
				// First check for any unconfirmed tranasactions and act on it immediately.
				self.sync_unconfirmed_transactions(&confirmables).await?;

				// Update the known tip to the newest one.
				if new_tip {
					*locked_last_sync_hash = Some(tip_hash);
					match self.sync_best_block_updated(&confirmables, &tip_hash).await {
						Ok(()) => {}
						Err(Error::ChainAccessInconsistency) => {
							// Immediately restart syncing when we encounter any inconsistencies.
							continue;
						}
						Err(err) => {
							// (Semi-)permanent failure, retry later.
							return Err(err);
						}
					}
				}

				match self.get_confirmed_transactions().await {
					Ok((confirmed_txs, unconfirmed_registered_txs, unspent_registered_outputs)) => {
						// Double-check best tip. If something changed, restart last-minute.
						tip_hash = client.get_tip_hash().await?;
						if Some(tip_hash) != *locked_last_sync_hash {
							continue;
						}

						self.sync_confirmed_transactions(
							&confirmables,
							confirmed_txs,
							unconfirmed_registered_txs,
							unspent_registered_outputs,
						);
					}
					Err(Error::ChainAccessInconsistency) => {
						// Immediately restart syncing when we encounter any inconsistencies.
						continue;
					}
					Err(err) => {
						// (Semi-)permanent failure, retry later.
						return Err(err);
					}
				}
			} else {
				break;
			}
		}
		Ok(())
	}

	// Processes the transaction and output queues, returns `true` if new items had been
	// registered.
	fn process_queues(&self) -> bool {
		let mut pending_registrations = false;
		{
			let mut locked_queued_transactions = self.queued_transactions.lock().unwrap();
			if !locked_queued_transactions.is_empty() {
				let mut locked_watched_transactions = self.watched_transactions.lock().unwrap();
				pending_registrations = true;

				locked_watched_transactions.extend(locked_queued_transactions.iter());
				*locked_queued_transactions = HashSet::new();
			}
		}
		{
			let mut locked_queued_outputs = self.queued_outputs.lock().unwrap();
			if !locked_queued_outputs.is_empty() {
				let mut locked_watched_outputs = self.watched_outputs.lock().unwrap();
				pending_registrations = true;

				locked_watched_outputs.extend(locked_queued_outputs.iter().cloned());
				*locked_queued_outputs = HashSet::new();
			}
		}
		pending_registrations
	}

	async fn sync_best_block_updated(
		&self, confirmables: &Vec<&(dyn Confirm + Sync)>, tip_hash: &BlockHash,
	) -> Result<(), Error> {
		let client = &*self.blockchain;

		// Inform the interface of the new block.
		let tip_header = client.get_header_by_hash(tip_hash).await?;
		let tip_status = client.get_block_status(&tip_hash).await?;
		if tip_status.in_best_chain {
			if let Some(tip_height) = tip_status.height {
				for c in confirmables {
					c.best_block_updated(&tip_header, tip_height);
				}
			}
		} else {
			return Err(Error::ChainAccessInconsistency);
		}
		Ok(())
	}

	fn sync_confirmed_transactions(
		&self, confirmables: &Vec<&(dyn Confirm + Sync)>, confirmed_txs: Vec<ConfirmedTx>,
		unconfirmed_registered_txs: HashSet<Txid>,
		unspent_registered_outputs: HashSet<WatchedOutput>,
	) {
		for ctx in confirmed_txs {
			for c in confirmables {
				c.transactions_confirmed(
					&ctx.block_header,
					&[(ctx.pos, &ctx.tx)],
					ctx.block_height,
				);
			}
		}

		*self.watched_transactions.lock().unwrap() = unconfirmed_registered_txs;
		*self.watched_outputs.lock().unwrap() = unspent_registered_outputs;
	}

	async fn get_confirmed_transactions(
		&self,
	) -> Result<(Vec<ConfirmedTx>, HashSet<Txid>, HashSet<WatchedOutput>), Error> {
		let client = &*self.blockchain;

		// First, check the confirmation status of registered transactions as well as the
		// status of dependent transactions of registered outputs.

		let mut confirmed_txs = Vec::new();

		// Check in the current queue, as well as in registered transactions leftover from
		// previous iterations.
		let registered_txs = self.watched_transactions.lock().unwrap().clone();

		// Remember all registered but unconfirmed transactions for future processing.
		let mut unconfirmed_registered_txs = HashSet::new();

		for txid in registered_txs {
			if let Some(confirmed_tx) = self.get_confirmed_tx(&txid, None, None).await? {
				confirmed_txs.push(confirmed_tx);
			} else {
				unconfirmed_registered_txs.insert(txid);
			}
		}

		// Check all registered outputs for dependent spending transactions.
		let registered_outputs = self.watched_outputs.lock().unwrap().clone();

		// Remember all registered outputs that haven't been spent for future processing.
		let mut unspent_registered_outputs = HashSet::new();

		for output in registered_outputs {
			if let Some(output_status) = client
				.get_output_status(&output.outpoint.txid, output.outpoint.index as u64)
				.await?
			{
				if let Some(spending_txid) = output_status.txid {
					if let Some(spending_tx_status) = output_status.status {
						if let Some(confirmed_tx) = self
							.get_confirmed_tx(
								&spending_txid,
								spending_tx_status.block_hash,
								spending_tx_status.block_height,
							)
							.await?
						{
							confirmed_txs.push(confirmed_tx);
							continue;
						}
					}
				}
			}
			unspent_registered_outputs.insert(output);
		}

		// Sort all confirmed transactions first by block height, then by in-block
		// position, and finally feed them to the interface in order.
		confirmed_txs.sort_unstable_by(|tx1, tx2| {
			tx1.block_height.cmp(&tx2.block_height).then_with(|| tx1.pos.cmp(&tx2.pos))
		});

		Ok((confirmed_txs, unconfirmed_registered_txs, unspent_registered_outputs))
	}

	async fn get_confirmed_tx(
		&self, txid: &Txid, expected_block_hash: Option<BlockHash>, known_block_height: Option<u32>,
	) -> Result<Option<ConfirmedTx>, Error> {
		let client = &*self.blockchain;
		if let Some(merkle_block) = client.get_merkle_block(&txid).await? {
			let block_header = merkle_block.header;
			let block_hash = block_header.block_hash();
			if let Some(expected_block_hash) = expected_block_hash {
				if expected_block_hash != block_hash {
					return Err(Error::ChainAccessInconsistency);
				}
			}

			let mut matches = vec![*txid];
			let mut indexes = Vec::new();
			let _ = merkle_block.txn.extract_matches(&mut matches, &mut indexes);
			assert_eq!(indexes.len(), 1);
			let pos = indexes[0] as usize;
			if let Some(tx) = client.get_tx(&txid).await? {
				if let Some(block_height) = known_block_height {
					// We can take a shortcut here if a previous call already gave us the height.
					return Ok(Some(ConfirmedTx { tx, block_header, pos, block_height }));
				}

				let block_status = client.get_block_status(&block_hash).await?;
				if let Some(block_height) = block_status.height {
					return Ok(Some(ConfirmedTx { tx, block_header, pos, block_height }));
				} else {
					// If any previously-confirmed block suddenly is no longer confirmed, we found
					// an inconsisteny and should start over.
					return Err(Error::ChainAccessInconsistency);
				}
			}
		}
		Ok(None)
	}

	async fn sync_unconfirmed_transactions(
		&self, confirmables: &Vec<&(dyn Confirm + Sync)>,
	) -> Result<(), Error> {
		let client = &*self.blockchain;
		// Query the interface for relevant txids and check whether the relevant blocks are still
		// in the best chain, mark them unconfirmed otherwise. If the transactions have been
		// reconfirmed in another block, we'll confirm them in the next sync iteration.
		let relevant_txids = confirmables
			.iter()
			.flat_map(|c| c.get_relevant_txids())
			.collect::<HashSet<(Txid, Option<BlockHash>)>>();
		for (txid, block_hash_opt) in relevant_txids {
			if let Some(block_hash) = block_hash_opt {
				let block_status = client.get_block_status(&block_hash).await?;
				if block_status.in_best_chain {
					// Skip if the block in question is still confirmed.
					continue;
				}
			}

			for c in confirmables {
				c.transaction_unconfirmed(&txid);
			}
		}

		Ok(())
	}

	pub(crate) fn create_funding_transaction(
		&self, output_script: &Script, value_sats: u64, confirmation_target: ConfirmationTarget,
	) -> Result<Transaction, Error> {
		let num_blocks = num_blocks_from_conf_target(confirmation_target);
		let fee_rate = self.blockchain.estimate_fee(num_blocks)?;

		let locked_wallet = self.wallet.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		tx_builder.add_recipient(output_script.clone(), value_sats).fee_rate(fee_rate).enable_rbf();

		let (mut psbt, _) = tx_builder.finish()?;
		log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);

		// We double-check that no inputs try to spend non-witness outputs. As we use a SegWit
		// wallet descriptor this technically shouldn't ever happen, but better safe than sorry.
		for input in &psbt.inputs {
			if input.witness_utxo.is_none() {
				log_error!(self.logger, "Tried to spend a non-witness funding output. This must not ever happen. Panicking!");
				panic!("Tried to spend a non-witness funding output. This must not ever happen.");
			}
		}

		if !locked_wallet.sign(&mut psbt, SignOptions::default())? {
			return Err(Error::FundingTxCreationFailed);
		}

		Ok(psbt.extract_tx())
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.wallet.lock().unwrap().get_address(AddressIndex::New)?;
		Ok(address_info.address)
	}
}

struct ConfirmedTx {
	tx: Transaction,
	block_header: BlockHeader,
	block_height: u32,
	pos: usize,
}

impl<D> FeeEstimator for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		let num_blocks = num_blocks_from_conf_target(confirmation_target);
		let fallback_fee = fallback_fee_from_conf_target(confirmation_target);
		self.blockchain
			.estimate_fee(num_blocks)
			.map_or(fallback_fee, |fee_rate| (fee_rate.fee_wu(1000) as u32).max(FEERATE_FLOOR_SATS_PER_KW)) as u32
	}
}

impl<D> BroadcasterInterface for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn broadcast_transaction(&self, tx: &Transaction) {
		match self.blockchain.broadcast(tx) {
			Ok(_) => {}
			Err(err) => {
				log_error!(self.logger, "Failed to broadcast transaction: {}", err),
				panic!("Failed to broadcast transaction: {}", err);
			}
		}
	}
}

impl<D> Filter for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn register_tx(&self, txid: &Txid, _script_pubkey: &Script) {
		self.queued_transactions.lock().unwrap().insert(*txid);
	}

	fn register_output(&self, output: WatchedOutput) {
		self.queued_outputs.lock().unwrap().insert(output);
	}
}

fn num_blocks_from_conf_target(confirmation_target: ConfirmationTarget) -> usize {
	match confirmation_target {
		ConfirmationTarget::Background => 12,
		ConfirmationTarget::Normal => 6,
		ConfirmationTarget::HighPriority => 3,
	}
}

fn fallback_fee_from_conf_target(confirmation_target: ConfirmationTarget) -> u32 {
	match confirmation_target {
		ConfirmationTarget::Background => FEERATE_FLOOR_SATS_PER_KW,
		ConfirmationTarget::Normal => 2000,
		ConfirmationTarget::HighPriority => 5000,
	}
}
