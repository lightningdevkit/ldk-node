// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::default::Default;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, Once, RwLock};
use std::time::SystemTime;
use std::{fmt, fs};

use bdk_wallet::template::{Bip44, Bip49, Bip84, Bip86, P2Pkh, P2Wpkh, P2Wpkh_P2Sh, P2TR};
use bdk_wallet::{KeychainKind, PersistedWallet, Wallet as BdkWallet};
use bip39::Mnemonic;
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv, Xpub};
use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::{BlockHash, Network};
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::{chainmonitor, BestBlock, Watch};
use lightning::io::Cursor;
use lightning::ln::channelmanager::{self, ChainParameters, ChannelManagerReadArgs};
use lightning::ln::msgs::{RoutingMessageHandler, SocketAddress};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::routing::gossip::NodeAlias;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{
	ChannelLiquidities, CombinedScorer, ProbabilisticScorer, ProbabilisticScoringDecayParameters,
	ProbabilisticScoringFeeParameters,
};
use lightning::sign::{EntropySource, InMemorySigner, NodeSigner, SignerProvider};
use lightning::util::persist::{
	KVStore, KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY,
	CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
	CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	OUTPUT_SWEEPER_PERSISTENCE_KEY, OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
	OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE, SCORER_PERSISTENCE_KEY,
	SCORER_PERSISTENCE_PRIMARY_NAMESPACE, SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{Readable, ReadableArgs};
use lightning::util::sweep::OutputSweeper;
use lightning::{log_info, log_trace, log_warn};
use lightning_persister::fs_store::FilesystemStore;
use vss_client::headers::{FixedHeaders, LnurlAuthToJwtProvider, VssHeaderProvider};

use crate::chain::ChainSource;
use crate::config::{
	default_user_config, may_announce_channel, AddressType, AddressTypeRuntimeConfig,
	AnnounceError, AsyncPaymentsRole, BitcoindRestClientConfig, Config, ElectrumSyncConfig,
	EsploraSyncConfig, OnchainWalletAccount, RuntimeSyncIntervals, ScoringDecayParameters,
	ScoringFeeParameters, BDK_CLIENT_STOP_GAP, DEFAULT_ESPLORA_SERVER_URL, DEFAULT_LOG_FILENAME,
	DEFAULT_LOG_LEVEL, MAX_ONCHAIN_WALLET_ACCOUNT_INDEX, WALLET_KEYS_SEED_LEN,
};
use crate::connection::ConnectionManager;
use crate::event::EventQueue;
use crate::fee_estimator::OnchainFeeEstimator;
use crate::gossip::GossipSource;
use crate::io::local_graph_store::read_local_graph_cache;
use crate::io::sqlite_store::SqliteStore;
use crate::io::utils::{
	read_network_graph, read_node_metrics, read_payments_async, write_node_metrics,
	EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
};
use crate::io::vss_store::VssStore;
use crate::io::{
	self, EVENT_QUEUE_PERSISTENCE_KEY, EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
	EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE, PEER_INFO_PERSISTENCE_KEY,
	PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::liquidity::{
	LSPS1ClientConfig, LSPS2ClientConfig, LSPS2ServiceConfig, LiquiditySourceBuilder,
};
use crate::logger::{log_error, LdkLogger, LogLevel, LogWriter, Logger};
use crate::message_handler::NodeCustomMessageHandler;
use crate::payment::asynchronous::om_mailbox::OnionMessageMailbox;
use crate::peer_store::{persist_missing_channel_peers, PeerStore};
use crate::runtime::Runtime;
use crate::tx_broadcaster::TransactionBroadcaster;
use crate::types::{
	ChainMonitor, ChannelManager, DynStore, GossipSync, Graph, KeysManager, MessageRouter,
	OnionMessenger, PaymentStore, PeerManager, Persister, Sweeper,
};
use crate::wallet::persist::KVStoreWalletPersister;
use crate::wallet::Wallet;
use crate::RgsPeerRecoveryExclusions;
use crate::{Node, NodeMetrics};

const VSS_HARDENED_CHILD_INDEX: u32 = 877;
const VSS_LNURL_AUTH_HARDENED_CHILD_INDEX: u32 = 138;
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

fn derived_account_lookahead(chain_data_source_config: Option<&ChainDataSourceConfig>) -> u32 {
	match chain_data_source_config {
		Some(ChainDataSourceConfig::Electrum { sync_config, .. }) => {
			sync_config.unwrap_or_default().additional_wallet_full_scan_stop_gap.max(1)
		},
		Some(ChainDataSourceConfig::Bitcoind { .. }) => {
			bdk_chain::keychain_txout::DEFAULT_LOOKAHEAD
		},
		Some(ChainDataSourceConfig::Esplora { .. }) | None => BDK_CLIENT_STOP_GAP as u32,
	}
}

#[derive(Debug, Clone)]
enum EntropySourceConfig {
	SeedFile(String),
	SeedBytes([u8; WALLET_KEYS_SEED_LEN]),
	Bip39Mnemonic { mnemonic: Mnemonic, passphrase: Option<String> },
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

/// Channel data to migrate from an external LDK implementation (e.g., react-native-ldk).
///
/// The data is written to the configured kv_store (including VSS) during the build process,
/// before channel monitors are read. The storage key for each monitor is derived by
/// deserializing the monitor and extracting the funding outpoint.
#[derive(Debug, Clone, Default)]
pub struct ChannelDataMigration {
	/// Serialized ChannelManager bytes.
	pub channel_manager: Option<Vec<u8>>,
	/// Serialized channel monitor bytes.
	pub channel_monitors: Vec<Vec<u8>>,
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
	/// The seed bytes or a seed-derived wallet configuration are invalid.
	InvalidSeedBytes,
	/// The given seed file is invalid, e.g., has invalid length, or could not be read.
	InvalidSeedFile,
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
	/// The deserialized channel data would be dangerous to use, typically because
	/// channel monitors are stale compared to the channel manager.
	DangerousValue,
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
			Self::InvalidSeedBytes => {
				write!(f, "Given seed bytes or seed-derived wallet configuration are invalid.")
			},
			Self::InvalidSeedFile => write!(f, "Given seed file is invalid or could not be read."),
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
			Self::DangerousValue => {
				write!(f, "Deserialized channel data is dangerous to use (stale channel monitors).")
			},
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
	entropy_source_config: Option<EntropySourceConfig>,
	chain_data_source_config: Option<ChainDataSourceConfig>,
	gossip_source_config: Option<GossipSourceConfig>,
	liquidity_source_config: Option<LiquiditySourceConfig>,
	log_writer_config: Option<LogWriterConfig>,
	async_payments_role: Option<AsyncPaymentsRole>,
	runtime_handle: Option<tokio::runtime::Handle>,
	pathfinding_scores_sync_config: Option<PathfindingScoresSyncConfig>,
	channel_data_migration: Option<ChannelDataMigration>,
}

impl NodeBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let config = Config::default();
		Self::from_config(config)
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let entropy_source_config = None;
		let chain_data_source_config = None;
		let gossip_source_config = None;
		let liquidity_source_config = None;
		let log_writer_config = None;
		let runtime_handle = None;
		let pathfinding_scores_sync_config = None;
		let channel_data_migration = None;
		Self {
			config,
			entropy_source_config,
			chain_data_source_config,
			gossip_source_config,
			liquidity_source_config,
			log_writer_config,
			runtime_handle,
			async_payments_role: None,
			pathfinding_scores_sync_config,
			channel_data_migration,
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

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	pub fn set_entropy_seed_path(&mut self, seed_path: String) -> &mut Self {
		self.entropy_source_config = Some(EntropySourceConfig::SeedFile(seed_path));
		self
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given
	/// [`WALLET_KEYS_SEED_LEN`] seed bytes.
	pub fn set_entropy_seed_bytes(&mut self, seed_bytes: [u8; WALLET_KEYS_SEED_LEN]) -> &mut Self {
		self.entropy_source_config = Some(EntropySourceConfig::SeedBytes(seed_bytes));
		self
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	pub fn set_entropy_bip39_mnemonic(
		&mut self, mnemonic: Mnemonic, passphrase: Option<String>,
	) -> &mut Self {
		self.entropy_source_config =
			Some(EntropySourceConfig::Bip39Mnemonic { mnemonic, passphrase });
		self
	}

	/// Sets channel data to migrate from an external LDK implementation.
	///
	/// Used when migrating from react-native-ldk or similar. The channel data is
	/// written to the configured kv_store (including VSS) during build, before
	/// channel monitors are read. Storage keys are derived from the monitor data.
	///
	/// **Note:** The serialized data must be compatible with the current LDK version.
	pub fn set_channel_data_migration(&mut self, migration: ChannelDataMigration) -> &mut Self {
		self.channel_data_migration = Some(migration);
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

	/// Sets scorer fee parameters used for pathfinding penalties.
	pub fn set_scoring_fee_params(&mut self, params: ScoringFeeParameters) -> &mut Self {
		self.config.scoring_fee_params = Some(params);
		self
	}

	/// Sets scorer decay parameters used for liquidity estimate decay.
	pub fn set_scoring_decay_params(&mut self, params: ScoringDecayParameters) -> &mut Self {
		self.config.scoring_decay_params = Some(params);
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

	/// Sets the primary address type for the on-chain wallet.
	///
	/// This determines what type of addresses will be generated for new receiving and change
	/// addresses. Default is [`AddressType::NativeSegwit`].
	pub fn set_address_type(&mut self, address_type: AddressType) -> &mut Self {
		self.config.address_type = address_type;
		self
	}

	/// Sets additional address types to monitor for existing funds.
	///
	/// These address types will be scanned alongside the primary address type during chain
	/// synchronization. Useful when migrating from one address type to another — for example,
	/// if switching from Legacy to NativeSegwit, add `Legacy` here to continue monitoring
	/// old Legacy addresses.
	///
	/// Default is empty (only the primary address type is monitored).
	pub fn set_address_types_to_monitor(
		&mut self, address_types_to_monitor: Vec<AddressType>,
	) -> &mut Self {
		self.config.address_types_to_monitor = address_types_to_monitor;
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
	pub fn build(&self) -> Result<Node, BuildError> {
		let storage_dir_path = self.config.storage_dir_path.clone();
		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = Arc::new(
			SqliteStore::new(
				storage_dir_path.into(),
				Some(io::sqlite_store::SQLITE_DB_FILE_NAME.to_string()),
				Some(io::sqlite_store::KV_TABLE_NAME.to_string()),
			)
			.map_err(|_| BuildError::KVStoreSetupFailed)?,
		);
		self.build_with_store(kv_store)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self) -> Result<Node, BuildError> {
		let mut storage_dir_path: PathBuf = self.config.storage_dir_path.clone().into();
		storage_dir_path.push("fs_store");

		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = Arc::new(FilesystemStore::new(storage_dir_path));
		self.build_with_store(kv_store)
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
		&self, vss_url: String, store_id: String, lnurl_auth_server_url: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<Node, BuildError> {
		use bitcoin::key::Secp256k1;

		let logger = setup_logger(&self.log_writer_config, &self.config)?;

		let seed_bytes = seed_bytes_from_config(
			&self.config,
			self.entropy_source_config.as_ref(),
			Arc::clone(&logger),
		)?;

		let config = Arc::new(self.config.clone());

		let vss_xprv =
			derive_xprv(config, &seed_bytes, VSS_HARDENED_CHILD_INDEX, Arc::clone(&logger))?;

		let lnurl_auth_xprv = vss_xprv
			.derive_priv(
				&Secp256k1::new(),
				&[ChildNumber::Hardened { index: VSS_LNURL_AUTH_HARDENED_CHILD_INDEX }],
			)
			.map_err(|e| {
				log_error!(logger, "Failed to derive VSS secret: {}", e);
				BuildError::KVStoreSetupFailed
			})?;

		let lnurl_auth_jwt_provider =
			LnurlAuthToJwtProvider::new(lnurl_auth_xprv, lnurl_auth_server_url, fixed_headers)
				.map_err(|e| {
					log_error!(logger, "Failed to create LnurlAuthToJwtProvider: {}", e);
					BuildError::KVStoreSetupFailed
				})?;

		let header_provider = Arc::new(lnurl_auth_jwt_provider);

		self.build_with_vss_store_and_header_provider(vss_url, store_id, header_provider)
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
		&self, vss_url: String, store_id: String, fixed_headers: HashMap<String, String>,
	) -> Result<Node, BuildError> {
		let header_provider = Arc::new(FixedHeaders::new(fixed_headers));

		self.build_with_vss_store_and_header_provider(vss_url, store_id, header_provider)
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
		&self, vss_url: String, store_id: String, header_provider: Arc<dyn VssHeaderProvider>,
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

		let seed_bytes = seed_bytes_from_config(
			&self.config,
			self.entropy_source_config.as_ref(),
			Arc::clone(&logger),
		)?;

		let config = Arc::new(self.config.clone());

		let vss_xprv = derive_xprv(
			config.clone(),
			&seed_bytes,
			VSS_HARDENED_CHILD_INDEX,
			Arc::clone(&logger),
		)?;

		let vss_seed_bytes: [u8; 32] = vss_xprv.private_key.secret_bytes();

		let vss_store =
			VssStore::new(vss_url, store_id, vss_seed_bytes, header_provider).map_err(|e| {
				log_error!(logger, "Failed to setup VSS store: {}", e);
				BuildError::KVStoreSetupFailed
			})?;

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
			Arc::new(vss_store),
			self.channel_data_migration.as_ref(),
		)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store(&self, kv_store: Arc<DynStore>) -> Result<Node, BuildError> {
		let logger = setup_logger(&self.log_writer_config, &self.config)?;

		let runtime = if let Some(handle) = self.runtime_handle.as_ref() {
			Arc::new(Runtime::with_handle(handle.clone(), Arc::clone(&logger)))
		} else {
			Arc::new(Runtime::new(Arc::clone(&logger)).map_err(|e| {
				log_error!(logger, "Failed to setup tokio runtime: {}", e);
				BuildError::RuntimeSetupFailed
			})?)
		};

		let seed_bytes = seed_bytes_from_config(
			&self.config,
			self.entropy_source_config.as_ref(),
			Arc::clone(&logger),
		)?;
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
			kv_store,
			self.channel_data_migration.as_ref(),
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

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	pub fn set_entropy_seed_path(&self, seed_path: String) {
		self.inner.write().unwrap().set_entropy_seed_path(seed_path);
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given
	/// [`WALLET_KEYS_SEED_LEN`] seed bytes.
	///
	/// **Note:** Will return an error if the length of the given `seed_bytes` differs from
	/// [`WALLET_KEYS_SEED_LEN`].
	pub fn set_entropy_seed_bytes(&self, seed_bytes: Vec<u8>) -> Result<(), BuildError> {
		if seed_bytes.len() != WALLET_KEYS_SEED_LEN {
			return Err(BuildError::InvalidSeedBytes);
		}
		let mut bytes = [0u8; WALLET_KEYS_SEED_LEN];
		bytes.copy_from_slice(&seed_bytes);

		self.inner.write().unwrap().set_entropy_seed_bytes(bytes);
		Ok(())
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	pub fn set_entropy_bip39_mnemonic(&self, mnemonic: Mnemonic, passphrase: Option<String>) {
		self.inner.write().unwrap().set_entropy_bip39_mnemonic(mnemonic, passphrase);
	}

	/// Sets channel data to migrate from an external LDK implementation.
	///
	/// See [`NodeBuilder::set_channel_data_migration`] for details.
	pub fn set_channel_data_migration(&self, migration: ChannelDataMigration) {
		self.inner.write().unwrap().set_channel_data_migration(migration);
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

	/// Sets scorer fee parameters used for pathfinding penalties.
	pub fn set_scoring_fee_params(&self, params: ScoringFeeParameters) {
		self.inner.write().unwrap().set_scoring_fee_params(params);
	}

	/// Sets scorer decay parameters used for liquidity estimate decay.
	pub fn set_scoring_decay_params(&self, params: ScoringDecayParameters) {
		self.inner.write().unwrap().set_scoring_decay_params(params);
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

	/// Sets the primary address type for the on-chain wallet.
	pub fn set_address_type(&self, address_type: AddressType) {
		self.inner.write().unwrap().set_address_type(address_type);
	}

	/// Sets additional address types to monitor for existing funds.
	pub fn set_address_types_to_monitor(&self, address_types_to_monitor: Vec<AddressType>) {
		self.inner.write().unwrap().set_address_types_to_monitor(address_types_to_monitor);
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
	pub fn build(&self) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build().map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build_with_fs_store().map(Arc::new)
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
		&self, vss_url: String, store_id: String, lnurl_auth_server_url: String,
		fixed_headers: HashMap<String, String>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner
			.read()
			.unwrap()
			.build_with_vss_store(vss_url, store_id, lnurl_auth_server_url, fixed_headers)
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
		&self, vss_url: String, store_id: String, fixed_headers: HashMap<String, String>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner
			.read()
			.unwrap()
			.build_with_vss_store_and_fixed_headers(vss_url, store_id, fixed_headers)
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
		&self, vss_url: String, store_id: String, header_provider: Arc<dyn VssHeaderProvider>,
	) -> Result<Arc<Node>, BuildError> {
		self.inner
			.read()
			.unwrap()
			.build_with_vss_store_and_header_provider(vss_url, store_id, header_provider)
			.map(Arc::new)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store(&self, kv_store: Arc<DynStore>) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build_with_store(kv_store).map(Arc::new)
	}
}

/// BIP coin type: `0'` for mainnet, `1'` for test networks.
pub(crate) fn bip_coin_type(network: Network) -> u32 {
	match network {
		Network::Bitcoin => 0,
		_ => 1,
	}
}

/// Derives the account-level xprv at `m/purpose'/coin_type'/account_index'` and the
/// master fingerprint used in BIP descriptor origin metadata.
pub(crate) fn derive_account_xprv(
	master_xprv: Xpriv, network: Network, address_type: AddressType, account_index: u32,
) -> Result<(Xpriv, bitcoin::bip32::Fingerprint), BuildError> {
	let secp = Secp256k1::new();
	let master_fingerprint = master_xprv.fingerprint(&secp);
	let account_path = account_derivation_path(address_type, network, account_index)?;
	let account_xprv =
		master_xprv.derive_priv(&secp, &account_path).map_err(|_| BuildError::InvalidSeedBytes)?;
	Ok((account_xprv, master_fingerprint))
}

/// Derives the account-level xpub string for `(address_type, account_index)`.
pub(crate) fn derive_account_xpub_string(
	master_xprv: Xpriv, network: Network, address_type: AddressType, account_index: u32,
) -> Result<String, BuildError> {
	let secp = Secp256k1::new();
	let (account_xprv, _) = derive_account_xprv(master_xprv, network, address_type, account_index)?;
	let account_xpub = Xpub::from_priv(&secp, &account_xprv);
	Ok(account_xpub.to_string())
}

fn account_derivation_path(
	address_type: AddressType, network: Network, account_index: u32,
) -> Result<DerivationPath, BuildError> {
	if account_index > MAX_ONCHAIN_WALLET_ACCOUNT_INDEX {
		return Err(BuildError::InvalidSeedBytes);
	}
	let purpose = address_type.bip_purpose();
	let coin_type = bip_coin_type(network);
	Ok(DerivationPath::from(vec![
		ChildNumber::from_hardened_idx(purpose).map_err(|_| BuildError::InvalidSeedBytes)?,
		ChildNumber::from_hardened_idx(coin_type).map_err(|_| BuildError::InvalidSeedBytes)?,
		ChildNumber::from_hardened_idx(account_index).map_err(|_| BuildError::InvalidSeedBytes)?,
	]))
}

fn account_keychain_derivation_path(
	address_type: AddressType, network: Network, account_index: u32, keychain: KeychainKind,
) -> Result<DerivationPath, BuildError> {
	let mut path = account_derivation_path(address_type, network, account_index)?
		.into_iter()
		.copied()
		.collect::<Vec<_>>();
	let keychain_index = match keychain {
		KeychainKind::External => 0,
		KeychainKind::Internal => 1,
	};
	path.push(
		ChildNumber::from_normal_idx(keychain_index).map_err(|_| BuildError::InvalidSeedBytes)?,
	);
	Ok(path.into())
}

/// Loads or creates a BDK wallet for `wallet_account`.
///
/// Returns `(wallet, loaded_from_store)` where `loaded_from_store` is `true` when an existing
/// persisted wallet was restored.
pub(crate) fn get_or_create_wallet_for_account(
	wallet_account: OnchainWalletAccount, master_xprv: Xpriv, network: Network,
	persister: &mut KVStoreWalletPersister, lookahead: Option<u32>,
) -> Result<(PersistedWallet<KVStoreWalletPersister>, bool), BuildError> {
	macro_rules! load_or_create {
		($ext:expr, $int:expr) => {{
			let mut load_params = BdkWallet::load()
				.descriptor(KeychainKind::External, Some($ext))
				.descriptor(KeychainKind::Internal, Some($int))
				.extract_keys()
				.check_network(network);
			if let Some(lookahead) = lookahead {
				load_params = load_params.lookahead(lookahead);
			}
			let wallet_opt = load_params.load_wallet(persister).map_err(|e| match e {
				bdk_wallet::LoadWithPersistError::InvalidChangeSet(
					bdk_wallet::LoadError::Mismatch(bdk_wallet::LoadMismatch::Network {
						loaded,
						expected,
					}),
				) => {
					log::error!(
						"Failed to setup wallet: Networks do not match. Expected {} but got {}",
						expected,
						loaded
					);
					BuildError::NetworkMismatch
				},
				_ => {
					log::error!("Failed to set up wallet: {}", e);
					BuildError::WalletSetupFailed
				},
			})?;
			match wallet_opt {
				Some(w) => Ok((w, true)),
				None => {
					let mut create_params = BdkWallet::create($ext, $int).network(network);
					if let Some(lookahead) = lookahead {
						create_params = create_params.lookahead(lookahead);
					}
					create_params
						.create_wallet(persister)
						.map(|w| (w, false))
						.map_err(|_| BuildError::WalletSetupFailed)
				},
			}
		}};
	}

	// Account 0 keeps the historical BipXX(master) templates for descriptor compatibility.
	if wallet_account.account_index == 0 {
		return match wallet_account.address_type {
			AddressType::Legacy => load_or_create!(
				Bip44(master_xprv, KeychainKind::External),
				Bip44(master_xprv, KeychainKind::Internal)
			),
			AddressType::NestedSegwit => load_or_create!(
				Bip49(master_xprv, KeychainKind::External),
				Bip49(master_xprv, KeychainKind::Internal)
			),
			AddressType::NativeSegwit => load_or_create!(
				Bip84(master_xprv, KeychainKind::External),
				Bip84(master_xprv, KeychainKind::Internal)
			),
			AddressType::Taproot => load_or_create!(
				Bip86(master_xprv, KeychainKind::External),
				Bip86(master_xprv, KeychainKind::Internal)
			),
		};
	}

	if wallet_account.account_index > MAX_ONCHAIN_WALLET_ACCOUNT_INDEX {
		return Err(BuildError::InvalidSeedBytes);
	}

	// Use full master→account keychain paths so descriptor origin metadata includes the real
	// account index (Bip*Public templates hard-code account `0'`).
	let external_key = (
		master_xprv,
		account_keychain_derivation_path(
			wallet_account.address_type,
			network,
			wallet_account.account_index,
			KeychainKind::External,
		)?,
	);
	let internal_key = (
		master_xprv,
		account_keychain_derivation_path(
			wallet_account.address_type,
			network,
			wallet_account.account_index,
			KeychainKind::Internal,
		)?,
	);

	match wallet_account.address_type {
		AddressType::Legacy => {
			load_or_create!(P2Pkh(external_key.clone()), P2Pkh(internal_key.clone()))
		},
		AddressType::NestedSegwit => {
			load_or_create!(P2Wpkh_P2Sh(external_key.clone()), P2Wpkh_P2Sh(internal_key.clone()))
		},
		AddressType::NativeSegwit => {
			load_or_create!(P2Wpkh(external_key.clone()), P2Wpkh(internal_key.clone()))
		},
		AddressType::Taproot => {
			load_or_create!(P2TR(external_key.clone()), P2TR(internal_key.clone()))
		},
	}
}

fn validate_configured_onchain_wallet_accounts(
	config: &Config, xprv: Xpriv,
) -> Result<Vec<OnchainWalletAccount>, BuildError> {
	let mut seen = HashSet::new();
	let mut accounts = Vec::new();
	for configured_account in &config.onchain_wallet_accounts {
		let account_index = configured_account.account_index;
		if account_index == 0 || account_index > MAX_ONCHAIN_WALLET_ACCOUNT_INDEX {
			return Err(BuildError::InvalidSeedBytes);
		}

		let account =
			OnchainWalletAccount { address_type: configured_account.address_type, account_index };
		let expected_xpub = derive_account_xpub_string(
			xprv,
			config.network,
			configured_account.address_type,
			account_index,
		)?;
		if configured_account.xpub.trim() != expected_xpub {
			return Err(BuildError::InvalidSeedBytes);
		}
		if seen.insert(account) {
			accounts.push(account);
		}
	}
	Ok(accounts)
}

fn build_wallet_for_account(
	config: &Config, wallet_account: OnchainWalletAccount, xprv: Xpriv, kv_store: Arc<DynStore>,
	logger: Arc<Logger>, chain_tip_opt: Option<&BestBlock>, lookahead: Option<u32>,
) -> Result<
	(OnchainWalletAccount, PersistedWallet<KVStoreWalletPersister>, KVStoreWalletPersister),
	BuildError,
> {
	let mut persister =
		KVStoreWalletPersister::new(Arc::clone(&kv_store), Arc::clone(&logger), wallet_account);

	let (mut wallet, loaded_from_store) = get_or_create_wallet_for_account(
		wallet_account,
		xprv,
		config.network,
		&mut persister,
		lookahead,
	)
	.map_err(|e| {
		log_error!(logger, "Failed to setup wallet for {:?}: {}", wallet_account, e);
		e
	})?;

	// Re-loaded wallets keep their persisted tip so chain backends can catch up. New wallets start
	// at the known tip; Esplora and Electrum recover history independently with a full scan, while
	// Bitcoind cannot recover confirmed pre-registration history.
	if !loaded_from_store {
		if let Some(best_block) = chain_tip_opt {
			let mut checkpoint = wallet.latest_checkpoint();
			let block_id =
				bdk_chain::BlockId { height: best_block.height, hash: best_block.block_hash };
			checkpoint = checkpoint.insert(block_id);
			let update = bdk_wallet::Update { chain: Some(checkpoint), ..Default::default() };
			wallet.apply_update(update).map_err(|e| {
				log_error!(logger, "Failed to apply checkpoint for {:?}: {}", wallet_account, e);
				BuildError::WalletSetupFailed
			})?;
		}
	}

	Ok((wallet_account, wallet, persister))
}

fn build_additional_wallets(
	config: &Config, configured_accounts: &[OnchainWalletAccount], xprv: Xpriv,
	kv_store: Arc<DynStore>, logger: Arc<Logger>, chain_tip_opt: Option<&BestBlock>,
	derived_account_lookahead: u32,
) -> Result<
	Vec<(OnchainWalletAccount, PersistedWallet<KVStoreWalletPersister>, KVStoreWalletPersister)>,
	BuildError,
> {
	let mut additional_wallets = Vec::new();

	for address_type in config.additional_address_types() {
		let wallet_account = OnchainWalletAccount::account_zero(address_type);
		match build_wallet_for_account(
			config,
			wallet_account,
			xprv,
			Arc::clone(&kv_store),
			Arc::clone(&logger),
			chain_tip_opt,
			None,
		) {
			Ok(tuple) => {
				log_info!(logger, "Created additional wallet for {:?}", tuple.0);
				additional_wallets.push(tuple);
			},
			Err(e) => {
				log_error!(
					logger,
					"Failed to create additional wallet for {:?}: {}. Continuing without it.",
					wallet_account,
					e
				);
			},
		}
	}

	for &wallet_account in configured_accounts {
		let wallet = build_wallet_for_account(
			config,
			wallet_account,
			xprv,
			Arc::clone(&kv_store),
			Arc::clone(&logger),
			chain_tip_opt,
			Some(derived_account_lookahead),
		)?;
		log_info!(logger, "Loaded configured wallet for {:?}", wallet_account);
		additional_wallets.push(wallet);
	}

	Ok(additional_wallets)
}

fn apply_channel_data_migration<K>(
	migration: &ChannelDataMigration, kv_store: &Arc<DynStore>, keys_manager: &K,
	logger: &Arc<Logger>, runtime: &Arc<Runtime>,
) -> Result<(), BuildError>
where
	K: EntropySource + SignerProvider<EcdsaSigner = InMemorySigner>,
{
	if let Some(manager_bytes) = &migration.channel_manager {
		// Skip if KV store already has a channel manager (existence-only check).
		let should_write = match runtime.block_on(KVStore::read(
			&**kv_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		)) {
			Ok(_existing_data) => {
				log_warn!(
					logger,
					"Skipping channel manager migration: store already contains a channel manager"
				);
				false
			},
			Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => true,
			Err(e) => {
				log_error!(
					logger,
					"Failed to read existing channel manager, refusing migration write \
					 to avoid overwriting potentially newer state: {}",
					e
				);
				return Err(BuildError::ReadFailed);
			},
		};

		if should_write {
			runtime
				.block_on(async {
					KVStore::write(
						&**kv_store,
						CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
						CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
						CHANNEL_MANAGER_PERSISTENCE_KEY,
						manager_bytes.clone(),
					)
					.await
				})
				.map_err(|e| {
					log_error!(logger, "Failed to write migrated channel_manager: {}", e);
					BuildError::WriteFailed
				})?;
		}
	}

	for monitor_data in &migration.channel_monitors {
		let mut reader = lightning::io::Cursor::new(monitor_data);
		let (_, channel_monitor) = match <(BlockHash, ChannelMonitor<InMemorySigner>)>::read(
			&mut reader,
			(keys_manager, keys_manager),
		) {
			Ok(monitor) => monitor,
			Err(e) => {
				log_error!(logger, "Failed to deserialize channel_monitor: {:?}", e);
				return Err(BuildError::ReadFailed);
			},
		};

		let funding_txo = channel_monitor.get_funding_txo();
		let monitor_key = format!("{}_{}", funding_txo.txid, funding_txo.index);
		let migrated_update_id = channel_monitor.get_latest_update_id();
		log_info!(
			logger,
			"Migrating channel monitor: {} (update_id={})",
			monitor_key,
			migrated_update_id
		);

		// Check if the store already has a newer monitor to avoid overwriting
		// current state with stale migration data.
		let should_write = match runtime.block_on(KVStore::read(
			&**kv_store,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			&monitor_key,
		)) {
			Ok(existing_data) => {
				let mut existing_reader = Cursor::new(&existing_data);
				match <(BlockHash, ChannelMonitor<InMemorySigner>)>::read(
					&mut existing_reader,
					(keys_manager, keys_manager),
				) {
					Ok((_, existing_monitor)) => {
						let existing_update_id = existing_monitor.get_latest_update_id();
						if existing_update_id >= migrated_update_id {
							log_warn!(
								logger,
								"Skipping migration for monitor {}: existing update_id {} is newer than or equal to migrated update_id {}",
								monitor_key,
								existing_update_id,
								migrated_update_id
							);
							false
						} else {
							true
						}
					},
					Err(e) => {
						log_warn!(
							logger,
							"Failed to deserialize existing monitor {}, skipping migration to avoid overwriting potentially newer state: {:?}",
							monitor_key,
							e
						);
						false
					},
				}
			},
			Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => true,
			Err(e) => {
				log_error!(
					logger,
					"Failed to read existing monitor {}, refusing migration write to avoid overwriting potentially newer state: {}",
					monitor_key,
					e
				);
				return Err(BuildError::ReadFailed);
			},
		};

		if should_write {
			runtime
				.block_on(async {
					KVStore::write(
						&**kv_store,
						CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
						CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
						&monitor_key,
						monitor_data.clone(),
					)
					.await
				})
				.map_err(|e| {
					log_error!(logger, "Failed to write channel_monitor {}: {}", monitor_key, e);
					BuildError::WriteFailed
				})?;
		}
	}

	log_info!(logger, "Applied channel migration: {} monitors", migration.channel_monitors.len());

	Ok(())
}

fn build_with_store_internal(
	config: Arc<Config>, chain_data_source_config: Option<&ChainDataSourceConfig>,
	gossip_source_config: Option<&GossipSourceConfig>,
	liquidity_source_config: Option<&LiquiditySourceConfig>,
	pathfinding_scores_sync_config: Option<&PathfindingScoresSyncConfig>,
	async_payments_role: Option<AsyncPaymentsRole>, seed_bytes: [u8; 64], runtime: Arc<Runtime>,
	logger: Arc<Logger>, kv_store: Arc<DynStore>,
	channel_data_migration: Option<&ChannelDataMigration>,
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

	// Prepare wallet components (instant operations) before parallel VSS reads
	let xprv = bitcoin::bip32::Xpriv::new_master(config.network, &seed_bytes).map_err(|e| {
		log_error!(logger, "Failed to derive master secret: {}", e);
		BuildError::InvalidSeedBytes
	})?;
	let configured_accounts = validate_configured_onchain_wallet_accounts(&config, xprv)?;
	let derived_account_lookahead = derived_account_lookahead(chain_data_source_config);

	let tx_broadcaster = Arc::new(TransactionBroadcaster::new(Arc::clone(&logger)));
	let fee_estimator = Arc::new(OnchainFeeEstimator::new());

	// Execute VSS reads in parallel: node_metrics, payments, and wallet
	let (node_metrics_result, payments_result, wallet_result) = runtime.block_on(async {
		let metrics_store = Arc::clone(&kv_store);
		let metrics_logger = Arc::clone(&logger);

		let payments_store = Arc::clone(&kv_store);
		let payments_logger = Arc::clone(&logger);

		let wallet_store = Arc::clone(&kv_store);
		let wallet_logger: Arc<Logger> = Arc::clone(&logger);

		tokio::join!(
			tokio::task::spawn_blocking(move || read_node_metrics(metrics_store, metrics_logger)),
			read_payments_async(payments_store, payments_logger),
			tokio::task::spawn_blocking({
				let network = config.network;
				let address_type = config.address_type;
				move || {
					let wallet_account = OnchainWalletAccount::account_zero(address_type);
					let mut persister =
						KVStoreWalletPersister::new(wallet_store, wallet_logger, wallet_account);
					let result = get_or_create_wallet_for_account(
						wallet_account,
						xprv,
						network,
						&mut persister,
						None,
					)
					.map(|(wallet, _loaded)| wallet);
					(result, persister)
				}
			})
		)
	});

	// Process node_metrics result
	let node_metrics = match node_metrics_result {
		Ok(Ok(metrics)) => Arc::new(RwLock::new(metrics)),
		Ok(Err(e)) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(RwLock::new(NodeMetrics::default()))
			} else {
				log_error!(logger, "Failed to read node metrics from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
		Err(e) => {
			log_error!(logger, "Task join error reading node metrics: {}", e);
			return Err(BuildError::ReadFailed);
		},
	};

	// Process payments result
	let payment_store = match payments_result {
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

	// Process wallet result
	let (wallet_load_result, wallet_persister) = match wallet_result {
		Ok(result) => result,
		Err(e) => {
			log_error!(logger, "Task join error loading wallet: {}", e);
			return Err(BuildError::WalletSetupFailed);
		},
	};
	let bdk_wallet_loaded = wallet_load_result?;

	let address_type_runtime_config = Arc::new(RwLock::new(AddressTypeRuntimeConfig::from_config(
		&config,
		configured_accounts.clone(),
	)));

	// Chain source setup
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
				Arc::clone(&address_type_runtime_config),
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
				Arc::clone(&address_type_runtime_config),
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
				Arc::clone(&address_type_runtime_config),
				Arc::clone(&logger),
				Arc::clone(&node_metrics),
			)
		},
	};
	let chain_source = Arc::new(chain_source);

	let mut bdk_wallet = bdk_wallet_loaded;
	if let Some(best_block) = chain_tip_opt {
		let mut latest_checkpoint = bdk_wallet.latest_checkpoint();
		let block_id =
			bdk_chain::BlockId { height: best_block.height, hash: best_block.block_hash };
		latest_checkpoint = latest_checkpoint.insert(block_id);
		let update = bdk_wallet::Update { chain: Some(latest_checkpoint), ..Default::default() };
		bdk_wallet.apply_update(update).map_err(|e| {
			log_error!(logger, "Failed to apply checkpoint during wallet setup: {}", e);
			BuildError::WalletSetupFailed
		})?;
	}

	let additional_wallets = build_additional_wallets(
		&config,
		&configured_accounts,
		xprv,
		Arc::clone(&kv_store),
		Arc::clone(&logger),
		chain_tip_opt.as_ref(),
		derived_account_lookahead,
	)?;

	let wallet = Arc::new(Wallet::new(
		bdk_wallet,
		wallet_persister,
		additional_wallets,
		seed_bytes,
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
		Arc::clone(&payment_store),
		Arc::clone(&config),
		Arc::clone(&kv_store),
		Arc::clone(&address_type_runtime_config),
		Arc::clone(&node_metrics),
		Arc::clone(&logger),
		derived_account_lookahead,
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
	let persister = Arc::new(Persister::new(
		Arc::clone(&kv_store),
		Arc::clone(&logger),
		PERSISTER_MAX_PENDING_UPDATES,
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
	));

	if let Some(migration) = channel_data_migration {
		apply_channel_data_migration(migration, &kv_store, &*keys_manager, &logger, &runtime)?;
	}

	// Initialize the network graph from local cache, with VSS fallback for migration.
	let (network_graph, local_rgs_timestamp) =
		match read_local_graph_cache(&config.storage_dir_path, Arc::clone(&logger)) {
			Ok(cache_data) => {
				log_trace!(
					logger,
					"Loaded network graph from local cache with RGS timestamp {}",
					cache_data.rgs_timestamp
				);
				(Arc::new(cache_data.graph), Arc::new(AtomicU32::new(cache_data.rgs_timestamp)))
			},
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				// Local cache doesn't exist, try VSS as fallback for migration.
				log_trace!(
					logger,
					"Local graph cache not found, trying VSS fallback for migration"
				);
				match read_network_graph(Arc::clone(&kv_store), Arc::clone(&logger)) {
					Ok(graph) => {
						let timestamp =
							node_metrics.read().unwrap().latest_rgs_snapshot_timestamp.unwrap_or(0);
						log_info!(
							logger,
							"Migrated network graph from VSS to local cache (RGS timestamp: {})",
							timestamp
						);
						(Arc::new(graph), Arc::new(AtomicU32::new(timestamp)))
					},
					Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
						// New user, create empty graph. RGS will download full snapshot.
						log_trace!(logger, "No network graph found, creating empty graph");
						(
							Arc::new(Graph::new(config.network.into(), Arc::clone(&logger))),
							Arc::new(AtomicU32::new(0)),
						)
					},
					Err(e) => {
						log_error!(logger, "Failed to read network graph from VSS: {}", e);
						return Err(BuildError::ReadFailed);
					},
				}
			},
			Err(e) => {
				// Local cache is invalid. Start fresh to avoid graph/timestamp mismatch.
				log_error!(
					logger,
					"Local graph cache invalid: {}, starting fresh with empty graph",
					e
				);
				(
					Arc::new(Graph::new(config.network.into(), Arc::clone(&logger))),
					Arc::new(AtomicU32::new(0)),
				)
			},
		};

	// Read channel monitors and scorer data in parallel
	let (monitors_result, scorer_data_res, external_scores_data_res) = {
		let persister_clone = Arc::clone(&persister);
		let store1 = Arc::clone(&kv_store);
		let store2 = Arc::clone(&kv_store);

		runtime.block_on(async {
			tokio::join!(
				// Task 1: read channel monitors (blocking)
				tokio::task::spawn_blocking(move || {
					persister_clone.read_all_channel_monitors_with_updates()
				}),
				// Task 2: read scorer (async)
				KVStore::read(
					&*store1,
					SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
					SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
					SCORER_PERSISTENCE_KEY,
				),
				// Task 3: read external scores (async)
				KVStore::read(
					&*store2,
					SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
					SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
					EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
				),
			)
		})
	};

	// Process channel monitors result
	let channel_monitors = match monitors_result {
		Ok(Ok(monitors)) => monitors,
		Ok(Err(e)) => {
			if e.kind() == lightning::io::ErrorKind::NotFound {
				Vec::new()
			} else {
				log_error!(logger, "Failed to read channel monitors from store: {}", e.to_string());
				return Err(BuildError::ReadFailed);
			}
		},
		Err(e) => {
			log_error!(logger, "Channel monitors read task failed: {}", e);
			return Err(BuildError::ReadFailed);
		},
	};

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

	// Deserialize scorer
	let scoring_decay_params = config
		.scoring_decay_params
		.clone()
		.map(ProbabilisticScoringDecayParameters::from)
		.unwrap_or_else(ProbabilisticScoringDecayParameters::default);
	let local_scorer = match scorer_data_res {
		Ok(data) => {
			let mut reader = Cursor::new(data);
			let args = (scoring_decay_params, Arc::clone(&network_graph), Arc::clone(&logger));
			match ProbabilisticScorer::read(&mut reader, args) {
				Ok(scorer) => scorer,
				Err(e) => {
					log_error!(logger, "Failed to deserialize scorer: {}", e);
					return Err(BuildError::ReadFailed);
				},
			}
		},
		Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => ProbabilisticScorer::new(
			scoring_decay_params,
			Arc::clone(&network_graph),
			Arc::clone(&logger),
		),
		Err(e) => {
			log_error!(logger, "Failed to read scoring data from store: {}", e);
			return Err(BuildError::ReadFailed);
		},
	};

	let scorer = Arc::new(Mutex::new(CombinedScorer::new(local_scorer)));

	// Deserialize and merge external pathfinding scores
	match external_scores_data_res {
		Ok(data) => {
			let mut reader = Cursor::new(data);
			match ChannelLiquidities::read(&mut reader) {
				Ok(external_scores) => {
					scorer.lock().unwrap().merge(external_scores, cur_time);
					log_trace!(logger, "External scores from cache merged successfully");
				},
				Err(e) => {
					log_error!(logger, "Failed to deserialize external scores: {}", e);
					return Err(BuildError::ReadFailed);
				},
			}
		},
		Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => {},
		Err(e) => {
			log_error!(logger, "Error while reading external scores from cache: {}", e);
			return Err(BuildError::ReadFailed);
		},
	}

	let scoring_fee_params = config
		.scoring_fee_params
		.clone()
		.map(ProbabilisticScoringFeeParameters::from)
		.unwrap_or_else(ProbabilisticScoringFeeParameters::default);
	let router = Arc::new(DefaultRouter::new(
		Arc::clone(&network_graph),
		Arc::clone(&logger),
		Arc::clone(&keys_manager),
		Arc::clone(&scorer),
		scoring_fee_params,
	));

	let mut user_config = default_user_config(&config);

	if liquidity_source_config.and_then(|lsc| lsc.lsps2_service.as_ref()).is_some() {
		// If we act as an LSPS2 service, we need to to be able to intercept HTLCs and forward the
		// information to the service handler.
		user_config.accept_intercept_htlcs = true;

		// If we act as an LSPS2 service, we allow forwarding to unnannounced channels.
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
		if let Ok(res) = KVStoreSync::read(
			&*kv_store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		) {
			let mut reader = Cursor::new(res);
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
				<(BlockHash, ChannelManager)>::read(&mut reader, read_args).map_err(|e| {
					if matches!(e, lightning::ln::msgs::DecodeError::DangerousValue) {
						log_error!(
							logger,
							"Channel manager deserialization returned DangerousValue \
							 (stale channel monitors): {}",
							e
						);
						BuildError::DangerousValue
					} else {
						log_error!(logger, "Failed to read channel manager from store: {}", e);
						BuildError::ReadFailed
					}
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
		chain_monitor.watch_channel(channel_id, channel_monitor).map_err(|e| {
			log_error!(logger, "Failed to watch channel monitor: {:?}", e);
			BuildError::InvalidChannelMonitor
		})?;
	}

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
				IgnoringMessageHandler {},
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
				IgnoringMessageHandler {},
				IgnoringMessageHandler {},
			))
		};
	let ephemeral_bytes: [u8; 32] = keys_manager.get_secure_random_bytes();

	// Initialize the GossipSource
	// Use the configured gossip source, if the user set one, otherwise default to P2PNetwork.
	let gossip_source_config = gossip_source_config.unwrap_or(&GossipSourceConfig::P2PNetwork);

	let gossip_source = match gossip_source_config {
		GossipSourceConfig::P2PNetwork => {
			let p2p_source =
				Arc::new(GossipSource::new_p2p(Arc::clone(&network_graph), Arc::clone(&logger)));

			// Reset the RGS sync timestamp in case we somehow switch gossip sources
			{
				let mut locked_node_metrics = node_metrics.write().unwrap();
				locked_node_metrics.latest_rgs_snapshot_timestamp = None;
				write_node_metrics(
					&*locked_node_metrics,
					Arc::clone(&kv_store),
					Arc::clone(&logger),
				)
				.map_err(|e| {
					log_error!(logger, "Failed writing to store: {}", e);
					BuildError::WriteFailed
				})?;
			}
			p2p_source
		},
		GossipSourceConfig::RapidGossipSync(rgs_server) => Arc::new(GossipSource::new_rgs(
			rgs_server.clone(),
			Arc::clone(&local_rgs_timestamp),
			Arc::clone(&network_graph),
			Arc::clone(&logger),
		)),
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

	liquidity_source.as_ref().map(|l| l.set_peer_manager(Arc::clone(&peer_manager)));

	gossip_source.set_gossip_verifier(
		Arc::clone(&chain_source),
		Arc::clone(&peer_manager),
		Arc::clone(&runtime),
	);

	let connection_manager =
		Arc::new(ConnectionManager::new(Arc::clone(&peer_manager), Arc::clone(&logger)));

	// Read output_sweeper, event_queue, and peer_store in parallel
	let (output_sweeper_res, event_queue_res, peer_store_res) = {
		let store1 = Arc::clone(&kv_store);
		let store2 = Arc::clone(&kv_store);
		let store3 = Arc::clone(&kv_store);

		runtime.block_on(async {
			let sweeper_fut = KVStore::read(
				&*store1,
				OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
				OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
				OUTPUT_SWEEPER_PERSISTENCE_KEY,
			);
			let event_queue_fut = KVStore::read(
				&*store2,
				EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_KEY,
			);
			let peer_store_fut = KVStore::read(
				&*store3,
				PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				PEER_INFO_PERSISTENCE_KEY,
			);

			tokio::join!(sweeper_fut, event_queue_fut, peer_store_fut)
		})
	};

	// Process output_sweeper result
	let output_sweeper = match output_sweeper_res {
		Ok(data) => {
			let mut reader = lightning::io::Cursor::new(data);
			let args = (
				Arc::clone(&tx_broadcaster),
				Arc::clone(&fee_estimator),
				Some(Arc::clone(&chain_source)),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			);
			match <(_, Sweeper)>::read(&mut reader, args) {
				Ok((_, sweeper)) => Arc::new(sweeper),
				Err(e) => {
					log_error!(logger, "Failed to deserialize OutputSweeper: {}", e);
					return Err(BuildError::ReadFailed);
				},
			}
		},
		Err(e) => {
			if e.kind() == lightning::io::ErrorKind::NotFound {
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

	// Process event_queue result
	let event_queue = match event_queue_res {
		Ok(data) => {
			let mut reader = lightning::io::Cursor::new(data);
			match EventQueue::read(&mut reader, (Arc::clone(&kv_store), Arc::clone(&logger))) {
				Ok(queue) => Arc::new(queue),
				Err(e) => {
					log_error!(logger, "Failed to deserialize EventQueue: {}", e);
					return Err(BuildError::ReadFailed);
				},
			}
		},
		Err(e) => {
			if e.kind() == lightning::io::ErrorKind::NotFound {
				Arc::new(EventQueue::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				log_error!(logger, "Failed to read event queue from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	// Process peer_store result
	let peer_store = match peer_store_res {
		Ok(data) => {
			let mut reader = lightning::io::Cursor::new(data);
			match PeerStore::read(&mut reader, (Arc::clone(&kv_store), Arc::clone(&logger))) {
				Ok(store) => Arc::new(store),
				Err(e) => {
					log_error!(logger, "Failed to deserialize PeerStore: {}", e);
					return Err(BuildError::ReadFailed);
				},
			}
		},
		Err(e) => {
			if e.kind() == lightning::io::ErrorKind::NotFound {
				Arc::new(PeerStore::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				log_error!(logger, "Failed to read peer data from store: {}", e);
				return Err(BuildError::ReadFailed);
			}
		},
	};

	runtime.block_on(persist_missing_channel_peers(
		channel_manager.list_channels().into_iter().map(|channel| channel.counterparty.node_id),
		&network_graph,
		&peer_store,
		Arc::clone(&logger),
	));

	let om_mailbox = if let Some(AsyncPaymentsRole::Server) = async_payments_role {
		Some(Arc::new(OnionMessageMailbox::new()))
	} else {
		None
	};

	let (stop_sender, _) = tokio::sync::watch::channel(());
	let (background_processor_stop_sender, _) = tokio::sync::watch::channel(());
	let is_running = Arc::new(RwLock::new(false));

	let pathfinding_scores_sync_url = pathfinding_scores_sync_config.map(|c| c.url.clone());

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
		rgs_peer_recovery_exclusions: Arc::new(RgsPeerRecoveryExclusions::default()),
		payment_store,
		is_running,
		node_metrics,
		om_mailbox,
		async_payments_role,
		runtime_sync_intervals: Arc::new(RwLock::new(RuntimeSyncIntervals::default())),
		local_rgs_timestamp,
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

fn seed_bytes_from_config(
	config: &Config, entropy_source_config: Option<&EntropySourceConfig>, logger: Arc<Logger>,
) -> Result<[u8; 64], BuildError> {
	match entropy_source_config {
		Some(EntropySourceConfig::SeedBytes(bytes)) => Ok(bytes.clone()),
		Some(EntropySourceConfig::SeedFile(seed_path)) => {
			Ok(io::utils::read_or_generate_seed_file(seed_path, Arc::clone(&logger))
				.map_err(|_| BuildError::InvalidSeedFile)?)
		},
		Some(EntropySourceConfig::Bip39Mnemonic { mnemonic, passphrase }) => match passphrase {
			Some(passphrase) => Ok(mnemonic.to_seed(passphrase)),
			None => Ok(mnemonic.to_seed("")),
		},
		None => {
			// Default to read or generate from the default location generate a seed file.
			let seed_path = format!("{}/keys_seed", config.storage_dir_path);
			Ok(io::utils::read_or_generate_seed_file(&seed_path, Arc::clone(&logger))
				.map_err(|_| BuildError::InvalidSeedFile)?)
		},
	}
}

fn derive_xprv(
	config: Arc<Config>, seed_bytes: &[u8; 64], hardened_child_index: u32, logger: Arc<Logger>,
) -> Result<Xpriv, BuildError> {
	use bitcoin::key::Secp256k1;

	let xprv = Xpriv::new_master(config.network, seed_bytes).map_err(|e| {
		log_error!(logger, "Failed to derive master secret: {}", e);
		BuildError::InvalidSeedBytes
	})?;

	xprv.derive_priv(&Secp256k1::new(), &[ChildNumber::Hardened { index: hardened_child_index }])
		.map_err(|e| {
			log_error!(logger, "Failed to derive hardened child secret: {}", e);
			BuildError::InvalidSeedBytes
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
	use std::future::Future;
	use std::pin::Pin;
	use std::sync::Arc;

	use bitcoin::bip32::Xpriv;
	use bitcoin::Network;
	use lightning::io;
	use lightning::ln::functional_test_utils::{
		create_announced_chan_between_nodes, create_chanmon_cfgs, create_network, create_node_cfgs,
		create_node_chanmgrs,
	};
	use lightning::sign::KeysManager as LdkKeysManager;
	use lightning::util::persist::{
		KVStore, KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY,
		CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use lightning::util::ser::Writeable;

	use super::{
		apply_channel_data_migration, derive_account_xpub_string, sanitize_alias,
		validate_configured_onchain_wallet_accounts, BuildError, ChannelDataMigration, Config,
		NodeAlias, NodeBuilder, ScoringDecayParameters, ScoringFeeParameters,
	};
	use crate::config::{AddressType, OnchainWalletAccountConfig};
	use crate::io::test_utils::InMemoryStore;
	use crate::logger::Logger;
	use crate::runtime::Runtime;
	use crate::types::DynStore;

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

	#[test]
	fn validates_and_deduplicates_configured_onchain_wallet_accounts() {
		let xprv = Xpriv::new_master(Network::Regtest, &[42; 64]).unwrap();
		let xpub =
			derive_account_xpub_string(xprv, Network::Regtest, AddressType::Taproot, 1).unwrap();
		let configured = OnchainWalletAccountConfig {
			address_type: AddressType::Taproot,
			account_index: 1,
			xpub,
		};
		let mut config = Config::default();
		config.network = Network::Regtest;
		config.onchain_wallet_accounts = vec![configured.clone(), configured];

		let accounts = validate_configured_onchain_wallet_accounts(&config, xprv).unwrap();
		assert_eq!(accounts.len(), 1);
		assert_eq!(accounts[0].address_type, AddressType::Taproot);
		assert_eq!(accounts[0].account_index, 1);

		config.onchain_wallet_accounts[0].xpub = "wrong xpub".to_string();
		assert_eq!(
			validate_configured_onchain_wallet_accounts(&config, xprv),
			Err(BuildError::InvalidSeedBytes)
		);

		config.onchain_wallet_accounts[0].account_index = 0;
		assert_eq!(
			validate_configured_onchain_wallet_accounts(&config, xprv),
			Err(BuildError::InvalidSeedBytes)
		);
	}

	#[test]
	fn build_rejects_configured_account_xpub_from_another_seed() {
		let foreign_xprv = Xpriv::new_master(Network::Regtest, &[43; 64]).unwrap();
		let foreign_xpub = derive_account_xpub_string(
			foreign_xprv,
			Network::Regtest,
			AddressType::NativeSegwit,
			1,
		)
		.unwrap();
		let mut config = Config::default();
		config.network = Network::Regtest;
		config.onchain_wallet_accounts = vec![OnchainWalletAccountConfig {
			address_type: AddressType::NativeSegwit,
			account_index: 1,
			xpub: foreign_xpub,
		}];
		let mut builder = NodeBuilder::from_config(config);
		builder.set_entropy_seed_bytes([42; 64]);
		builder.set_log_facade_logger();
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());

		assert!(matches!(builder.build_with_store(store), Err(BuildError::InvalidSeedBytes)));
	}

	#[test]
	fn set_scoring_params_updates_builder_config() {
		let mut builder = NodeBuilder::from_config(Config::default());
		let fee_params = ScoringFeeParameters {
			base_penalty_msat: 2_000,
			base_penalty_amount_multiplier_msat: 200_000,
			liquidity_penalty_multiplier_msat: 1,
			liquidity_penalty_amount_multiplier_msat: 2,
			historical_liquidity_penalty_multiplier_msat: 20_000,
			historical_liquidity_penalty_amount_multiplier_msat: 2_500,
			anti_probing_penalty_msat: 500,
			considered_impossible_penalty_msat: 999_999,
			linear_success_probability: true,
			probing_diversity_penalty_msat: 321,
		};
		let decay_params = ScoringDecayParameters {
			historical_no_updates_half_life_secs: 1_234,
			liquidity_offset_half_life_secs: 567,
		};

		builder.set_scoring_fee_params(fee_params.clone());
		builder.set_scoring_decay_params(decay_params.clone());

		assert_eq!(
			builder.config.scoring_fee_params.as_ref().unwrap().base_penalty_msat,
			fee_params.base_penalty_msat
		);
		assert_eq!(
			builder.config.scoring_fee_params.as_ref().unwrap().linear_success_probability,
			fee_params.linear_success_probability
		);
		assert_eq!(
			builder
				.config
				.scoring_decay_params
				.as_ref()
				.unwrap()
				.historical_no_updates_half_life_secs,
			decay_params.historical_no_updates_half_life_secs
		);
	}

	/// Creates valid serialized (BlockHash, ChannelMonitor) bytes for testing.
	///
	/// Uses LDK's functional test utilities to create a real channel between two
	/// nodes, then serializes one of the resulting channel monitors. Returns the
	/// seed used for node 0's KeysManager so callers can create a matching
	/// KeysManager for deserialization.
	///
	/// Returns (monitor_bytes, monitor_key, update_id, seed).
	fn create_test_monitor_bytes() -> (Vec<u8>, String, u64, [u8; 32]) {
		let chanmon_cfgs = create_chanmon_cfgs(2);
		let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
		let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
		let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

		let _ = create_announced_chan_between_nodes(&nodes, 0, 1);

		let channel_id = nodes[0].node.list_channels()[0].channel_id;
		let monitor = nodes[0].chain_monitor.chain_monitor.get_monitor(channel_id).unwrap();

		let funding_txo = monitor.get_funding_txo();
		let monitor_key = format!("{}_{}", funding_txo.txid, funding_txo.index);
		let update_id = monitor.get_latest_update_id();

		let seed = nodes[0].node_seed;
		let mut serialized = Vec::new();
		monitor.write(&mut serialized).unwrap();

		(serialized, monitor_key, update_id, seed)
	}

	fn make_test_deps(
		seed: &[u8; 32],
	) -> (Arc<DynStore>, LdkKeysManager, Arc<Logger>, Arc<Runtime>) {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let keys_manager = LdkKeysManager::new(seed, 0, 0, false);
		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());
		(store, keys_manager, logger, runtime)
	}

	/// A KVStore wrapper that returns IO errors for reads from the channel
	/// monitor namespace, simulating a storage backend failure.
	struct FailingReadStore;

	impl KVStore for FailingReadStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str,
		) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send>> {
			Box::pin(async {
				Err(io::Error::new(io::ErrorKind::ConnectionReset, "simulated IO error"))
			})
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + 'static + Send>> {
			Box::pin(async { Ok(()) })
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + 'static + Send>> {
			Box::pin(async { Ok(()) })
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + 'static + Send>> {
			Box::pin(async { Ok(Vec::new()) })
		}
	}

	impl KVStoreSync for FailingReadStore {
		fn read(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str,
		) -> io::Result<Vec<u8>> {
			Err(io::Error::new(io::ErrorKind::ConnectionReset, "simulated IO error"))
		}

		fn write(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _buf: Vec<u8>,
		) -> io::Result<()> {
			Ok(())
		}

		fn remove(
			&self, _primary_namespace: &str, _secondary_namespace: &str, _key: &str, _lazy: bool,
		) -> io::Result<()> {
			Ok(())
		}

		fn list(
			&self, _primary_namespace: &str, _secondary_namespace: &str,
		) -> io::Result<Vec<String>> {
			Ok(Vec::new())
		}
	}

	unsafe impl Sync for FailingReadStore {}
	unsafe impl Send for FailingReadStore {}

	#[test]
	fn test_migration_invalid_monitor_data_returns_read_failed() {
		let (store, keys_manager, logger, runtime) = make_test_deps(&[42u8; 32]);
		let migration = ChannelDataMigration {
			channel_manager: None,
			channel_monitors: vec![vec![0xDE, 0xAD, 0xBE, 0xEF]],
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert_eq!(result, Err(BuildError::ReadFailed));
	}

	#[test]
	fn test_migration_empty_monitors_succeeds() {
		let (store, keys_manager, logger, runtime) = make_test_deps(&[42u8; 32]);
		let migration =
			ChannelDataMigration { channel_manager: None, channel_monitors: Vec::new() };

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert!(result.is_ok());
	}

	#[test]
	fn test_migration_fresh_write_to_empty_store() {
		let (monitor_bytes, monitor_key, _, seed) = create_test_monitor_bytes();
		let (store, keys_manager, logger, runtime) = make_test_deps(&seed);

		let migration = ChannelDataMigration {
			channel_manager: None,
			channel_monitors: vec![monitor_bytes.clone()],
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert!(result.is_ok());

		// Verify the monitor was written to the store.
		let stored = runtime.block_on(KVStore::read(
			&*store,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			&monitor_key,
		));
		assert!(stored.is_ok());
		assert_eq!(stored.unwrap(), monitor_bytes);
	}

	#[test]
	fn test_migration_skips_when_existing_is_newer_or_equal() {
		let (monitor_bytes, monitor_key, _, seed) = create_test_monitor_bytes();
		let (store, keys_manager, logger, runtime) = make_test_deps(&seed);

		// Pre-populate the store with valid monitor data (update_id=0).
		// Migration data also has update_id=0. Since existing >= migrated,
		// the function should skip the redundant write and succeed.
		runtime
			.block_on(KVStore::write(
				&*store,
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				&monitor_key,
				monitor_bytes.clone(),
			))
			.unwrap();

		let migration = ChannelDataMigration {
			channel_manager: None,
			channel_monitors: vec![monitor_bytes.clone()],
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert!(result.is_ok());
	}

	#[test]
	fn test_migration_skips_on_corrupt_existing_data() {
		let (monitor_bytes, monitor_key, _, seed) = create_test_monitor_bytes();
		let (store, keys_manager, logger, runtime) = make_test_deps(&seed);

		let corrupt_data = vec![0xFF, 0xFE, 0xFD, 0xFC];

		// Pre-populate the store with garbage data for this monitor key.
		runtime
			.block_on(KVStore::write(
				&*store,
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				&monitor_key,
				corrupt_data.clone(),
			))
			.unwrap();

		// Migration should skip (not fail) when existing data can't be deserialized,
		// to avoid blocking node startup while still not overwriting existing state.
		let migration =
			ChannelDataMigration { channel_manager: None, channel_monitors: vec![monitor_bytes] };

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert_eq!(result, Ok(()));

		// Verify the corrupt data was NOT overwritten — existing state preserved.
		let stored = runtime
			.block_on(KVStore::read(
				&*store,
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				&monitor_key,
			))
			.unwrap();
		assert_eq!(stored, corrupt_data);
	}

	#[test]
	fn test_migration_fails_on_store_read_error() {
		let (monitor_bytes, _, _, seed) = create_test_monitor_bytes();

		let store: Arc<DynStore> = Arc::new(FailingReadStore);
		let keys_manager = LdkKeysManager::new(&seed, 0, 0, false);
		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());

		// Migration should fail because the store returns an IO error on read
		// (fail-closed: non-NotFound errors refuse migration).
		let migration =
			ChannelDataMigration { channel_manager: None, channel_monitors: vec![monitor_bytes] };

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert_eq!(result, Err(BuildError::ReadFailed));
	}

	#[test]
	fn test_channel_manager_migration_fresh_write_to_empty_store() {
		let (store, keys_manager, logger, runtime) = make_test_deps(&[42u8; 32]);
		let manager_bytes = vec![0x01, 0x02, 0x03, 0x04];

		let migration = ChannelDataMigration {
			channel_manager: Some(manager_bytes.clone()),
			channel_monitors: Vec::new(),
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert!(result.is_ok());

		// Verify the channel manager was written to the store.
		let stored = runtime.block_on(KVStore::read(
			&*store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		));
		assert!(stored.is_ok());
		assert_eq!(stored.unwrap(), manager_bytes);
	}

	#[test]
	fn test_channel_manager_migration_skips_when_existing_data_present() {
		let (store, keys_manager, logger, runtime) = make_test_deps(&[42u8; 32]);

		// Pre-populate the store with existing channel manager data.
		let existing_data = vec![0xAA, 0xBB, 0xCC];
		runtime
			.block_on(KVStore::write(
				&*store,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
				existing_data.clone(),
			))
			.unwrap();

		// Attempt migration with different data.
		let migration_bytes = vec![0x01, 0x02, 0x03];
		let migration = ChannelDataMigration {
			channel_manager: Some(migration_bytes),
			channel_monitors: Vec::new(),
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert!(result.is_ok());

		// Verify the original data is preserved (migration was skipped).
		let stored = runtime
			.block_on(KVStore::read(
				&*store,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			))
			.unwrap();
		assert_eq!(stored, existing_data);
	}

	#[test]
	fn test_channel_manager_migration_skips_even_when_existing_data_is_corrupt() {
		let (store, keys_manager, logger, runtime) = make_test_deps(&[42u8; 32]);

		// Pre-populate the store with corrupt/garbage data.
		let corrupt_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
		runtime
			.block_on(KVStore::write(
				&*store,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
				corrupt_data.clone(),
			))
			.unwrap();

		// Migration should skip (existence-only check, no deserialization).
		// This is intentional: overwriting potentially valid data with stale
		// FS bytes risks fund loss; a node that fails to start on corrupt
		// data can be recovered by clearing the KV store.
		let migration = ChannelDataMigration {
			channel_manager: Some(vec![0x01, 0x02, 0x03]),
			channel_monitors: Vec::new(),
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert!(result.is_ok());

		// Verify the corrupt data is preserved (migration was skipped).
		let stored = runtime
			.block_on(KVStore::read(
				&*store,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			))
			.unwrap();
		assert_eq!(stored, corrupt_data);
	}

	#[test]
	fn test_channel_manager_migration_fails_on_store_read_error() {
		let store: Arc<DynStore> = Arc::new(FailingReadStore);
		let keys_manager = LdkKeysManager::new(&[42u8; 32], 0, 0, false);
		let logger = Arc::new(Logger::new_log_facade());
		let runtime = Arc::new(Runtime::new(Arc::clone(&logger)).unwrap());

		let migration = ChannelDataMigration {
			channel_manager: Some(vec![0x01, 0x02, 0x03]),
			channel_monitors: Vec::new(),
		};

		let result =
			apply_channel_data_migration(&migration, &store, &keys_manager, &logger, &runtime);
		assert_eq!(result, Err(BuildError::ReadFailed));
	}

	#[test]
	fn derived_account_xpub_differs_from_account_zero() {
		use bitcoin::bip32::Xpriv;
		use bitcoin::Network;

		use super::{derive_account_xpub_string, AddressType};

		let seed = [7u8; 64];
		let master = Xpriv::new_master(Network::Regtest, &seed).unwrap();
		let xpub0 =
			derive_account_xpub_string(master, Network::Regtest, AddressType::NativeSegwit, 0)
				.unwrap();
		let xpub1 =
			derive_account_xpub_string(master, Network::Regtest, AddressType::NativeSegwit, 1)
				.unwrap();
		assert_ne!(xpub0, xpub1);
		assert!(xpub1.starts_with('t') || xpub1.starts_with('x'));
	}

	#[test]
	fn derived_account_descriptors_include_account_index_in_origin() {
		use std::str::FromStr;
		use std::sync::Arc;

		use bitcoin::bip32::{DerivationPath, Xpriv};
		use bitcoin::secp256k1::Secp256k1;
		use bitcoin::Network;

		use super::{
			get_or_create_wallet_for_account, AddressType, KeychainKind, OnchainWalletAccount,
		};
		use crate::io::test_utils::InMemoryStore;
		use crate::logger::Logger;
		use crate::types::DynStore;
		use crate::wallet::persist::KVStoreWalletPersister;

		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(Logger::new_log_facade());
		let network = Network::Regtest;
		let master = Xpriv::new_master(network, &[42u8; 64]).unwrap();
		let account =
			OnchainWalletAccount { address_type: AddressType::NativeSegwit, account_index: 7 };
		let mut persister =
			KVStoreWalletPersister::new(Arc::clone(&store), Arc::clone(&logger), account);
		let (wallet, loaded) =
			get_or_create_wallet_for_account(account, master, network, &mut persister, None)
				.unwrap();
		assert!(!loaded);

		let descriptor = wallet.public_descriptor(KeychainKind::External).to_string();
		assert!(
			descriptor.contains("/84'/1'/7'"),
			"descriptor origin must include account 7, got {}",
			descriptor
		);

		let expected_path = DerivationPath::from_str("m/84'/1'/7'").unwrap();
		let expected_xpub = bitcoin::bip32::Xpub::from_priv(
			&Secp256k1::new(),
			&master.derive_priv(&Secp256k1::new(), &expected_path).unwrap(),
		);
		assert!(descriptor.contains(&expected_xpub.to_string()));
		assert!(wallet.peek_address(KeychainKind::External, 0).address.script_pubkey().is_p2wpkh());
	}

	#[test]
	fn derived_account_xpub_rejects_different_seed() {
		use bitcoin::bip32::Xpriv;
		use bitcoin::Network;

		use super::{derive_account_xpub_string, AddressType};

		let master_a = Xpriv::new_master(Network::Regtest, &[1u8; 64]).unwrap();
		let master_b = Xpriv::new_master(Network::Regtest, &[2u8; 64]).unwrap();
		let xpub_a =
			derive_account_xpub_string(master_a, Network::Regtest, AddressType::NativeSegwit, 1)
				.unwrap();
		let xpub_b =
			derive_account_xpub_string(master_b, Network::Regtest, AddressType::NativeSegwit, 1)
				.unwrap();
		assert_ne!(xpub_a, xpub_b);
	}
}
