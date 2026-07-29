// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod bitcoind;
mod electrum;
mod esplora;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_wallet::event::WalletEvent as BdkWalletEvent;
use bdk_wallet::{KeychainKind, Update as BdkUpdate};
use bitcoin::{Script, Txid};
use lightning::chain::{BestBlock, Filter};
use lightning::log_warn;
use lightning_block_sync::gossip::UtxoSource;

use crate::chain::bitcoind::BitcoindChainSource;
use crate::chain::electrum::ElectrumChainSource;
use crate::chain::esplora::EsploraChainSource;
use crate::config::{
	AddressTypeRuntimeConfig, BackgroundSyncConfig, BitcoindRestClientConfig, Config,
	ElectrumSyncConfig, EsploraSyncConfig, OnchainWalletAccount,
	RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::event::{Event, EventQueue, SyncType, TransactionDetails};
use crate::fee_estimator::OnchainFeeEstimator;
use crate::io::utils::write_node_metrics;
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{check_and_emit_balance_update, Error, NodeMetrics};

pub(super) enum WalletSyncRequest {
	FullScan(FullScanRequest<KeychainKind>),
	Incremental(SyncRequest<(KeychainKind, u32)>),
}

pub(crate) enum WalletSyncStatus {
	Completed,
	InProgress { subscribers: tokio::sync::broadcast::Sender<Result<(), Error>> },
}

impl WalletSyncStatus {
	fn register_or_subscribe_pending_sync(
		&mut self,
	) -> Option<tokio::sync::broadcast::Receiver<Result<(), Error>>> {
		match self {
			WalletSyncStatus::Completed => {
				// We're first to register for a sync.
				let (tx, _) = tokio::sync::broadcast::channel(1);
				*self = WalletSyncStatus::InProgress { subscribers: tx };
				None
			},
			WalletSyncStatus::InProgress { subscribers } => {
				// A sync is in-progress, we subscribe.
				let rx = subscribers.subscribe();
				Some(rx)
			},
		}
	}

	fn propagate_result_to_subscribers(&mut self, res: Result<(), Error>) {
		// Send the notification to any other tasks that might be waiting on it by now.
		{
			match self {
				WalletSyncStatus::Completed => {
					// No sync in-progress, do nothing.
					return;
				},
				WalletSyncStatus::InProgress { subscribers } => {
					// A sync is in-progress, we notify subscribers.
					if subscribers.receiver_count() > 0 {
						match subscribers.send(res) {
							Ok(_) => (),
							Err(e) => {
								debug_assert!(
									false,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
							},
						}
					}
					*self = WalletSyncStatus::Completed;
				},
			}
		}
	}
}

pub(crate) struct ChainSource {
	kind: ChainSourceKind,
	tx_broadcaster: Arc<Broadcaster>,
	logger: Arc<Logger>,
	onchain_wallet: Arc<Mutex<Option<Arc<Wallet>>>>,
	event_queue: Arc<Mutex<Option<Arc<EventQueue<Arc<Logger>>>>>>,
	sync_config_sender: Option<tokio::sync::watch::Sender<BackgroundSyncConfig>>,
}

enum ChainSourceKind {
	Esplora(EsploraChainSource),
	Electrum(ElectrumChainSource),
	Bitcoind(BitcoindChainSource),
}

// Check for evicted transactions by comparing unconfirmed txids before and after sync.
// Returns a list of txids that were unconfirmed before but are no longer unconfirmed
// and are not confirmed in the wallet.
fn check_evicted_transactions(
	prev_unconfirmed_txids: Vec<Txid>, wallet: &crate::wallet::Wallet, logger: &Logger,
) -> Vec<Txid> {
	let current_unconfirmed_txids: std::collections::HashSet<Txid> =
		wallet.get_unconfirmed_txids().into_iter().collect();
	let transaction_confirmations = wallet.transaction_confirmations();

	let mut evicted_txids = Vec::new();
	for txid in prev_unconfirmed_txids {
		// If transaction is still unconfirmed, skip it
		if current_unconfirmed_txids.contains(&txid) {
			continue;
		}

		// Check if transaction is confirmed in wallet
		// If it's confirmed, it wasn't evicted - it was included in a block
		if transaction_confirmations.contains_key(&txid) {
			continue;
		}

		// Transaction is not unconfirmed and not confirmed in wallet
		// This means it was evicted from the mempool
		// (We don't need to check via chain source since the wallet state after sync
		//  should be up-to-date - if it were confirmed, it would be in the wallet)
		log_info!(logger, "Transaction {} was evicted from the mempool", txid);
		evicted_txids.push(txid);
	}

	evicted_txids
}

// Check for evicted transactions and emit events for them.
async fn check_and_emit_evicted_transactions<L2: Deref>(
	prev_unconfirmed_txids: Vec<Txid>, wallet: &crate::wallet::Wallet,
	event_queue: &EventQueue<L2>, logger: &Logger,
) where
	L2::Target: LdkLogger,
{
	let evicted_txids = check_evicted_transactions(prev_unconfirmed_txids, wallet, logger);

	for txid in evicted_txids {
		if let Err(e) = event_queue.add_event(Event::OnchainTransactionEvicted { txid }).await {
			log_error!(logger, "Failed to push evicted transaction event to queue: {}", e);
		}
	}
}

// Get transaction details including inputs and outputs.
fn get_transaction_details(
	txid: &bitcoin::Txid, wallet: &crate::wallet::Wallet,
	_channel_manager: Option<&Arc<ChannelManager>>,
) -> Option<TransactionDetails> {
	// Get transaction details from wallet
	let (amount_sats, inputs, outputs) = wallet.get_tx_details(txid)?;

	Some(TransactionDetails { amount_sats, inputs, outputs })
}

pub(super) fn collect_additional_sync_requests(
	additional_accounts: &[OnchainWalletAccount], onchain_wallet: &Wallet,
	node_metrics: &Arc<RwLock<NodeMetrics>>, logger: &Arc<Logger>,
) -> Result<Vec<(OnchainWalletAccount, WalletSyncRequest)>, Error> {
	additional_accounts
		.iter()
		.copied()
		.map(|wallet_account| {
			let do_incremental =
				should_use_incremental_sync(wallet_account, onchain_wallet, node_metrics);
			let request = if do_incremental {
				onchain_wallet
					.get_wallet_incremental_sync_request(wallet_account)
					.map(WalletSyncRequest::Incremental)
			} else {
				onchain_wallet
					.get_wallet_full_scan_request(wallet_account)
					.map(WalletSyncRequest::FullScan)
			};
			request.map(|request| (wallet_account, request)).map_err(|e| {
				log_warn!(logger, "Failed to build sync request for {:?}: {}", wallet_account, e);
				Error::WalletOperationFailed
			})
		})
		.collect()
}

fn should_use_incremental_sync(
	wallet_account: OnchainWalletAccount, onchain_wallet: &Wallet,
	node_metrics: &Arc<RwLock<NodeMetrics>>,
) -> bool {
	if wallet_account.account_index == 0 {
		node_metrics.read().unwrap().get_wallet_sync_timestamp(wallet_account).is_some()
	} else {
		onchain_wallet.has_synced_derived_account(wallet_account)
	}
}

pub(super) struct AdditionalSyncOutcome {
	pub(super) events: Vec<BdkWalletEvent>,
	pub(super) any_applied: bool,
	pub(super) error: Option<Error>,
}

pub(super) struct WalletSyncOutcome {
	pub(super) events: Vec<BdkWalletEvent>,
	pub(super) error: Option<Error>,
}

impl WalletSyncOutcome {
	pub(super) fn new(events: Vec<BdkWalletEvent>, error: Option<Error>) -> Self {
		Self { events, error }
	}

	pub(super) fn failed(error: Error) -> Self {
		Self { events: Vec::new(), error: Some(error) }
	}

	pub(super) fn result(&self) -> Result<(), Error> {
		self.error.map_or(Ok(()), Err)
	}
}

pub(super) struct AppliedWalletSyncOutcome {
	pub(super) events: Vec<BdkWalletEvent>,
	pub(super) any_applied: bool,
	pub(super) primary_applied: bool,
	pub(super) error: Option<Error>,
}

pub(super) fn apply_additional_sync_results(
	results: Vec<(OnchainWalletAccount, Result<BdkUpdate, Error>)>, onchain_wallet: &Wallet,
	node_metrics: &Arc<RwLock<NodeMetrics>>, logger: &Arc<Logger>,
) -> AdditionalSyncOutcome {
	let mut events = Vec::new();
	let mut any_applied = false;
	let mut error = None;
	for (wallet_account, result) in results {
		let update = match result {
			Ok(update) => update,
			Err(e) => {
				log_warn!(logger, "Failed to sync wallet {:?}: {}", wallet_account, e);
				error.get_or_insert(e);
				continue;
			},
		};

		match onchain_wallet.apply_update_for_wallet_account(wallet_account, update) {
			Ok(Some(wallet_events)) => {
				any_applied = true;
				if let Some(ts) =
					SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
				{
					node_metrics.write().unwrap().set_wallet_sync_timestamp(wallet_account, ts);
				}
				if wallet_account.account_index != 0 {
					onchain_wallet.mark_derived_account_synced(wallet_account);
				}
				events.extend(wallet_events);
			},
			Ok(None) => {
				log_debug!(logger, "Ignoring sync result for unloaded wallet {:?}", wallet_account);
			},
			Err(e) => {
				log_warn!(logger, "Failed to apply update to wallet {:?}: {}", wallet_account, e);
				error.get_or_insert(e);
			},
		}
	}

	AdditionalSyncOutcome { events, any_applied, error }
}

pub(super) fn apply_wallet_sync_results(
	primary_update: Option<BdkUpdate>, primary_error: Option<Error>, task_error: Option<Error>,
	additional_results: Vec<(OnchainWalletAccount, Result<BdkUpdate, Error>)>,
	onchain_wallet: &Wallet, node_metrics: &Arc<RwLock<NodeMetrics>>, logger: &Arc<Logger>,
) -> AppliedWalletSyncOutcome {
	let mut events = Vec::new();
	let mut primary_applied = false;
	let mut error = primary_error;

	if let Some(update) = primary_update {
		match onchain_wallet.apply_update(update) {
			Ok(wallet_events) => {
				primary_applied = true;
				node_metrics.write().unwrap().latest_onchain_wallet_sync_timestamp =
					SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
				events.extend(wallet_events);
			},
			Err(e) => {
				log_warn!(logger, "Failed to apply primary wallet update: {}", e);
				error.get_or_insert(e);
			},
		}
	}

	error = error.or(task_error);
	let additional_outcome =
		apply_additional_sync_results(additional_results, onchain_wallet, node_metrics, logger);
	events.extend(additional_outcome.events);
	error = error.or(additional_outcome.error);

	AppliedWalletSyncOutcome {
		events,
		any_applied: primary_applied || additional_outcome.any_applied,
		primary_applied,
		error,
	}
}

#[cfg(test)]
mod sync_tests {
	use std::sync::Arc;

	use bdk_chain::{BlockId, CheckPoint};
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::{hashes::Hash as _, Network};

	use super::*;
	use crate::builder::NodeBuilder;
	use crate::config::{AddressType, Config};
	use crate::io::test_utils::InMemoryStore;
	use crate::types::DynStore;

	#[test]
	fn successful_primary_update_is_applied_when_a_secondary_fetch_fails() {
		let mut config = Config::default();
		config.network = Network::Regtest;
		let mut builder = NodeBuilder::from_config(config);
		builder.set_chain_source_esplora("http://127.0.0.1:1".to_string(), None);
		builder.set_entropy_seed_bytes([42u8; 64]);
		builder.set_log_facade_logger();
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = builder.build_with_store(store).unwrap();
		let secondary =
			OnchainWalletAccount { address_type: AddressType::NativeSegwit, account_index: 1 };
		let genesis = genesis_block(Network::Regtest);
		let next_block = BlockId { height: 1, hash: bitcoin::BlockHash::from_byte_array([1; 32]) };
		let checkpoint = CheckPoint::new(BlockId { height: 0, hash: genesis.block_hash() })
			.push(next_block)
			.unwrap();
		let primary_update = BdkUpdate { chain: Some(checkpoint), ..Default::default() };

		let outcome = apply_wallet_sync_results(
			Some(primary_update),
			None,
			None,
			vec![(secondary, Err(Error::WalletOperationTimeout))],
			&node.wallet,
			&node.node_metrics,
			&node.logger,
		);

		assert!(outcome.primary_applied);
		assert!(outcome.any_applied);
		assert!(matches!(outcome.events.as_slice(), [BdkWalletEvent::ChainTipChanged { .. }]));
		assert_eq!(outcome.error, Some(Error::WalletOperationTimeout));
		assert_eq!(node.wallet.current_best_block().height, 1);
		assert!(node.node_metrics.read().unwrap().latest_onchain_wallet_sync_timestamp.is_some());
	}

	#[test]
	fn wallet_sync_outcome_preserves_events_on_failure() {
		let block = bitcoin::BlockHash::all_zeros();
		let event = BdkWalletEvent::ChainTipChanged {
			old_tip: bdk_chain::BlockId { height: 0, hash: block },
			new_tip: bdk_chain::BlockId { height: 1, hash: block },
		};
		let outcome = WalletSyncOutcome::new(vec![event], Some(Error::WalletOperationTimeout));

		assert_eq!(outcome.events.len(), 1);
		assert_eq!(outcome.result(), Err(Error::WalletOperationTimeout));
	}

	#[test]
	fn late_sync_result_for_removed_account_is_ignored() {
		let mut config = Config::default();
		config.network = Network::Regtest;
		let mut builder = NodeBuilder::from_config(config);
		builder.set_chain_source_esplora("http://127.0.0.1:1".to_string(), None);
		builder.set_entropy_seed_bytes([42u8; 64]);
		builder.set_log_facade_logger();
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = builder.build_with_store(store).unwrap();
		let account =
			OnchainWalletAccount { address_type: AddressType::NativeSegwit, account_index: 1 };
		let xpub = node
			.export_onchain_wallet_account_xpub(account.address_type, account.account_index)
			.unwrap();
		node.add_onchain_wallet_account(account.address_type, account.account_index, xpub).unwrap();
		node.remove_onchain_wallet_account(account.address_type, account.account_index).unwrap();

		let outcome = apply_additional_sync_results(
			vec![(account, Ok(BdkUpdate::default()))],
			&node.wallet,
			&node.node_metrics,
			&node.logger,
		);

		assert!(!outcome.any_applied);
		assert!(outcome.events.is_empty());
		assert_eq!(outcome.error, None);
		assert_eq!(
			node.remove_onchain_wallet_account(account.address_type, account.account_index),
			Err(Error::OnchainWalletAccountNotRegistered)
		);
	}
}

// Process BDK wallet events and emit corresponding ldk-node events via the event queue.
// When a transaction touches multiple wallet accounts, each wallet emits its own
// BdkWalletEvent, so we deduplicate by txid before forwarding to the event queue.
async fn process_wallet_events<L2: Deref>(
	wallet_events: Vec<BdkWalletEvent>, wallet: &crate::wallet::Wallet,
	event_queue: &EventQueue<L2>, logger: &Arc<Logger>,
	channel_manager: Option<&Arc<ChannelManager>>, _chain_monitor: Option<&Arc<ChainMonitor>>,
) -> Result<(), Error>
where
	L2::Target: LdkLogger,
{
	// Use per-type sets so that two wallets with different prior state can each contribute
	// their event type for the same txid without suppressing the other.
	let mut seen_received_txids = std::collections::HashSet::new();
	let mut seen_confirmed_txids = std::collections::HashSet::new();
	let mut seen_reorged_txids = std::collections::HashSet::new();
	let mut seen_replaced_txids = std::collections::HashSet::new();
	let transaction_confirmations = wallet.transaction_confirmations();

	for wallet_event in wallet_events {
		match wallet_event {
			BdkWalletEvent::TxConfirmed { txid, .. } => {
				let Some(block_time) = transaction_confirmations.get(&txid).copied() else {
					log_trace!(
						logger,
						"Deferring confirmation event for {} until tracked wallets agree",
						txid
					);
					continue;
				};
				if !seen_confirmed_txids.insert(txid) {
					continue;
				}
				let details = get_transaction_details(&txid, wallet, channel_manager)
					.unwrap_or_else(|| {
						log_error!(logger, "Transaction {} not found in wallet", txid);
						TransactionDetails {
							amount_sats: 0,
							inputs: Vec::new(),
							outputs: Vec::new(),
						}
					});

				log_info!(
					logger,
					"Onchain transaction {} confirmed at height {}",
					txid,
					block_time.block_id.height
				);

				let event = Event::OnchainTransactionConfirmed {
					txid,
					block_hash: block_time.block_id.hash,
					block_height: block_time.block_id.height,
					confirmation_time: block_time.confirmation_time,
					details,
				};
				event_queue.add_event(event).await.map_err(|e| {
					log_error!(logger, "Failed to push onchain event to queue: {}", e);
					e
				})?;
			},
			BdkWalletEvent::TxUnconfirmed { txid, old_block_time, .. } => {
				match old_block_time {
					Some(_) => {
						if !seen_reorged_txids.insert(txid) {
							continue;
						}
						// Transaction was previously confirmed but is now unconfirmed (reorg)
						log_info!(
							logger,
							"Onchain transaction {} became unconfirmed (reorg)",
							txid
						);
						let event = Event::OnchainTransactionReorged { txid };
						event_queue.add_event(event).await.map_err(|e| {
							log_error!(logger, "Failed to push onchain event to queue: {}", e);
							e
						})?;
					},
					None => {
						if !seen_received_txids.insert(txid) {
							continue;
						}
						// New unconfirmed transaction detected in mempool
						let details = get_transaction_details(&txid, wallet, channel_manager)
							.unwrap_or_else(|| {
								log_error!(logger, "Transaction {} not found in wallet", txid);
								TransactionDetails {
									amount_sats: 0,
									inputs: Vec::new(),
									outputs: Vec::new(),
								}
							});

						log_info!(
							logger,
							"New unconfirmed transaction {} detected in mempool (amount: {} sats)",
							txid,
							details.amount_sats
						);

						let event = Event::OnchainTransactionReceived { txid, details };
						event_queue.add_event(event).await.map_err(|e| {
							log_error!(logger, "Failed to push onchain event to queue: {}", e);
							e
						})?;
					},
				}
			},
			BdkWalletEvent::ChainTipChanged { old_tip, new_tip } => {
				log_trace!(
					logger,
					"Chain tip changed from block {} at height {} to block {} at height {}",
					old_tip.hash,
					old_tip.height,
					new_tip.hash,
					new_tip.height
				);
				// We don't emit an event for chain tip changes as this is too noisy
			},
			BdkWalletEvent::TxReplaced { txid, conflicts, .. } => {
				if !seen_replaced_txids.insert(txid) {
					continue;
				}
				let conflict_txids: Vec<Txid> =
					conflicts.iter().map(|(_, conflict_txid)| *conflict_txid).collect();
				log_info!(
					logger,
					"Onchain transaction {} was replaced by {} transaction(s)",
					txid,
					conflict_txids.len()
				);
				let event = Event::OnchainTransactionReplaced { txid, conflicts: conflict_txids };
				event_queue.add_event(event).await.map_err(|e| {
					log_error!(logger, "Failed to push onchain event to queue: {}", e);
					e
				})?;
			},
			_ => {
				// TxDropped is handled via check_and_emit_evicted_transactions; skip here.
			},
		}
	}
	Ok(())
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, headers: HashMap<String, String>, sync_config: EsploraSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>,
		address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> (Self, Option<BestBlock>) {
		// Create watch channel for runtime sync config updates if background sync is enabled
		let sync_config_sender = sync_config.background_sync_config.as_ref().map(|cfg| {
			let (tx, _) = tokio::sync::watch::channel(cfg.clone());
			tx
		});

		let esplora_chain_source = EsploraChainSource::new(
			server_url,
			headers,
			sync_config,
			fee_estimator,
			kv_store,
			config,
			address_type_runtime_config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Esplora(esplora_chain_source);
		(
			Self {
				kind,
				tx_broadcaster,
				logger,
				onchain_wallet: Arc::new(Mutex::new(None)),
				event_queue: Arc::new(Mutex::new(None)),
				sync_config_sender,
			},
			None,
		)
	}

	pub(crate) fn new_electrum(
		server_url: String, sync_config: ElectrumSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>,
		address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> (Self, Option<BestBlock>) {
		// Create watch channel for runtime sync config updates if background sync is enabled
		let sync_config_sender = sync_config.background_sync_config.as_ref().map(|cfg| {
			let (tx, _) = tokio::sync::watch::channel(cfg.clone());
			tx
		});

		let electrum_chain_source = ElectrumChainSource::new(
			server_url,
			sync_config,
			fee_estimator,
			kv_store,
			config,
			address_type_runtime_config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Electrum(electrum_chain_source);
		(
			Self {
				kind,
				tx_broadcaster,
				logger,
				onchain_wallet: Arc::new(Mutex::new(None)),
				event_queue: Arc::new(Mutex::new(None)),
				sync_config_sender,
			},
			None,
		)
	}

	pub(crate) async fn new_bitcoind_rpc(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> (Self, Option<BestBlock>) {
		let bitcoind_chain_source = BitcoindChainSource::new_rpc(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let best_block = bitcoind_chain_source.poll_best_block().await.ok();
		let kind = ChainSourceKind::Bitcoind(bitcoind_chain_source);
		(
			Self {
				kind,
				tx_broadcaster,
				logger,
				onchain_wallet: Arc::new(Mutex::new(None)),
				event_queue: Arc::new(Mutex::new(None)),
				sync_config_sender: None,
			},
			best_block,
		)
	}

	pub(crate) async fn new_bitcoind_rest(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, rest_client_config: BitcoindRestClientConfig,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> (Self, Option<BestBlock>) {
		let bitcoind_chain_source = BitcoindChainSource::new_rest(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			fee_estimator,
			kv_store,
			config,
			rest_client_config,
			Arc::clone(&logger),
			node_metrics,
		);
		let best_block = bitcoind_chain_source.poll_best_block().await.ok();
		let kind = ChainSourceKind::Bitcoind(bitcoind_chain_source);
		(
			Self {
				kind,
				tx_broadcaster,
				logger,
				onchain_wallet: Arc::new(Mutex::new(None)),
				event_queue: Arc::new(Mutex::new(None)),
				sync_config_sender: None,
			},
			best_block,
		)
	}

	pub(crate) fn start(&self, runtime_handle: tokio::runtime::Handle) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.start(runtime_handle)?
			},
			_ => {
				// Nothing to do for other chain sources.
			},
		}
		Ok(())
	}

	pub(crate) fn stop(&self) {
		match &self.kind {
			ChainSourceKind::Electrum(electrum_chain_source) => electrum_chain_source.stop(),
			_ => {
				// Nothing to do for other chain sources.
			},
		}
	}

	pub(crate) fn as_utxo_source(&self) -> Option<Arc<dyn UtxoSource>> {
		match &self.kind {
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				Some(bitcoind_chain_source.as_utxo_source())
			},
			_ => None,
		}
	}

	pub(crate) fn is_transaction_based(&self) -> bool {
		match &self.kind {
			ChainSourceKind::Esplora(_) => true,
			ChainSourceKind::Electrum(_) => true,
			ChainSourceKind::Bitcoind(_) => false,
		}
	}

	pub(crate) async fn continuously_sync_wallets(
		&self, stop_sync_receiver: tokio::sync::watch::Receiver<()>, onchain_wallet: Arc<Wallet>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) {
		self.set_onchain_wallet(Arc::clone(&onchain_wallet));

		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				if let Some(background_sync_config) =
					esplora_chain_source.sync_config.background_sync_config.as_ref()
				{
					// Get config receiver for runtime updates
					let config_receiver = self
						.sync_config_sender
						.as_ref()
						.expect(
							"sync_config_sender should be set when background_sync_config is Some",
						)
						.subscribe();

					self.start_tx_based_sync_loop(
						stop_sync_receiver,
						config_receiver,
						channel_manager,
						chain_monitor,
						output_sweeper,
						background_sync_config,
						Arc::clone(&self.logger),
					)
					.await
				} else {
					// Background syncing is disabled
					log_info!(
						self.logger,
						"Background syncing is disabled. Manual syncing required for onchain wallet, lightning wallet, and fee rate updates.",
					);
					return;
				}
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				if let Some(background_sync_config) =
					electrum_chain_source.sync_config.background_sync_config.as_ref()
				{
					// Get config receiver for runtime updates
					let config_receiver = self
						.sync_config_sender
						.as_ref()
						.expect(
							"sync_config_sender should be set when background_sync_config is Some",
						)
						.subscribe();

					self.start_tx_based_sync_loop(
						stop_sync_receiver,
						config_receiver,
						channel_manager,
						chain_monitor,
						output_sweeper,
						background_sync_config,
						Arc::clone(&self.logger),
					)
					.await
				} else {
					// Background syncing is disabled
					log_info!(
						self.logger,
						"Background syncing is disabled. Manual syncing required for onchain wallet, lightning wallet, and fee rate updates.",
					);
					return;
				}
			},
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				bitcoind_chain_source
					.continuously_sync_wallets(
						stop_sync_receiver,
						onchain_wallet,
						channel_manager,
						chain_monitor,
						output_sweeper,
					)
					.await
			},
		}
	}

	// Synchronize the onchain wallet via transaction-based protocols (Esplora, Electrum).
	// If event_queue is set, emits onchain events.
	pub(crate) async fn sync_onchain_wallet(
		&self, wallet: Arc<Wallet>, channel_manager: Option<Arc<ChannelManager>>,
		chain_monitor: Option<Arc<ChainMonitor>>,
	) -> Result<(), Error> {
		let event_queue = self.event_queue.lock().unwrap().clone();
		if let Some(event_queue) = event_queue {
			// Use event-emitting sync path
			self.sync_onchain_wallet_with_events(
				Some(&event_queue),
				channel_manager.as_ref(),
				chain_monitor.as_ref(),
				self.config(),
			)
			.await
		} else {
			// Simple sync without events (event_queue not set)
			match &self.kind {
				ChainSourceKind::Esplora(esplora_chain_source) => {
					esplora_chain_source.sync_onchain_wallet(wallet).await.result()
				},
				ChainSourceKind::Electrum(electrum_chain_source) => {
					electrum_chain_source.sync_onchain_wallet(wallet).await.result()
				},
				ChainSourceKind::Bitcoind(_) => {
					// Bitcoind sync is handled differently
					Ok(())
				},
			}
		}
	}

	fn config(&self) -> Option<&Arc<Config>> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => Some(&esplora_chain_source.config),
			ChainSourceKind::Electrum(electrum_chain_source) => Some(&electrum_chain_source.config),
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => Some(&bitcoind_chain_source.config),
		}
	}

	async fn start_tx_based_sync_loop(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		mut config_receiver: tokio::sync::watch::Receiver<BackgroundSyncConfig>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>, background_sync_config: &BackgroundSyncConfig,
		logger: Arc<Logger>,
	) {
		// Setup syncing intervals
		let onchain_wallet_sync_interval_secs = background_sync_config
			.onchain_wallet_sync_interval_secs
			.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		let mut onchain_wallet_sync_interval =
			tokio::time::interval(Duration::from_secs(onchain_wallet_sync_interval_secs));
		onchain_wallet_sync_interval
			.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		let fee_rate_cache_update_interval_secs = background_sync_config
			.fee_rate_cache_update_interval_secs
			.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		let mut fee_rate_update_interval =
			tokio::time::interval(Duration::from_secs(fee_rate_cache_update_interval_secs));
		// When starting up, we just blocked on updating, so skip the first tick.
		fee_rate_update_interval.reset();
		fee_rate_update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		let lightning_wallet_sync_interval_secs = background_sync_config
			.lightning_wallet_sync_interval_secs
			.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		let mut lightning_wallet_sync_interval =
			tokio::time::interval(Duration::from_secs(lightning_wallet_sync_interval_secs));
		lightning_wallet_sync_interval
			.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		// Start the syncing loop.
		loop {
			tokio::select! {
				_ = stop_sync_receiver.changed() => {
					log_trace!(
						logger,
						"Stopping background syncing on-chain wallet.",
						);
					return;
				}
				Ok(()) = config_receiver.changed() => {
					let new_config = config_receiver.borrow().clone();
					log_info!(
						logger,
						"Background sync intervals updated: onchain={}s, lightning={}s, fee_rate={}s",
						new_config.onchain_wallet_sync_interval_secs,
						new_config.lightning_wallet_sync_interval_secs,
						new_config.fee_rate_cache_update_interval_secs,
					);

					// Reset intervals with new durations (enforce minimum)
					let new_onchain_secs = new_config
						.onchain_wallet_sync_interval_secs
						.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
					onchain_wallet_sync_interval =
						tokio::time::interval(Duration::from_secs(new_onchain_secs));
					onchain_wallet_sync_interval
						.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

					let new_fee_rate_secs = new_config
						.fee_rate_cache_update_interval_secs
						.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
					fee_rate_update_interval =
						tokio::time::interval(Duration::from_secs(new_fee_rate_secs));
					fee_rate_update_interval
						.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

					let new_lightning_secs = new_config
						.lightning_wallet_sync_interval_secs
						.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
					lightning_wallet_sync_interval =
						tokio::time::interval(Duration::from_secs(new_lightning_secs));
					lightning_wallet_sync_interval
						.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
				}
				_ = onchain_wallet_sync_interval.tick() => {
					// Access event_queue from struct for event emission
					let event_queue = self.event_queue.lock().unwrap().clone();
					let _ = self.sync_onchain_wallet_with_events(
						event_queue.as_ref(),
						Some(&channel_manager),
						Some(&chain_monitor),
						self.config(),
					).await;
				}
				_ = fee_rate_update_interval.tick() => {
					let _ = self.update_fee_rate_estimates().await;
				}
				_ = lightning_wallet_sync_interval.tick() => {
					let _ = self.sync_lightning_wallet(
						Arc::clone(&channel_manager),
						Arc::clone(&chain_monitor),
						Arc::clone(&output_sweeper),
						).await;
				}
			}
		}
	}

	pub(crate) fn set_onchain_wallet(&self, wallet: Arc<Wallet>) {
		*self.onchain_wallet.lock().unwrap() = Some(wallet);
	}

	pub(crate) fn set_event_queue(&self, event_queue: Arc<EventQueue<Arc<Logger>>>) {
		*self.event_queue.lock().unwrap() = Some(event_queue);
	}

	/// Update the background sync configuration at runtime.
	///
	/// This allows changing sync intervals while the node is running.
	/// Returns an error if background syncing was disabled at build time.
	pub(crate) fn set_background_sync_config(
		&self, config: BackgroundSyncConfig,
	) -> Result<(), Error> {
		if let Some(ref sender) = self.sync_config_sender {
			// Send will only fail if there are no receivers, which shouldn't happen
			// while the sync loop is running
			let _ = sender.send(config);
			Ok(())
		} else {
			Err(Error::BackgroundSyncNotEnabled)
		}
	}

	// Synchronize the onchain wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.) with event emission support.
	async fn sync_onchain_wallet_with_events(
		&self, event_queue: Option<&Arc<EventQueue<Arc<Logger>>>>,
		channel_manager: Option<&Arc<ChannelManager>>, chain_monitor: Option<&Arc<ChainMonitor>>,
		config: Option<&Arc<Config>>,
	) -> Result<(), Error> {
		let wallet = self.onchain_wallet.lock().unwrap().clone();
		let wallet = wallet.ok_or(Error::WalletOperationFailed)?;

		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				// Track unconfirmed transactions before sync to detect evictions
				let prev_unconfirmed_txids = wallet.get_unconfirmed_txids();

				let WalletSyncOutcome { events: wallet_events, error: sync_error } =
					esplora_chain_source.sync_onchain_wallet(Arc::clone(&wallet)).await;

				// Process wallet events if event queue is provided
				if let Some(event_queue) = event_queue {
					process_wallet_events(
						wallet_events,
						&wallet,
						event_queue,
						&self.logger,
						channel_manager,
						chain_monitor,
					)
					.await?;
				}

				if let Some(error) = sync_error {
					return Err(error);
				}

				if let Some(event_queue) = event_queue {
					// Check for evicted transactions
					check_and_emit_evicted_transactions(
						prev_unconfirmed_txids,
						&wallet,
						event_queue,
						&self.logger,
					)
					.await;

					// Emit SyncCompleted event
					let synced_height = wallet.current_best_block().height;
					event_queue
						.add_event(Event::SyncCompleted {
							sync_type: SyncType::OnchainWallet,
							synced_block_height: synced_height,
						})
						.await?;
					// Check for balance changes and emit BalanceChanged event if needed
					if let (Some(cm), Some(chain_mon), Some(cfg)) =
						(channel_manager, chain_monitor, config)
					{
						let cur_anchor_reserve_sats =
							crate::total_anchor_channels_reserve_sats(cm, cfg);
						let (total_onchain_balance_sats, spendable_onchain_balance_sats) =
							wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

						let mut total_lightning_balance_sats = 0;
						for channel_id in chain_mon.list_monitors() {
							if let Ok(monitor) = chain_mon.get_monitor(channel_id) {
								for ldk_balance in monitor.get_claimable_balances() {
									total_lightning_balance_sats +=
										ldk_balance.claimable_amount_satoshis();
								}
							}
						}

						let balance_details = crate::BalanceDetails {
							total_onchain_balance_sats,
							spendable_onchain_balance_sats,
							total_anchor_channels_reserve_sats: std::cmp::min(
								cur_anchor_reserve_sats,
								total_onchain_balance_sats,
							),
							total_lightning_balance_sats,
							lightning_balances: Vec::new(),
							pending_balances_from_channel_closures: Vec::new(),
						};

						let node_metrics = match &self.kind {
							ChainSourceKind::Esplora(es) => Arc::clone(&es.node_metrics),
							ChainSourceKind::Electrum(el) => Arc::clone(&el.node_metrics),
							ChainSourceKind::Bitcoind(bd) => Arc::clone(&bd.node_metrics),
						};
						let kv_store = match &self.kind {
							ChainSourceKind::Esplora(es) => Arc::clone(&es.kv_store),
							ChainSourceKind::Electrum(el) => Arc::clone(&el.kv_store),
							ChainSourceKind::Bitcoind(bd) => Arc::clone(&bd.kv_store),
						};

						check_and_emit_balance_update(
							&node_metrics,
							&balance_details,
							event_queue,
							&kv_store,
							&self.logger,
						)
						.await?;
					}
				}
				Ok(())
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				// Track unconfirmed transactions before sync to detect evictions
				let prev_unconfirmed_txids = wallet.get_unconfirmed_txids();

				let WalletSyncOutcome { events: wallet_events, error: sync_error } =
					electrum_chain_source.sync_onchain_wallet(Arc::clone(&wallet)).await;

				// Process wallet events if event queue is provided
				if let Some(event_queue) = event_queue {
					process_wallet_events(
						wallet_events,
						&wallet,
						event_queue,
						&self.logger,
						channel_manager,
						chain_monitor,
					)
					.await?;
				}

				if let Some(error) = sync_error {
					return Err(error);
				}

				if let Some(event_queue) = event_queue {
					// Check for evicted transactions
					check_and_emit_evicted_transactions(
						prev_unconfirmed_txids,
						&wallet,
						event_queue,
						&self.logger,
					)
					.await;

					// Emit SyncCompleted event
					let synced_height = wallet.current_best_block().height;
					event_queue
						.add_event(Event::SyncCompleted {
							sync_type: SyncType::OnchainWallet,
							synced_block_height: synced_height,
						})
						.await?;

					// Check for balance changes and emit BalanceChanged event if needed
					if let (Some(cm), Some(chain_mon), Some(cfg)) =
						(channel_manager, chain_monitor, config)
					{
						let cur_anchor_reserve_sats =
							crate::total_anchor_channels_reserve_sats(cm, cfg);
						let (total_onchain_balance_sats, spendable_onchain_balance_sats) =
							wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

						let mut total_lightning_balance_sats = 0;
						for channel_id in chain_mon.list_monitors() {
							if let Ok(monitor) = chain_mon.get_monitor(channel_id) {
								for ldk_balance in monitor.get_claimable_balances() {
									total_lightning_balance_sats +=
										ldk_balance.claimable_amount_satoshis();
								}
							}
						}

						let balance_details = crate::BalanceDetails {
							total_onchain_balance_sats,
							spendable_onchain_balance_sats,
							total_anchor_channels_reserve_sats: std::cmp::min(
								cur_anchor_reserve_sats,
								total_onchain_balance_sats,
							),
							total_lightning_balance_sats,
							lightning_balances: Vec::new(),
							pending_balances_from_channel_closures: Vec::new(),
						};

						let node_metrics = match &self.kind {
							ChainSourceKind::Esplora(es) => Arc::clone(&es.node_metrics),
							ChainSourceKind::Electrum(el) => Arc::clone(&el.node_metrics),
							ChainSourceKind::Bitcoind(bd) => Arc::clone(&bd.node_metrics),
						};
						let kv_store = match &self.kind {
							ChainSourceKind::Esplora(es) => Arc::clone(&es.kv_store),
							ChainSourceKind::Electrum(el) => Arc::clone(&el.kv_store),
							ChainSourceKind::Bitcoind(bd) => Arc::clone(&bd.kv_store),
						};

						check_and_emit_balance_update(
							&node_metrics,
							&balance_details,
							event_queue,
							&kv_store,
							&self.logger,
						)
						.await?;
					}
				}
				Ok(())
			},
			ChainSourceKind::Bitcoind { .. } => {
				// In BitcoindRpc mode we sync lightning and onchain wallet in one go via
				// `ChainPoller`. So nothing to do here.
				unreachable!("Onchain wallet will be synced via chain polling")
			},
		}
	}

	// Synchronize the Lightning wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_lightning_wallet(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source
					.sync_lightning_wallet(channel_manager, chain_monitor, output_sweeper)
					.await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source
					.sync_lightning_wallet(channel_manager, chain_monitor, output_sweeper)
					.await
			},
			ChainSourceKind::Bitcoind { .. } => {
				// In BitcoindRpc mode we sync lightning and onchain wallet in one go via
				// `ChainPoller`. So nothing to do here.
				unreachable!("Lightning wallet will be synced via chain polling")
			},
		}
	}

	pub(crate) async fn poll_and_update_listeners(
		&self, onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora { .. } => {
				// In Esplora mode we sync lightning and onchain wallets via
				// `sync_onchain_wallet` and `sync_lightning_wallet`. So nothing to do here.
				unreachable!("Listeners will be synced via transction-based syncing")
			},
			ChainSourceKind::Electrum { .. } => {
				// In Electrum mode we sync lightning and onchain wallets via
				// `sync_onchain_wallet` and `sync_lightning_wallet`. So nothing to do here.
				unreachable!("Listeners will be synced via transction-based syncing")
			},
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				bitcoind_chain_source
					.poll_and_update_listeners(
						onchain_wallet,
						channel_manager,
						chain_monitor,
						output_sweeper,
					)
					.await
			},
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.update_fee_rate_estimates().await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.update_fee_rate_estimates().await
			},
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				bitcoind_chain_source.update_fee_rate_estimates().await
			},
		}
	}

	pub(crate) async fn continuously_process_broadcast_queue(
		&self, mut stop_tx_bcast_receiver: tokio::sync::watch::Receiver<()>,
	) {
		let mut receiver = self.tx_broadcaster.get_broadcast_queue().await;
		loop {
			let tx_bcast_logger = Arc::clone(&self.logger);
			tokio::select! {
				_ = stop_tx_bcast_receiver.changed() => {
					log_debug!(
						tx_bcast_logger,
						"Stopping broadcasting transactions.",
					);
					return;
				}
				Some(next_package) = receiver.recv() => {
					match &self.kind {
						ChainSourceKind::Esplora(esplora_chain_source) => {
							esplora_chain_source.process_broadcast_package(next_package).await
						},
						ChainSourceKind::Electrum(electrum_chain_source) => {
							electrum_chain_source.process_broadcast_package(next_package).await
						},
						ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
							bitcoind_chain_source.process_broadcast_package(next_package).await
						},
					}
				}
			}
		}
	}
}

impl Filter for ChainSource {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.register_tx(txid, script_pubkey)
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.register_tx(txid, script_pubkey)
			},
			ChainSourceKind::Bitcoind { .. } => (),
		}
	}
	fn register_output(&self, output: lightning::chain::WatchedOutput) {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.register_output(output)
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.register_output(output)
			},
			ChainSourceKind::Bitcoind { .. } => (),
		}
	}
}

impl ChainSource {
	pub(crate) async fn get_address_balance(&self, address: &bitcoin::Address) -> Option<u64> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.get_address_balance(address).await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.get_address_balance(address).await
			},
			ChainSourceKind::Bitcoind(_) => {
				// BitcoindRpc doesn't have a direct address balance query API
				None
			},
		}
	}
}

fn periodically_archive_fully_resolved_monitors(
	channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
	kv_store: Arc<DynStore>, logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
) -> Result<(), Error> {
	let mut locked_node_metrics = node_metrics.write().unwrap();
	let cur_height = channel_manager.current_best_block().height;
	let should_archive = locked_node_metrics
		.latest_channel_monitor_archival_height
		.as_ref()
		.map_or(true, |h| cur_height >= h + RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL);

	if should_archive {
		chain_monitor.archive_fully_resolved_channel_monitors();
		locked_node_metrics.latest_channel_monitor_archival_height = Some(cur_height);
		write_node_metrics(&*locked_node_metrics, kv_store, logger)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use bitcoin::Network;

	use super::{collect_additional_sync_requests, WalletSyncRequest};
	use crate::builder::NodeBuilder;
	use crate::config::{AddressType, Config, ElectrumSyncConfig, OnchainWalletAccount};
	use crate::io::test_utils::InMemoryStore;
	use crate::types::DynStore;

	#[test]
	fn derived_accounts_switch_to_incremental_sync_after_initial_scan() {
		let seed = [42u8; 64];
		let mut config = Config::default();
		config.network = Network::Regtest;
		let mut builder = NodeBuilder::from_config(config);
		builder.set_chain_source_esplora("http://127.0.0.1:1".to_string(), None);
		builder.set_entropy_seed_bytes(seed);
		builder.set_log_facade_logger();
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = builder.build_with_store(store).unwrap();
		let account =
			OnchainWalletAccount { address_type: AddressType::NativeSegwit, account_index: 1 };
		let xpub = node
			.export_onchain_wallet_account_xpub(account.address_type, account.account_index)
			.unwrap();
		node.add_onchain_wallet_account(account.address_type, account.account_index, xpub).unwrap();

		let requests = collect_additional_sync_requests(
			&[account],
			&node.wallet,
			&node.node_metrics,
			&node.logger,
		)
		.unwrap();
		assert!(matches!(requests[0].1, WalletSyncRequest::FullScan(_)));

		node.wallet.mark_derived_account_synced(account);
		let requests = collect_additional_sync_requests(
			&[account],
			&node.wallet,
			&node.node_metrics,
			&node.logger,
		)
		.unwrap();
		assert!(matches!(requests[0].1, WalletSyncRequest::Incremental(_)));
	}

	#[test]
	fn electrum_stop_gap_sets_derived_account_incremental_lookahead() {
		let mut config = Config::default();
		config.network = Network::Regtest;
		let sync_config = ElectrumSyncConfig {
			background_sync_config: None,
			additional_wallet_full_scan_stop_gap: 1_000,
			..ElectrumSyncConfig::default()
		};
		let mut builder = NodeBuilder::from_config(config);
		builder.set_chain_source_electrum("ssl://127.0.0.1:1".to_string(), Some(sync_config));
		builder.set_entropy_seed_bytes([42u8; 64]);
		builder.set_log_facade_logger();
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = builder.build_with_store(store).unwrap();
		let account =
			OnchainWalletAccount { address_type: AddressType::NativeSegwit, account_index: 1 };
		let xpub = node
			.export_onchain_wallet_account_xpub(account.address_type, account.account_index)
			.unwrap();
		node.add_onchain_wallet_account(account.address_type, account.account_index, xpub).unwrap();
		node.wallet.mark_derived_account_synced(account);

		let mut requests = collect_additional_sync_requests(
			&[account],
			&node.wallet,
			&node.node_metrics,
			&node.logger,
		)
		.unwrap();
		match requests.pop().unwrap().1 {
			WalletSyncRequest::Incremental(request) => {
				assert_eq!(request.progress().spks_remaining, 1_000);
			},
			WalletSyncRequest::FullScan(_) => panic!("expected incremental sync request"),
		}
	}
}
