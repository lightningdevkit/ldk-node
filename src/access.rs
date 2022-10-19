use crate::logger::{
	log_error, log_given_level, log_internal, log_trace, FilesystemLogger, Logger,
};
use crate::Error;

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};

use bdk::blockchain::{Blockchain, EsploraBlockchain, GetBlockHash, GetHeight, GetTx};
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{SignOptions, SyncOptions};

use bitcoin::{BlockHash, Script, Transaction, Txid};

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

pub struct ChainAccess<D>
where
	D: BatchDatabase,
{
	blockchain: EsploraBlockchain,
	wallet: Mutex<bdk::Wallet<D>>,
	queued_transactions: Mutex<Vec<Txid>>,
	watched_transactions: Mutex<Vec<Txid>>,
	queued_outputs: Mutex<Vec<WatchedOutput>>,
	watched_outputs: Mutex<Vec<WatchedOutput>>,
	last_sync_height: tokio::sync::Mutex<Option<u32>>,
	logger: Arc<FilesystemLogger>,
}

impl<D> ChainAccess<D>
where
	D: BatchDatabase,
{
	pub(crate) fn new(
		blockchain: EsploraBlockchain, wallet: bdk::Wallet<D>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let wallet = Mutex::new(wallet);
		let watched_transactions = Mutex::new(Vec::new());
		let queued_transactions = Mutex::new(Vec::new());
		let watched_outputs = Mutex::new(Vec::new());
		let queued_outputs = Mutex::new(Vec::new());
		let last_sync_height = tokio::sync::Mutex::new(None);
		Self {
			blockchain,
			wallet,
			queued_transactions,
			watched_transactions,
			queued_outputs,
			watched_outputs,
			last_sync_height,
			logger,
		}
	}

	pub(crate) async fn sync_wallet(&self) -> Result<(), Error> {
		let sync_options = SyncOptions { progress: None };

		self.wallet.lock().unwrap().sync(&self.blockchain, sync_options)?;

		Ok(())
	}

	pub(crate) async fn sync(&self, confirmables: Vec<&(dyn Confirm + Sync)>) -> Result<(), Error> {
		let client = &*self.blockchain;

		let cur_height = client.get_height().await?;

		let mut locked_last_sync_height = self.last_sync_height.lock().await;
		if cur_height >= locked_last_sync_height.unwrap_or(0) {
			self.sync_best_block_updated(&confirmables, cur_height, &mut locked_last_sync_height)
				.await?;
			self.sync_transactions_confirmed(&confirmables).await?;
			self.sync_transaction_unconfirmed(&confirmables).await?;
		}
		// TODO: check whether new outputs have been registered by now and process them
		Ok(())
	}

	async fn sync_best_block_updated(
		&self, confirmables: &Vec<&(dyn Confirm + Sync)>, cur_height: u32,
		locked_last_sync_height: &mut tokio::sync::MutexGuard<'_, Option<u32>>,
	) -> Result<(), Error> {
		let client = &*self.blockchain;

		// Inform the interface of the new block.
		let cur_block_header = client.get_header(cur_height).await?;
		for c in confirmables {
			c.best_block_updated(&cur_block_header, cur_height);
		}

		**locked_last_sync_height = Some(cur_height);
		Ok(())
	}

	async fn sync_transactions_confirmed(
		&self, confirmables: &Vec<&(dyn Confirm + Sync)>,
	) -> Result<(), Error> {
		let client = &*self.blockchain;

		// First, check the confirmation status of registered transactions as well as the
		// status of dependent transactions of registered outputs.

		let mut confirmed_txs = Vec::new();

		// Check in the current queue, as well as in registered transactions leftover from
		// previous iterations.
		let registered_txs: HashSet<Txid> = {
			let locked_queued_transactions = self.queued_transactions.lock().unwrap();
			let locked_watched_transactions = self.watched_transactions.lock().unwrap();
			locked_watched_transactions
				.iter()
				.chain(locked_queued_transactions.iter())
				.cloned()
				.collect()
		};

		// Remember all registered but unconfirmed transactions for future processing.
		let mut unconfirmed_registered_txs = Vec::new();

		for txid in registered_txs {
			if let Some(tx_status) = client.get_tx_status(&txid).await? {
				if tx_status.confirmed {
					if let Some(tx) = client.get_tx(&txid).await? {
						if let Some(block_height) = tx_status.block_height {
							// TODO: Switch to `get_header_by_hash` once released upstream (https://github.com/bitcoindevkit/rust-esplora-client/pull/17)
							let block_header = client.get_header(block_height).await?;
							if let Some(merkle_proof) = client.get_merkle_proof(&txid).await? {
								if block_height == merkle_proof.block_height {
									confirmed_txs.push((
										tx,
										block_height,
										block_header,
										merkle_proof.pos,
									));
									continue;
								}
							}
						}
					}
				}
			}
			unconfirmed_registered_txs.push(txid);
		}

		// Check all registered outputs for dependent spending transactions.
		let registered_outputs: Vec<WatchedOutput> = {
			let locked_queued_outputs = self.queued_outputs.lock().unwrap();
			let locked_watched_outputs = self.watched_outputs.lock().unwrap();
			locked_watched_outputs.iter().chain(locked_queued_outputs.iter()).cloned().collect()
		};

		// Remember all registered outputs that haven't been spent for future processing.
		let mut unspent_registered_outputs = Vec::new();

		for output in registered_outputs {
			if let Some(output_status) = client
				.get_output_status(&output.outpoint.txid, output.outpoint.index as u64)
				.await?
			{
				if output_status.spent {
					if let Some(spending_tx_status) = output_status.status {
						if spending_tx_status.confirmed {
							let spending_txid = output_status.txid.unwrap();
							if let Some(spending_tx) = client.get_tx(&spending_txid).await? {
								let block_height = spending_tx_status.block_height.unwrap();
								// TODO: Switch to `get_header_by_hash` once released upstream (https://github.com/bitcoindevkit/rust-esplora-client/pull/17)
								let block_header = client.get_header(block_height).await?;
								if let Some(merkle_proof) =
									client.get_merkle_proof(&spending_txid).await?
								{
									confirmed_txs.push((
										spending_tx,
										block_height,
										block_header,
										merkle_proof.pos,
									));
									continue;
								}
							}
						}
					}
				}
			}
			unspent_registered_outputs.push(output);
		}

		// Sort all confirmed transactions first by block height, then by in-block
		// position, and finally feed them to the interface in order.
		confirmed_txs.sort_unstable_by(
			|(_, block_height1, _, pos1), (_, block_height2, _, pos2)| {
				block_height1.cmp(&block_height2).then_with(|| pos1.cmp(&pos2))
			},
		);
		for (tx, block_height, block_header, pos) in confirmed_txs {
			for c in confirmables {
				c.transactions_confirmed(&block_header, &[(pos, &tx)], block_height);
			}
		}

		*self.queued_transactions.lock().unwrap() = Vec::new();
		*self.watched_transactions.lock().unwrap() = unconfirmed_registered_txs;
		*self.queued_outputs.lock().unwrap() = Vec::new();
		*self.watched_outputs.lock().unwrap() = unspent_registered_outputs;

		Ok(())
	}

	async fn sync_transaction_unconfirmed(
		&self, confirmables: &Vec<&(dyn Confirm + Sync)>,
	) -> Result<(), Error> {
		let client = &*self.blockchain;
		// Query the interface for relevant txids and check whether they have been
		// reorged-out of the chain.
		let relevant_txids =
			confirmables.iter().flat_map(|c| c.get_relevant_txids()).collect::<HashSet<Txid>>();
		for txid in relevant_txids {
			let tx_unconfirmed = client
				.get_tx_status(&txid)
				.await
				.ok()
				.unwrap_or(None)
				.map_or(true, |status| !status.confirmed);
			if tx_unconfirmed {
				for c in confirmables {
					c.transaction_unconfirmed(&txid);
				}
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

impl<D> FeeEstimator for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		let num_blocks = num_blocks_from_conf_target(confirmation_target);
		let fallback_fee = fallback_fee_from_conf_target(confirmation_target);
		self.blockchain
			.estimate_fee(num_blocks)
			.map_or(fallback_fee, |fee_rate| (fee_rate.fee_wu(1000) as u32).max(MIN_FEERATE)) as u32
	}
}

impl<D> BroadcasterInterface for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn broadcast_transaction(&self, tx: &Transaction) {
		match self.blockchain.broadcast(tx) {
			Ok(_) => {}
			Err(err) => {
				log_error!(self.logger, "Failed to broadcast transaction: {}", err);
				panic!("Failed to broadcast transaction: {}", err);
			}
		}
	}
}

impl<D> Filter for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn register_tx(&self, txid: &Txid, _script_pubkey: &Script) {
		self.queued_transactions.lock().unwrap().push(*txid);
	}

	fn register_output(&self, output: WatchedOutput) -> Option<(usize, Transaction)> {
		self.queued_outputs.lock().unwrap().push(output);
		return None;
	}
}

impl<D> GetHeight for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_height(&self) -> Result<u32, bdk::Error> {
		self.blockchain.get_height()
	}
}

impl<D> GetBlockHash for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_block_hash(&self, height: u64) -> Result<BlockHash, bdk::Error> {
		self.blockchain.get_block_hash(height)
	}
}

impl<D> GetTx for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, bdk::Error> {
		self.blockchain.get_tx(txid)
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
		ConfirmationTarget::Background => MIN_FEERATE,
		ConfirmationTarget::Normal => 2000,
		ConfirmationTarget::HighPriority => 5000,
	}
}
