// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod bitcoind;
mod electrum;
mod esplora;

use crate::chain::bitcoind::BitcoindChainSource;
use crate::chain::electrum::ElectrumChainSource;
use crate::chain::esplora::EsploraChainSource;
use crate::config::{
	BackgroundSyncConfig, BitcoindRestClientConfig, Config, ElectrumSyncConfig, EsploraSyncConfig,
	RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use crate::fee_estimator::OnchainFeeEstimator;
use crate::io::utils::write_node_metrics;
use crate::logger::{log_info, log_trace, LdkLogger, Logger};
use crate::types::{Broadcaster, ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

use lightning::chain::Filter;

use lightning_block_sync::gossip::UtxoSource;

use bitcoin::{Script, Txid};

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

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
}

enum ChainSourceKind {
	Esplora(EsploraChainSource),
	Electrum(ElectrumChainSource),
	Bitcoind(BitcoindChainSource),
}

impl ChainSource {
	pub(crate) fn new_esplora(
		server_url: String, headers: HashMap<String, String>, sync_config: EsploraSyncConfig,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let esplora_chain_source = EsploraChainSource::new(
			server_url,
			headers,
			sync_config,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Esplora(esplora_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn new_electrum(
		server_url: String, sync_config: ElectrumSyncConfig, onchain_wallet: Arc<Wallet>,
		fee_estimator: Arc<OnchainFeeEstimator>, tx_broadcaster: Arc<Broadcaster>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let electrum_chain_source = ElectrumChainSource::new(
			server_url,
			sync_config,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Electrum(electrum_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn new_bitcoind_rpc(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let bitcoind_chain_source = BitcoindChainSource::new_rpc(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Bitcoind(bitcoind_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn new_bitcoind_rest(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		onchain_wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		tx_broadcaster: Arc<Broadcaster>, kv_store: Arc<DynStore>, config: Arc<Config>,
		rest_client_config: BitcoindRestClientConfig, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let bitcoind_chain_source = BitcoindChainSource::new_rest(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			onchain_wallet,
			fee_estimator,
			kv_store,
			config,
			rest_client_config,
			Arc::clone(&logger),
			node_metrics,
		);
		let kind = ChainSourceKind::Bitcoind(bitcoind_chain_source);
		Self { kind, tx_broadcaster, logger }
	}

	pub(crate) fn start(&self, runtime: Arc<tokio::runtime::Runtime>) -> Result<(), Error> {
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

	pub(crate) fn is_transaction_based(&self) -> bool {
		match &self.kind {
			ChainSourceKind::Esplora(_) => true,
			ChainSourceKind::Electrum { .. } => true,
			ChainSourceKind::Bitcoind { .. } => false,
		}
	}

	pub(crate) async fn continuously_sync_wallets(
		&self, stop_sync_receiver: tokio::sync::watch::Receiver<()>,
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
				_ = onchain_wallet_sync_interval.tick() => {
					let _ = self.sync_onchain_wallet().await;
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
	pub(crate) async fn sync_onchain_wallet(&self) -> Result<(), Error> {
		match &self.kind {
			ChainSourceKind::Esplora(esplora_chain_source) => {
				esplora_chain_source.sync_onchain_wallet().await
			},
			ChainSourceKind::Electrum(electrum_chain_source) => {
				electrum_chain_source.sync_onchain_wallet().await
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
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
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
					.poll_and_update_listeners(channel_manager, chain_monitor, output_sweeper)
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

	pub(crate) async fn process_broadcast_queue(&self) {
		let mut receiver = self.tx_broadcaster.get_broadcast_queue().await;
		while let Some(next_package) = receiver.recv().await {
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
