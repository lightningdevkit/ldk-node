use crate::logger::{
	log_error, log_given_level, log_internal, log_trace, FilesystemLogger, Logger,
};
use crate::{Config, Error};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};

use bdk::blockchain::EsploraBlockchain;
use bdk::database::BatchDatabase;
use bdk::esplora_client;
use bdk::wallet::AddressIndex;
use bdk::{FeeRate, SignOptions, SyncOptions};

use bitcoin::{Script, Transaction, Txid};

use std::collections::HashSet;
use std::sync::{mpsc, Arc, Mutex, RwLock};

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

// The used 'stop gap' parameter used by BDK's wallet sync. This seems to configure the threshold
// number of blocks after which BDK stops looking for scripts belonging to the wallet.
const BDK_CLIENT_STOP_GAP: usize = 20;

// The number of concurrent requests made against the API provider.
const BDK_CLIENT_CONCURRENCY: u8 = 8;

pub struct ChainAccess<D>
where
	D: BatchDatabase,
{
	blockchain: Arc<EsploraBlockchain>,
	client: Arc<esplora_client::AsyncClient>,
	wallet: Mutex<bdk::Wallet<D>>,
	queued_transactions: Mutex<Vec<Txid>>,
	watched_transactions: Mutex<Vec<Txid>>,
	queued_outputs: Mutex<Vec<WatchedOutput>>,
	watched_outputs: Mutex<Vec<WatchedOutput>>,
	last_sync_height: tokio::sync::Mutex<Option<u32>>,
	tokio_runtime: RwLock<Option<Arc<tokio::runtime::Runtime>>>,
	_config: Arc<Config>,
	logger: Arc<FilesystemLogger>,
}

impl<D> ChainAccess<D>
where
	D: BatchDatabase,
{
	pub(crate) fn new(
		wallet: bdk::Wallet<D>, config: Arc<Config>, logger: Arc<FilesystemLogger>,
	) -> Self {
		let wallet = Mutex::new(wallet);
		let watched_transactions = Mutex::new(Vec::new());
		let queued_transactions = Mutex::new(Vec::new());
		let watched_outputs = Mutex::new(Vec::new());
		let queued_outputs = Mutex::new(Vec::new());
		let last_sync_height = tokio::sync::Mutex::new(None);
		let tokio_runtime = RwLock::new(None);
		// TODO: Check that we can be sure that the Esplora client re-connects in case of failure
		// and and exits cleanly on drop. Otherwise we need to handle this/move it to the runtime?
		let blockchain = Arc::new(
			EsploraBlockchain::new(&config.esplora_server_url, BDK_CLIENT_STOP_GAP)
				.with_concurrency(BDK_CLIENT_CONCURRENCY),
		);
		let client_builder =
			esplora_client::Builder::new(&format!("http://{}", &config.esplora_server_url));
		let client = Arc::new(client_builder.build_async().unwrap());
		Self {
			blockchain,
			client,
			wallet,
			queued_transactions,
			watched_transactions,
			queued_outputs,
			watched_outputs,
			last_sync_height,
			tokio_runtime,
			_config: config,
			logger,
		}
	}

	pub(crate) fn set_runtime(&self, tokio_runtime: Arc<tokio::runtime::Runtime>) {
		*self.tokio_runtime.write().unwrap() = Some(tokio_runtime);
	}

	pub(crate) fn drop_runtime(&self) {
		*self.tokio_runtime.write().unwrap() = None;
	}

	pub(crate) async fn sync_wallet(&self) -> Result<(), Error> {
		let sync_options = SyncOptions { progress: None };

		self.wallet.lock().unwrap().sync(&self.blockchain, sync_options).await?;

		Ok(())
	}

	pub(crate) async fn sync(
		&self, confirmables: Vec<&(dyn Confirm + Send + Sync)>,
	) -> Result<(), Error> {
		let cur_height = self.client.get_height().await?;

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
		&self, confirmables: &Vec<&(dyn Confirm + Send + Sync)>, cur_height: u32,
		locked_last_sync_height: &mut tokio::sync::MutexGuard<'_, Option<u32>>,
	) -> Result<(), Error> {
		// Inform the interface of the new block.
		let cur_block_header = self.client.get_header(cur_height).await?;
		for c in confirmables {
			c.best_block_updated(&cur_block_header, cur_height);
		}

		**locked_last_sync_height = Some(cur_height);
		Ok(())
	}

	async fn sync_transactions_confirmed(
		&self, confirmables: &Vec<&(dyn Confirm + Send + Sync)>,
	) -> Result<(), Error> {
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
			if let Some(tx_status) = self.client.get_tx_status(&txid).await? {
				if tx_status.confirmed {
					if let Some(tx) = self.client.get_tx(&txid).await? {
						if let Some(block_height) = tx_status.block_height {
							// TODO: Switch to `get_header_by_hash` once released upstream (https://github.com/bitcoindevkit/rust-esplora-client/pull/17)
							let block_header = self.client.get_header(block_height).await?;
							if let Some(merkle_proof) = self.client.get_merkle_proof(&txid).await? {
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
			if let Some(output_status) = self
				.client
				.get_output_status(&output.outpoint.txid, output.outpoint.index as u64)
				.await?
			{
				if output_status.spent {
					if let Some(spending_tx_status) = output_status.status {
						if spending_tx_status.confirmed {
							let spending_txid = output_status.txid.unwrap();
							if let Some(spending_tx) = self.client.get_tx(&spending_txid).await? {
								let block_height = spending_tx_status.block_height.unwrap();
								// TODO: Switch to `get_header_by_hash` once released upstream (https://github.com/bitcoindevkit/rust-esplora-client/pull/17)
								let block_header = self.client.get_header(block_height).await?;
								if let Some(merkle_proof) =
									self.client.get_merkle_proof(&spending_txid).await?
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
		&self, confirmables: &Vec<&(dyn Confirm + Send + Sync)>,
	) -> Result<(), Error> {
		// Query the interface for relevant txids and check whether they have been
		// reorged-out of the chain.
		let relevant_txids =
			confirmables.iter().flat_map(|c| c.get_relevant_txids()).collect::<HashSet<Txid>>();
		for txid in relevant_txids {
			let tx_unconfirmed = self
				.client
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
		let locked_wallet = self.wallet.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		let fallback_fee = fallback_fee_from_conf_target(confirmation_target);
		let fee_rate = self
			.estimate_fee(confirmation_target)
			.unwrap_or(FeeRate::from_sat_per_kwu(fallback_fee as f32));

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

	fn estimate_fee(&self, confirmation_target: ConfirmationTarget) -> Result<bdk::FeeRate, Error> {
		let num_blocks = num_blocks_from_conf_target(confirmation_target);

		let locked_runtime = self.tokio_runtime.read().unwrap();
		if locked_runtime.as_ref().is_none() {
			return Err(Error::FeeEstimationFailed);
		}

		let tokio_client = Arc::clone(&self.client);
		let (sender, receiver) = mpsc::sync_channel(1);

		locked_runtime.as_ref().unwrap().spawn(async move {
			let res = tokio_client.get_fee_estimates().await;
			let _ = sender.send(res);
		});

		let estimates = receiver
			.recv()
			.map_err(|_| Error::FeeEstimationFailed)?
			.map_err(|_| Error::FeeEstimationFailed)?;

		Ok(bdk::FeeRate::from_sat_per_vb(
			esplora_client::convert_fee_rate(num_blocks, estimates)
				.map_err(|_| Error::FeeEstimationFailed)?,
		))
	}
}

impl<D> FeeEstimator for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		let fallback_fee = fallback_fee_from_conf_target(confirmation_target);

		self.estimate_fee(confirmation_target)
			.map_or(fallback_fee, |fee_rate| (fee_rate.fee_wu(1000) as u32).max(MIN_FEERATE)) as u32
	}
}

impl<D> BroadcasterInterface for ChainAccess<D>
where
	D: BatchDatabase,
{
	fn broadcast_transaction(&self, tx: &Transaction) {
		let locked_runtime = self.tokio_runtime.read().unwrap();
		if locked_runtime.as_ref().is_none() {
			log_error!(self.logger, "Failed to broadcast transaction: No runtime.");
			return;
		}

		let tokio_client = Arc::clone(&self.client);
		let tokio_tx = tx.clone();
		let (sender, receiver) = mpsc::sync_channel(1);

		locked_runtime.as_ref().unwrap().spawn(async move {
			let res = tokio_client.broadcast(&tokio_tx).await;
			let _ = sender.send(res);
		});

		match receiver.recv().unwrap() {
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
