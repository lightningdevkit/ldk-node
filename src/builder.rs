// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::convert::TryInto;
use std::default::Default;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once, RwLock};
use std::time::SystemTime;
use std::{fmt, fs};

use bdk_wallet::template::Bip84;
use bdk_wallet::{KeychainKind, Wallet as BdkWallet};
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{BlockHash, Network};
use bitcoin_payment_instructions::onion_message_resolver::LDKOnionMessageDNSSECHrnResolver;
use lightning::chain::{chainmonitor, BestBlock};
use lightning::ln::channelmanager::{self, ChainParameters, ChannelManagerReadArgs};
use lightning::ln::msgs::{RoutingMessageHandler, SocketAddress};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::log_trace;
use lightning::routing::gossip::NodeAlias;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{
	CombinedScorer, ProbabilisticScorer, ProbabilisticScoringDecayParameters,
	ProbabilisticScoringFeeParameters,
};
use lightning::sign::{EntropySource, NodeSigner};
use lightning::util::persist::{
	KVStore, CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::ReadableArgs;
use lightning::util::sweep::OutputSweeper;
use lightning_persister::fs_store::FilesystemStore;
use vss_client::headers::VssHeaderProvider;

use crate::chain::ChainSource;
use crate::config::{
	default_user_config, may_announce_channel, AnnounceError, AsyncPaymentsRole,
	BitcoindRestClientConfig, Config, ElectrumSyncConfig, EsploraSyncConfig,
	DEFAULT_ESPLORA_SERVER_URL, DEFAULT_LOG_FILENAME, DEFAULT_LOG_LEVEL,
};
use crate::connection::ConnectionManager;
use crate::entropy::NodeEntropy;
use crate::event::EventQueue;
use crate::fee_estimator::OnchainFeeEstimator;
use crate::gossip::GossipSource;
use crate::io::sqlite_store::SqliteStore;
use crate::io::utils::{
	read_event_queue, read_external_pathfinding_scores_from_cache, read_network_graph,
	read_node_metrics, read_output_sweeper, read_payments, read_peer_info, read_scorer,
	write_node_metrics,
};
use crate::io::vss_store::VssStoreBuilder;
use crate::io::{
	self, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::liquidity::{
	LSPS1ClientConfig, LSPS2ClientConfig, LSPS2ServiceConfig, LiquiditySourceBuilder,
};
use crate::lnurl_auth::LnurlAuth;
use crate::logger::{log_error, LdkLogger, LogLevel, LogWriter, Logger};
use crate::message_handler::NodeCustomMessageHandler;
use crate::payment::asynchronous::om_mailbox::OnionMessageMailbox;
use crate::peer_store::PeerStore;
use crate::runtime::{Runtime, RuntimeSpawner};
use crate::tx_broadcaster::TransactionBroadcaster;
use crate::types::{
	AsyncPersister, ChainMonitor, ChannelManager, DynStore, DynStoreWrapper, GossipSync, Graph,
	KeysManager, MessageRouter, OnionMessenger, PaymentStore, PeerManager, Persister,
	SyncAndAsyncKVStore,
};
use crate::wallet::persist::KVStoreWalletPersister;
use crate::wallet::Wallet;
use crate::{Node, NodeMetrics};

const LSPS_HARDENED_CHILD_INDEX: u32 = 577;
const PERSISTER_MAX_PENDING_UPDATES: u64 = 100;

#[derive(Debug, Clone)]
enum ChainDataSourceConfig {
	Esplora {
		server_url: String,
		headers: HashMap<String, String>,
		sync_config: Option<EsploraSyncConfig>,
	},
	Electrum {
		server_url: String,
		sync_config: Option<ElectrumSyncConfig>,
	},
	Bitcoind {
		rpc_host: String,
		rpc_port: u16,
		rpc_user: String,
		rpc_password: String,
		rest_client_config: Option<BitcoindRestClientConfig>,
	},
}

#[derive(Debug, Clone)]
enum GossipSourceConfig {
	P2PNetwork,
	RapidGossipSync(String),
}

#[derive(Debug, Clone)]
struct PathfindingScoresSyncConfig {
	url: String,
}

#[derive(Debug, Clone, Default)]
struct LiquiditySourceConfig {
	// Act as an LSPS1 client connecting to the given service.
	lsps1_client: Option<LSPS1ClientConfig>,
	// Act as an LSPS2 client connecting to the given service.
	lsps2_client: Option<LSPS2ClientConfig>,
	// Act as an LSPS2 service.
	lsps2_service: Option<LSPS2ServiceConfig>,
}

#[derive(Clone)]
enum LogWriterConfig {
	File { log_file_path: Option<String>, max_log_level: Option<LogLevel> },
	Log,
	Custom(Arc<dyn LogWriter>),
}

impl std::fmt::Debug for LogWriterConfig {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			LogWriterConfig::File { max_log_level, log_file_path } => f
				.debug_struct("LogWriterConfig")
				.field("max_log_level", max_log_level)
				.field("log_file_path", log_file_path)
				.finish(),
			LogWriterConfig::Log => write!(f, "LogWriterConfig::Log"),
			LogWriterConfig::Custom(_) => {
				f.debug_tuple("Custom").field(&"<config internal to custom log writer>").finish()
			},
		}
	}
}

/// An error encountered during building a [`Node`].
///
/// [`Node`]: crate::Node
#[derive(Debug, Clone, PartialEq)]
pub enum BuildError {
	/// The current system time is invalid, clocks might have gone backwards.
	InvalidSystemTime,
	/// The a read channel monitor is invalid.
	InvalidChannelMonitor,
	/// The given listening addresses are invalid, e.g. too many were passed.
	InvalidListeningAddresses,
	/// The given announcement addresses are invalid, e.g. too many were passed.
	InvalidAnnouncementAddresses,
	/// The provided alias is invalid.
	InvalidNodeAlias,
	/// An attempt to setup a runtime has failed.
	RuntimeSetupFailed,
	/// We failed to read data from the [`KVStore`].
	///
	/// [`KVStore`]: lightning::util::persist::KVStoreSync
	ReadFailed,
	/// We failed to write data to the [`KVStore`].
	///
	/// [`KVStore`]: lightning::util::persist::KVStoreSync
	WriteFailed,
	/// We failed to access the given `storage_dir_path`.
	StoragePathAccessFailed,
	/// We failed to setup our [`KVStore`].
	///
	/// [`KVStore`]: lightning::util::persist::KVStoreSync
	KVStoreSetupFailed,
	/// We failed to setup the onchain wallet.
	WalletSetupFailed,
	/// We failed to setup the logger.
	LoggerSetupFailed,
	/// The given network does not match the node's previously configured network.
	NetworkMismatch,
	/// The role of the node in an asynchronous payments context is not compatible with the current configuration.
	AsyncPaymentsConfigMismatch,
}

impl fmt::Display for BuildError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::InvalidSystemTime => {
				write!(f, "System time is invalid. Clocks might have gone back in time.")
			},
			Self::InvalidChannelMonitor => {
				write!(f, "Failed to watch a deserialized ChannelMonitor")
			},
			Self::InvalidListeningAddresses => write!(f, "Given listening addresses are invalid."),
			Self::InvalidAnnouncementAddresses => {
				write!(f, "Given announcement addresses are invalid.")
			},
			Self::RuntimeSetupFailed => write!(f, "Failed to setup a runtime."),
			Self::ReadFailed => write!(f, "Failed to read from store."),
			Self::WriteFailed => write!(f, "Failed to write to store."),
			Self::StoragePathAccessFailed => write!(f, "Failed to access the given storage path."),
			Self::KVStoreSetupFailed => write!(f, "Failed to setup KVStore."),
			Self::WalletSetupFailed => write!(f, "Failed to setup onchain wallet."),
			Self::LoggerSetupFailed => write!(f, "Failed to setup the logger."),
			Self::InvalidNodeAlias => write!(f, "Given node alias is invalid."),
			Self::NetworkMismatch => {
				write!(f, "Given network does not match the node's previously configured network.")
			},
			Self::AsyncPaymentsConfigMismatch => {
				write!(
					f,
					"The async payments role is not compatible with the current configuration."
				)
			},
		}
	}
}

impl std::error::Error for BuildError {}

/// A builder for an [`Node`] instance, allowing to set some configuration and module choices from
/// the getgo.
///
/// ### Defaults
/// - Wallet entropy is sourced from a `keys_seed` file located under [`Config::storage_dir_path`]
/// - Chain data is sourced from the Esplora endpoint `https://blockstream.info/api`
/// - Gossip data is sourced via the peer-to-peer network
#[derive(Debug)]
pub struct NodeBuilder {
	config: Config,
	chain_data_source_config: Option<ChainDataSourceConfig>,
	gossip_source_config: Option<GossipSourceConfig>,
	liquidity_source_config: Option<LiquiditySourceConfig>,
	log_writer_config: Option<LogWriterConfig>,
	async_payments_role: Option<AsyncPaymentsRole>,
	runtime_handle: Option<tokio::runtime::Handle>,
	pathfinding_scores_sync_config: Option<PathfindingScoresSyncConfig>,
}

impl NodeBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let config = Config::default();
		Self::from_config(config)
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let chain_data_source_config = None;
		let gossip_source_config = None;
		let liquidity_source_config = None;
		let log_writer_config = None;
		let runtime_handle = None;
		let pathfinding_scores_sync_config = None;
		Self {
			config,
			chain_data_source_config,
			gossip_source_config,
			liquidity_source_config,
			log_writer_config,
			runtime_handle,
			async_payments_role: None,
			pathfinding_scores_sync_config,
		}
	}

	/// Configures the [`Node`] instance to (re-)use a specific `tokio` runtime.
	///
	/// If not provided, the node will spawn its own runtime or reuse any outer runtime context it
	/// can detect.
	#[cfg_attr(feature = "uniffi", allow(dead_code))]
	pub fn set_runtime(&mut self, runtime_handle: tokio::runtime::Handle) -> &mut Self {
		self.runtime_handle = Some(runtime_handle);
		self
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	///
	/// If no `sync_config` is given, default values are used. See [`EsploraSyncConfig`] for more
	/// information.
	pub fn set_chain_source_esplora(
		&mut self, server_url: String, sync_config: Option<EsploraSyncConfig>,
	) -> &mut Self {
		self.chain_data_source_config = Some(ChainDataSourceConfig::Esplora {
			server_url,
			headers: Default::default(),
			sync_config,
		});
		self
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	///
	/// The given `headers` will be included in all requests to the Esplora server, typically used for
	/// authentication purposes.
	///
	/// If no `sync_config` is given, default values are used. See [`EsploraSyncConfig`] for more
	/// information.
	pub fn set_chain_source_esplora_with_headers(
		&mut self, server_url: String, headers: HashMap<String, String>,
		sync_config: Option<EsploraSyncConfig>,
	) -> &mut Self {
		self.chain_data_source_config =
			Some(ChainDataSourceConfig::Esplora { server_url, headers, sync_config });
		self
	}

	/// Configures the [`Node`] instance to source its chain data from the given Electrum server.
	///
	/// If no `sync_config` is given, default values are used. See [`ElectrumSyncConfig`] for more
	/// information.
	pub fn set_chain_source_electrum(
		&mut self, server_url: String, sync_config: Option<ElectrumSyncConfig>,
	) -> &mut Self {
		self.chain_data_source_config =
			Some(ChainDataSourceConfig::Electrum { server_url, sync_config });
		self
	}

	/// Configures the [`Node`] instance to connect to a Bitcoin Core node via RPC.
	///
	/// This method establishes an RPC connection that enables all essential chain operations including
	/// transaction broadcasting and chain data synchronization.
	///
	/// ## Parameters:
	/// * `rpc_host`, `rpc_port`, `rpc_user`, `rpc_password` - Required parameters for the Bitcoin Core RPC
	///   connection.
	pub fn set_chain_source_bitcoind_rpc(
		&mut self, rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
	) -> &mut Self {
		self.chain_data_source_config = Some(ChainDataSourceConfig::Bitcoind {
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			rest_client_config: None,
		});
		self
	}

	/// Configures the [`Node`] instance to synchronize chain data from a Bitcoin Core REST endpoint.
	///
	/// This method enables chain data synchronization via Bitcoin Core's REST interface. We pass
	/// additional RPC configuration to non-REST-supported API calls like transaction broadcasting.
	///
	/// ## Parameters:
	/// * `rest_host`, `rest_port` - Required parameters for the Bitcoin Core REST connection.
	/// * `rpc_host`, `rpc_port`, `rpc_user`, `rpc_password` - Required parameters for the Bitcoin Core RPC
	///   connection
	pub fn set_chain_source_bitcoind_rest(
		&mut self, rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16,
		rpc_user: String, rpc_password: String,
	) -> &mut Self {
		self.chain_data_source_config = Some(ChainDataSourceConfig::Bitcoind {
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			rest_client_config: Some(BitcoindRestClientConfig { rest_host, rest_port }),
		});

		self
	}

	/// Configures the [`Node`] instance to source its gossip data from the Lightning peer-to-peer
	/// network.
	pub fn set_gossip_source_p2p(&mut self) -> &mut Self {
		self.gossip_source_config = Some(GossipSourceConfig::P2PNetwork);
		self
	}

	/// Configures the [`Node`] instance to source its gossip data from the given RapidGossipSync
	/// server.
	pub fn set_gossip_source_rgs(&mut self, rgs_server_url: String) -> &mut Self {
		self.gossip_source_config = Some(GossipSourceConfig::RapidGossipSync(rgs_server_url));
		self
	}

	/// Configures the [`Node`] instance to source its external scores from the given URL.
	///
	/// The external scores are merged into the local scoring system to improve routing.
	pub fn set_pathfinding_scores_source(&mut self, url: String) -> &mut Self {
		self.pathfinding_scores_sync_config = Some(PathfindingScoresSyncConfig { url });
		self
	}

	/// Configures the [`Node`] instance to source inbound liquidity from the given
	/// [bLIP-51 / LSPS1] service.
	///
	/// Will mark the LSP as trusted for 0-confirmation channels, see [`Config::trusted_peers_0conf`].
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	///
	/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
	pub fn set_liquidity_source_lsps1(
		&mut self, node_id: PublicKey, address: SocketAddress, token: Option<String>,
	) -> &mut Self {
		// Mark the LSP as trusted for 0conf
		self.config.trusted_peers_0conf.push(node_id.clone());

		let liquidity_source_config =
			self.liquidity_source_config.get_or_insert(LiquiditySourceConfig::default());
		let lsps1_client_config = LSPS1ClientConfig { node_id, address, token };
		liquidity_source_config.lsps1_client = Some(lsps1_client_config);
		self
	}

	/// Configures the [`Node`] instance to source just-in-time inbound liquidity from the given
	/// [bLIP-52 / LSPS2] service.
	///
	/// Will mark the LSP as trusted for 0-confirmation channels, see [`Config::trusted_peers_0conf`].
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	///
	/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
	pub fn set_liquidity_source_lsps2(
		&mut self, node_id: PublicKey, address: SocketAddress, token: Option<String>,
	) -> &mut Self {
		// Mark the LSP as trusted for 0conf
		self.config.trusted_peers_0conf.push(node_id.clone());

		let liquidity_source_config =
			self.liquidity_source_config.get_or_insert(LiquiditySourceConfig::default());
		let lsps2_client_config = LSPS2ClientConfig { node_id, address, token };
		liquidity_source_config.lsps2_client = Some(lsps2_client_config);
		self
	}

	/// Configures the [`Node`] instance to provide an [LSPS2] service, issuing just-in-time
	/// channels to clients.
	///
	/// **Caution**: LSP service support is in **alpha** and is considered an experimental feature.
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	pub fn set_liquidity_provider_lsps2(
		&mut self, service_config: LSPS2ServiceConfig,
	) -> &mut Self {
		let liquidity_source_config =
			self.liquidity_source_config.get_or_insert(LiquiditySourceConfig::default());
		liquidity_source_config.lsps2_service = Some(service_config);
		self
	}

	/// Sets the used storage directory path.
	pub fn set_storage_dir_path(&mut self, storage_dir_path: String) -> &mut Self {
		self.config.storage_dir_path = storage_dir_path;
		self
	}

	/// Configures the [`Node`] instance to write logs to the filesystem.
	///
	/// The `log_file_path` defaults to [`DEFAULT_LOG_FILENAME`] in the configured
	/// [`Config::storage_dir_path`] if set to `None`.
	///
	/// If set, the `max_log_level` sets the maximum log level. Otherwise, the latter defaults to
	/// [`DEFAULT_LOG_LEVEL`].
	///
	/// [`DEFAULT_LOG_FILENAME`]: crate::config::DEFAULT_LOG_FILENAME
	pub fn set_filesystem_logger(
		&mut self, log_file_path: Option<String>, max_log_level: Option<LogLevel>,
	) -> &mut Self {
		self.log_writer_config = Some(LogWriterConfig::File { log_file_path, max_log_level });
		self
	}

	/// Configures the [`Node`] instance to write logs to the [`log`](https://crates.io/crates/log) facade.
	pub fn set_log_facade_logger(&mut self) -> &mut Self {
		self.log_writer_config = Some(LogWriterConfig::Log);
		self
	}

	/// Configures the [`Node`] instance to write logs to the provided custom [`LogWriter`].
	pub fn set_custom_logger(&mut self, log_writer: Arc<dyn LogWriter>) -> &mut Self {
		self.log_writer_config = Some(LogWriterConfig::Custom(log_writer));
		self
	}

	/// Sets the Bitcoin network used.
	pub fn set_network(&mut self, network: Network) -> &mut Self {
		self.config.network = network;
		self
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	pub fn set_listening_addresses(
		&mut self, listening_addresses: Vec<SocketAddress>,
	) -> Result<&mut Self, BuildError> {
		if listening_addresses.len() > 100 {
			return Err(BuildError::InvalidListeningAddresses);
		}

		self.config.listening_addresses = Some(listening_addresses);
		Ok(self)
	}

	/// Sets the IP address and TCP port which [`Node`] will announce to the gossip network that it accepts connections on.
	///
	/// **Note**: If unset, the [`listening_addresses`] will be used as the list of addresses to announce.
	///
	/// [`listening_addresses`]: Self::set_listening_addresses
	pub fn set_announcement_addresses(
		&mut self, announcement_addresses: Vec<SocketAddress>,
	) -> Result<&mut Self, BuildError> {
		if announcement_addresses.len() > 100 {
			return Err(BuildError::InvalidAnnouncementAddresses);
		}

		self.config.announcement_addresses = Some(announcement_addresses);
		Ok(self)
	}

	/// Sets the node alias that will be used when broadcasting announcements to the gossip
	/// network.
	///
	/// The provided alias must be a valid UTF-8 string and no longer than 32 bytes in total.
	pub fn set_node_alias(&mut self, node_alias: String) -> Result<&mut Self, BuildError> {
		let node_alias = sanitize_alias(&node_alias)?;

		self.config.node_alias = Some(node_alias);
		Ok(self)
	}

	/// Sets the role of the node in an asynchronous payments context.
	///
	/// See <https://github.com/lightning/bolts/pull/1149> for more information about the async payments protocol.
	pub fn set_async_payments_role(
		&mut self, role: Option<AsyncPaymentsRole>,
	) -> Result<&mut Self, BuildError> {
		if let Some(AsyncPaymentsRole::Server) = role {
			may_announce_channel(&self.config)
				.map_err(|_| BuildError::AsyncPaymentsConfigMismatch)?;
		}

		self.async_payments_role = role;
		Ok(self)
	}

	/// Builds a [`Node`] instance with a [`SqliteStore`] backend and according to the options
	/// previously configured.
	pub fn build(&self, node_entropy: NodeEntropy) -> Result<Node, BuildError> {
		let storage_dir_path = self.config.storage_dir_path.clone();
		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = SqliteStore::new(
			storage_dir_path.into(),
			Some(io::sqlite_store::SQLITE_DB_FILE_NAME.to_string()),
			Some(io::sqlite_store::KV_TABLE_NAME.to_string()),
		)
		.map_err(|_| BuildError::KVStoreSetupFailed)?;
		self.build_with_store(node_entropy, kv_store)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self, node_entropy: NodeEntropy) -> Result<Node, BuildError> {
		let mut storage_dir_path: PathBuf = self.config.storage_dir_path.clone().into();
		storage_dir_path.push("fs_store");

		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = FilesystemStore::new(storage_dir_path);
		self.build_with_store(node_entropy, kv_store)
	}

	/// Builds a [`Node`] instance with a [VSS] backend and according to the options
	/// previously configured.
	///
	/// Uses [LNURL-auth] based authentication scheme as default method for authentication/authorization.
	///
	/// The LNURL challenge will be retrieved by making a request to the given `lnurl_auth_server_url`.
	/// The returned JWT token in response to the signed LNURL request, will be used for
	/// authentication/authorization of all the requests made to VSS.
	///
	/// `fixed_headers` are included as it is in all the requests made to VSS and LNURL auth server.
	///
	/// **Caution**: VSS support is in **alpha** and is considered experimental.
	/// Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are
	/// unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
	///
	/// [VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md
	/// [LNURL-auth]: https://github.com/lnurl/luds/blob/luds/04.md
	pub fn build_with_vss_store(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		lnurl_auth_server_url: String, fixed_headers: HashMap<String, String>,
	) -> Result<Node, BuildError> {
		let logger = setup_logger(&self.log_writer_config, &self.config)?;
		let builder = VssStoreBuilder::new(node_entropy, vss_url, store_id, self.config.network);
		let vss_store = builder.build(lnurl_auth_server_url, fixed_headers).map_err(|e| {
			log_error!(logger, "Failed to setup VSS store: {}", e);
			BuildError::KVStoreSetupFailed
		})?;

		self.build_with_store(node_entropy, vss_store)
	}

	/// Builds a [`Node`] instance with a [VSS] backend and according to the options
	/// previously configured.
	///
	/// Uses [`FixedHeaders`] as default method for authentication/authorization.
	///
	/// Given `fixed_headers` are included as it is in all the requests made to VSS.
	///
	/// **Caution**: VSS support is in **alpha** and is considered experimental.
	/// Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are
	/// unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
	///
	/// [VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md
	/// [`FixedHeaders`]: vss_client::headers::FixedHeaders
	pub fn build_with_vss_store_and_fixed_headers(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<Node, BuildError> {
		let logger = setup_logger(&self.log_writer_config, &self.config)?;
		let builder = VssStoreBuilder::new(node_entropy, vss_url, store_id, self.config.network);
		let vss_store = builder.build_with_fixed_headers(fixed_headers).map_err(|e| {
			log_error!(logger, "Failed to setup VSS store: {}", e);
			BuildError::KVStoreSetupFailed
		})?;

		self.build_with_store(node_entropy, vss_store)
	}

	/// Builds a [`Node`] instance with a [VSS] backend and according to the options
	/// previously configured.
	///
	/// Given `header_provider` is used to attach headers to every request made
	/// to VSS.
	///
	/// **Caution**: VSS support is in **alpha** and is considered experimental.
	/// Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are
	/// unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
	///
	/// [VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md
	pub fn build_with_vss_store_and_header_provider(
		&self, node_entropy: NodeEntropy, vss_url: String, store_id: String,
		header_provider: Arc<dyn VssHeaderProvider>,
	) -> Result<Node, BuildError> {
		let logger = setup_logger(&self.log_writer_config, &self.config)?;
		let builder = VssStoreBuilder::new(node_entropy, vss_url, store_id, self.config.network);
		let vss_store = builder.build_with_header_provider(header_provider).map_err(|e| {
			log_error!(logger, "Failed to setup VSS store: {}", e);
			BuildError::KVStoreSetupFailed
		})?;

		self.build_with_store(node_entropy, vss_store)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store<S: SyncAndAsyncKVStore + Send + Sync + 'static>(
		&self, node_entropy: NodeEntropy, kv_store: S,
	) -> Result<Node, BuildError> {
		let logger = setup_logger(&self.log_writer_config, &self.config)?;

		let runtime = if let Some(handle) = self.runtime_handle.as_ref() {
			Arc::new(Runtime::with_handle(handle.clone(), Arc::clone(&logger)))
		} else {
			Arc::new(Runtime::new(Arc::clone(&logger)).map_err(|e| {
				log_error!(logger, "Failed to setup tokio runtime: {}", e);
				BuildError::RuntimeSetupFailed
			})?)
		};

		let seed_bytes = node_entropy.to_seed_bytes();
		let config = Arc::new(self.config.clone());

		build_with_store_internal(
			config,
			self.chain_data_source_config.as_ref(),
			self.gossip_source_config.as_ref(),
			self.liquidity_source_config.as_ref(),
			self.pathfinding_scores_sync_config.as_ref(),
			self.async_payments_role,
			seed_bytes,
			runtime,
			logger,
			Arc::new(DynStoreWrapper(kv_store)),
		)
	}
}

/// A builder for an [`Node`] instance, allowing to set some configuration and module choices from
/// the getgo.
///
/// ### Defaults
/// - Wallet entropy is sourced from a `keys_seed` file located under [`Config::storage_dir_path`]
/// - Chain data is sourced from the Esplora endpoint `https://blockstream.info/api`
/// - Gossip data is sourced via the peer-to-peer network
#[derive(Debug)]
#[cfg(feature = "uniffi")]
pub struct ArcedNodeBuilder {
	inner: RwLock<NodeBuilder>,
}

#[cfg(feature = "uniffi")]
impl ArcedNodeBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let inner = RwLock::new(NodeBuilder::new());
		Self { inner }
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let inner = RwLock::new(NodeBuilder::from_config(config));
		Self { inner }
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	///
	/// If no `sync_config` is given, default values are used. See [`EsploraSyncConfig`] for more
	/// information.
	pub fn set_chain_source_esplora(
		&self, server_url: String, sync_config: Option<EsploraSyncConfig>,
	) {
		self.inner.write().unwrap().set_chain_source_esplora(server_url, sync_config);
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	///
	/// The given `headers` will be included in all requests to the Esplora server, typically used for
	/// authentication purposes.
	///
	/// If no `sync_config` is given, default values are used. See [`EsploraSyncConfig`] for more
	/// information.
	pub fn set_chain_source_esplora_with_headers(
		&self, server_url: String, headers: HashMap<String, String>,
		sync_config: Option<EsploraSyncConfig>,
	) {
		self.inner.write().unwrap().set_chain_source_esplora_with_headers(
			server_url,
			headers,
			sync_config,
		);
	}

	/// Configures the [`Node`] instance to source its chain data from the given Electrum server.
	///
	/// If no `sync_config` is given, default values are used. See [`ElectrumSyncConfig`] for more
	/// information.
	pub fn set_chain_source_electrum(
		&self, server_url: String, sync_config: Option<ElectrumSyncConfig>,
	) {
		self.inner.write().unwrap().set_chain_source_electrum(server_url, sync_config);
	}

	/// Configures the [`Node`] instance to connect to a Bitcoin Core node via RPC.
	///
	/// This method establishes an RPC connection that enables all essential chain operations including
	/// transaction broadcasting and chain data synchronization.
	///
	/// ## Parameters:
	/// * `rpc_host`, `rpc_port`, `rpc_user`, `rpc_password` - Required parameters for the Bitcoin Core RPC
	///   connection.
	pub fn set_chain_source_bitcoind_rpc(
		&self, rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
	) {
		self.inner.write().unwrap().set_chain_source_bitcoind_rpc(
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
		);
	}

	/// Configures the [`Node`] instance to synchronize chain data from a Bitcoin Core REST endpoint.
	///
	/// This method enables chain data synchronization via Bitcoin Core's REST interface. We pass
	/// additional RPC configuration to non-REST-supported API calls like transaction broadcasting.
	///
	/// ## Parameters:
	/// * `rest_host`, `rest_port` - Required parameters for the Bitcoin Core REST connection.
	/// * `rpc_host`, `rpc_port`, `rpc_user`, `rpc_password` - Required parameters for the Bitcoin Core RPC
	///   connection
	pub fn set_chain_source_bitcoind_rest(
		&self, rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16,
		rpc_user: String, rpc_password: String,
	) {
		self.inner.write().unwrap().set_chain_source_bitcoind_rest(
			rest_host,
			rest_port,
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
		);
	}

	/// Configures the [`Node`] instance to source its gossip data from the Lightning peer-to-peer
	/// network.
	pub fn set_gossip_source_p2p(&self) {
		self.inner.write().unwrap().set_gossip_source_p2p();
	}

	/// Configures the [`Node`] instance to source its gossip data from the given RapidGossipSync
	/// server.
	pub fn set_gossip_source_rgs(&self, rgs_server_url: String) {
		self.inner.write().unwrap().set_gossip_source_rgs(rgs_server_url);
	}

	/// Configures the [`Node`] instance to source its external scores from the given URL.
	///
	/// The external scores are merged into the local scoring system to improve routing.
	pub fn set_pathfinding_scores_source(&self, url: String) {
		self.inner.write().unwrap().set_pathfinding_scores_source(url);
	}

	/// Configures the [`Node`] instance to source inbound liquidity from the given
	/// [bLIP-51 / LSPS1] service.
	///
	/// Will mark the LSP as trusted for 0-confirmation channels, see [`Config::trusted_peers_0conf`].
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	///
	/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
	pub fn set_liquidity_source_lsps1(
		&self, node_id: PublicKey, address: SocketAddress, token: Option<String>,
	) {
		self.inner.write().unwrap().set_liquidity_source_lsps1(node_id, address, token);
	}

	/// Configures the [`Node`] instance to source just-in-time inbound liquidity from the given
	/// [bLIP-52 / LSPS2] service.
	///
	/// Will mark the LSP as trusted for 0-confirmation channels, see [`Config::trusted_peers_0conf`].
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	///
	/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
	pub fn set_liquidity_source_lsps2(
		&self, node_id: PublicKey, address: SocketAddress, token: Option<String>,
	) {
		self.inner.write().unwrap().set_liquidity_source_lsps2(node_id, address, token);
	}

	/// Configures the [`Node`] instance to provide an [LSPS2] service, issuing just-in-time
	/// channels to clients.
	///
	/// **Caution**: LSP service support is in **alpha** and is considered an experimental feature.
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	pub fn set_liquidity_provider_lsps2(&self, service_config: LSPS2ServiceConfig) {
		self.inner.write().unwrap().set_liquidity_provider_lsps2(service_config);
	}

	/// Sets the used storage directory path.
	pub fn set_storage_dir_path(&self, storage_dir_path: String) {
		self.inner.write().unwrap().set_storage_dir_path(storage_dir_path);
	}

	/// Configures the [`Node`] instance to write logs to the filesystem.
	///
	/// The `log_file_path` defaults to [`DEFAULT_LOG_FILENAME`] in the configured
	/// [`Config::storage_dir_path`] if set to `None`.
	///
	/// If set, the `max_log_level` sets the maximum log level. Otherwise, the latter defaults to
	/// [`DEFAULT_LOG_LEVEL`].
	///
	/// [`DEFAULT_LOG_FILENAME`]: crate::config::DEFAULT_LOG_FILENAME
	pub fn set_filesystem_logger(
		&self, log_file_path: Option<String>, log_level: Option<LogLevel>,
	) {
		self.inner.write().unwrap().set_filesystem_logger(log_file_path, log_level);
	}

	/// Configures the [`Node`] instance to write logs to the [`log`](https://crates.io/crates/log) facade.
	pub fn set_log_facade_logger(&self) {
		self.inner.write().unwrap().set_log_facade_logger();
	}

	/// Configures the [`Node`] instance to write logs to the provided custom [`LogWriter`].
	pub fn set_custom_logger(&self, log_writer: Arc<dyn LogWriter>) {
		self.inner.write().unwrap().set_custom_logger(log_writer);
	}

	/// Sets the Bitcoin network used.
	pub fn set_network(&self, network: Network) {
		self.inner.write().unwrap().set_network(network);
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	pub fn set_listening_addresses(
		&self, listening_addresses: Vec<SocketAddress>,
	) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_listening_addresses(listening_addresses).map(|_| ())
	}

	/// Sets the IP address and TCP port which [`Node`] will announce to the gossip network that it accepts connections on.
	///
	/// **Note**: If unset, the [`listening_addresses`] will be used as the list of addresses to announce.
	///
	/// [`listening_addresses`]: Self::set_listening_addresses
	pub fn set_announcement_addresses(
		&self, announcement_addresses: Vec<SocketAddress>,
	) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_announcement_addresses(announcement_addresses).map(|_| ())
	}

	/// Sets the node alias that will be used when broadcasting announcements to the gossip
	/// network.
	///
	/// The provided alias must be a valid UTF-8 string and no longer than 32 bytes in total.
	pub fn set_node_alias(&self, node_alias: String) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_node_alias(node_alias).map(|_| ())
	}

	/// Sets the role of the node in an asynchronous payments context.
	pub fn set_async_payments_role(
		&self, role: Option<AsyncPaymentsRole>,
	) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_async_payments_role(role).map(|_| ())
	}

	/// Builds a [`Node`] instance with a [`SqliteStore`] backend and according to the options
	/// previously configured.
	pub fn build(&self, node_entropy: Arc<NodeEntropy>) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build(*node_entropy).map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(
		&self, node_entropy: Arc<NodeEntropy>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build_with_fs_store(*node_entropy).map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [VSS] backend and according to the options
	/// previously configured.
	///
	/// Uses [LNURL-auth] based authentication scheme as default method for authentication/authorization.
	///
	/// The LNURL challenge will be retrieved by making a request to the given `lnurl_auth_server_url`.
	/// The returned JWT token in response to the signed LNURL request, will be used for
	/// authentication/authorization of all the requests made to VSS.
	///
	/// `fixed_headers` are included as it is in all the requests made to VSS and LNURL auth server.
	///
	/// **Caution**: VSS support is in **alpha** and is considered experimental.
	/// Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are
	/// unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
	///
	/// [VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md
	/// [LNURL-auth]: https://github.com/lnurl/luds/blob/luds/04.md
	pub fn build_with_vss_store(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		lnurl_auth_server_url: String, fixed_headers: HashMap<String, String>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner
			.read()
			.unwrap()
			.build_with_vss_store(
				*node_entropy,
				vss_url,
				store_id,
				lnurl_auth_server_url,
				fixed_headers,
			)
			.map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [VSS] backend and according to the options
	/// previously configured.
	///
	/// Uses [`FixedHeaders`] as default method for authentication/authorization.
	///
	/// Given `fixed_headers` are included as it is in all the requests made to VSS.
	///
	/// **Caution**: VSS support is in **alpha** and is considered experimental.
	/// Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are
	/// unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
	///
	/// [VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md
	pub fn build_with_vss_store_and_fixed_headers(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner
			.read()
			.unwrap()
			.build_with_vss_store_and_fixed_headers(*node_entropy, vss_url, store_id, fixed_headers)
			.map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [VSS] backend and according to the options
	/// previously configured.
	///
	/// Given `header_provider` is used to attach headers to every request made
	/// to VSS.
	///
	/// **Caution**: VSS support is in **alpha** and is considered experimental.
	/// Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are
	/// unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
	///
	/// [VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md
	pub fn build_with_vss_store_and_header_provider(
		&self, node_entropy: Arc<NodeEntropy>, vss_url: String, store_id: String,
		header_provider: Arc<dyn VssHeaderProvider>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner
			.read()
			.unwrap()
			.build_with_vss_store_and_header_provider(
				*node_entropy,
				vss_url,
				store_id,
				header_provider,
			)
			.map(Arc::new)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	// Note that the generics here don't actually work for Uniffi, but we don't currently expose
	// this so its not needed.
	pub fn build_with_store<S: SyncAndAsyncKVStore + Send + Sync + 'static>(
		&self, node_entropy: Arc<NodeEntropy>, kv_store: S,
	) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build_with_store(*node_entropy, kv_store).map(Arc::new)
	}
}

/// Builds a [`Node`] instance according to the options previously configured.
fn build_with_store_internal(
	config: Arc<Config>, chain_data_source_config: Option<&ChainDataSourceConfig>,
	gossip_source_config: Option<&GossipSourceConfig>,
	liquidity_source_config: Option<&LiquiditySourceConfig>,
	pathfinding_scores_sync_config: Option<&PathfindingScoresSyncConfig>,
	async_payments_role: Option<AsyncPaymentsRole>, seed_bytes: [u8; 64], runtime: Arc<Runtime>,
	logger: Arc<Logger>, kv_store: Arc<DynStore>,
) -> Result<Node, BuildError> {
	optionally_install_rustls_cryptoprovider();

	if let Err(err) = may_announce_channel(&config) {
		if config.announcement_addresses.is_some() {
			log_error!(logger, "Announcement addresses were set but some required configuration options for node announcement are missing: {}", err);
			let build_error = if matches!(err, AnnounceError::MissingNodeAlias) {
				BuildError::InvalidNodeAlias
			} else {
				BuildError::InvalidListeningAddresses
			};
			return Err(build_error);
		}

		if config.node_alias.is_some() {
			log_error!(logger, "Node alias was set but some required configuration options for node announcement are missing: {}", err);
			return Err(BuildError::InvalidListeningAddresses);
		}
	}

	let tx_broadcaster = Arc::new(TransactionBroadcaster::new(Arc::clone(&logger)));
	let fee_estimator = Arc::new(OnchainFeeEstimator::new());

	let kv_store_ref = Arc::clone(&kv_store);
	let logger_ref = Arc::clone(&logger);
	let (payment_store_res, node_metris_res) = runtime.block_on(async move {
		tokio::join!(
			read_payments(&*kv_store_ref, Arc::clone(&logger_ref)),
			read_node_metrics(&*kv_store_ref, Arc::clone(&logger_ref)),
		)
	});

	// Initialize the status fields.
	let node_metrics = match node_metris_res {
		Ok(metrics) => Arc::new(RwLock::new(metrics)),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(RwLock::new(NodeMetrics::default()))
			} else {
				log_error!(logger, "Failed to read node metrics from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let payment_store = match payment_store_res {
		Ok(payments) => Arc::new(PaymentStore::new(
			payments,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		)),
		Err(e) => {
			log_error!(logger, "Failed to read payment data from store: {}", e);
			return Err(BuildError::ReadFailed);
		},
	};

	let (chain_source, chain_tip_opt) = match chain_data_source_config {
		Some(ChainDataSourceConfig::Esplora { server_url, headers, sync_config }) => {
			let sync_config = sync_config.unwrap_or(EsploraSyncConfig::default());
			ChainSource::new_esplora(
				server_url.clone(),
				headers.clone(),
				sync_config,
				Arc::clone(&fee_estimator),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&kv_store),
				Arc::clone(&config),
				Arc::clone(&logger),
				Arc::clone(&node_metrics),
			)
		},
		Some(ChainDataSourceConfig::Electrum { server_url, sync_config }) => {
			let sync_config = sync_config.unwrap_or(ElectrumSyncConfig::default());
			ChainSource::new_electrum(
				server_url.clone(),
				sync_config,
				Arc::clone(&fee_estimator),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&kv_store),
				Arc::clone(&config),
				Arc::clone(&logger),
				Arc::clone(&node_metrics),
			)
		},
		Some(ChainDataSourceConfig::Bitcoind {
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
			rest_client_config,
		}) => match rest_client_config {
			Some(rest_client_config) => runtime.block_on(async {
				ChainSource::new_bitcoind_rest(
					rpc_host.clone(),
					*rpc_port,
					rpc_user.clone(),
					rpc_password.clone(),
					Arc::clone(&fee_estimator),
					Arc::clone(&tx_broadcaster),
					Arc::clone(&kv_store),
					Arc::clone(&config),
					rest_client_config.clone(),
					Arc::clone(&logger),
					Arc::clone(&node_metrics),
				)
				.await
			}),
			None => runtime.block_on(async {
				ChainSource::new_bitcoind_rpc(
					rpc_host.clone(),
					*rpc_port,
					rpc_user.clone(),
					rpc_password.clone(),
					Arc::clone(&fee_estimator),
					Arc::clone(&tx_broadcaster),
					Arc::clone(&kv_store),
					Arc::clone(&config),
					Arc::clone(&logger),
					Arc::clone(&node_metrics),
				)
				.await
			}),
		},

		None => {
			// Default to Esplora client.
			let server_url = DEFAULT_ESPLORA_SERVER_URL.to_string();
			let sync_config = EsploraSyncConfig::default();
			ChainSource::new_esplora(
				server_url.clone(),
				HashMap::new(),
				sync_config,
				Arc::clone(&fee_estimator),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&kv_store),
				Arc::clone(&config),
				Arc::clone(&logger),
				Arc::clone(&node_metrics),
			)
		},
	};
	let chain_source = Arc::new(chain_source);

	// Initialize the on-chain wallet and chain access
	let xprv = bitcoin::bip32::Xpriv::new_master(config.network, &seed_bytes).map_err(|e| {
		log_error!(logger, "Failed to derive master secret: {}", e);
		BuildError::WalletSetupFailed
	})?;

	let descriptor = Bip84(xprv, KeychainKind::External);
	let change_descriptor = Bip84(xprv, KeychainKind::Internal);
	let mut wallet_persister =
		KVStoreWalletPersister::new(Arc::clone(&kv_store), Arc::clone(&logger));
	let wallet_opt = BdkWallet::load()
		.descriptor(KeychainKind::External, Some(descriptor.clone()))
		.descriptor(KeychainKind::Internal, Some(change_descriptor.clone()))
		.extract_keys()
		.check_network(config.network)
		.load_wallet(&mut wallet_persister)
		.map_err(|e| match e {
			bdk_wallet::LoadWithPersistError::InvalidChangeSet(
				bdk_wallet::LoadError::Mismatch(bdk_wallet::LoadMismatch::Network {
					loaded,
					expected,
				}),
			) => {
				log_error!(
					logger,
					"Failed to setup wallet: Networks do not match. Expected {} but got {}",
					expected,
					loaded
				);
				BuildError::NetworkMismatch
			},
			_ => {
				log_error!(logger, "Failed to set up wallet: {}", e);
				BuildError::WalletSetupFailed
			},
		})?;
	let bdk_wallet = match wallet_opt {
		Some(wallet) => wallet,
		None => {
			let mut wallet = BdkWallet::create(descriptor, change_descriptor)
				.network(config.network)
				.create_wallet(&mut wallet_persister)
				.map_err(|e| {
					log_error!(logger, "Failed to set up wallet: {}", e);
					BuildError::WalletSetupFailed
				})?;

			if let Some(best_block) = chain_tip_opt {
				// Insert the first checkpoint if we have it, to avoid resyncing from genesis.
				// TODO: Use a proper wallet birthday once BDK supports it.
				let mut latest_checkpoint = wallet.latest_checkpoint();
				let block_id =
					bdk_chain::BlockId { height: best_block.height, hash: best_block.block_hash };
				latest_checkpoint = latest_checkpoint.insert(block_id);
				let update =
					bdk_wallet::Update { chain: Some(latest_checkpoint), ..Default::default() };
				wallet.apply_update(update).map_err(|e| {
					log_error!(logger, "Failed to apply checkpoint during wallet setup: {}", e);
					BuildError::WalletSetupFailed
				})?;
			}
			wallet
		},
	};

	let wallet = Arc::new(Wallet::new(
		bdk_wallet,
		wallet_persister,
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
		Arc::clone(&payment_store),
		Arc::clone(&config),
		Arc::clone(&logger),
	));

	// Initialize the KeysManager
	let cur_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_err(|e| {
		log_error!(logger, "Failed to get current time: {}", e);
		BuildError::InvalidSystemTime
	})?;

	let ldk_seed_bytes: [u8; 32] = xprv.private_key.secret_bytes();
	let keys_manager = Arc::new(KeysManager::new(
		&ldk_seed_bytes,
		cur_time.as_secs(),
		cur_time.subsec_nanos(),
		Arc::clone(&wallet),
		Arc::clone(&logger),
	));

	let peer_storage_key = keys_manager.get_peer_storage_key();
	let monitor_reader = Arc::new(AsyncPersister::new(
		Arc::clone(&kv_store),
		RuntimeSpawner::new(Arc::clone(&runtime)),
		Arc::clone(&logger),
		PERSISTER_MAX_PENDING_UPDATES,
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
	));

	// Read ChannelMonitors and the NetworkGraph
	let kv_store_ref = Arc::clone(&kv_store);
	let logger_ref = Arc::clone(&logger);
	let (monitor_read_res, network_graph_res) = runtime.block_on(async move {
		tokio::join!(
			monitor_reader.read_all_channel_monitors_with_updates_parallel(),
			read_network_graph(&*kv_store_ref, logger_ref),
		)
	});

	// Read ChannelMonitor state from store
	let channel_monitors = match monitor_read_res {
		Ok(monitors) => monitors,
		Err(e) => {
			if e.kind() == lightning::io::ErrorKind::NotFound {
				Vec::new()
			} else {
				log_error!(logger, "Failed to read channel monitors from store: {}", e.to_string());
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let persister = Arc::new(Persister::new(
		Arc::clone(&kv_store),
		Arc::clone(&logger),
		PERSISTER_MAX_PENDING_UPDATES,
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
	));

	// Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		Some(Arc::clone(&chain_source)),
		Arc::clone(&tx_broadcaster),
		Arc::clone(&logger),
		Arc::clone(&fee_estimator),
		Arc::clone(&persister),
		Arc::clone(&keys_manager),
		peer_storage_key,
	));

	// Initialize the network graph, scorer, and router
	let network_graph = match network_graph_res {
		Ok(graph) => Arc::new(graph),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(Graph::new(config.network.into(), Arc::clone(&logger)))
			} else {
				log_error!(logger, "Failed to read network graph from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	// Read various smaller LDK and ldk-node objects from the store
	let kv_store_ref = Arc::clone(&kv_store);
	let logger_ref = Arc::clone(&logger);
	let network_graph_ref = Arc::clone(&network_graph);
	let output_sweeper_future = read_output_sweeper(
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
		Arc::clone(&chain_source),
		Arc::clone(&keys_manager),
		Arc::clone(&kv_store_ref),
		Arc::clone(&logger_ref),
	);
	let (
		scorer_res,
		external_scores_res,
		channel_manager_bytes_res,
		sweeper_bytes_res,
		event_queue_res,
		peer_info_res,
	) = runtime.block_on(async move {
		tokio::join!(
			read_scorer(&*kv_store_ref, network_graph_ref, Arc::clone(&logger_ref)),
			read_external_pathfinding_scores_from_cache(&*kv_store_ref, Arc::clone(&logger_ref)),
			KVStore::read(
				&*kv_store_ref,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			),
			output_sweeper_future,
			read_event_queue(Arc::clone(&kv_store_ref), Arc::clone(&logger_ref)),
			read_peer_info(Arc::clone(&kv_store_ref), Arc::clone(&logger_ref)),
		)
	});

	let local_scorer = match scorer_res {
		Ok(scorer) => scorer,
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				let params = ProbabilisticScoringDecayParameters::default();
				ProbabilisticScorer::new(params, Arc::clone(&network_graph), Arc::clone(&logger))
			} else {
				log_error!(logger, "Failed to read scoring data from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let scorer = Arc::new(Mutex::new(CombinedScorer::new(local_scorer)));

	// Restore external pathfinding scores from cache if possible.
	match external_scores_res {
		Ok(external_scores) => {
			scorer.lock().unwrap().merge(external_scores, cur_time);
			log_trace!(logger, "External scores from cache merged successfully");
		},
		Err(e) => {
			if e.kind() != std::io::ErrorKind::NotFound {
				log_error!(logger, "Error while reading external scores from cache: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	}

	let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
	let router = Arc::new(DefaultRouter::new(
		Arc::clone(&network_graph),
		Arc::clone(&logger),
		Arc::clone(&keys_manager),
		Arc::clone(&scorer),
		scoring_fee_params,
	));

	let mut user_config = default_user_config(&config);

	if liquidity_source_config.and_then(|lsc| lsc.lsps2_service.as_ref()).is_some() {
		// If we act as an LSPS2 service, we need to be able to intercept HTLCs and forward the
		// information to the service handler.
		user_config.accept_intercept_htlcs = true;

		// If we act as an LSPS2 service, we allow forwarding to unannounced channels.
		user_config.accept_forwards_to_priv_channels = true;

		// If we act as an LSPS2 service, set the HTLC-value-in-flight to 100% of the channel value
		// to ensure we can forward the initial payment.
		user_config.channel_handshake_config.max_inbound_htlc_value_in_flight_percent_of_channel =
			100;
	}

	if let Some(role) = async_payments_role {
		match role {
			AsyncPaymentsRole::Server => {
				user_config.accept_forwards_to_priv_channels = true;
				user_config.enable_htlc_hold = true;
			},
			AsyncPaymentsRole::Client => user_config.hold_outbound_htlcs_at_next_hop = true,
		}
	}

	let message_router =
		Arc::new(MessageRouter::new(Arc::clone(&network_graph), Arc::clone(&keys_manager)));

	// Initialize the ChannelManager
	let channel_manager = {
		if let Ok(reader) = channel_manager_bytes_res {
			let channel_monitor_references =
				channel_monitors.iter().map(|(_, chanmon)| chanmon).collect();
			let read_args = ChannelManagerReadArgs::new(
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&fee_estimator),
				Arc::clone(&chain_monitor),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&router),
				Arc::clone(&message_router),
				Arc::clone(&logger),
				user_config,
				channel_monitor_references,
			);
			let (_hash, channel_manager) =
				<(BlockHash, ChannelManager)>::read(&mut &*reader, read_args).map_err(|e| {
					log_error!(logger, "Failed to read channel manager from store: {}", e);
					BuildError::ReadFailed
				})?;
			channel_manager
		} else {
			// We're starting a fresh node.
			let best_block =
				chain_tip_opt.unwrap_or_else(|| BestBlock::from_network(config.network));

			let chain_params = ChainParameters { network: config.network.into(), best_block };
			channelmanager::ChannelManager::new(
				Arc::clone(&fee_estimator),
				Arc::clone(&chain_monitor),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&router),
				Arc::clone(&message_router),
				Arc::clone(&logger),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				user_config,
				chain_params,
				cur_time.as_secs() as u32,
			)
		}
	};

	let channel_manager = Arc::new(channel_manager);

	// Give ChannelMonitors to ChainMonitor
	for (_blockhash, channel_monitor) in channel_monitors.into_iter() {
		let channel_id = channel_monitor.channel_id();
		chain_monitor.load_existing_monitor(channel_id, channel_monitor).map_err(|e| {
			log_error!(logger, "Failed to load channel monitor: {:?}", e);
			BuildError::InvalidChannelMonitor
		})?;
	}

	let hrn_resolver = Arc::new(LDKOnionMessageDNSSECHrnResolver::new(Arc::clone(&network_graph)));

	// Initialize the PeerManager
	let onion_messenger: Arc<OnionMessenger> =
		if let Some(AsyncPaymentsRole::Server) = async_payments_role {
			Arc::new(OnionMessenger::new_with_offline_peer_interception(
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&logger),
				Arc::clone(&channel_manager),
				message_router,
				Arc::clone(&channel_manager),
				Arc::clone(&channel_manager),
				Arc::clone(&hrn_resolver),
				IgnoringMessageHandler {},
			))
		} else {
			Arc::new(OnionMessenger::new(
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&logger),
				Arc::clone(&channel_manager),
				message_router,
				Arc::clone(&channel_manager),
				Arc::clone(&channel_manager),
				Arc::clone(&hrn_resolver),
				IgnoringMessageHandler {},
			))
		};
	let ephemeral_bytes: [u8; 32] = keys_manager.get_secure_random_bytes();

	// Initialize the GossipSource
	// Use the configured gossip source, if the user set one, otherwise default to P2PNetwork.
	let gossip_source_config = gossip_source_config.unwrap_or(&GossipSourceConfig::P2PNetwork);

	let gossip_source = match gossip_source_config {
		GossipSourceConfig::P2PNetwork => {
			let p2p_source = Arc::new(GossipSource::new_p2p(
				Arc::clone(&network_graph),
				Arc::clone(&chain_source),
				Arc::clone(&runtime),
				Arc::clone(&logger),
			));

			// Reset the RGS sync timestamp in case we somehow switch gossip sources
			{
				let mut locked_node_metrics = node_metrics.write().unwrap();
				locked_node_metrics.latest_rgs_snapshot_timestamp = None;
				write_node_metrics(&*locked_node_metrics, &*kv_store, Arc::clone(&logger))
					.map_err(|e| {
						log_error!(logger, "Failed writing to store: {}", e);
						BuildError::WriteFailed
					})?;
			}
			p2p_source
		},
		GossipSourceConfig::RapidGossipSync(rgs_server) => {
			let latest_sync_timestamp =
				node_metrics.read().unwrap().latest_rgs_snapshot_timestamp.unwrap_or(0);
			Arc::new(GossipSource::new_rgs(
				rgs_server.clone(),
				latest_sync_timestamp,
				Arc::clone(&network_graph),
				Arc::clone(&logger),
			))
		},
	};

	let (liquidity_source, custom_message_handler) =
		if let Some(lsc) = liquidity_source_config.as_ref() {
			let mut liquidity_source_builder = LiquiditySourceBuilder::new(
				Arc::clone(&wallet),
				Arc::clone(&channel_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&chain_source),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&kv_store),
				Arc::clone(&config),
				Arc::clone(&logger),
			);

			lsc.lsps1_client.as_ref().map(|config| {
				liquidity_source_builder.lsps1_client(
					config.node_id,
					config.address.clone(),
					config.token.clone(),
				)
			});

			lsc.lsps2_client.as_ref().map(|config| {
				liquidity_source_builder.lsps2_client(
					config.node_id,
					config.address.clone(),
					config.token.clone(),
				)
			});

			let promise_secret = {
				let lsps_xpriv = derive_xprv(
					Arc::clone(&config),
					&seed_bytes,
					LSPS_HARDENED_CHILD_INDEX,
					Arc::clone(&logger),
				)?;
				lsps_xpriv.private_key.secret_bytes()
			};
			lsc.lsps2_service.as_ref().map(|config| {
				liquidity_source_builder.lsps2_service(promise_secret, config.clone())
			});

			let liquidity_source = runtime
				.block_on(async move { liquidity_source_builder.build().await.map(Arc::new) })?;
			let custom_message_handler =
				Arc::new(NodeCustomMessageHandler::new_liquidity(Arc::clone(&liquidity_source)));
			(Some(liquidity_source), custom_message_handler)
		} else {
			(None, Arc::new(NodeCustomMessageHandler::new_ignoring()))
		};

	let msg_handler = match gossip_source.as_gossip_sync() {
		GossipSync::P2P(p2p_gossip_sync) => MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::clone(&p2p_gossip_sync)
				as Arc<dyn RoutingMessageHandler + Sync + Send>,
			onion_message_handler: Arc::clone(&onion_messenger),
			custom_message_handler,
			send_only_message_handler: Arc::clone(&chain_monitor),
		},
		GossipSync::Rapid(_) => MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::new(IgnoringMessageHandler {})
				as Arc<dyn RoutingMessageHandler + Sync + Send>,
			onion_message_handler: Arc::clone(&onion_messenger),
			custom_message_handler,
			send_only_message_handler: Arc::clone(&chain_monitor),
		},
		GossipSync::None => {
			unreachable!("We must always have a gossip sync!");
		},
	};

	let cur_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_err(|e| {
		log_error!(logger, "Failed to get current time: {}", e);
		BuildError::InvalidSystemTime
	})?;

	let peer_manager = Arc::new(PeerManager::new(
		msg_handler,
		cur_time.as_secs().try_into().map_err(|e| {
			log_error!(logger, "Failed to get current time: {}", e);
			BuildError::InvalidSystemTime
		})?,
		&ephemeral_bytes,
		Arc::clone(&logger),
		Arc::clone(&keys_manager),
	));

	let peer_manager_clone = Arc::downgrade(&peer_manager);
	hrn_resolver.register_post_queue_action(Box::new(move || {
		if let Some(upgraded_pointer) = peer_manager_clone.upgrade() {
			upgraded_pointer.process_events();
		}
	}));

	liquidity_source.as_ref().map(|l| l.set_peer_manager(Arc::downgrade(&peer_manager)));

	let connection_manager =
		Arc::new(ConnectionManager::new(Arc::clone(&peer_manager), Arc::clone(&logger)));

	let output_sweeper = match sweeper_bytes_res {
		Ok(output_sweeper) => Arc::new(output_sweeper),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(OutputSweeper::new(
					channel_manager.current_best_block(),
					Arc::clone(&tx_broadcaster),
					Arc::clone(&fee_estimator),
					Some(Arc::clone(&chain_source)),
					Arc::clone(&keys_manager),
					Arc::clone(&keys_manager),
					Arc::clone(&kv_store),
					Arc::clone(&logger),
				))
			} else {
				log_error!(logger, "Failed to read output sweeper data from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let event_queue = match event_queue_res {
		Ok(event_queue) => Arc::new(event_queue),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(EventQueue::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				log_error!(logger, "Failed to read event queue from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let peer_store = match peer_info_res {
		Ok(peer_store) => Arc::new(peer_store),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(PeerStore::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				log_error!(logger, "Failed to read peer data from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let om_mailbox = if let Some(AsyncPaymentsRole::Server) = async_payments_role {
		Some(Arc::new(OnionMessageMailbox::new()))
	} else {
		None
	};

	let lnurl_auth = Arc::new(LnurlAuth::new(&keys_manager, Arc::clone(&logger)));

	let (stop_sender, _) = tokio::sync::watch::channel(());
	let (background_processor_stop_sender, _) = tokio::sync::watch::channel(());
	let is_running = Arc::new(RwLock::new(false));

	let pathfinding_scores_sync_url = pathfinding_scores_sync_config.map(|c| c.url.clone());

	#[cfg(cycle_tests)]
	let mut _leak_checker = crate::LeakChecker(Vec::new());
	#[cfg(cycle_tests)]
	{
		use std::any::Any;
		use std::sync::Weak;

		_leak_checker.0.push(Arc::downgrade(&channel_manager) as Weak<dyn Any + Send + Sync>);
		_leak_checker.0.push(Arc::downgrade(&network_graph) as Weak<dyn Any + Send + Sync>);
		_leak_checker.0.push(Arc::downgrade(&wallet) as Weak<dyn Any + Send + Sync>);
	}

	Ok(Node {
		runtime,
		stop_sender,
		background_processor_stop_sender,
		config,
		wallet,
		chain_source,
		tx_broadcaster,
		fee_estimator,
		event_queue,
		channel_manager,
		chain_monitor,
		output_sweeper,
		peer_manager,
		onion_messenger,
		connection_manager,
		keys_manager,
		network_graph,
		gossip_source,
		pathfinding_scores_sync_url,
		liquidity_source,
		kv_store,
		logger,
		_router: router,
		scorer,
		peer_store,
		payment_store,
		lnurl_auth,
		is_running,
		node_metrics,
		om_mailbox,
		async_payments_role,
		hrn_resolver,
		#[cfg(cycle_tests)]
		_leak_checker,
	})
}

fn optionally_install_rustls_cryptoprovider() {
	// Acquire a global Mutex, ensuring that only one process at a time install the provider. This
	// is mostly required for running tests concurrently.
	static INIT_CRYPTO: Once = Once::new();

	INIT_CRYPTO.call_once(|| {
		// Ensure we always install a `CryptoProvider` for `rustls` if it was somehow not previously installed by now.
		if rustls::crypto::CryptoProvider::get_default().is_none() {
			let _ = rustls::crypto::ring::default_provider().install_default();
		}

		// Refuse to startup without TLS support. Better to catch it now than even later at runtime.
		assert!(
			rustls::crypto::CryptoProvider::get_default().is_some(),
			"We need to have a CryptoProvider"
		);
	});
}

/// Sets up the node logger.
fn setup_logger(
	log_writer_config: &Option<LogWriterConfig>, config: &Config,
) -> Result<Arc<Logger>, BuildError> {
	let logger = match log_writer_config {
		Some(LogWriterConfig::File { log_file_path, max_log_level }) => {
			let log_file_path = log_file_path
				.clone()
				.unwrap_or_else(|| format!("{}/{}", config.storage_dir_path, DEFAULT_LOG_FILENAME));
			let max_log_level = max_log_level.unwrap_or_else(|| DEFAULT_LOG_LEVEL);

			Logger::new_fs_writer(log_file_path, max_log_level)
				.map_err(|_| BuildError::LoggerSetupFailed)?
		},
		Some(LogWriterConfig::Log) => Logger::new_log_facade(),

		Some(LogWriterConfig::Custom(custom_log_writer)) => {
			Logger::new_custom_writer(Arc::clone(&custom_log_writer))
		},
		None => {
			// Default to use `FileWriter`
			let log_file_path = format!("{}/{}", config.storage_dir_path, DEFAULT_LOG_FILENAME);
			let log_level = DEFAULT_LOG_LEVEL;
			Logger::new_fs_writer(log_file_path, log_level)
				.map_err(|_| BuildError::LoggerSetupFailed)?
		},
	};

	Ok(Arc::new(logger))
}

fn derive_xprv(
	config: Arc<Config>, seed_bytes: &[u8; 64], hardened_child_index: u32, logger: Arc<Logger>,
) -> Result<Xpriv, BuildError> {
	let xprv = Xpriv::new_master(config.network, seed_bytes).map_err(|e| {
		log_error!(logger, "Failed to derive master secret: {}", e);
		BuildError::WalletSetupFailed
	})?;

	xprv.derive_priv(&Secp256k1::new(), &[ChildNumber::Hardened { index: hardened_child_index }])
		.map_err(|e| {
			log_error!(logger, "Failed to derive hardened child secret: {}", e);
			BuildError::WalletSetupFailed
		})
}

/// Sanitize the user-provided node alias to ensure that it is a valid protocol-specified UTF-8 string.
pub(crate) fn sanitize_alias(alias_str: &str) -> Result<NodeAlias, BuildError> {
	let alias = alias_str.trim();

	// Alias must be 32-bytes long or less.
	if alias.as_bytes().len() > 32 {
		return Err(BuildError::InvalidNodeAlias);
	}

	let mut bytes = [0u8; 32];
	bytes[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());
	Ok(NodeAlias(bytes))
}

#[cfg(test)]
mod tests {
	use super::{sanitize_alias, BuildError, NodeAlias};

	#[test]
	fn sanitize_empty_node_alias() {
		// Empty node alias
		let alias = "";
		let mut buf = [0u8; 32];
		buf[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());

		let expected_node_alias = NodeAlias([0; 32]);
		let node_alias = sanitize_alias(alias).unwrap();
		assert_eq!(node_alias, expected_node_alias);
	}

	#[test]
	fn sanitize_alias_with_sandwiched_null() {
		// Alias with emojis
		let alias = "I\u{1F496}LDK-Node!";
		let mut buf = [0u8; 32];
		buf[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());
		let expected_alias = NodeAlias(buf);

		let user_provided_alias = "I\u{1F496}LDK-Node!\0\u{26A1}";
		let node_alias = sanitize_alias(user_provided_alias).unwrap();

		let node_alias_display = format!("{}", node_alias);

		assert_eq!(alias, &node_alias_display);
		assert_ne!(expected_alias, node_alias);
	}

	#[test]
	fn sanitize_alias_gt_32_bytes() {
		let alias = "This is a string longer than thirty-two bytes!"; // 46 bytes
		let node = sanitize_alias(alias);
		assert_eq!(node.err().unwrap(), BuildError::InvalidNodeAlias);
	}
}
