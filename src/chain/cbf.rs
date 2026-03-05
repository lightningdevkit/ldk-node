// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::{Arc, RwLock};

use bitcoin::{Script, Transaction, Txid};
use lightning::chain::WatchedOutput;

use crate::config::{CbfSyncConfig, Config};
use crate::fee_estimator::OnchainFeeEstimator;
use crate::logger::{log_error, LdkLogger, Logger};
use crate::runtime::Runtime;
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

pub(super) struct CbfChainSource {
	/// Peer addresses for sourcing compact block filters via P2P.
	peers: Vec<String>,
	/// User-provided sync configuration (timeouts, background sync intervals).
	pub(super) sync_config: CbfSyncConfig,
	/// Shared fee rate estimator, updated by this chain source.
	fee_estimator: Arc<OnchainFeeEstimator>,
	/// Persistent key-value store for node metrics.
	kv_store: Arc<DynStore>,
	/// Node configuration (network, storage path, etc.).
	config: Arc<Config>,
	/// Logger instance.
	logger: Arc<Logger>,
	/// Shared node metrics (sync timestamps, etc.).
	node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl CbfChainSource {
	pub(crate) fn new(
		peers: Vec<String>, sync_config: CbfSyncConfig, fee_estimator: Arc<OnchainFeeEstimator>,
		kv_store: Arc<DynStore>, config: Arc<Config>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		Self { peers, sync_config, fee_estimator, kv_store, config, logger, node_metrics }
	}

	/// Start the bip157 node and spawn background tasks for event processing.
	pub(crate) fn start(&self, _runtime: Arc<Runtime>) {
		log_error!(self.logger, "CBF chain source start is not yet implemented.");
	}

	/// Shut down the bip157 node and stop all background tasks.
	pub(crate) fn stop(&self) {
		log_error!(self.logger, "CBF chain source stop is not yet implemented.");
	}

	/// Sync the on-chain wallet by scanning compact block filters for relevant transactions.
	pub(crate) async fn sync_onchain_wallet(
		&self, _onchain_wallet: Arc<Wallet>,
	) -> Result<(), Error> {
		log_error!(self.logger, "On-chain wallet sync via CBF is not yet implemented.");
		Err(Error::WalletOperationFailed)
	}

	/// Sync the Lightning wallet by confirming channel transactions via compact block filters.
	pub(crate) async fn sync_lightning_wallet(
		&self, _channel_manager: Arc<ChannelManager>, _chain_monitor: Arc<ChainMonitor>,
		_output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		log_error!(self.logger, "Lightning wallet sync via CBF is not yet implemented.");
		Err(Error::TxSyncFailed)
	}

	/// Estimate fee rates from recent block data.
	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		log_error!(self.logger, "Fee rate estimation via CBF is not yet implemented.");
		Err(Error::FeerateEstimationUpdateFailed)
	}

	/// Broadcast a package of transactions via the P2P network.
	pub(crate) async fn process_broadcast_package(&self, _package: Vec<Transaction>) {
		log_error!(self.logger, "Transaction broadcasting via CBF is not yet implemented.");
	}

	/// Register a transaction script for Lightning channel monitoring.
	pub(crate) fn register_tx(&self, _txid: &Txid, _script_pubkey: &Script) {
		log_error!(self.logger, "CBF register_tx is not yet implemented.");
	}

	/// Register a watched output script for Lightning channel monitoring.
	pub(crate) fn register_output(&self, _output: WatchedOutput) {
		log_error!(self.logger, "CBF register_output is not yet implemented.");
	}
}
