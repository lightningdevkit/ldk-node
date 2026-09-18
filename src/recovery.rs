// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Seed-only recovery for an LDK Node whose channel state has been lost.

#[cfg(any(feature = "chain-esplora", feature = "storage-vss"))]
use std::collections::HashMap;
#[cfg(feature = "chain-bitcoind")]
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use bitcoin::secp256k1::PublicKey;
#[cfg(feature = "chain-electrum")]
use bitcoin::Amount;
use bitcoin::{Network, ScriptBuf, TxOut};
#[cfg(feature = "chain-electrum")]
use electrum_client::ElectrumApi;
use lightning::chain::transaction::OutPoint;
use lightning::events::ReplayEvent;
use lightning::impl_writeable_tlv_based;
use lightning::ln::msgs::SocketAddress;
use lightning::sign::SpendableOutputDescriptor;
use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
use lightning::util::ser::{Readable, WithoutLength, Writeable};
use lightning_background_processor::process_events_async;
#[cfg(feature = "chain-bitcoind")]
use lightning_block_sync::gossip::UtxoSource;
#[cfg(feature = "chain-bitcoind")]
use lightning_block_sync::{BlockData, BlockSource};
#[cfg(feature = "storage-vss")]
use vss_client::headers::VssHeaderProvider;

use crate::balance::BalanceDetails;
use crate::builder::{BuildError, NodeBuilder};
use crate::chain::{ChainSource, RecoveryChainSource};
#[cfg(feature = "chain-electrum")]
use crate::config::ElectrumSyncConfig;
#[cfg(feature = "chain-esplora")]
use crate::config::EsploraSyncConfig;
use crate::config::{Config, TorConfig};
use crate::entropy::NodeEntropy;
use crate::logger::{
	log_debug, log_error, log_info, log_trace, LdkLogger, LogLevel, LogWriter, Logger,
};
use crate::types::{DynStore, GossipSync, KeysManager, Sweeper};
#[cfg(feature = "uniffi")]
use crate::RecoveryBuilder;
use crate::{Error, Node};

const RECOVERY_NAMESPACE_PREFIX: &str = "ldk_node_recovery";
const RECOVERY_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const RECOVERY_RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(feature = "chain-esplora")]
const ESPLORA_SCAN_WORKERS: usize = 16;
#[cfg(feature = "chain-electrum")]
const ELECTRUM_SCAN_BATCH_SIZE: usize = 100;
#[cfg(feature = "chain-bitcoind")]
const BITCOIND_REORG_BUFFER: u32 = 6;
pub(crate) const RECOVERY_STATE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const RECOVERY_STATE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const RECOVERY_STATE_KEY: &str = "recovery_state";

#[cfg(feature = "chain-bitcoind")]
fn bitcoind_scan_start(configured_start: u32, last_scanned_height: Option<u32>) -> u32 {
	last_scanned_height
		.map(|height| height.saturating_sub(BITCOIND_REORG_BUFFER))
		.unwrap_or(configured_start)
		.max(configured_start)
}

/// Presents an isolated recovery-only namespace over an existing store.
pub(crate) struct RecoveryStore<S> {
	inner: Arc<S>,
}

impl<S> RecoveryStore<S> {
	pub(crate) fn new(inner: S) -> Self {
		Self { inner: Arc::new(inner) }
	}

	fn primary_namespace(primary_namespace: &str) -> String {
		if primary_namespace.is_empty() {
			RECOVERY_NAMESPACE_PREFIX.to_string()
		} else {
			format!("{RECOVERY_NAMESPACE_PREFIX}_{primary_namespace}")
		}
	}
}

impl<S: PaginatedKVStore + Send + Sync + 'static> KVStore for RecoveryStore<S> {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, bitcoin::io::Error>> + Send + 'static {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = Self::primary_namespace(primary_namespace);
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		async move { KVStore::read(&*inner, &primary_namespace, &secondary_namespace, &key).await }
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), bitcoin::io::Error>> + Send + 'static {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = Self::primary_namespace(primary_namespace);
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		async move { KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await }
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), bitcoin::io::Error>> + Send + 'static {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = Self::primary_namespace(primary_namespace);
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		async move {
			KVStore::remove(&*inner, &primary_namespace, &secondary_namespace, &key, lazy).await
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, bitcoin::io::Error>> + Send + 'static {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = Self::primary_namespace(primary_namespace);
		let secondary_namespace = secondary_namespace.to_string();
		async move { KVStore::list(&*inner, &primary_namespace, &secondary_namespace).await }
	}
}

impl<S: PaginatedKVStore + Send + Sync + 'static> PaginatedKVStore for RecoveryStore<S> {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, bitcoin::io::Error>> + Send + 'static {
		let inner = Arc::clone(&self.inner);
		let primary_namespace = Self::primary_namespace(primary_namespace);
		let secondary_namespace = secondary_namespace.to_string();
		async move {
			PaginatedKVStore::list_paginated(
				&*inner,
				&primary_namespace,
				&secondary_namespace,
				page_token,
			)
			.await
		}
	}
}

/// A peer which may hold surviving channel state for the recovering node.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RecoveryPeer {
	/// The peer's Lightning node ID.
	pub node_id: PublicKey,
	/// The network address at which the peer can be reached.
	pub address: SocketAddress,
}

impl_writeable_tlv_based!(RecoveryPeer, {
	(0, node_id, required),
	(2, address, required),
});

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecoveryState {
	peers: Vec<RecoveryPeer>,
	bitcoind_start_height: Option<u32>,
	last_scanned_height: Option<u32>,
	recovered_outputs: u64,
	scans_completed: u64,
}

impl_writeable_tlv_based!(RecoveryState, {
	(0, peers, (required_vec, encoding: (Vec<RecoveryPeer>, WithoutLength))),
	(2, bitcoind_start_height, option),
	(4, last_scanned_height, option),
	(6, recovered_outputs, required),
	(8, scans_completed, required),
});

/// The current state of a [`RecoveryNode`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RecoveryStatus {
	/// Whether recovery background work is running.
	pub is_running: bool,
	/// The Bitcoin network being scanned.
	pub network: Network,
	/// The number of allowlisted peers.
	pub configured_peer_count: u64,
	/// The number of allowlisted peers currently connected.
	pub connected_peer_count: u64,
	/// The latest block height scanned by a Bitcoin Core recovery source.
	pub last_scanned_height: Option<u32>,
	/// The number of recovered outputs currently tracked by the output sweeper.
	pub recovered_outputs: u64,
	/// The number of completed recovery scans.
	pub scans_completed: u64,
}

/// Builds a restricted [`RecoveryNode`] from a seed and an allowlist of known peers.
///
/// Recovery state is isolated from ordinary [`Node`] state, even when both use the same physical
/// storage backend.
pub struct RecoveryNodeBuilder {
	inner: NodeBuilder,
	peers: Vec<RecoveryPeer>,
	bitcoind_start_height: Option<u32>,
}

impl RecoveryNodeBuilder {
	/// Creates a recovery builder with the default configuration.
	pub fn new() -> Self {
		Self::from_config(Config::default())
	}

	/// Creates a recovery builder from the given configuration.
	pub fn from_config(mut config: Config) -> Self {
		config.listening_addresses = None;
		config.announcement_addresses = None;
		config.node_alias = None;
		Self {
			inner: NodeBuilder::for_recovery(config),
			peers: Vec::new(),
			bitcoind_start_height: None,
		}
	}

	/// Uses an existing Tokio runtime.
	///
	/// The runtime must use the multithreaded flavor.
	#[cfg_attr(feature = "uniffi", allow(dead_code))]
	pub fn set_runtime(
		&mut self, runtime_handle: tokio::runtime::Handle,
	) -> Result<&mut Self, BuildError> {
		self.inner.set_runtime(runtime_handle)?;
		Ok(self)
	}

	/// Uses the given Esplora server and forces a full BIP84 wallet scan.
	#[cfg(feature = "chain-esplora")]
	pub fn set_chain_source_esplora(
		&mut self, server_url: String, sync_config: Option<EsploraSyncConfig>,
	) -> &mut Self {
		let mut sync_config = sync_config.unwrap_or_default();
		sync_config.force_wallet_full_scan = true;
		self.inner.set_chain_source_esplora(server_url, Some(sync_config));
		self.bitcoind_start_height = None;
		self
	}

	/// Uses the given Esplora server with HTTP headers and forces a full BIP84 wallet scan.
	#[cfg(feature = "chain-esplora")]
	pub fn set_chain_source_esplora_with_headers(
		&mut self, server_url: String, headers: HashMap<String, String>,
		sync_config: Option<EsploraSyncConfig>,
	) -> &mut Self {
		let mut sync_config = sync_config.unwrap_or_default();
		sync_config.force_wallet_full_scan = true;
		self.inner.set_chain_source_esplora_with_headers(server_url, headers, Some(sync_config));
		self.bitcoind_start_height = None;
		self
	}

	/// Uses the given Electrum server and forces a full BIP84 wallet scan.
	#[cfg(feature = "chain-electrum")]
	pub fn set_chain_source_electrum(
		&mut self, server_url: String, sync_config: Option<ElectrumSyncConfig>,
	) -> &mut Self {
		let mut sync_config = sync_config.unwrap_or_default();
		sync_config.force_wallet_full_scan = true;
		self.inner.set_chain_source_electrum(server_url, Some(sync_config));
		self.bitcoind_start_height = None;
		self
	}

	/// Uses Bitcoin Core RPC, initially scanning from `recovery_from_height` and resuming from
	/// persisted progress on later runs. An earlier recovery height restarts the output scan.
	#[cfg(feature = "chain-bitcoind")]
	pub fn set_chain_source_bitcoind_rpc(
		&mut self, rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		recovery_from_height: u32,
	) -> &mut Self {
		self.inner.set_chain_source_bitcoind_rpc(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			Some(recovery_from_height.saturating_sub(1)),
		);
		self.bitcoind_start_height = Some(recovery_from_height);
		self
	}

	/// Uses Bitcoin Core REST, initially scanning from `recovery_from_height` and resuming from
	/// persisted progress on later runs. An earlier recovery height restarts the output scan.
	#[cfg(feature = "chain-bitcoind")]
	pub fn set_chain_source_bitcoind_rest(
		&mut self, rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16,
		rpc_user: String, rpc_password: String, recovery_from_height: u32,
	) -> &mut Self {
		self.inner.set_chain_source_bitcoind_rest(
			rest_host,
			rest_port,
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			Some(recovery_from_height.saturating_sub(1)),
		);
		self.bitcoind_start_height = Some(recovery_from_height);
		self
	}

	/// Adds or replaces an allowlisted peer.
	pub fn add_peer(&mut self, peer: RecoveryPeer) -> &mut Self {
		self.peers.retain(|known| known.node_id != peer.node_id);
		self.peers.push(peer);
		self
	}

	/// Sets the storage directory used by the recovery node.
	pub fn set_storage_dir_path(&mut self, storage_dir_path: String) -> &mut Self {
		self.inner.set_storage_dir_path(storage_dir_path);
		self
	}

	/// Writes logs to the filesystem.
	pub fn set_filesystem_logger(
		&mut self, log_file_path: Option<String>, max_log_level: Option<LogLevel>,
	) -> &mut Self {
		self.inner.set_filesystem_logger(log_file_path, max_log_level);
		self
	}

	/// Writes logs through the Rust `log` facade.
	pub fn set_log_facade_logger(&mut self) -> &mut Self {
		self.inner.set_log_facade_logger();
		self
	}

	/// Writes logs through a custom log writer.
	pub fn set_custom_logger(&mut self, log_writer: Arc<dyn LogWriter>) -> &mut Self {
		self.inner.set_custom_logger(log_writer);
		self
	}

	/// Sets the Bitcoin network to recover.
	pub fn set_network(&mut self, network: Network) -> &mut Self {
		self.inner.set_network(network);
		self
	}

	/// Configures a Tor proxy for allowlisted OnionV3 peers.
	pub fn set_tor_config(&mut self, tor_config: TorConfig) -> Result<&mut Self, BuildError> {
		self.inner.set_tor_config(tor_config)?;
		Ok(self)
	}

	/// Builds a recovery node with SQLite storage.
	#[cfg(feature = "storage-sqlite")]
	pub fn build(&self, node_entropy: NodeEntropy) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build(node_entropy)?)
	}

	/// Builds a recovery node with filesystem storage.
	#[cfg(feature = "storage-filesystem")]
	pub fn build_with_fs_store(
		&self, node_entropy: NodeEntropy,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_fs_store(node_entropy)?)
	}

	/// Builds a recovery node with VSS signature authentication.
	#[cfg(feature = "storage-vss")]
	pub fn build_with_vss_store(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_vss_store(
			node_entropy,
			vss_url,
			store_id,
			fixed_headers,
		)?)
	}

	/// Builds a recovery node with VSS LNURL-auth authentication.
	#[cfg(feature = "storage-vss")]
	pub fn build_with_vss_store_and_lnurl_auth(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		lnurl_auth_server_url: String, fixed_headers: HashMap<String, String>,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_vss_store_and_lnurl_auth(
			node_entropy,
			vss_url,
			store_id,
			lnurl_auth_server_url,
			fixed_headers,
		)?)
	}

	/// Builds a recovery node with fixed-header VSS authentication.
	#[cfg(feature = "storage-vss")]
	pub fn build_with_vss_store_and_fixed_headers(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_vss_store_and_fixed_headers(
			node_entropy,
			vss_url,
			store_id,
			fixed_headers,
		)?)
	}

	/// Builds a recovery node with a custom VSS header provider.
	#[cfg(feature = "storage-vss")]
	pub fn build_with_vss_store_and_header_provider(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		header_provider: Arc<dyn VssHeaderProvider>,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_vss_store_and_header_provider(
			node_entropy,
			vss_url,
			store_id,
			header_provider,
		)?)
	}

	/// Builds a recovery node with a custom key-value store.
	pub fn build_with_store<S: PaginatedKVStore + Send + Sync + 'static>(
		&self, node_entropy: NodeEntropy, kv_store: S,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_store(node_entropy, kv_store)?)
	}

	/// Builds a recovery node with PostgreSQL storage.
	#[cfg(feature = "storage-postgres")]
	pub fn build_with_postgres_store(
		&self, node_entropy: NodeEntropy, connection_string: String, db_name: Option<String>,
		kv_table_name: Option<String>, certificate_pem: Option<String>,
	) -> Result<RecoveryNode, BuildError> {
		self.wrap(self.inner.build_with_postgres_store(
			node_entropy,
			connection_string,
			db_name,
			kv_table_name,
			certificate_pem,
		)?)
	}

	fn wrap(&self, mut node: Node) -> Result<RecoveryNode, BuildError> {
		let mut state = match node.pending_recovery_state.take() {
			Some(bytes) => {
				RecoveryState::read(&mut bytes.as_slice()).map_err(|_| BuildError::ReadFailed)?
			},
			None => RecoveryState::default(),
		};

		for peer in &self.peers {
			state.peers.retain(|known| known.node_id != peer.node_id);
			state.peers.push(peer.clone());
		}
		if let (Some(persisted), Some(configured)) =
			(state.bitcoind_start_height, self.bitcoind_start_height)
		{
			if configured < persisted {
				state.last_scanned_height = None;
			}
		}
		state.bitcoind_start_height =
			match (state.bitcoind_start_height, self.bitcoind_start_height) {
				(Some(persisted), Some(configured)) => Some(persisted.min(configured)),
				(None, configured) => configured,
				(persisted, None) => persisted,
			};

		let recovery_node = RecoveryNode { inner: node, state: Arc::new(RwLock::new(state)) };
		recovery_node.persist_state().map_err(|_| BuildError::WriteFailed)?;
		Ok(recovery_node)
	}
}

impl Default for RecoveryNodeBuilder {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(feature = "uniffi")]
pub struct ArcedRecoveryNodeBuilder {
	inner: RwLock<RecoveryNodeBuilder>,
}

#[cfg(feature = "uniffi")]
impl ArcedRecoveryNodeBuilder {
	pub fn new() -> Self {
		Self { inner: RwLock::new(RecoveryNodeBuilder::new()) }
	}

	pub fn from_config(config: Config) -> Self {
		Self { inner: RwLock::new(RecoveryNodeBuilder::from_config(config)) }
	}
}

#[cfg(all(feature = "uniffi", feature = "chain-esplora"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn set_chain_source_esplora(
		&self, server_url: String, sync_config: Option<EsploraSyncConfig>,
	) {
		self.inner.write().expect("lock").set_chain_source_esplora(server_url, sync_config);
	}
}

#[cfg(all(feature = "uniffi", feature = "chain-electrum"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn set_chain_source_electrum(
		&self, server_url: String, sync_config: Option<ElectrumSyncConfig>,
	) {
		self.inner.write().expect("lock").set_chain_source_electrum(server_url, sync_config);
	}
}

#[cfg(all(feature = "uniffi", feature = "chain-bitcoind"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn set_chain_source_bitcoind_rpc(
		&self, rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		recovery_from_height: u32,
	) {
		self.inner.write().expect("lock").set_chain_source_bitcoind_rpc(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			recovery_from_height,
		);
	}

	pub fn set_chain_source_bitcoind_rest(
		&self, rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16,
		rpc_user: String, rpc_password: String, recovery_from_height: u32,
	) {
		self.inner.write().expect("lock").set_chain_source_bitcoind_rest(
			rest_host,
			rest_port,
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			recovery_from_height,
		);
	}
}

#[cfg(feature = "uniffi")]
impl ArcedRecoveryNodeBuilder {
	pub fn add_peer(&self, peer: RecoveryPeer) {
		self.inner.write().expect("lock").add_peer(peer);
	}

	pub fn set_storage_dir_path(&self, storage_dir_path: String) {
		self.inner.write().expect("lock").set_storage_dir_path(storage_dir_path);
	}

	pub fn set_filesystem_logger(
		&self, log_file_path: Option<String>, max_log_level: Option<LogLevel>,
	) {
		self.inner.write().expect("lock").set_filesystem_logger(log_file_path, max_log_level);
	}

	pub fn set_log_facade_logger(&self) {
		self.inner.write().expect("lock").set_log_facade_logger();
	}

	pub fn set_custom_logger(&self, log_writer: Arc<dyn LogWriter>) {
		self.inner.write().expect("lock").set_custom_logger(log_writer);
	}

	pub fn set_network(&self, network: Network) {
		self.inner.write().expect("lock").set_network(network);
	}

	pub fn set_tor_config(&self, tor_config: TorConfig) -> Result<(), BuildError> {
		self.inner.write().expect("lock").set_tor_config(tor_config).map(|_| ())
	}
}

#[cfg(all(feature = "uniffi", feature = "storage-sqlite"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn build(&self, node_entropy: Arc<NodeEntropy>) -> Result<Arc<RecoveryNode>, BuildError> {
		self.inner.read().expect("lock").build(*node_entropy).map(Arc::new)
	}
}

#[cfg(all(feature = "uniffi", feature = "storage-filesystem"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn build_with_fs_store(
		&self, node_entropy: Arc<NodeEntropy>,
	) -> Result<Arc<RecoveryNode>, BuildError> {
		self.inner.read().expect("lock").build_with_fs_store(*node_entropy).map(Arc::new)
	}
}

#[cfg(all(feature = "uniffi", feature = "storage-vss"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn build_with_vss_store(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<Arc<RecoveryNode>, BuildError> {
		self.inner
			.read()
			.expect("lock")
			.build_with_vss_store(*node_entropy, vss_url, store_id, fixed_headers)
			.map(Arc::new)
	}

	pub fn build_with_vss_store_and_lnurl_auth(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		lnurl_auth_server_url: String, fixed_headers: HashMap<String, String>,
	) -> Result<Arc<RecoveryNode>, BuildError> {
		self.inner
			.read()
			.expect("lock")
			.build_with_vss_store_and_lnurl_auth(
				*node_entropy,
				vss_url,
				store_id,
				lnurl_auth_server_url,
				fixed_headers,
			)
			.map(Arc::new)
	}

	pub fn build_with_vss_store_and_fixed_headers(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<Arc<RecoveryNode>, BuildError> {
		self.inner
			.read()
			.expect("lock")
			.build_with_vss_store_and_fixed_headers(*node_entropy, vss_url, store_id, fixed_headers)
			.map(Arc::new)
	}

	pub fn build_with_vss_store_and_header_provider(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		header_provider: Arc<dyn crate::ffi::VssHeaderProvider>,
	) -> Result<Arc<RecoveryNode>, BuildError> {
		let adapter = Arc::new(crate::ffi::VssHeaderProviderAdapter::new(header_provider));
		self.inner
			.read()
			.expect("lock")
			.build_with_vss_store_and_header_provider(*node_entropy, vss_url, store_id, adapter)
			.map(Arc::new)
	}
}

#[cfg(all(feature = "uniffi", feature = "storage-postgres"))]
#[uniffi::export]
impl RecoveryBuilder {
	pub fn build_with_postgres_store(
		&self, node_entropy: Arc<NodeEntropy>, connection_string: String, db_name: Option<String>,
		kv_table_name: Option<String>, certificate_pem: Option<String>,
	) -> Result<Arc<RecoveryNode>, BuildError> {
		self.inner
			.read()
			.expect("lock")
			.build_with_postgres_store(
				*node_entropy,
				connection_string,
				db_name,
				kv_table_name,
				certificate_pem,
			)
			.map(Arc::new)
	}
}

#[derive(Clone)]
struct RecoveryContext {
	chain_source: Arc<ChainSource>,
	keys_manager: Arc<KeysManager>,
	output_sweeper: Arc<Sweeper>,
	kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
	state: Arc<RwLock<RecoveryState>>,
}

impl RecoveryContext {
	async fn persist_state(&self) -> Result<(), Error> {
		let bytes = self.state.read().expect("lock").encode();
		KVStore::write(
			&*self.kv_store,
			RECOVERY_STATE_PRIMARY_NAMESPACE,
			RECOVERY_STATE_SECONDARY_NAMESPACE,
			RECOVERY_STATE_KEY,
			bytes,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Failed to persist recovery state: {}", e);
			Error::PersistenceFailed
		})
	}

	async fn scan_once(&self) -> Result<(), Error> {
		let secp_ctx = bitcoin::secp256k1::Secp256k1::new();
		let scripts = self.keys_manager.possible_v2_counterparty_closed_balance_spks(&secp_ctx);
		let outputs = match self.chain_source.recovery_source()? {
			#[cfg(feature = "chain-esplora")]
			RecoveryChainSource::Esplora(client) => self.scan_esplora(client, scripts).await?,
			#[cfg(feature = "chain-electrum")]
			RecoveryChainSource::Electrum { client, runtime } => {
				self.scan_electrum(client, runtime, scripts).await?
			},
			#[cfg(feature = "chain-bitcoind")]
			RecoveryChainSource::Bitcoind(client) => {
				self.scan_bitcoind(client, scripts).await?;
				Vec::new()
			},
		};
		self.track_outputs(outputs).await?;

		let recovered_outputs = self
			.output_sweeper
			.tracked_spendable_outputs()
			.iter()
			.filter(|output| {
				matches!(
					output.descriptor,
					SpendableOutputDescriptor::RecoveredStaticPaymentOutput(_)
				)
			})
			.count() as u64;
		{
			let mut state = self.state.write().expect("lock");
			state.recovered_outputs = recovered_outputs;
			state.scans_completed = state.scans_completed.saturating_add(1);
		}
		self.persist_state().await?;
		log_info!(
			self.logger,
			"Recovery scan complete; tracking {} recovered output(s).",
			recovered_outputs
		);
		Ok(())
	}

	async fn track_outputs(&self, outputs: Vec<(OutPoint, TxOut)>) -> Result<(), Error> {
		if outputs.is_empty() {
			return Ok(());
		}
		let secp_ctx = bitcoin::secp256k1::Secp256k1::new();
		let descriptors: Vec<_> = outputs
			.into_iter()
			.filter_map(|(outpoint, output)| {
				self.keys_manager.spendable_output_descriptor_for_v2_counterparty_closed_balance(
					outpoint, output, &secp_ctx,
				)
			})
			.collect();
		self.output_sweeper
			.track_spendable_outputs(descriptors, None, None, false, None)
			.await
			.map_err(|_| Error::PersistenceFailed)
	}

	#[cfg(feature = "chain-esplora")]
	async fn scan_esplora(
		&self, client: esplora_client::AsyncClient, scripts: Vec<ScriptBuf>,
	) -> Result<Vec<(OutPoint, TxOut)>, Error> {
		let mut worker_scripts = vec![Vec::new(); ESPLORA_SCAN_WORKERS];
		for (index, script) in scripts.into_iter().enumerate() {
			worker_scripts[index % ESPLORA_SCAN_WORKERS].push(script);
		}

		let mut workers = tokio::task::JoinSet::new();
		for scripts in worker_scripts {
			let client = client.clone();
			workers.spawn(async move {
				let mut outputs = Vec::new();
				for script in scripts {
					let utxos = client.get_scripthash_utxos(&script).await?;
					for utxo in utxos.into_iter().filter(|utxo| utxo.status.confirmed) {
						let Ok(index) = u16::try_from(utxo.vout) else { continue };
						outputs.push((
							OutPoint { txid: utxo.txid, index },
							TxOut { value: utxo.value, script_pubkey: script.clone() },
						));
					}
				}
				Ok::<_, esplora_client::Error>(outputs)
			});
		}

		let mut outputs = Vec::new();
		while let Some(result) = workers.join_next().await {
			outputs.extend(
				result
					.map_err(|e| {
						log_error!(self.logger, "Esplora recovery scan task failed: {}", e);
						Error::TxSyncFailed
					})?
					.map_err(|e| {
						log_error!(self.logger, "Esplora recovery scan failed: {}", e);
						Error::TxSyncFailed
					})?,
			);
		}
		Ok(outputs)
	}

	#[cfg(feature = "chain-electrum")]
	async fn scan_electrum(
		&self, client: Arc<electrum_client::Client>, runtime: Arc<crate::runtime::Runtime>,
		scripts: Vec<ScriptBuf>,
	) -> Result<Vec<(OutPoint, TxOut)>, Error> {
		let mut outputs = Vec::new();
		for scripts in scripts.chunks(ELECTRUM_SCAN_BATCH_SIZE) {
			let scripts = scripts.to_vec();
			let query_scripts = scripts.clone();
			let client = Arc::clone(&client);
			let results = runtime
				.spawn_blocking(move || {
					client.batch_script_list_unspent(query_scripts.iter().map(ScriptBuf::as_script))
				})
				.await
				.map_err(|e| {
					log_error!(self.logger, "Electrum recovery scan task failed: {}", e);
					Error::TxSyncFailed
				})?
				.map_err(|e| {
					log_error!(self.logger, "Electrum recovery scan failed: {}", e);
					Error::TxSyncFailed
				})?;
			for (script, utxos) in scripts.into_iter().zip(results) {
				for utxo in utxos.into_iter().filter(|utxo| utxo.height > 0) {
					let Ok(index) = u16::try_from(utxo.tx_pos) else { continue };
					outputs.push((
						OutPoint { txid: utxo.tx_hash, index },
						TxOut {
							value: Amount::from_sat(utxo.value),
							script_pubkey: script.clone(),
						},
					));
				}
			}
		}
		Ok(outputs)
	}

	#[cfg(feature = "chain-bitcoind")]
	async fn scan_bitcoind(
		&self, client: crate::chain::bitcoind::UtxoSourceClient, scripts: Vec<ScriptBuf>,
	) -> Result<(), Error> {
		let (_, best_height) = client.get_best_block().await.map_err(|e| {
			log_error!(self.logger, "Failed to retrieve Bitcoin Core chain tip: {:?}", e);
			Error::TxSyncFailed
		})?;
		let best_height = best_height.ok_or(Error::TxSyncFailed)?;
		let (configured_start, last_scanned_height) = {
			let state = self.state.read().expect("lock");
			(state.bitcoind_start_height.unwrap_or(0), state.last_scanned_height)
		};
		let start_height = bitcoind_scan_start(configured_start, last_scanned_height);
		let scripts: HashSet<_> = scripts.into_iter().collect();
		let mut candidates = Vec::new();

		for height in start_height..=best_height {
			let block_hash = client.get_block_hash_by_height(height).await.map_err(|e| {
				log_error!(self.logger, "Failed to retrieve block hash at {}: {:?}", height, e);
				Error::TxSyncFailed
			})?;
			let block = match client.get_block(&block_hash).await.map_err(|e| {
				log_error!(self.logger, "Failed to retrieve block at {}: {:?}", height, e);
				Error::TxSyncFailed
			})? {
				BlockData::FullBlock(block) => block,
				BlockData::HeaderOnly(_) => {
					log_error!(
						self.logger,
						"Bitcoin Core returned no block body at height {}; archival data is required for recovery.",
						height
					);
					return Err(Error::ChainSourceNotSupported);
				},
			};
			for transaction in block.txdata {
				let txid = transaction.compute_txid();
				for (index, output) in transaction.output.into_iter().enumerate() {
					if scripts.contains(&output.script_pubkey) {
						let Ok(index) = u16::try_from(index) else { continue };
						candidates.push((OutPoint { txid, index }, output));
					}
				}
			}
			if height % 100 == 0 || height == best_height {
				let mut unspent = Vec::new();
				for (outpoint, output) in candidates.drain(..) {
					if client.is_output_unspent(outpoint.into_bitcoin_outpoint()).await.map_err(
						|e| {
							log_error!(
								self.logger,
								"Failed to check recovered output {}: {:?}",
								outpoint,
								e
							);
							Error::TxSyncFailed
						},
					)? {
						unspent.push((outpoint, output));
					}
				}
				// Only advance past outputs once the sweeper has durably recorded them. Failed or
				// interrupted batches must replay the same range, including after a restart.
				self.track_outputs(unspent).await?;
				self.state.write().expect("lock").last_scanned_height = Some(height);
				self.persist_state().await?;
			}
		}
		Ok(())
	}
}

/// A restricted node which recovers seed-derived on-chain balances and contacts only allowlisted
/// peers.
pub struct RecoveryNode {
	inner: Node,
	state: Arc<RwLock<RecoveryState>>,
}

impl RecoveryNode {
	fn context(&self) -> RecoveryContext {
		RecoveryContext {
			chain_source: Arc::clone(&self.inner.chain_source),
			keys_manager: Arc::clone(&self.inner.keys_manager),
			output_sweeper: Arc::clone(&self.inner.output_sweeper),
			kv_store: Arc::clone(&self.inner.kv_store),
			logger: Arc::clone(&self.inner.logger),
			state: Arc::clone(&self.state),
		}
	}

	fn persist_state(&self) -> Result<(), Error> {
		self.inner.runtime.block_on(self.context().persist_state())
	}

	/// Starts long-running recovery work.
	pub fn start(&self) -> Result<(), Error> {
		let mut is_running = self.inner.is_running.write().expect("lock");
		if *is_running {
			return Err(Error::AlreadyRunning);
		}

		match self.start_inner(&mut is_running) {
			Ok(()) => Ok(()),
			Err(e) => {
				self.inner.chain_source.stop();
				Err(e)
			},
		}
	}

	fn start_inner(&self, is_running: &mut bool) -> Result<(), Error> {
		log_info!(
			self.inner.logger,
			"Starting LDK Node recovery with node ID {} on network: {}",
			self.node_id(),
			self.inner.config.network
		);
		self.inner.runtime.allow_cancellable_background_task_spawns();
		self.inner.chain_source.start(Arc::clone(&self.inner.runtime))?;
		self.inner.chain_source.force_recovery_wallet_full_scan();

		let chain_source = Arc::clone(&self.inner.chain_source);
		let wallet = Arc::clone(&self.inner.wallet);
		let channel_manager = Arc::clone(&self.inner.channel_manager);
		let chain_monitor = Arc::clone(&self.inner.chain_monitor);
		let output_sweeper = Arc::clone(&self.inner.output_sweeper);
		let context = self.context();
		self.inner.runtime.block_on(async {
			chain_source.update_fee_rate_estimates().await?;
			if chain_source.is_transaction_based() {
				tokio::try_join!(
					chain_source.sync_onchain_wallet(Arc::clone(&wallet)),
					chain_source.sync_lightning_wallet(
						Arc::clone(&channel_manager),
						Arc::clone(&chain_monitor),
						Arc::clone(&output_sweeper),
					)
				)?;
			} else {
				chain_source
					.poll_and_update_listeners(
						Arc::clone(&wallet),
						Arc::clone(&channel_manager),
						Arc::clone(&chain_monitor),
						Arc::clone(&output_sweeper),
					)
					.await?;
			}
			context.scan_once().await?;
			self.recover_peer_storage().await
		})?;

		let stop_sync = self.inner.stop_sender.subscribe();
		let chain_source = Arc::clone(&self.inner.chain_source);
		let wallet = Arc::clone(&self.inner.wallet);
		let channel_manager = Arc::clone(&self.inner.channel_manager);
		let chain_monitor = Arc::clone(&self.inner.chain_monitor);
		let output_sweeper = Arc::clone(&self.inner.output_sweeper);
		self.inner.runtime.spawn_background_task(async move {
			chain_source
				.continuously_sync_wallets(
					stop_sync,
					wallet,
					channel_manager,
					chain_monitor,
					output_sweeper,
				)
				.await;
		});

		let mut stop_scan = self.inner.stop_sender.subscribe();
		let scan_context = self.context();
		self.inner.runtime.spawn_cancellable_background_task(async move {
			let mut interval = tokio::time::interval(RECOVERY_SCAN_INTERVAL);
			interval.reset();
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					_ = stop_scan.changed() => {
						log_trace!(scan_context.logger, "Stopping recovery scans.");
						return;
					},
					_ = interval.tick() => {
						if let Err(e) = scan_context.scan_once().await {
							log_error!(scan_context.logger, "Recovery scan failed: {}", e);
						}
					},
				}
			}
		});

		let mut stop_connect = self.inner.stop_sender.subscribe();
		let connection_manager = Arc::clone(&self.inner.connection_manager);
		let peer_manager = Arc::clone(&self.inner.peer_manager);
		let logger = Arc::clone(&self.inner.logger);
		let peers = self.state.read().expect("lock").peers.clone();
		self.inner.runtime.spawn_cancellable_background_task(async move {
			let mut interval = tokio::time::interval(RECOVERY_RECONNECT_INTERVAL);
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					_ = stop_connect.changed() => {
						log_trace!(logger, "Stopping recovery peer connections.");
						return;
					},
					_ = interval.tick() => {
						for peer in &peers {
							if peer_manager.peer_by_node_id(&peer.node_id).is_none() {
								let _ = connection_manager
									.do_connect_peer(peer.node_id, peer.address.clone())
									.await;
							}
						}
					},
				}
			}
		});

		let stop_broadcast = self.inner.stop_sender.subscribe();
		let chain_source = Arc::clone(&self.inner.chain_source);
		self.inner.runtime.spawn_cancellable_background_task(async move {
			chain_source.continuously_process_broadcast_queue(stop_broadcast).await;
		});

		let background_persister = Arc::clone(&self.inner.kv_store);
		let background_chain_monitor = Arc::clone(&self.inner.chain_monitor);
		let background_channel_manager = Arc::clone(&self.inner.channel_manager);
		let background_peer_manager = Arc::clone(&self.inner.peer_manager);
		let background_onion_messenger = Arc::clone(&self.inner.onion_messenger);
		let background_liquidity_manager = self.inner.liquidity_source.liquidity_manager();
		let background_sweeper = Arc::clone(&self.inner.output_sweeper);
		let event_sweeper = Arc::clone(&self.inner.output_sweeper);
		let background_logger = Arc::clone(&self.inner.logger);
		let error_logger = Arc::clone(&self.inner.logger);
		let background_scorer = Arc::clone(&self.inner.scorer);
		let gossip_sync: GossipSync = GossipSync::None;
		let stop_background_processor = self.inner.background_processor_stop_sender.subscribe();
		let sleeper_logger = Arc::clone(&self.inner.logger);
		let sleeper = move |duration| {
			let mut stop = stop_background_processor.clone();
			let logger = Arc::clone(&sleeper_logger);
			Box::pin(async move {
				tokio::select! {
					_ = stop.changed() => {
						log_debug!(logger, "Stopping recovery event processing.");
						true
					},
					_ = tokio::time::sleep(duration) => false,
				}
			})
		};

		self.inner.runtime.spawn_background_processor_task(async move {
			process_events_async(
				background_persister,
				move |event| {
					let sweeper = Arc::clone(&event_sweeper);
					async move {
						if let lightning::events::Event::SpendableOutputs {
							outputs,
							channel_id,
							counterparty_node_id,
						} = event
						{
							sweeper
								.track_spendable_outputs(
									outputs,
									channel_id,
									counterparty_node_id,
									false,
									None,
								)
								.await
								.map_err(|_| ReplayEvent())?;
						}
						Ok(())
					}
				},
				background_chain_monitor,
				background_channel_manager,
				Some(background_onion_messenger),
				gossip_sync,
				background_peer_manager,
				Some(background_liquidity_manager),
				Some(background_sweeper),
				background_logger,
				Some(background_scorer),
				sleeper,
				true,
				|| {
					Some(
						SystemTime::now()
							.duration_since(SystemTime::UNIX_EPOCH)
							.expect("current time should not predate the Unix epoch"),
					)
				},
			)
			.await
			.unwrap_or_else(|e| {
				log_error!(error_logger, "Recovery event processing failed: {}", e);
				panic!("Recovery event processing failed");
			});
		});

		*is_running = true;
		log_info!(self.inner.logger, "Recovery startup complete.");
		Ok(())
	}

	/// Stops recovery work and disconnects all allowlisted peers.
	pub fn stop(&self) -> Result<(), Error> {
		self.inner.stop()
	}

	/// Returns the recovery node ID derived from the supplied seed.
	pub fn node_id(&self) -> PublicKey {
		self.inner.node_id()
	}

	/// Returns on-chain and pending sweep balances found so far.
	pub fn list_balances(&self) -> BalanceDetails {
		self.inner.list_balances()
	}

	/// Returns progress without declaring recovery complete.
	pub fn status(&self) -> RecoveryStatus {
		let is_running = *self.inner.is_running.read().expect("lock");
		let state = self.state.read().expect("lock");
		let connected = self
			.inner
			.peer_manager
			.list_peers()
			.iter()
			.filter(|peer| {
				state.peers.iter().any(|known| known.node_id == peer.counterparty_node_id)
			})
			.count() as u64;
		RecoveryStatus {
			is_running,
			network: self.inner.config.network,
			configured_peer_count: state.peers.len() as u64,
			connected_peer_count: connected,
			last_scanned_height: state.last_scanned_height,
			recovered_outputs: state.recovered_outputs,
			scans_completed: state.scans_completed,
		}
	}

	// Peer Storage recovery will plug in here once LDK exposes the remaining retrieval flow. See
	// https://github.com/lightningdevkit/ldk-server/issues/257 for the intended follow-up.
	async fn recover_peer_storage(&self) -> Result<(), Error> {
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	#[cfg(feature = "chain-bitcoind")]
	use std::io::{BufRead, BufReader, Read, Write};
	#[cfg(feature = "chain-bitcoind")]
	use std::net::TcpListener;
	use std::str::FromStr;

	#[cfg(feature = "chain-bitcoind")]
	use bitcoin::consensus::encode::serialize_hex;
	use bitcoin::secp256k1::{Secp256k1, SecretKey};
	#[cfg(feature = "chain-bitcoind")]
	use serde_json::{json, Value};

	use super::*;
	use crate::entropy::NodeEntropy;
	use crate::io::test_utils::InMemoryStore;

	fn test_builder() -> RecoveryNodeBuilder {
		let mut builder = RecoveryNodeBuilder::new();
		builder.set_log_facade_logger();
		builder.set_network(Network::Regtest);
		builder
	}

	fn test_entropy() -> NodeEntropy {
		#[cfg(not(feature = "uniffi"))]
		return NodeEntropy::from_seed_bytes([44; 64]);
		#[cfg(feature = "uniffi")]
		return NodeEntropy::from_seed_bytes(vec![44; 64]).unwrap();
	}

	#[test]
	fn recovery_accepts_config_with_node_alias() {
		let mut config = Config::default();
		config.node_alias = Some(lightning::routing::gossip::NodeAlias([42; 32]));
		config.listening_addresses = Some(vec![SocketAddress::from_str("127.0.0.1:9735").unwrap()]);
		let mut builder = RecoveryNodeBuilder::from_config(config);
		builder.set_log_facade_logger();
		assert!(
			builder.build_with_store(test_entropy(), InMemoryStore::new()).is_ok(),
			"recovery must accept an ordinary node's announcement configuration"
		);
	}

	#[test]
	#[cfg(feature = "chain-bitcoind")]
	fn recovery_earlier_start_rewinds_scan() {
		for (persisted_start, configured_start, expected_last_height) in
			[(100, 50, None), (100, 100, Some(200)), (100, 150, Some(200))]
		{
			let mut builder = test_builder();
			let mut node =
				builder.inner.build_with_store(test_entropy(), InMemoryStore::new()).unwrap();
			builder.set_chain_source_bitcoind_rpc(
				"127.0.0.1".into(),
				1,
				"user".into(),
				"password".into(),
				configured_start,
			);
			node.pending_recovery_state = Some(
				RecoveryState {
					bitcoind_start_height: Some(persisted_start),
					last_scanned_height: Some(200),
					..RecoveryState::default()
				}
				.encode(),
			);
			let recovery = builder.wrap(node).unwrap();
			assert_eq!(
				recovery.status().last_scanned_height,
				expected_last_height,
				"an earlier recovery start must invalidate the old scan cursor"
			);
			assert_eq!(
				recovery.state.read().unwrap().bitcoind_start_height,
				Some(persisted_start.min(configured_start))
			);
		}
	}

	#[test]
	fn recovery_status_does_not_lock_state_while_waiting_for_start() {
		let node = Arc::new(
			test_builder().build_with_store(test_entropy(), InMemoryStore::new()).unwrap(),
		);
		let running = node.inner.is_running.write().unwrap();
		let status_node = Arc::clone(&node);
		let (started, waiting) = std::sync::mpsc::channel();
		let status_thread = std::thread::spawn(move || {
			started.send(()).unwrap();
			status_node.status()
		});
		waiting.recv().unwrap();
		let deadline = std::time::Instant::now() + Duration::from_secs(1);
		let mut state_available = true;
		while std::time::Instant::now() < deadline {
			if node.state.try_write().is_err() {
				state_available = false;
				break;
			}
			std::thread::yield_now();
		}
		drop(running);
		status_thread.join().unwrap();
		assert!(
			state_available,
			"status must leave recovery state unlocked while startup holds is_running"
		);
	}

	// Serve just the RPCs used by a recovery scan, without requiring a running Bitcoin Core node.
	#[cfg(feature = "chain-bitcoind")]
	fn serve_scan_rpc(listener: TcpListener, block: bitcoin::Block) {
		let mut failed_once = false;
		let mut scans = 0;
		loop {
			let (mut stream, _) = listener.accept().unwrap();
			stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
			let mut reader = BufReader::new(&mut stream);
			let mut content_length = 0;
			loop {
				let mut line = String::new();
				assert_ne!(reader.read_line(&mut line).unwrap(), 0);
				if line == "\r\n" {
					break;
				}
				if let Some((name, value)) = line.split_once(':') {
					if name.eq_ignore_ascii_case("content-length") {
						content_length = value.trim().parse().unwrap();
					}
				}
			}
			let mut body = vec![0; content_length];
			reader.read_exact(&mut body).unwrap();
			let request: Value = serde_json::from_slice(&body).unwrap();
			let mut error = Value::Null;
			let result = match request["method"].as_str().unwrap() {
				"getblockchaininfo" => json!({"bestblockhash": block.block_hash(), "blocks": 100}),
				"getblockheader" => json!({
					"version": block.header.version.to_consensus(),
					"previousblockhash": block.header.prev_blockhash,
					"merkleroot": block.header.merkle_root,
					"time": block.header.time,
					"bits": format!("{:08x}", block.header.bits.to_consensus()),
					"nonce": block.header.nonce,
					"chainwork": format!("{:064x}", 2),
					"height": 100,
				}),
				"getblockhash" => json!(block.block_hash()),
				"getblock" => json!(serialize_hex(&block)),
				"gettxout" => {
					scans += 1;
					if !failed_once {
						failed_once = true;
						error = json!({"code": -1, "message": "injected UTXO lookup failure"});
					}
					json!({})
				},
				method => panic!("unexpected RPC: {method}"),
			};
			let body = json!({"result": result, "error": error, "id": request["id"]}).to_string();
			write!(
				stream,
				"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
				body.len(),
				body
			)
			.unwrap();
			if scans == 2 {
				break;
			}
		}
	}

	#[test]
	#[cfg(feature = "chain-bitcoind")]
	fn recovery_failed_scan_preserves_cursor() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let script = test_builder()
			.build_with_store(test_entropy(), InMemoryStore::new())
			.unwrap()
			.inner
			.keys_manager
			.possible_v2_counterparty_closed_balance_spks(&Secp256k1::new())[0]
			.clone();
		let mut block = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
		block.txdata[0].output[0].script_pubkey = script;
		let port = listener.local_addr().unwrap().port();
		let server = std::thread::spawn(move || serve_scan_rpc(listener, block));
		let mut builder = test_builder();
		builder.set_chain_source_bitcoind_rpc(
			"127.0.0.1".into(),
			port,
			"user".into(),
			"password".into(),
			100,
		);
		let node = builder.build_with_store(test_entropy(), InMemoryStore::new()).unwrap();
		let context = node.context();
		assert_eq!(node.inner.runtime.block_on(context.scan_once()), Err(Error::TxSyncFailed));
		let failed_height = node.status().last_scanned_height;
		let persisted = node
			.inner
			.runtime
			.block_on(KVStore::read(
				&*node.inner.kv_store,
				RECOVERY_STATE_PRIMARY_NAMESPACE,
				RECOVERY_STATE_SECONDARY_NAMESPACE,
				RECOVERY_STATE_KEY,
			))
			.unwrap();
		let failed_persisted_height =
			RecoveryState::read(&mut persisted.as_slice()).unwrap().last_scanned_height;
		assert!(node.inner.output_sweeper.tracked_spendable_outputs().is_empty());
		node.inner.runtime.block_on(context.scan_once()).unwrap();
		server.join().unwrap();
		assert_eq!(failed_height, None, "a failed scan must not skip untracked outputs on retry");
		assert_eq!(
			failed_persisted_height, None,
			"a failed scan must not skip untracked outputs after restart"
		);
		assert_eq!(node.status().last_scanned_height, Some(100));
		assert_eq!(node.status().recovered_outputs, 1);
		assert_eq!(node.status().scans_completed, 1);
		assert_eq!(node.inner.output_sweeper.tracked_spendable_outputs().len(), 1);
	}

	#[tokio::test]
	async fn recovery_store_is_isolated() {
		let store = RecoveryStore::new(InMemoryStore::new());

		KVStore::write(&*store.inner, "", "", "state", b"normal".to_vec()).await.unwrap();
		KVStore::write(&store, "", "", "state", b"recovery".to_vec()).await.unwrap();

		assert_eq!(KVStore::read(&*store.inner, "", "", "state").await.unwrap(), b"normal");
		assert_eq!(KVStore::read(&store, "", "", "state").await.unwrap(), b"recovery");
	}

	#[test]
	fn recovery_state_round_trips() {
		let secp_ctx = Secp256k1::new();
		let peer = RecoveryPeer {
			node_id: PublicKey::from_secret_key(
				&secp_ctx,
				&SecretKey::from_slice(&[42; 32]).unwrap(),
			),
			address: SocketAddress::from_str("127.0.0.1:9735").unwrap(),
		};
		let state = RecoveryState {
			peers: vec![peer],
			bitcoind_start_height: Some(100),
			last_scanned_height: Some(200),
			recovered_outputs: 3,
			scans_completed: 4,
		};

		let decoded = RecoveryState::read(&mut state.encode().as_slice()).unwrap();
		assert_eq!(decoded, state);
	}

	#[test]
	#[cfg(feature = "chain-bitcoind")]
	fn bitcoind_rescan_replays_reorg_buffer() {
		assert_eq!(bitcoind_scan_start(100, None), 100);
		assert_eq!(bitcoind_scan_start(100, Some(103)), 100);
		assert_eq!(bitcoind_scan_start(100, Some(200)), 194);
	}

	#[test]
	fn recovery_builder_replaces_peer_addresses() {
		let secp_ctx = Secp256k1::new();
		let node_id =
			PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[43; 32]).unwrap());
		let mut builder = RecoveryNodeBuilder::new();
		builder.set_log_facade_logger();
		builder.add_peer(RecoveryPeer {
			node_id,
			address: SocketAddress::from_str("127.0.0.1:9735").unwrap(),
		});
		builder.add_peer(RecoveryPeer {
			node_id,
			address: SocketAddress::from_str("127.0.0.1:9736").unwrap(),
		});

		#[cfg(not(feature = "uniffi"))]
		let node_entropy = NodeEntropy::from_seed_bytes([44; 64]);
		#[cfg(feature = "uniffi")]
		let node_entropy = NodeEntropy::from_seed_bytes(vec![44; 64]).unwrap();
		let node = builder.build_with_store(node_entropy, InMemoryStore::new()).unwrap();
		assert_eq!(node.status().configured_peer_count, 1);
		assert!(!node.status().is_running);
		assert_eq!(node.state.read().expect("lock").peers[0].address.to_string(), "127.0.0.1:9736");
	}
}
