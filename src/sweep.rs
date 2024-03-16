use crate::hex_utils;
use crate::io::{
	SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, Logger};
use crate::wallet::{Wallet, WalletKeysManager};
use crate::Error;

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::{self, BestBlock, Confirm, Filter, Listen, WatchedOutput};
use lightning::impl_writeable_tlv_based;
use lightning::ln::ChannelId;
use lightning::sign::{EntropySource, SpendableOutputDescriptor};
use lightning::util::persist::KVStore;
use lightning::util::ser::Writeable;

use bitcoin::blockdata::block::Header;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{BlockHash, Transaction, Txid};

use std::ops::Deref;
use std::sync::{Arc, Mutex};

const CONSIDERED_SPENT_THRESHOLD_CONF: u32 = 6;

const REGENERATE_SPEND_THRESHOLD: u32 = 144;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpendableOutputInfo {
	pub(crate) id: [u8; 32],
	pub(crate) descriptor: SpendableOutputDescriptor,
	pub(crate) channel_id: Option<ChannelId>,
	pub(crate) first_broadcast_hash: Option<BlockHash>,
	pub(crate) latest_broadcast_height: Option<u32>,
	pub(crate) latest_spending_tx: Option<Transaction>,
	pub(crate) confirmation_height: Option<u32>,
	pub(crate) confirmation_hash: Option<BlockHash>,
}

impl SpendableOutputInfo {
	fn to_watched_output(&self) -> WatchedOutput {
		match &self.descriptor {
			SpendableOutputDescriptor::StaticOutput { outpoint, output, channel_keys_id: _ } => {
				WatchedOutput {
					block_hash: self.first_broadcast_hash,
					outpoint: *outpoint,
					script_pubkey: output.script_pubkey.clone(),
				}
			},
			SpendableOutputDescriptor::DelayedPaymentOutput(output) => WatchedOutput {
				block_hash: self.first_broadcast_hash,
				outpoint: output.outpoint,
				script_pubkey: output.output.script_pubkey.clone(),
			},
			SpendableOutputDescriptor::StaticPaymentOutput(output) => WatchedOutput {
				block_hash: self.first_broadcast_hash,
				outpoint: output.outpoint,
				script_pubkey: output.output.script_pubkey.clone(),
			},
		}
	}

	fn is_spent_in(&self, tx: &Transaction) -> bool {
		let prev_outpoint = match &self.descriptor {
			SpendableOutputDescriptor::StaticOutput { outpoint, .. } => *outpoint,
			SpendableOutputDescriptor::DelayedPaymentOutput(output) => output.outpoint,
			SpendableOutputDescriptor::StaticPaymentOutput(output) => output.outpoint,
		};

		for input in &tx.input {
			if input.previous_output == prev_outpoint.into_bitcoin_outpoint() {
				return true;
			}
		}

		false
	}

	pub(crate) fn value_satoshis(&self) -> u64 {
		match &self.descriptor {
			SpendableOutputDescriptor::StaticOutput { output, .. } => output.value,
			SpendableOutputDescriptor::DelayedPaymentOutput(output) => output.output.value,
			SpendableOutputDescriptor::StaticPaymentOutput(output) => output.output.value,
		}
	}
}

impl_writeable_tlv_based!(SpendableOutputInfo, {
	(0, id, required),
	(2, descriptor, required),
	(4, channel_id, option),
	(6, first_broadcast_hash, option),
	(8, latest_broadcast_height, option),
	(10, latest_spending_tx, option),
	(12, confirmation_height, option),
	(14, confirmation_hash, option),
});

pub(crate) struct OutputSweeper<B: Deref, E: Deref, F: Deref, K: Deref, L: Deref>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	F::Target: Filter,
	K::Target: KVStore,
	L::Target: Logger,
{
	outputs: Mutex<Vec<SpendableOutputInfo>>,
	wallet: Arc<Wallet<bdk::database::SqliteDatabase, B, E, L>>,
	broadcaster: B,
	fee_estimator: E,
	keys_manager: Arc<WalletKeysManager<bdk::database::SqliteDatabase, B, E, L>>,
	kv_store: K,
	best_block: Mutex<BestBlock>,
	chain_source: Option<F>,
	logger: L,
}

impl<B: Deref, E: Deref, F: Deref, K: Deref, L: Deref> OutputSweeper<B, E, F, K, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	F::Target: Filter,
	K::Target: KVStore,
	L::Target: Logger,
{
	pub(crate) fn new(
		outputs: Vec<SpendableOutputInfo>,
		wallet: Arc<Wallet<bdk::database::SqliteDatabase, B, E, L>>, broadcaster: B,
		fee_estimator: E,
		keys_manager: Arc<WalletKeysManager<bdk::database::SqliteDatabase, B, E, L>>, kv_store: K,
		best_block: BestBlock, chain_source: Option<F>, logger: L,
	) -> Self {
		if let Some(filter) = chain_source.as_ref() {
			for output_info in &outputs {
				let watched_output = output_info.to_watched_output();
				filter.register_output(watched_output);
			}
		}

		let outputs = Mutex::new(outputs);
		let best_block = Mutex::new(best_block);
		Self {
			outputs,
			wallet,
			broadcaster,
			fee_estimator,
			keys_manager,
			kv_store,
			best_block,
			chain_source,
			logger,
		}
	}

	pub(crate) fn add_outputs(
		&self, mut output_descriptors: Vec<SpendableOutputDescriptor>,
		channel_id: Option<ChannelId>,
	) {
		let non_static_outputs = output_descriptors
			.drain(..)
			.filter(|desc| !matches!(desc, SpendableOutputDescriptor::StaticOutput { .. }))
			.collect::<Vec<_>>();

		if non_static_outputs.is_empty() {
			return;
		}

		{
			let mut locked_outputs = self.outputs.lock().unwrap();
			for descriptor in non_static_outputs {
				let id = self.keys_manager.get_secure_random_bytes();
				let output_info = SpendableOutputInfo {
					id,
					descriptor,
					channel_id,
					first_broadcast_hash: None,
					latest_broadcast_height: None,
					latest_spending_tx: None,
					confirmation_height: None,
					confirmation_hash: None,
				};

				locked_outputs.push(output_info.clone());
				self.persist_info(&output_info).unwrap_or_else(|e| {
					log_error!(self.logger, "Error persisting SpendableOutputInfo: {:?}", e)
				});
			}
		}

		self.rebroadcast_if_necessary();
	}

	pub(crate) fn tracked_spendable_outputs(&self) -> Vec<SpendableOutputInfo> {
		self.outputs.lock().unwrap().clone()
	}

	fn rebroadcast_if_necessary(&self) {
		let (cur_height, cur_hash) = {
			let best_block = self.best_block.lock().unwrap();
			(best_block.height, best_block.block_hash)
		};

		let mut respend_descriptors = Vec::new();
		let mut respend_ids = Vec::new();

		{
			let mut locked_outputs = self.outputs.lock().unwrap();
			for output_info in locked_outputs.iter_mut() {
				if output_info.confirmation_height.is_some() {
					// Don't rebroadcast confirmed txs
					debug_assert!(output_info.confirmation_hash.is_some());
					continue;
				}

				if let Some(latest_broadcast_height) = output_info.latest_broadcast_height {
					// Re-generate spending tx after REGENERATE_SPEND_THRESHOLD, rebroadcast
					// after every block
					if latest_broadcast_height + REGENERATE_SPEND_THRESHOLD >= cur_height {
						respend_descriptors.push(output_info.descriptor.clone());
						respend_ids.push(output_info.id);
					} else if latest_broadcast_height < cur_height {
						if let Some(latest_spending_tx) = output_info.latest_spending_tx.as_ref() {
							self.broadcaster.broadcast_transactions(&[&latest_spending_tx]);
							output_info.latest_broadcast_height = Some(cur_height);
							self.persist_info(&output_info).unwrap_or_else(|e| {
								log_error!(
									self.logger,
									"Error persisting SpendableOutputInfo: {:?}",
									e
								)
							});
						}
					}
				} else {
					// Our first broadcast.
					respend_descriptors.push(output_info.descriptor.clone());
					respend_ids.push(output_info.id);
					output_info.first_broadcast_hash = Some(cur_hash);
					self.persist_info(&output_info).unwrap_or_else(|e| {
						log_error!(self.logger, "Error persisting SpendableOutputInfo: {:?}", e)
					});
				}
			}
		}

		if !respend_descriptors.is_empty() {
			match self.get_spending_tx(&respend_descriptors, cur_height) {
				Ok(spending_tx) => {
					self.broadcaster.broadcast_transactions(&[&spending_tx]);
					let mut locked_outputs = self.outputs.lock().unwrap();
					for output_info in locked_outputs.iter_mut() {
						if respend_ids.contains(&output_info.id) {
							if let Some(filter) = self.chain_source.as_ref() {
								let watched_output = output_info.to_watched_output();
								filter.register_output(watched_output);
							}

							output_info.latest_spending_tx = Some(spending_tx.clone());
							output_info.latest_broadcast_height = Some(cur_height);
							self.persist_info(&output_info).unwrap_or_else(|e| {
								log_error!(
									self.logger,
									"Error persisting SpendableOutputInfo: {:?}",
									e
								)
							});
						}
					}
				},
				Err(e) => {
					log_error!(self.logger, "Error spending outputs: {:?}", e);
				},
			};
		}
	}

	fn prune_confirmed_outputs(&self) {
		let cur_height = self.best_block.lock().unwrap().height;
		let mut locked_outputs = self.outputs.lock().unwrap();

		// Prune all outputs that have sufficient depth by now.
		locked_outputs.retain(|o| {
			if let Some(confirmation_height) = o.confirmation_height {
				if cur_height >= confirmation_height + CONSIDERED_SPENT_THRESHOLD_CONF - 1 {
					let key = hex_utils::to_string(&o.id);
					match self.kv_store.remove(
						SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
						SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
						&key,
						false,
					) {
						Ok(_) => return false,
						Err(e) => {
							log_error!(
								self.logger,
								"Removal of key {}/{}/{} failed due to: {}",
								SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
								SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
								key,
								e
							);
							return true;
						},
					}
				}
			}
			true
		});
	}

	fn get_spending_tx(
		&self, output_descriptors: &Vec<SpendableOutputDescriptor>, cur_height: u32,
	) -> Result<Transaction, ()> {
		let tx_feerate =
			self.fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::NonAnchorChannelFee);

		let destination_address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to get destination address from wallet: {}", e);
		})?;

		let locktime = LockTime::from_height(cur_height).unwrap_or(LockTime::ZERO);

		let output_descriptors = output_descriptors.iter().collect::<Vec<_>>();
		self.keys_manager.spend_spendable_outputs(
			&output_descriptors,
			Vec::new(),
			destination_address.script_pubkey(),
			tx_feerate,
			Some(locktime),
			&Secp256k1::new(),
		)
	}

	fn persist_info(&self, output: &SpendableOutputInfo) -> Result<(), Error> {
		let key = hex_utils::to_string(&output.id);
		let data = output.encode();
		self.kv_store
			.write(
				SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
				&data,
			)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Write for key {}/{}/{} failed due to: {}",
					SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					key,
					e
				);
				Error::PersistenceFailed
			})
	}
}

impl<B: Deref, E: Deref, F: Deref, K: Deref, L: Deref> Listen for OutputSweeper<B, E, F, K, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	F::Target: Filter,
	K::Target: KVStore,
	L::Target: Logger,
{
	fn filtered_block_connected(
		&self, header: &Header, txdata: &chain::transaction::TransactionData, height: u32,
	) {
		{
			let best_block = self.best_block.lock().unwrap();
			assert_eq!(best_block.block_hash, header.prev_blockhash,
			"Blocks must be connected in chain-order - the connected header must build on the last connected header");
			assert_eq!(best_block.height, height - 1,
			"Blocks must be connected in chain-order - the connected block height must be one greater than the previous height");
		}

		self.transactions_confirmed(header, txdata, height);
		self.best_block_updated(header, height);
	}

	fn block_disconnected(&self, header: &Header, height: u32) {
		let new_height = height - 1;
		{
			let mut best_block = self.best_block.lock().unwrap();
			assert_eq!(best_block.block_hash, header.block_hash(),
				"Blocks must be disconnected in chain-order - the disconnected header must be the last connected header");
			assert_eq!(best_block.height, height,
				"Blocks must be disconnected in chain-order - the disconnected block must have the correct height");
			*best_block = BestBlock::new(header.prev_blockhash, new_height)
		}

		let mut locked_outputs = self.outputs.lock().unwrap();
		for output_info in locked_outputs.iter_mut() {
			if output_info.confirmation_hash == Some(header.block_hash()) {
				debug_assert_eq!(output_info.confirmation_height, Some(height));
				output_info.confirmation_hash = None;
				output_info.confirmation_height = None;
				self.persist_info(&output_info).unwrap_or_else(|e| {
					log_error!(self.logger, "Error persisting SpendableOutputInfo: {:?}", e)
				});
			}
		}
	}
}

impl<B: Deref, E: Deref, F: Deref, K: Deref, L: Deref> Confirm for OutputSweeper<B, E, F, K, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	F::Target: Filter,
	K::Target: KVStore,
	L::Target: Logger,
{
	fn transactions_confirmed(
		&self, header: &Header, txdata: &chain::transaction::TransactionData, height: u32,
	) {
		let mut locked_outputs = self.outputs.lock().unwrap();
		for (_, tx) in txdata {
			for output_info in locked_outputs.iter_mut() {
				if output_info.is_spent_in(*tx) {
					debug_assert!(Some(height) > output_info.latest_broadcast_height);
					output_info.confirmation_hash = Some(header.block_hash());
					output_info.confirmation_height = Some(height);
					output_info.latest_spending_tx = Some((*tx).clone());
					self.persist_info(&output_info).unwrap_or_else(|e| {
						log_error!(self.logger, "Error persisting SpendableOutputInfo: {:?}", e)
					});
				}
			}
		}
	}

	fn transaction_unconfirmed(&self, txid: &Txid) {
		let mut locked_outputs = self.outputs.lock().unwrap();

		// Get what height was unconfirmed.
		let unconf_height = locked_outputs
			.iter()
			.find(|o| o.latest_spending_tx.as_ref().map(|tx| tx.txid()) == Some(*txid))
			.and_then(|o| o.confirmation_height);

		// Unconfirm all >= this height.
		locked_outputs.iter_mut().filter(|o| o.confirmation_height >= unconf_height).for_each(
			|o| {
				o.confirmation_hash = None;
				o.confirmation_height = None;
				self.persist_info(&o).unwrap_or_else(|e| {
					log_error!(self.logger, "Error persisting SpendableOutputInfo: {:?}", e)
				});
			},
		);
	}

	fn best_block_updated(&self, header: &Header, height: u32) {
		*self.best_block.lock().unwrap() = BestBlock::new(header.block_hash(), height);
		self.prune_confirmed_outputs();
		self.rebroadcast_if_necessary();
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		let locked_outputs = self.outputs.lock().unwrap();
		locked_outputs
			.iter()
			.filter_map(|o| {
				if let Some(confirmation_hash) = o.confirmation_hash {
					if let Some(confirmation_height) = o.confirmation_height {
						if let Some(latest_spending_tx) = o.latest_spending_tx.as_ref() {
							return Some((
								latest_spending_tx.txid(),
								confirmation_height,
								Some(confirmation_hash),
							));
						}
					}
				}

				None
			})
			.collect::<Vec<_>>()
	}
}
