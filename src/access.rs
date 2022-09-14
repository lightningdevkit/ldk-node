use crate::error::LdkLiteError as Error;
#[allow(unused_imports)]
use crate::logger::{
	log_error, log_given_level, log_info, log_internal, log_trace, log_warn, FilesystemLogger,
	Logger,
};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};

use bdk::blockchain::{Blockchain, EsploraBlockchain, GetBlockHash, GetHeight, GetTx};
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::{SignOptions, SyncOptions};

use bitcoin::{BlockHash, Script, Transaction, Txid};

use std::sync::{Arc, Mutex};

pub struct LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	blockchain: EsploraBlockchain,
	wallet: Mutex<bdk::Wallet<D>>,
	queued_transactions: Mutex<Vec<Txid>>,
	watched_transactions: Mutex<Vec<Txid>>,
	queued_outputs: Mutex<Vec<WatchedOutput>>,
	watched_outputs: Mutex<Vec<WatchedOutput>>,
	last_sync_height: Mutex<Option<u32>>,
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
		let watched_transactions = Mutex::new(Vec::new());
		let queued_transactions = Mutex::new(Vec::new());
		let watched_outputs = Mutex::new(Vec::new());
		let queued_outputs = Mutex::new(Vec::new());
		let last_sync_height = Mutex::new(None);
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

	pub(crate) fn sync_wallet(&self) -> Result<(), Error> {
		let sync_options = SyncOptions { progress: None };

		self.wallet
			.lock()
			.unwrap()
			.sync(&self.blockchain, sync_options)
			.map_err(|e| Error::Bdk(e))?;

		Ok(())
	}

	pub(crate) fn sync(&self, confirmables: Vec<&(dyn Confirm + Sync)>) -> Result<(), Error> {
		let client = &*self.blockchain;

		let cur_height = client.get_height()?;

		let mut locked_last_sync_height = self.last_sync_height.lock().unwrap();
		if cur_height >= locked_last_sync_height.unwrap_or(0) {
			{
				// First, inform the interface of the new block.
				let cur_block_header = client.get_header(cur_height)?;
				for c in &confirmables {
					c.best_block_updated(&cur_block_header, cur_height);
				}

				*locked_last_sync_height = Some(cur_height);
			}

			{
				// First, check the confirmation status of registered transactions as well as the
				// status of dependent transactions of registered outputs.
				let mut locked_queued_transactions = self.queued_transactions.lock().unwrap();
				let mut locked_queued_outputs = self.queued_outputs.lock().unwrap();
				let mut locked_watched_transactions = self.watched_transactions.lock().unwrap();
				let mut locked_watched_outputs = self.watched_outputs.lock().unwrap();

				let mut confirmed_txs = Vec::new();

				// Check in the current queue, as well as in registered transactions leftover from
				// previous iterations.
				let mut registered_txs: Vec<Txid> = locked_watched_transactions
					.iter()
					.chain(locked_queued_transactions.iter())
					.cloned()
					.collect();

				registered_txs.sort_unstable_by(|txid1, txid2| txid1.cmp(&txid2));
				registered_txs.dedup_by(|txid1, txid2| txid1.eq(&txid2));

				// Remember all registered but unconfirmed transactions for future processing.
				let mut unconfirmed_registered_txs = Vec::new();

				for txid in registered_txs {
					if let Some(tx_status) = client.get_tx_status(&txid)? {
						if tx_status.confirmed {
							if let Some(tx) = client.get_tx(&txid)? {
								if let Some(block_height) = tx_status.block_height {
									let block_header = client.get_header(block_height)?;
									if let Some(merkle_proof) = client.get_merkle_proof(&txid)? {
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
					unconfirmed_registered_txs.push(txid);
				}

				// Check all registered outputs for dependent spending transactions.
				let registered_outputs: Vec<WatchedOutput> = locked_watched_outputs
					.iter()
					.chain(locked_queued_outputs.iter())
					.cloned()
					.collect();

				// Remember all registered outputs that haven't been spent for future processing.
				let mut unspent_registered_outputs = Vec::new();

				for output in registered_outputs {
					if let Some(output_status) = client
						.get_output_status(&output.outpoint.txid, output.outpoint.index as u64)?
					{
						if output_status.spent {
							if let Some(spending_tx_status) = output_status.status {
								if spending_tx_status.confirmed {
									let spending_txid = output_status.txid.unwrap();
									if let Some(spending_tx) = client.get_tx(&spending_txid)? {
										let block_height = spending_tx_status.block_height.unwrap();
										let block_header = client.get_header(block_height)?;
										if let Some(merkle_proof) =
											client.get_merkle_proof(&spending_txid)?
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

				// Sort all confirmed transactions by block height and feed them to the interface
				// in order.
				confirmed_txs.sort_unstable_by(
					|(_, block_height1, _, _), (_, block_height2, _, _)| {
						block_height1.cmp(&block_height2)
					},
				);
				for (tx, block_height, block_header, pos) in confirmed_txs {
					for c in &confirmables {
						c.transactions_confirmed(&block_header, &[(pos, &tx)], block_height);
					}
				}

				*locked_watched_transactions = unconfirmed_registered_txs;
				*locked_queued_transactions = Vec::new();
				*locked_watched_outputs = unspent_registered_outputs;
				*locked_queued_outputs = Vec::new();
			}

			{
				// Query the interface for relevant txids and check whether they have been
				// reorged-out of the chain.
				let unconfirmed_txids = confirmables
					.iter()
					.flat_map(|c| c.get_relevant_txids())
					.filter(|txid| {
						client
							.get_tx_status(txid)
							.ok()
							.unwrap_or(None)
							.map_or(true, |status| !status.confirmed)
					})
					.collect::<Vec<Txid>>();

				// Mark all relevant unconfirmed transactions as unconfirmed.
				for txid in &unconfirmed_txids {
					for c in &confirmables {
						c.transaction_unconfirmed(txid);
					}
				}
			}
		}

		// TODO: check whether new outputs have been registered by now and process them
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

impl<D> FeeEstimator for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		let num_blocks = num_blocks_from_conf_target(confirmation_target);
		self.blockchain.estimate_fee(num_blocks).map_or(253, |fee_rate| fee_rate.fee_wu(1000))
			as u32
	}
}

impl<D> BroadcasterInterface for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn broadcast_transaction(&self, tx: &Transaction) {
		self.blockchain.broadcast(tx).ok();
	}
}

impl<D> Filter for LdkLiteChainAccess<D>
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

impl<D> GetHeight for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_height(&self) -> Result<u32, bdk::Error> {
		self.blockchain.get_height()
	}
}

impl<D> GetBlockHash for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_block_hash(&self, height: u64) -> Result<BlockHash, bdk::Error> {
		self.blockchain.get_block_hash(height)
	}
}

impl<D> GetTx for LdkLiteChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, bdk::Error> {
		self.blockchain.get_tx(txid)
	}
}

fn num_blocks_from_conf_target(confirmation_target: ConfirmationTarget) -> usize {
	match confirmation_target {
		ConfirmationTarget::Background => 6,
		ConfirmationTarget::Normal => 3,
		ConfirmationTarget::HighPriority => 1,
	}
}
