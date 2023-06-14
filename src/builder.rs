use crate::event::EventQueue;
use crate::gossip::GossipSource;
use crate::io;
use crate::io::fs_store::FilesystemStore;
use crate::io::sqlite_store::SqliteStore;
use crate::io::{KVStore, CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_NAMESPACE};
use crate::logger::{log_error, FilesystemLogger, Logger};
use crate::payment_store::PaymentStore;
use crate::peer_store::PeerStore;
use crate::types::{
	ChainMonitor, ChannelManager, GossipSync, KeysManager, NetAddress, NetworkGraph,
	OnionMessenger, PeerManager,
};
use crate::wallet::Wallet;
use crate::LogLevel;
use crate::{
	Config, Node, BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, DEFAULT_ESPLORA_SERVER_URL,
	WALLET_KEYS_SEED_LEN,
};

use lightning::chain::keysinterface::EntropySource;
use lightning::chain::{chainmonitor, BestBlock, Watch};
use lightning::ln::channelmanager::{self, ChainParameters, ChannelManagerReadArgs};
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringParameters};

use lightning::util::config::UserConfig;
use lightning::util::ser::ReadableArgs;

use lightning_transaction_sync::EsploraSyncClient;

use bdk::bitcoin::secp256k1::Secp256k1;
use bdk::blockchain::esplora::EsploraBlockchain;
use bdk::database::SqliteDatabase;
use bdk::template::Bip84;

use bitcoin::Network;

use bip39::Mnemonic;

use bitcoin::BlockHash;

use std::convert::TryInto;
use std::default::Default;
use std::fmt;
use std::fs;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

#[derive(Debug, Clone)]
enum ChainDataSourceConfig {
	Esplora(String),
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

/// An error encountered during building a [`Node`].
///
/// [`Node`]: crate::Node
#[derive(Debug, Clone)]
pub enum BuildError {
	/// The given seed bytes are invalid, e.g, are of invalid length.
	InvalidSeedBytes,
	/// The current system time is invalid, clocks might have gone backwards.
	InvalidSystemTime,
	/// We failed to read data from the [`KVStore`].
	ReadFailed,
	/// We failed to write data to the [`KVStore`].
	WriteFailed,
	/// We failed to access the given `storage_dir_path`.
	StoragePathAccessFailed,
	/// We failed to setup the onchain wallet.
	WalletSetupFailed,
	/// We failed to setup the logger.
	LoggerSetupFailed,
}

impl fmt::Display for BuildError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::InvalidSeedBytes => write!(f, "Given seed bytes are invalid."),
			Self::InvalidSystemTime => {
				write!(f, "System time is invalid. Clocks might have gone back in time.")
			}
			Self::ReadFailed => write!(f, "Failed to read from store."),
			Self::WriteFailed => write!(f, "Failed to write to store."),
			Self::StoragePathAccessFailed => write!(f, "Failed to access the given storage path."),
			Self::WalletSetupFailed => write!(f, "Failed to setup onchain wallet."),
			Self::LoggerSetupFailed => write!(f, "Failed to setup the logger."),
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
}

impl NodeBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let config = Config::default();
		let entropy_source_config = None;
		let chain_data_source_config = None;
		let gossip_source_config = None;
		Self { config, entropy_source_config, chain_data_source_config, gossip_source_config }
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let config = config;
		let entropy_source_config = None;
		let chain_data_source_config = None;
		let gossip_source_config = None;
		Self { config, entropy_source_config, chain_data_source_config, gossip_source_config }
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	pub fn set_entropy_seed_path(&mut self, seed_path: String) -> &mut Self {
		self.entropy_source_config = Some(EntropySourceConfig::SeedFile(seed_path));
		self
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given 64 seed bytes.
	///
	/// **Note:** Panics if the length of the given `seed_bytes` differs from 64.
	pub fn set_entropy_seed_bytes(&mut self, seed_bytes: Vec<u8>) -> Result<&mut Self, BuildError> {
		if seed_bytes.len() != WALLET_KEYS_SEED_LEN {
			return Err(BuildError::InvalidSeedBytes);
		}
		let mut bytes = [0u8; WALLET_KEYS_SEED_LEN];
		bytes.copy_from_slice(&seed_bytes);
		self.entropy_source_config = Some(EntropySourceConfig::SeedBytes(bytes));
		Ok(self)
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

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	pub fn set_esplora_server(&mut self, esplora_server_url: String) -> &mut Self {
		self.chain_data_source_config = Some(ChainDataSourceConfig::Esplora(esplora_server_url));
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

	/// Sets the used storage directory path.
	pub fn set_storage_dir_path(&mut self, storage_dir_path: String) -> &mut Self {
		self.config.storage_dir_path = storage_dir_path;
		self
	}

	/// Sets the Bitcoin network used.
	pub fn set_network(&mut self, network: Network) -> &mut Self {
		self.config.network = network;
		self
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	pub fn set_listening_address(&mut self, listening_address: NetAddress) -> &mut Self {
		self.config.listening_address = Some(listening_address);
		self
	}

	/// Sets the level at which [`Node`] will log messages.
	pub fn set_log_level(&mut self, level: LogLevel) -> &mut Self {
		self.config.log_level = level;
		self
	}

	/// Builds a [`Node`] instance with a [`SqliteStore`] backend and according to the options
	/// previously configured.
	pub fn build(&self) -> Result<Node<SqliteStore>, BuildError> {
		let storage_dir_path = self.config.storage_dir_path.clone();
		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = Arc::new(SqliteStore::new(storage_dir_path.into()));
		self.build_with_store(kv_store)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self) -> Result<Node<FilesystemStore>, BuildError> {
		let storage_dir_path = self.config.storage_dir_path.clone();
		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = Arc::new(FilesystemStore::new(storage_dir_path.into()));
		self.build_with_store(kv_store)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store<K: KVStore + Sync + Send + 'static>(
		&self, kv_store: Arc<K>,
	) -> Result<Node<K>, BuildError> {
		let config = Arc::new(self.config.clone());

		let runtime = Arc::new(RwLock::new(None));
		build_with_store_internal(
			config,
			self.entropy_source_config.as_ref(),
			self.chain_data_source_config.as_ref(),
			self.gossip_source_config.as_ref(),
			kv_store,
			runtime,
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

	/// Configures the [`Node`] instance to source its wallet entropy from the given 64 seed bytes.
	///
	/// **Note:** Panics if the length of the given `seed_bytes` differs from 64.
	pub fn set_entropy_seed_bytes(&self, seed_bytes: Vec<u8>) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_entropy_seed_bytes(seed_bytes).map(|_| ())
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	pub fn set_entropy_bip39_mnemonic(&self, mnemonic: Mnemonic, passphrase: Option<String>) {
		self.inner.write().unwrap().set_entropy_bip39_mnemonic(mnemonic, passphrase);
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	pub fn set_esplora_server(&self, esplora_server_url: String) {
		self.inner.write().unwrap().set_esplora_server(esplora_server_url);
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

	/// Sets the used storage directory path.
	pub fn set_storage_dir_path(&self, storage_dir_path: String) {
		self.inner.write().unwrap().set_storage_dir_path(storage_dir_path);
	}

	/// Sets the Bitcoin network used.
	pub fn set_network(&self, network: Network) {
		self.inner.write().unwrap().set_network(network);
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	pub fn set_listening_address(&self, listening_address: NetAddress) {
		self.inner.write().unwrap().set_listening_address(listening_address);
	}

	/// Sets the level at which [`Node`] will log messages.
	pub fn set_log_level(&self, level: LogLevel) {
		self.inner.write().unwrap().set_log_level(level);
	}

	/// Builds a [`Node`] instance with a [`SqliteStore`] backend and according to the options
	/// previously configured.
	pub fn build(&self) -> Result<Arc<Node<SqliteStore>>, BuildError> {
		self.inner.read().unwrap().build().map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self) -> Result<Arc<Node<FilesystemStore>>, BuildError> {
		self.inner.read().unwrap().build_with_fs_store().map(Arc::new)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store<K: KVStore + Sync + Send + 'static>(
		&self, kv_store: Arc<K>,
	) -> Result<Arc<Node<K>>, BuildError> {
		self.inner.read().unwrap().build_with_store(kv_store).map(Arc::new)
	}
}

/// Builds a [`Node`] instance according to the options previously configured.
fn build_with_store_internal<K: KVStore + Sync + Send + 'static>(
	config: Arc<Config>, entropy_source_config: Option<&EntropySourceConfig>,
	chain_data_source_config: Option<&ChainDataSourceConfig>,
	gossip_source_config: Option<&GossipSourceConfig>, kv_store: Arc<K>,
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
) -> Result<Node<K>, BuildError> {
	let ldk_data_dir = format!("{}/ldk", config.storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).map_err(|_| BuildError::StoragePathAccessFailed)?;

	let bdk_data_dir = format!("{}/bdk", config.storage_dir_path);
	fs::create_dir_all(bdk_data_dir.clone()).map_err(|_| BuildError::StoragePathAccessFailed)?;

	// Initialize the Logger
	let log_file_path = format!(
		"{}/logs/ldk_node_{}.log",
		config.storage_dir_path,
		chrono::offset::Local::now().format("%Y_%m_%d")
	);
	let logger = Arc::new(
		FilesystemLogger::new(log_file_path.clone(), config.log_level)
			.map_err(|_| BuildError::LoggerSetupFailed)?,
	);

	// Initialize the on-chain wallet and chain access
	let seed_bytes = match entropy_source_config {
		Some(EntropySourceConfig::SeedBytes(bytes)) => bytes.clone(),
		Some(EntropySourceConfig::SeedFile(seed_path)) => {
			io::utils::read_or_generate_seed_file(seed_path)
		}
		Some(EntropySourceConfig::Bip39Mnemonic { mnemonic, passphrase }) => match passphrase {
			Some(passphrase) => mnemonic.to_seed(passphrase),
			None => mnemonic.to_seed(""),
		},
		None => {
			// Default to read or generate from the default location generate a seed file.
			let seed_path = format!("{}/keys_seed", config.storage_dir_path);
			io::utils::read_or_generate_seed_file(&seed_path)
		}
	};

	let xprv = bitcoin::util::bip32::ExtendedPrivKey::new_master(config.network, &seed_bytes)
		.map_err(|_| BuildError::InvalidSeedBytes)?;

	let wallet_name = bdk::wallet::wallet_name_from_descriptor(
		Bip84(xprv, bdk::KeychainKind::External),
		Some(Bip84(xprv, bdk::KeychainKind::Internal)),
		config.network,
		&Secp256k1::new(),
	)
	.map_err(|_| BuildError::WalletSetupFailed)?;

	let database_path = format!("{}/bdk_wallet_{}.sqlite", config.storage_dir_path, wallet_name);
	let database = SqliteDatabase::new(database_path);

	let bdk_wallet = bdk::Wallet::new(
		Bip84(xprv, bdk::KeychainKind::External),
		Some(Bip84(xprv, bdk::KeychainKind::Internal)),
		config.network,
		database,
	)
	.map_err(|_| BuildError::WalletSetupFailed)?;

	let (blockchain, tx_sync) = match chain_data_source_config {
		Some(ChainDataSourceConfig::Esplora(server_url)) => {
			let tx_sync = Arc::new(EsploraSyncClient::new(server_url.clone(), Arc::clone(&logger)));
			let blockchain =
				EsploraBlockchain::from_client(tx_sync.client().clone(), BDK_CLIENT_STOP_GAP)
					.with_concurrency(BDK_CLIENT_CONCURRENCY);
			(blockchain, tx_sync)
		}
		None => {
			// Default to Esplora client.
			let server_url = DEFAULT_ESPLORA_SERVER_URL.to_string();
			let tx_sync = Arc::new(EsploraSyncClient::new(server_url, Arc::clone(&logger)));
			let blockchain =
				EsploraBlockchain::from_client(tx_sync.client().clone(), BDK_CLIENT_STOP_GAP)
					.with_concurrency(BDK_CLIENT_CONCURRENCY);
			(blockchain, tx_sync)
		}
	};

	let wallet =
		Arc::new(Wallet::new(blockchain, bdk_wallet, Arc::clone(&runtime), Arc::clone(&logger)));

	// Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor<K>> = Arc::new(chainmonitor::ChainMonitor::new(
		Some(Arc::clone(&tx_sync)),
		Arc::clone(&wallet),
		Arc::clone(&logger),
		Arc::clone(&wallet),
		Arc::clone(&kv_store),
	));

	// Initialize the KeysManager
	let cur_time = SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.map_err(|_| BuildError::InvalidSystemTime)?;
	let ldk_seed_bytes: [u8; 32] = xprv.private_key.secret_bytes();
	let keys_manager = Arc::new(KeysManager::new(
		&ldk_seed_bytes,
		cur_time.as_secs(),
		cur_time.subsec_nanos(),
		Arc::clone(&wallet),
	));

	// Initialize the network graph, scorer, and router
	let network_graph =
		match io::utils::read_network_graph(Arc::clone(&kv_store), Arc::clone(&logger)) {
			Ok(graph) => Arc::new(graph),
			Err(e) => {
				if e.kind() == std::io::ErrorKind::NotFound {
					Arc::new(NetworkGraph::new(config.network, Arc::clone(&logger)))
				} else {
					return Err(BuildError::ReadFailed);
				}
			}
		};

	let scorer = match io::utils::read_scorer(
		Arc::clone(&kv_store),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	) {
		Ok(scorer) => Arc::new(Mutex::new(scorer)),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				let params = ProbabilisticScoringParameters::default();
				Arc::new(Mutex::new(ProbabilisticScorer::new(
					params,
					Arc::clone(&network_graph),
					Arc::clone(&logger),
				)))
			} else {
				return Err(BuildError::ReadFailed);
			}
		}
	};

	let router = Arc::new(DefaultRouter::new(
		Arc::clone(&network_graph),
		Arc::clone(&logger),
		keys_manager.get_secure_random_bytes(),
		Arc::clone(&scorer),
	));

	// Read ChannelMonitor state from store
	let mut channel_monitors = match io::utils::read_channel_monitors(
		Arc::clone(&kv_store),
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
	) {
		Ok(monitors) => monitors,
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Vec::new()
			} else {
				log_error!(logger, "Failed to read channel monitors: {}", e.to_string());
				return Err(BuildError::ReadFailed);
			}
		}
	};

	// Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.channel_handshake_limits.force_announced_channel_preference = false;

	if !config.trusted_peers_0conf.is_empty() {
		// Manually accept inbound channels if we expect 0conf channel requests, avoid
		// generating the events otherwise.
		user_config.manually_accept_inbound_channels = true;
	}
	let channel_manager = {
		if let Ok(mut reader) =
			kv_store.read(CHANNEL_MANAGER_PERSISTENCE_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_KEY)
		{
			let channel_monitor_references =
				channel_monitors.iter_mut().map(|(_, chanmon)| chanmon).collect();
			let read_args = ChannelManagerReadArgs::new(
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&wallet),
				Arc::clone(&chain_monitor),
				Arc::clone(&wallet),
				Arc::clone(&router),
				Arc::clone(&logger),
				user_config,
				channel_monitor_references,
			);
			let (_hash, channel_manager) =
				<(BlockHash, ChannelManager<K>)>::read(&mut reader, read_args).map_err(|e| {
					log_error!(logger, "Failed to read channel manager from KVStore: {}", e);
					BuildError::ReadFailed
				})?;
			channel_manager
		} else {
			// We're starting a fresh node.
			let genesis_block_hash =
				bitcoin::blockdata::constants::genesis_block(config.network).block_hash();

			let chain_params = ChainParameters {
				network: config.network,
				best_block: BestBlock::new(genesis_block_hash, 0),
			};
			channelmanager::ChannelManager::new(
				Arc::clone(&wallet),
				Arc::clone(&chain_monitor),
				Arc::clone(&wallet),
				Arc::clone(&router),
				Arc::clone(&logger),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				user_config,
				chain_params,
			)
		}
	};

	let channel_manager = Arc::new(channel_manager);

	// Give ChannelMonitors to ChainMonitor
	for (_blockhash, channel_monitor) in channel_monitors.into_iter() {
		let funding_outpoint = channel_monitor.get_funding_txo().0;
		chain_monitor.watch_channel(funding_outpoint, channel_monitor);
	}

	// Initialize the PeerManager
	let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		IgnoringMessageHandler {},
	));
	let ephemeral_bytes: [u8; 32] = keys_manager.get_secure_random_bytes();

	let cur_time = SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.map_err(|_| BuildError::InvalidSystemTime)?;

	// Initialize the GossipSource
	// Use the configured gossip source, if the user set one, otherwise default to P2PNetwork.
	let gossip_source_config = gossip_source_config.unwrap_or(&GossipSourceConfig::P2PNetwork);

	let gossip_source = match gossip_source_config {
		GossipSourceConfig::P2PNetwork => {
			let p2p_source =
				Arc::new(GossipSource::new_p2p(Arc::clone(&network_graph), Arc::clone(&logger)));

			// Reset the RGS sync timestamp in case we somehow switch gossip sources
			io::utils::write_latest_rgs_sync_timestamp(
				0,
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			)
			.map_err(|_| BuildError::WriteFailed)?;
			p2p_source
		}
		GossipSourceConfig::RapidGossipSync(rgs_server) => {
			let latest_sync_timestamp = io::utils::read_latest_rgs_sync_timestamp(
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			)
			.unwrap_or(0);
			Arc::new(GossipSource::new_rgs(
				rgs_server.clone(),
				latest_sync_timestamp,
				Arc::clone(&network_graph),
				Arc::clone(&logger),
			))
		}
	};

	let msg_handler = match gossip_source.as_gossip_sync() {
		GossipSync::P2P(p2p_gossip_sync) => MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::clone(&p2p_gossip_sync)
				as Arc<dyn RoutingMessageHandler + Sync + Send>,
			onion_message_handler: onion_messenger,
		},
		GossipSync::Rapid(_) => MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::new(IgnoringMessageHandler {})
				as Arc<dyn RoutingMessageHandler + Sync + Send>,
			onion_message_handler: onion_messenger,
		},
		GossipSync::None => {
			unreachable!("We must always have a gossip sync!");
		}
	};

	let peer_manager = Arc::new(PeerManager::new(
		msg_handler,
		cur_time.as_secs().try_into().map_err(|_| BuildError::InvalidSystemTime)?,
		&ephemeral_bytes,
		Arc::clone(&logger),
		IgnoringMessageHandler {},
		Arc::clone(&keys_manager),
	));

	// Init payment info storage
	let payment_store = match io::utils::read_payments(Arc::clone(&kv_store), Arc::clone(&logger)) {
		Ok(payments) => {
			Arc::new(PaymentStore::new(payments, Arc::clone(&kv_store), Arc::clone(&logger)))
		}
		Err(_) => {
			return Err(BuildError::ReadFailed);
		}
	};

	let event_queue = match io::utils::read_event_queue(Arc::clone(&kv_store), Arc::clone(&logger))
	{
		Ok(event_queue) => Arc::new(event_queue),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(EventQueue::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				return Err(BuildError::ReadFailed);
			}
		}
	};

	let peer_store = match io::utils::read_peer_info(Arc::clone(&kv_store), Arc::clone(&logger)) {
		Ok(peer_store) => Arc::new(peer_store),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(PeerStore::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				return Err(BuildError::ReadFailed);
			}
		}
	};

	let (stop_sender, stop_receiver) = tokio::sync::watch::channel(());

	Ok(Node {
		runtime,
		stop_sender,
		stop_receiver,
		config,
		wallet,
		tx_sync,
		event_queue,
		channel_manager,
		chain_monitor,
		peer_manager,
		keys_manager,
		network_graph,
		gossip_source,
		kv_store,
		logger,
		scorer,
		peer_store,
		payment_store,
	})
}
