// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

pub(crate) mod bitcoind;
mod electrum;
mod esplora;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bitcoin::{Script, Txid};
use lightning::chain::{BestBlock, Filter};

use crate::chain::bitcoind::{BitcoindChainSource, UtxoSourceClient};
use crate::chain::electrum::ElectrumChainSource;
use crate::chain::esplora::EsploraChainSource;
use crate::config::{
	BackgroundSyncConfig, BitcoindRestClientConfig, Config, ElectrumSyncConfig, EsploraSyncConfig,
	WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::fee_estimator::OnchainFeeEstimator;
use crate::logger::{log_debug, log_info, log_trace, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

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
	registered_txids: Mutex<Vec<Txid>>,
	tx_broadcaster: Arc<Broadcaster>,
	logger: Arc<Logger>,
}

enum ChainSourceKind {
	Esplora(EsploraChainSource),
	Electrum(ElectrumChainSource),
	Bitcoind(BitcoindChainSource),
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
		let registered_txids = Mutex::new(Vec::new());
		(Self { kind, registered_txids, tx_broadcaster, logger }, None)
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
		let registered_txids = Mutex::new(Vec::new());
		(Self { kind, registered_txids, tx_broadcaster, logger }, None)
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
		let registered_txids = Mutex::new(Vec::new());
		(Self { kind, registered_txids, tx_broadcaster, logger }, best_block)
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
		let registered_txids = Mutex::new(Vec::new());
		(Self { kind, registered_txids, tx_broadcaster, logger }, best_block)
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

	pub(crate) fn as_utxo_source(&self) -> Option<UtxoSourceClient> {
		match &self.kind {
			ChainSourceKind::Bitcoind(bitcoind_chain_source) => {
				Some(bitcoind_chain_source.as_utxo_source())
			},
			_ => None,
		}
	}

	pub(crate) fn registered_txids(&self) -> Vec<Txid> {
		self.registered_txids.lock().unwrap().clone()
	}

	pub(crate) fn is_transaction_based(&self) -> bool {
		match &self.kind {
			ChainSourceKind::Esplora(_) => true,
			ChainSourceKind::Electrum { .. } => true,
			ChainSourceKind::Bitcoind { .. } => false,
		}
	}

	pub(crate) async fn continuously_sync_wallets(
		&self, stop_sync_receiver: tokio::sync::watch::Receiver<()>, onchain_wallet: Arc<Wallet>,
		channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				if let Some(background_sync_config) =
					esplora_chain_source.sync_config.background_sync_config.as_ref()
				{
					self.start_tx_based_sync_loop(
						stop_sync_receiver,
						onchain_wallet,
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
					self.start_tx_based_sync_loop(
						stop_sync_receiver,
						onchain_wallet,
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

	async fn start_tx_based_sync_loop(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
		background_sync_config: &BackgroundSyncConfig, logger: Arc<Logger>,
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
				_ = onchain_wallet_sync_interval.tick() => {
					let _ = self.sync_onchain_wallet(Arc::clone(&onchain_wallet)).await;
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

	// Synchronize the onchain wallet via transaction-based protocols (i.e., Esplora, Electrum,
	// etc.)
	pub(crate) async fn sync_onchain_wallet(
		&self, onchain_wallet: Arc<Wallet>,
	) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.sync_onchain_wallet(onchain_wallet).await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.sync_onchain_wallet(onchain_wallet).await
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
		self.registered_txids.lock().unwrap().push(*txid);
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
