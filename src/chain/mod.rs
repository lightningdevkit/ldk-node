// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod bitcoind;
mod electrum;
mod esplora;

use crate::event::{Event, EventQueue, SyncType};
use crate::check_and_emit_balance_update;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitcoin::{Script, Txid};
use lightning::chain::{BestBlock, Filter};
use lightning_block_sync::gossip::UtxoSource;

use crate::chain::bitcoind::BitcoindChainSource;
use crate::chain::electrum::ElectrumChainSource;
use crate::chain::esplora::EsploraChainSource;
use crate::config::{
	BackgroundSyncConfig, BitcoindRestClientConfig, Config, ElectrumSyncConfig, EsploraSyncConfig,
	RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::fee_estimator::OnchainFeeEstimator;
use crate::io::utils::write_node_metrics;
use crate::logger::{log_debug, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

use bdk_wallet::event::WalletEvent as BdkWalletEvent;
use bdk_wallet::Update as BdkUpdate;

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
}

enum ChainSourceKind {
	Esplora(EsploraChainSource),
	Electrum(ElectrumChainSource),
	Bitcoind(BitcoindChainSource),
}

use crate::event::TransactionContext;
use crate::types::UserChannelId;
use lightning::ln::types::ChannelId;

/// Determine transaction context by checking against known channel funding outpoints.
///
/// This function attempts to identify whether a transaction is related to channel operations
/// by checking against the channel manager and channel monitor data.
fn determine_transaction_context(
	txid: &bitcoin::Txid,
	channel_manager: Option<&Arc<ChannelManager>>,
	chain_monitor: Option<&Arc<ChainMonitor>>,
) -> TransactionContext {
	// Check if this transaction is a known channel funding transaction
	if let Some(cm) = channel_manager {
		// Check all channels for matching funding txids
		for channel_details in cm.list_channels() {
			if let Some(funding_txo) = channel_details.funding_txo {
				if funding_txo.txid == *txid {
					// This is a channel funding transaction
					return TransactionContext::ChannelFunding {
						channel_id: channel_details.channel_id,
						user_channel_id: UserChannelId(channel_details.user_channel_id),
						counterparty_node_id: channel_details.counterparty.node_id,
					};
				}
			}
		}

		// Check recently closed channels - they might not be in the active list anymore
		// but could still have pending transactions
		for _channel_details in cm.list_recent_payments() {
			// TODO: Once we have access to closed channel data, we can check those as well
			// For now, we'll need to rely on the channel monitor
		}
	}

	// Check channel monitors for channel closure transactions
	if let Some(_monitor) = chain_monitor {
		// The chain monitor can help identify channel closures
		// TODO: Implement channel closure detection through monitor data
		// This would require iterating through monitored channels and checking
		// their closing transactions
	}

	// Default to regular wallet transaction if we can't identify it as channel-related
	// Applications can still cross-reference with ChannelPending/ChannelClosed events
	TransactionContext::RegularWallet
}

/// Process BDK wallet events and emit corresponding ldk-node events via the event queue.
fn process_wallet_events<B: Deref, E: Deref, L: Deref, L2: Deref>(
	wallet_events: Vec<BdkWalletEvent>, wallet: &crate::wallet::Wallet<B, E, L>,
	event_queue: &EventQueue<L2>, logger: &Arc<Logger>,
	channel_manager: Option<&Arc<ChannelManager>>, chain_monitor: Option<&Arc<ChainMonitor>>,
) -> Result<(), Error>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: LdkLogger,
	L2::Target: LdkLogger,
{
	for wallet_event in wallet_events {
		match wallet_event {
			BdkWalletEvent::TxConfirmed { txid, block_time, .. } => {
				log_info!(
					logger,
					"Onchain transaction {} confirmed at height {}",
					txid,
					block_time.block_id.height
				);

				// Determine the transaction context by checking channel data
				let context = determine_transaction_context(&txid, channel_manager, chain_monitor);

				let event = Event::OnchainTransactionConfirmed {
					txid,
					block_hash: block_time.block_id.hash,
					block_height: block_time.block_id.height,
					confirmation_time: block_time.confirmation_time,
					context,
				};
				event_queue.add_event(event).map_err(|e| {
					log_error!(logger, "Failed to push onchain event to queue: {}", e);
					e
				})?;
			},
			BdkWalletEvent::TxUnconfirmed { txid, old_block_time, .. } => {
				match old_block_time {
					Some(_) => {
						// Transaction was previously confirmed but is now unconfirmed (reorg)
						log_info!(logger, "Onchain transaction {} became unconfirmed (reorg)", txid);
						let event = Event::OnchainTransactionUnconfirmed { txid };
						event_queue.add_event(event).map_err(|e| {
							log_error!(logger, "Failed to push onchain event to queue: {}", e);
							e
						})?;
					},
					None => {
						// New unconfirmed transaction detected in mempool
						// Get transaction details from wallet to calculate the amount
						let amount_sats = wallet.get_tx_net_amount(&txid).unwrap_or_else(|| {
							// Shouldn't happen, but handle gracefully
							log_error!(logger, "Transaction {} not found in wallet", txid);
							0
						});

						log_info!(
							logger,
							"New unconfirmed transaction {} detected in mempool (amount: {} sats)",
							txid,
							amount_sats
						);

						let context = determine_transaction_context(&txid, channel_manager, chain_monitor);
						let event = Event::OnchainTransactionReceived { txid, amount_sats, context };
						event_queue.add_event(event).map_err(|e| {
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
			BdkWalletEvent::TxReplaced { txid, .. } => {
				log_info!(logger, "Onchain transaction {} was replaced", txid);
				// Treat a replacement as an unconfirm event
				let event = Event::OnchainTransactionUnconfirmed { txid };
				event_queue.add_event(event).map_err(|e| {
					log_error!(logger, "Failed to push onchain event to queue: {}", e);
					e
				})?;
			},
			_ => {
				// Ignore other event types
			},
		}
	}
	Ok(())
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, headers: HashMap<String, String>, sync_config: EsploraSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> (Self, Option<BestBlock>) {
		let esplora_chain_source = EsploraChainSource::new(
			server_url,
			headers,
			sync_config,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Esplora(esplora_chain_source);
		(Self { kind, tx_broadcaster, logger, onchain_wallet: Arc::new(Mutex::new(None)) }, None)
	}

	pub(crate) fn new_electrum(
		server_url: String, sync_config: ElectrumSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> (Self, Option<BestBlock>) {
		let electrum_chain_source = ElectrumChainSource::new(
			server_url,
			sync_config,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Electrum(electrum_chain_source);
		(Self { kind, tx_broadcaster, logger, onchain_wallet: Arc::new(Mutex::new(None)) }, None)
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
		(Self { kind, tx_broadcaster, logger, onchain_wallet: Arc::new(Mutex::new(None)) }, best_block)
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
		(Self { kind, tx_broadcaster, logger, onchain_wallet: Arc::new(Mutex::new(None)) }, best_block)
	}

	pub(crate) fn start(&self, runtime: Arc<Runtime>) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.start(runtime)?
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

	fn config(&self) -> Option<&Arc<Config>> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => Some(&esplora_chain_source.config),
			ChainSourceKind::Electrum(electrum_chain_source) => Some(&electrum_chain_source.config),
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => Some(&bitcoind_chain_source.config),
		}
	}

	async fn start_tx_based_sync_loop<L: Deref>(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>, background_sync_config: &BackgroundSyncConfig,
		logger: Arc<Logger>, event_queue: Arc<EventQueue<L>>,
	) where
		L::Target: LdkLogger,
	{
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
				_ = onchain_wallet_sync_interval.tick() => {
					let _ = self.sync_onchain_wallet(
						Some(&*event_queue),
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

	/// Set the onchain wallet for event processing.
	pub(crate) fn set_onchain_wallet(&self, wallet: Arc<Wallet>) {
		*self.onchain_wallet.lock().unwrap() = Some(wallet);
	}

	// Synchronize the onchain wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_onchain_wallet<L: Deref>(
		&self, event_queue: Option<&EventQueue<L>>, channel_manager: Option<&Arc<ChannelManager>>,
		chain_monitor: Option<&Arc<ChainMonitor>>, config: Option<&Arc<Config>>,
	) -> Result<(), Error>
	where
		L::Target: LdkLogger,
	{
		let wallet = self.onchain_wallet.lock().unwrap().clone();
		let wallet = wallet.ok_or(Error::WalletOperationFailed)?;

		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				let wallet_events = esplora_chain_source.sync_onchain_wallet(Arc::clone(&wallet)).await?;
				
				// Process wallet events if event queue is provided
				if let Some(event_queue) = event_queue {
					process_wallet_events(
						wallet_events,
						&wallet,
						event_queue,
						&self.logger,
						channel_manager,
						chain_monitor,
					)?;

					// Emit SyncCompleted event
					let synced_height = wallet.current_best_block().height;
					event_queue.add_event(Event::SyncCompleted {
						sync_type: SyncType::OnchainWallet,
						synced_block_height: synced_height,
					})?;

					// Check for balance changes and emit BalanceChanged event if needed
					if let (Some(cm), Some(chain_mon), Some(cfg)) = (channel_manager, chain_monitor, config) {
						let cur_anchor_reserve_sats = crate::total_anchor_channels_reserve_sats(cm, cfg);
						let (total_onchain_balance_sats, spendable_onchain_balance_sats) =
							wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

						let mut total_lightning_balance_sats = 0;
						for (funding_txo, _channel_id) in chain_mon.list_monitors() {
							if let Ok(monitor) = chain_mon.get_monitor(funding_txo) {
								for ldk_balance in monitor.get_claimable_balances() {
									total_lightning_balance_sats += ldk_balance.claimable_amount_satoshis();
								}
							}
						}

						let balance_details = crate::BalanceDetails {
							total_onchain_balance_sats,
							spendable_onchain_balance_sats,
							total_anchor_channels_reserve_sats: std::cmp::min(
								cur_anchor_reserve_sats,
								total_onchain_balance_sats
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
						)?;
					}
				}
				Ok(())
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				let wallet_events = electrum_chain_source.sync_onchain_wallet(Arc::clone(&wallet)).await?;
				
				// Process wallet events if event queue is provided
				if let Some(event_queue) = event_queue {
					process_wallet_events(
						wallet_events,
						&wallet,
						event_queue,
						&self.logger,
						channel_manager,
						chain_monitor,
					)?;

					// Emit SyncCompleted event
					let synced_height = wallet.current_best_block().height;
					event_queue.add_event(Event::SyncCompleted {
						sync_type: SyncType::OnchainWallet,
						synced_block_height: synced_height,
					})?;

					// Check for balance changes and emit BalanceChanged event if needed
					if let (Some(cm), Some(chain_mon), Some(cfg)) = (channel_manager, chain_monitor, config) {
						let cur_anchor_reserve_sats = crate::total_anchor_channels_reserve_sats(cm, cfg);
						let (total_onchain_balance_sats, spendable_onchain_balance_sats) =
							wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

						let mut total_lightning_balance_sats = 0;
						for (funding_txo, _channel_id) in chain_mon.list_monitors() {
							if let Ok(monitor) = chain_mon.get_monitor(funding_txo) {
								for ldk_balance in monitor.get_claimable_balances() {
									total_lightning_balance_sats += ldk_balance.claimable_amount_satoshis();
								}
							}
						}

						let balance_details = crate::BalanceDetails {
							total_onchain_balance_sats,
							spendable_onchain_balance_sats,
							total_anchor_channels_reserve_sats: std::cmp::min(
								cur_anchor_reserve_sats,
								total_onchain_balance_sats
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
						)?;
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

