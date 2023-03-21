// This file is Copyright its original authors, visible in version contror
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#![crate_name = "ldk_node"]

//! # LDK Node
//! A ready-to-go Lightning node library built using [LDK](https://lightningdevkit.org/) and
//! [BDK](https://bitcoindevkit.org/).
//!
//! LDK Node is a non-custodial Lightning node in library form. Its central goal is to provide a
//! small, simple, and straightforward interface that enables users to easily set up and run a
//! Lightning node with an integrated on-chain wallet. While minimalism is at its core, LDK Node
//! aims to be sufficiently modular and configurable to be useful for a variety of use cases.
//!
//! ## Getting Started
//!
//! The primary abstraction of the library is the [`Node`], which can be retrieved by setting up
//! and configuring a [`Builder`] to your liking and calling [`build`]. `Node` can then be
//! controlled via commands such as [`start`], [`stop`], [`connect_open_channel`],
//! [`send_payment`], etc.:
//!
//! ```no_run
//! use ldk_node::Builder;
//! use ldk_node::lightning_invoice::Invoice;
//! use std::str::FromStr;
//!
//! fn main() {
//! 	let node = Builder::new()
//! 		.set_network("testnet")
//! 		.set_esplora_server_url("https://blockstream.info/testnet/api".to_string())
//! 		.build();
//!
//! 	node.start().unwrap();
//!
//! 	let _funding_address = node.new_funding_address();
//!
//! 	// .. fund address ..
//!
//! 	node.sync_wallets().unwrap();
//!
//! 	node.connect_open_channel("NODE_ID@PEER_ADDR:PORT", 10000, false).unwrap();
//!
//! 	let invoice = Invoice::from_str("INVOICE_STR").unwrap();
//! 	node.send_payment(invoice).unwrap();
//!
//! 	node.stop().unwrap();
//! }
//! ```
//!
//! [`build`]: Builder::build
//! [`start`]: Node::start
//! [`stop`]: Node::stop
//! [`connect_open_channel`]: Node::connect_open_channel
//! [`send_payment`]: Node::send_payment
//!
#![deny(missing_docs)]
#![deny(broken_intra_doc_links)]
#![deny(private_intra_doc_links)]
#![allow(bare_trait_objects)]
#![allow(ellipsis_inclusive_range_patterns)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod error;
mod event;
mod hex_utils;
mod io_utils;
mod logger;
mod peer_store;
#[cfg(test)]
mod tests;
mod types;
mod wallet;

pub use bitcoin;
pub use lightning;
pub use lightning_invoice;

pub use error::Error;
pub use event::Event;
use event::{EventHandler, EventQueue};
use peer_store::{PeerInfo, PeerInfoStorage};
use types::{
	ChainMonitor, ChannelManager, GossipSync, KeysManager, NetworkGraph, OnionMessenger,
	PaymentInfoStorage, PeerManager, Scorer,
};
pub use types::{PaymentInfo, PaymentStatus};
use wallet::Wallet;

use logger::{log_error, log_info, FilesystemLogger, Logger};

use lightning::chain::keysinterface::EntropySource;
use lightning::chain::{chainmonitor, BestBlock, Confirm, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{
	ChainParameters, ChannelDetails, ChannelManagerReadArgs, PaymentId, Retry,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::utxo::UtxoLookup;

use lightning::util::config::{ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig};
use lightning::util::ser::ReadableArgs;

use lightning_background_processor::BackgroundProcessor;
use lightning_background_processor::GossipSync as BPGossipSync;
use lightning_persister::FilesystemPersister;

use lightning_transaction_sync::EsploraSyncClient;

use lightning::routing::router::{DefaultRouter, PaymentParameters, RouteParameters};
use lightning_invoice::{payment, Currency, Invoice};

use bdk::bitcoin::secp256k1::Secp256k1;
use bdk::blockchain::esplora::EsploraBlockchain;
use bdk::database::SqliteDatabase;
use bdk::template::Bip84;

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::BlockHash;

use rand::Rng;

use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::default::Default;
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

// The 'stop gap' parameter used by BDK's wallet sync. This seems to configure the threshold
// number of blocks after which BDK stops looking for scripts belonging to the wallet.
const BDK_CLIENT_STOP_GAP: usize = 20;

// The number of concurrent requests made against the API provider.
const BDK_CLIENT_CONCURRENCY: u8 = 8;

// The timeout after which we abandon retrying failed payments.
const LDK_PAYMENT_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

// The time in between peer reconnection attempts.
const PEER_RECONNECTION_INTERVAL: Duration = Duration::from_secs(10);

// The length in bytes of our wallets' keys seed.
const WALLET_KEYS_SEED_LEN: usize = 64;

#[derive(Debug, Clone)]
/// Represents the configuration of an [`Node`] instance.
pub struct Config {
	/// The path where the underlying LDK and BDK persist their data.
	pub storage_dir_path: String,
	/// The URL of the utilized Esplora server.
	pub esplora_server_url: String,
	/// The used Bitcoin network.
	pub network: bitcoin::Network,
	/// The IP address and TCP port the node will listen on.
	pub listening_address: Option<String>,
	/// The default CLTV expiry delta to be used for payments.
	pub default_cltv_expiry_delta: u32,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			storage_dir_path: "/tmp/ldk_node/".to_string(),
			esplora_server_url: "http://localhost:3002".to_string(),
			network: bitcoin::Network::Regtest,
			listening_address: Some("0.0.0.0:9735".to_string()),
			default_cltv_expiry_delta: 144,
		}
	}
}

#[derive(Debug, Clone)]
enum WalletEntropySource {
	SeedFile(String),
	SeedBytes([u8; WALLET_KEYS_SEED_LEN]),
}

/// A builder for an [`Node`] instance, allowing to set some configuration and module choices from
/// the getgo.
#[derive(Debug, Clone)]
pub struct Builder {
	config: Config,
	entropy_source: Option<WalletEntropySource>,
}

impl Builder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let config = Config::default();
		let entropy_source = None;
		Self { config, entropy_source }
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let entropy_source = None;
		Self { config, entropy_source }
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	pub fn set_entropy_seed_path(&mut self, seed_path: String) -> &mut Self {
		self.entropy_source = Some(WalletEntropySource::SeedFile(seed_path));
		self
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given seed bytes.
	pub fn set_entropy_seed_bytes(&mut self, seed_bytes: [u8; WALLET_KEYS_SEED_LEN]) -> &mut Self {
		self.entropy_source = Some(WalletEntropySource::SeedBytes(seed_bytes));
		self
	}

	/// Sets the used storage directory path.
	///
	/// Default: `/tmp/ldk_node/`
	pub fn set_storage_dir_path(&mut self, storage_dir_path: String) -> &mut Self {
		self.config.storage_dir_path = storage_dir_path;
		self
	}

	/// Sets the Esplora server URL.
	///
	/// Default: `https://blockstream.info/api`
	pub fn set_esplora_server_url(&mut self, esplora_server_url: String) -> &mut Self {
		self.config.esplora_server_url = esplora_server_url;
		self
	}

	/// Sets the Bitcoin network used.
	///
	/// Options: `mainnet`/`bitcoin`, `testnet`, `regtest`, `signet`
	///
	/// Default: `testnet`
	pub fn set_network(&mut self, network: &str) -> &mut Self {
		self.config.network = match network {
			"mainnet" => bitcoin::Network::Bitcoin,
			"bitcoin" => bitcoin::Network::Bitcoin,
			"testnet" => bitcoin::Network::Testnet,
			"regtest" => bitcoin::Network::Regtest,
			"signet" => bitcoin::Network::Signet,
			_ => bitcoin::Network::Regtest,
		};
		self
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	///
	/// Format: `ADDR:PORT`
	/// Default: `0.0.0.0:9735`
	pub fn set_listening_address(&mut self, listening_address: String) -> &mut Self {
		self.config.listening_address = Some(listening_address);
		self
	}

	/// Builds an [`Node`] instance according to the options previously configured.
	pub fn build(&self) -> Node {
		let config = Arc::new(self.config.clone());

		let ldk_data_dir = format!("{}/ldk", config.storage_dir_path);
		fs::create_dir_all(ldk_data_dir.clone()).expect("Failed to create LDK data directory");

		let bdk_data_dir = format!("{}/bdk", config.storage_dir_path);
		fs::create_dir_all(bdk_data_dir.clone()).expect("Failed to create BDK data directory");

		// Initialize the Logger
		let log_file_path = format!("{}/ldk_node.log", config.storage_dir_path);
		let logger = Arc::new(FilesystemLogger::new(log_file_path));

		// Initialize the on-chain wallet and chain access
		let seed_bytes = if let Some(entropy_source) = &self.entropy_source {
			// Use the configured entropy source, if the user set one.
			match entropy_source {
				WalletEntropySource::SeedBytes(bytes) => bytes.clone(),
				WalletEntropySource::SeedFile(seed_path) => {
					io_utils::read_or_generate_seed_file(seed_path)
				}
			}
		} else {
			// Default to read or generate from the default location generate a seed file.
			let seed_path = format!("{}/keys_seed", config.storage_dir_path);
			io_utils::read_or_generate_seed_file(&seed_path)
		};

		let xprv = bitcoin::util::bip32::ExtendedPrivKey::new_master(config.network, &seed_bytes)
			.expect("Failed to read wallet master key");

		let wallet_name = bdk::wallet::wallet_name_from_descriptor(
			Bip84(xprv, bdk::KeychainKind::External),
			Some(Bip84(xprv, bdk::KeychainKind::Internal)),
			config.network,
			&Secp256k1::new(),
		)
		.expect("Failed to derive on-chain wallet name");

		let database_path = format!("{}/{}.sqlite", bdk_data_dir, wallet_name);
		let database = SqliteDatabase::new(database_path);

		let bdk_wallet = bdk::Wallet::new(
			Bip84(xprv, bdk::KeychainKind::External),
			Some(Bip84(xprv, bdk::KeychainKind::Internal)),
			config.network,
			database,
		)
		.expect("Failed to set up on-chain wallet");

		let tx_sync = Arc::new(EsploraSyncClient::new(
			config.esplora_server_url.clone(),
			Arc::clone(&logger),
		));

		let blockchain =
			EsploraBlockchain::from_client(tx_sync.client().clone(), BDK_CLIENT_STOP_GAP)
				.with_concurrency(BDK_CLIENT_CONCURRENCY);

		let wallet = Arc::new(Wallet::new(blockchain, bdk_wallet, Arc::clone(&logger)));

		// Initialize Persist
		let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

		// Initialize the ChainMonitor
		let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
			Some(Arc::clone(&tx_sync)),
			Arc::clone(&wallet),
			Arc::clone(&logger),
			Arc::clone(&wallet),
			Arc::clone(&persister),
		));

		// Initialize the KeysManager
		let cur_time = SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.expect("System time error: Clock may have gone backwards");
		let ldk_seed_bytes: [u8; 32] = xprv.private_key.secret_bytes();
		let keys_manager = Arc::new(KeysManager::new(
			&ldk_seed_bytes,
			cur_time.as_secs(),
			cur_time.subsec_nanos(),
			Arc::clone(&wallet),
		));

		// Initialize the network graph, scorer, and router
		let network_graph =
			Arc::new(io_utils::read_network_graph(config.as_ref(), Arc::clone(&logger)));
		let scorer = Arc::new(Mutex::new(io_utils::read_scorer(
			config.as_ref(),
			Arc::clone(&network_graph),
			Arc::clone(&logger),
		)));

		let router = Arc::new(DefaultRouter::new(
			Arc::clone(&network_graph),
			Arc::clone(&logger),
			keys_manager.get_secure_random_bytes(),
			Arc::clone(&scorer),
		));

		// Read ChannelMonitor state from disk
		let mut channel_monitors = persister
			.read_channelmonitors(Arc::clone(&keys_manager), Arc::clone(&keys_manager))
			.expect("Failed to read channel monitors from disk");

		// Initialize the ChannelManager
		let mut user_config = UserConfig::default();
		user_config.channel_handshake_limits.force_announced_channel_preference = false;
		let channel_manager = {
			if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir)) {
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
					<(BlockHash, ChannelManager)>::read(&mut f, read_args)
						.expect("Failed to read channel manager from disk");
				channel_manager
			} else {
				// We're starting a fresh node.
				let genesis_block_hash =
					bitcoin::blockdata::constants::genesis_block(config.network)
						.header
						.block_hash();

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

		// Initialize the P2PGossipSync
		let gossip_sync = Arc::new(P2PGossipSync::new(
			Arc::clone(&network_graph),
			None::<Arc<dyn UtxoLookup + Send + Sync>>,
			Arc::clone(&logger),
		));

		// Initialize the PeerManager
		let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
			Arc::clone(&keys_manager),
			Arc::clone(&keys_manager),
			Arc::clone(&logger),
			IgnoringMessageHandler {},
		));
		let ephemeral_bytes: [u8; 32] = keys_manager.get_secure_random_bytes();
		let lightning_msg_handler = MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::clone(&gossip_sync),
			onion_message_handler: onion_messenger,
		};

		let cur_time = SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.expect("System time error: Clock may have gone backwards");
		let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
			lightning_msg_handler,
			cur_time.as_secs().try_into().expect("System time error"),
			&ephemeral_bytes,
			Arc::clone(&logger),
			IgnoringMessageHandler {},
			Arc::clone(&keys_manager),
		));

		// Init payment info storage
		// TODO: persist payment info to disk
		let inbound_payments = Arc::new(Mutex::new(HashMap::new()));
		let outbound_payments = Arc::new(Mutex::new(HashMap::new()));

		// Restore event handler from disk or create a new one.
		let event_queue = if let Ok(mut f) =
			fs::File::open(format!("{}/{}", ldk_data_dir, event::EVENTS_PERSISTENCE_KEY))
		{
			Arc::new(
				EventQueue::read(&mut f, Arc::clone(&persister))
					.expect("Failed to read event queue from disk."),
			)
		} else {
			Arc::new(EventQueue::new(Arc::clone(&persister)))
		};

		let peer_store = if let Ok(mut f) =
			fs::File::open(format!("{}/{}", ldk_data_dir, peer_store::PEER_INFO_PERSISTENCE_KEY))
		{
			Arc::new(
				PeerInfoStorage::read(&mut f, Arc::clone(&persister))
					.expect("Failed to read peer information from disk."),
			)
		} else {
			Arc::new(PeerInfoStorage::new(Arc::clone(&persister)))
		};

		let running = RwLock::new(None);

		Node {
			running,
			config,
			wallet,
			tx_sync,
			event_queue,
			channel_manager,
			chain_monitor,
			peer_manager,
			keys_manager,
			network_graph,
			gossip_sync,
			persister,
			logger,
			scorer,
			inbound_payments,
			outbound_payments,
			peer_store,
		}
	}
}

/// Wraps all objects that need to be preserved during the run time of [`Node`]. Will be dropped
/// upon [`Node::stop()`].
struct Runtime {
	tokio_runtime: Arc<tokio::runtime::Runtime>,
	_background_processor: BackgroundProcessor,
	stop_networking: Arc<AtomicBool>,
	stop_wallet_sync: Arc<AtomicBool>,
}

/// The main interface object of LDK Node, wrapping the necessary LDK and BDK functionalities.
///
/// Needs to be initialized and instantiated through [`Builder::build`].
pub struct Node {
	running: RwLock<Option<Runtime>>,
	config: Arc<Config>,
	wallet: Arc<Wallet<bdk::database::SqliteDatabase>>,
	tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	event_queue: Arc<EventQueue<Arc<FilesystemPersister>>>,
	channel_manager: Arc<ChannelManager>,
	chain_monitor: Arc<ChainMonitor>,
	peer_manager: Arc<PeerManager>,
	keys_manager: Arc<KeysManager>,
	network_graph: Arc<NetworkGraph>,
	gossip_sync: Arc<GossipSync>,
	persister: Arc<FilesystemPersister>,
	logger: Arc<FilesystemLogger>,
	scorer: Arc<Mutex<Scorer>>,
	inbound_payments: Arc<PaymentInfoStorage>,
	outbound_payments: Arc<PaymentInfoStorage>,
	peer_store: Arc<PeerInfoStorage<FilesystemPersister>>,
}

impl Node {
	/// Starts the necessary background tasks, such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network.
	///
	/// After this returns, the [`Node`] instance can be controlled via the provided API methods in
	/// a thread-safe manner.
	pub fn start(&self) -> Result<(), Error> {
		// Acquire a run lock and hold it until we're setup.
		let mut run_lock = self.running.write().unwrap();
		if run_lock.is_some() {
			// We're already running.
			return Err(Error::AlreadyRunning);
		}

		let runtime = self.setup_runtime()?;
		*run_lock = Some(runtime);
		Ok(())
	}

	/// Disconnects all peers, stops all running background tasks, and shuts down [`Node`].
	///
	/// After this returns most API methods will return [`Error::NotRunning`].
	pub fn stop(&self) -> Result<(), Error> {
		let mut run_lock = self.running.write().unwrap();
		if run_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let runtime = run_lock.as_ref().unwrap();

		// Stop wallet sync
		runtime.stop_wallet_sync.store(true, Ordering::Release);

		// Stop networking
		runtime.stop_networking.store(true, Ordering::Release);
		self.peer_manager.disconnect_all_peers();

		// Drop the held runtimes.
		self.wallet.drop_runtime();

		// Drop the runtime, which stops the background processor and any possibly remaining tokio threads.
		*run_lock = None;
		Ok(())
	}

	fn setup_runtime(&self) -> Result<Runtime, Error> {
		let tokio_runtime =
			Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap());

		self.wallet.set_runtime(Arc::clone(&tokio_runtime));

		let event_handler = Arc::new(EventHandler::new(
			Arc::clone(&self.wallet),
			Arc::clone(&self.event_queue),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.network_graph),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.inbound_payments),
			Arc::clone(&self.outbound_payments),
			Arc::clone(&tokio_runtime),
			Arc::clone(&self.logger),
			Arc::clone(&self.config),
		));

		// Setup wallet sync
		let wallet = Arc::clone(&self.wallet);
		let tx_sync = Arc::clone(&self.tx_sync);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let sync_logger = Arc::clone(&self.logger);
		let stop_wallet_sync = Arc::new(AtomicBool::new(false));
		let stop_sync = Arc::clone(&stop_wallet_sync);

		std::thread::spawn(move || {
			tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
				async move {
					loop {
						if stop_sync.load(Ordering::Acquire) {
							return;
						}
						let now = Instant::now();
						match wallet.sync().await {
							Ok(()) => log_info!(
								sync_logger,
								"Background sync of on-chain wallet finished in {}ms.",
								now.elapsed().as_millis()
							),
							Err(err) => {
								log_error!(
									sync_logger,
									"Background sync of on-chain wallet failed: {}",
									err
								)
							}
						}
						tokio::time::sleep(Duration::from_secs(20)).await;
					}
				},
			);
		});

		let sync_logger = Arc::clone(&self.logger);
		let stop_sync = Arc::clone(&stop_wallet_sync);
		tokio_runtime.spawn(async move {
			loop {
				if stop_sync.load(Ordering::Acquire) {
					return;
				}
				let now = Instant::now();
				let confirmables = vec![
					&*sync_cman as &(dyn Confirm + Sync + Send),
					&*sync_cmon as &(dyn Confirm + Sync + Send),
				];
				match tx_sync.sync(confirmables).await {
					Ok(()) => log_info!(
						sync_logger,
						"Background sync of Lightning wallet finished in {}ms.",
						now.elapsed().as_millis()
					),
					Err(e) => {
						log_error!(sync_logger, "Background sync of Lightning wallet failed: {}", e)
					}
				}
				tokio::time::sleep(Duration::from_secs(5)).await;
			}
		});

		let stop_networking = Arc::new(AtomicBool::new(false));
		if let Some(listening_address) = &self.config.listening_address {
			// Setup networking
			let peer_manager_connection_handler = Arc::clone(&self.peer_manager);
			let stop_listen = Arc::clone(&stop_networking);
			let listening_address = listening_address.clone();

			tokio_runtime.spawn(async move {
				let listener =
					tokio::net::TcpListener::bind(listening_address).await.expect(
						"Failed to bind to listen address/port - is something else already listening on it?",
						);
				loop {
					if stop_listen.load(Ordering::Acquire) {
						return;
					}
					let peer_mgr = Arc::clone(&peer_manager_connection_handler);
					let tcp_stream = listener.accept().await.unwrap().0;
					tokio::spawn(async move {
						lightning_net_tokio::setup_inbound(
							Arc::clone(&peer_mgr),
							tcp_stream.into_std().unwrap(),
						)
						.await;
					});
				}
			});
		}

		// Regularly reconnect to channel peers.
		let connect_cm = Arc::clone(&self.channel_manager);
		let connect_pm = Arc::clone(&self.peer_manager);
		let connect_logger = Arc::clone(&self.logger);
		let connect_peer_store = Arc::clone(&self.peer_store);
		let stop_connect = Arc::clone(&stop_networking);
		tokio_runtime.spawn(async move {
			let mut interval = tokio::time::interval(PEER_RECONNECTION_INTERVAL);
			loop {
				if stop_connect.load(Ordering::Acquire) {
					return;
				}
				let pm_peers = connect_pm
					.get_peer_node_ids()
					.iter()
					.map(|(peer, _addr)| *peer)
					.collect::<Vec<_>>();
				for node_id in connect_cm
					.list_channels()
					.iter()
					.map(|chan| chan.counterparty.node_id)
					.filter(|id| !pm_peers.contains(id))
				{
					if let Some(peer_info) = connect_peer_store.get_peer(&node_id) {
						let _ = do_connect_peer(
							peer_info.pubkey,
							peer_info.address,
							Arc::clone(&connect_pm),
							Arc::clone(&connect_logger),
						)
						.await;
					}
				}
				interval.tick().await;
			}
		});

		// Setup background processing
		let _background_processor = BackgroundProcessor::start(
			Arc::clone(&self.persister),
			Arc::clone(&event_handler),
			Arc::clone(&self.chain_monitor),
			Arc::clone(&self.channel_manager),
			BPGossipSync::p2p(Arc::clone(&self.gossip_sync)),
			Arc::clone(&self.peer_manager),
			Arc::clone(&self.logger),
			Some(Arc::clone(&self.scorer)),
		);

		// TODO: frequently check back on background_processor if there was an error
		Ok(Runtime { tokio_runtime, _background_processor, stop_networking, stop_wallet_sync })
	}

	/// Blocks until the next event is available.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Node::event_handled`].
	pub fn next_event(&self) -> Event {
		self.event_queue.next_event()
	}

	/// Confirm the last retrieved event handled.
	///
	/// **Note:** This **MUST** be called after each event has been handled.
	pub fn event_handled(&self) {
		self.event_queue.event_handled().unwrap();
	}

	/// Returns our own node id
	pub fn node_id(&self) -> PublicKey {
		self.channel_manager.get_our_node_id()
	}

	/// Returns our own listening address and port.
	pub fn listening_address(&self) -> Option<String> {
		self.config.listening_address.clone()
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_funding_address(&self) -> Result<bitcoin::Address, Error> {
		let funding_address = self.wallet.get_new_address()?;
		log_info!(self.logger, "Generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	/// Retrieve the current on-chain balance.
	pub fn on_chain_balance(&self) -> Result<bdk::Balance, Error> {
		self.wallet.get_balance()
	}

	/// Retrieve a list of known channels.
	pub fn list_channels(&self) -> Vec<ChannelDetails> {
		self.channel_manager.list_channels()
	}

	/// Connect to a node and open a new channel. Disconnects and re-connects are handled automatically
	///
	/// Returns a temporary channel id
	pub fn connect_open_channel(
		&self, node_pubkey_and_address: &str, channel_amount_sats: u64, announce_channel: bool,
	) -> Result<(), Error> {
		let runtime_lock = self.running.read().unwrap();
		if runtime_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let runtime = runtime_lock.as_ref().unwrap();

		let cur_balance = self.wallet.get_balance()?;
		if cur_balance.get_spendable() < channel_amount_sats {
			log_error!(self.logger, "Unable to create channel due to insufficient funds.");
			return Err(Error::InsufficientFunds);
		}

		let peer_info = PeerInfo::try_from(node_pubkey_and_address.to_string())?;

		let con_peer_pubkey = peer_info.pubkey.clone();
		let con_peer_addr = peer_info.address.clone();
		let con_success = Arc::new(AtomicBool::new(false));
		let con_success_cloned = Arc::clone(&con_success);
		let con_logger = Arc::clone(&self.logger);
		let con_pm = Arc::clone(&self.peer_manager);

		tokio::task::block_in_place(move || {
			runtime.tokio_runtime.block_on(async move {
				let res =
					connect_peer_if_necessary(con_peer_pubkey, con_peer_addr, con_pm, con_logger)
						.await;
				con_success_cloned.store(res.is_ok(), Ordering::Release);
			})
		});

		if !con_success.load(Ordering::Acquire) {
			return Err(Error::ConnectionFailed);
		}

		let user_config = UserConfig {
			channel_handshake_limits: ChannelHandshakeLimits {
				// lnd's max to_self_delay is 2016, so we want to be compatible.
				their_to_self_delay: 2016,
				..Default::default()
			},
			channel_handshake_config: ChannelHandshakeConfig {
				announced_channel: announce_channel,
				..Default::default()
			},
			..Default::default()
		};

		let user_channel_id: u128 = rand::thread_rng().gen::<u128>();

		match self.channel_manager.create_channel(
			peer_info.pubkey,
			channel_amount_sats,
			0,
			user_channel_id,
			Some(user_config),
		) {
			Ok(_) => {
				self.peer_store.add_peer(peer_info.clone())?;
				log_info!(
					self.logger,
					"Initiated channel creation with peer {}. ",
					peer_info.pubkey
				);
				Ok(())
			}
			Err(e) => {
				log_error!(self.logger, "Failed to initiate channel creation: {:?}", e);
				Err(Error::ChannelCreationFailed)
			}
		}
	}

	/// Sync the LDK and BDK wallets with the current chain state.
	///
	/// Note that the wallets will be also synced regularly in the background.
	pub fn sync_wallets(&self) -> Result<(), Error> {
		let runtime_lock = self.running.read().unwrap();
		if runtime_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let wallet = Arc::clone(&self.wallet);
		let tx_sync = Arc::clone(&self.tx_sync);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let sync_logger = Arc::clone(&self.logger);
		let confirmables = vec![
			&*sync_cman as &(dyn Confirm + Sync + Send),
			&*sync_cmon as &(dyn Confirm + Sync + Send),
		];

		let runtime = runtime_lock.as_ref().unwrap();
		tokio::task::block_in_place(move || {
			tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
				async move {
					let now = Instant::now();
					match wallet.sync().await {
						Ok(()) => {
							log_info!(
								sync_logger,
								"Sync of on-chain wallet finished in {}ms.",
								now.elapsed().as_millis()
							);
							Ok(())
						}
						Err(e) => {
							log_error!(sync_logger, "Sync of on-chain wallet failed: {}", e);
							Err(e)
						}
					}
				},
			)
		})?;

		let sync_logger = Arc::clone(&self.logger);
		tokio::task::block_in_place(move || {
			runtime.tokio_runtime.block_on(async move {
				let now = Instant::now();
				match tx_sync.sync(confirmables).await {
					Ok(()) => {
						log_info!(
							sync_logger,
							"Sync of Lightning wallet finished in {}ms.",
							now.elapsed().as_millis()
						);
						Ok(())
					}
					Err(e) => {
						log_error!(sync_logger, "Sync of Lightning wallet failed: {}", e);
						Err(e)
					}
				}
			})
		})?;

		Ok(())
	}

	/// Close a previously opened channel.
	pub fn close_channel(
		&self, channel_id: &[u8; 32], counterparty_node_id: &PublicKey,
	) -> Result<(), Error> {
		self.peer_store.remove_peer(counterparty_node_id)?;
		match self.channel_manager.close_channel(channel_id, counterparty_node_id) {
			Ok(_) => Ok(()),
			Err(_) => Err(Error::ChannelClosingFailed),
		}
	}

	/// Send a payement given an invoice.
	pub fn send_payment(&self, invoice: Invoice) -> Result<PaymentHash, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		let mut outbound_payments_lock = self.outbound_payments.lock().unwrap();

		let payment_hash = PaymentHash((*invoice.payment_hash()).into_inner());
		let payment_secret = Some(*invoice.payment_secret());

		match lightning_invoice::payment::pay_invoice(
			&invoice,
			Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
			self.channel_manager.as_ref(),
		) {
			Ok(_payment_id) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				let amt_msat = invoice.amount_milli_satoshis().unwrap();
				log_info!(self.logger, "Initiated sending {}msat to {}", amt_msat, payee_pubkey);

				outbound_payments_lock.insert(
					payment_hash,
					PaymentInfo {
						preimage: None,
						secret: payment_secret,
						status: PaymentStatus::Pending,
						amount_msat: invoice.amount_milli_satoshis(),
					},
				);

				Ok(payment_hash)
			}
			Err(payment::PaymentError::Invoice(e)) => {
				log_error!(self.logger, "Failed to send payment due to invalid invoice: {}", e);
				Err(Error::InvalidInvoice)
			}
			Err(payment::PaymentError::Sending(e)) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);

				outbound_payments_lock.insert(
					payment_hash,
					PaymentInfo {
						preimage: None,
						secret: payment_secret,
						status: PaymentStatus::Failed,
						amount_msat: invoice.amount_milli_satoshis(),
					},
				);
				Err(Error::PaymentFailed)
			}
		}
	}

	/// Send a payment given an invoice and an amount in millisatoshi.
	///
	/// This will fail if the amount given is less than the value required by the given invoice.
	///
	/// This can be used to pay a so-called "zero-amount" invoice, i.e., an invoice that leaves the
	/// amount paid to be determined by the user.
	pub fn send_payment_using_amount(
		&self, invoice: Invoice, amount_msat: u64,
	) -> Result<PaymentHash, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		let mut outbound_payments_lock = self.outbound_payments.lock().unwrap();

		if let Some(invoice_amount_msat) = invoice.amount_milli_satoshis() {
			if amount_msat < invoice_amount_msat {
				log_error!(
					self.logger,
					"Failed to pay as the given amount needs to be at least the invoice amount: required {}msat, gave {}msat.", invoice_amount_msat, amount_msat);
				return Err(Error::InvalidAmount);
			}
		}

		let payment_id = PaymentId(invoice.payment_hash().into_inner());
		let payment_hash = PaymentHash((*invoice.payment_hash()).into_inner());
		let payment_secret = Some(*invoice.payment_secret());
		let expiry_time = invoice.duration_since_epoch().saturating_add(invoice.expiry_time());
		let mut payment_params = PaymentParameters::from_node_id(
			invoice.recover_payee_pub_key(),
			invoice.min_final_cltv_expiry_delta() as u32,
		)
		.with_expiry_time(expiry_time.as_secs())
		.with_route_hints(invoice.route_hints());
		if let Some(features) = invoice.features() {
			payment_params = payment_params.with_features(features.clone());
		}
		let route_params = RouteParameters { payment_params, final_value_msat: amount_msat };

		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);

		match self
			.channel_manager
			.send_payment_with_retry(
				payment_hash,
				&payment_secret,
				payment_id,
				route_params,
				retry_strategy,
			)
			.map_err(payment::PaymentError::Sending)
		{
			Ok(_payment_id) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				log_info!(
					self.logger,
					"Initiated sending {} msat to {}",
					amount_msat,
					payee_pubkey
				);

				outbound_payments_lock.insert(
					payment_hash,
					PaymentInfo {
						preimage: None,
						secret: payment_secret,
						status: PaymentStatus::Pending,
						amount_msat: Some(amount_msat),
					},
				);

				Ok(payment_hash)
			}
			Err(payment::PaymentError::Invoice(e)) => {
				log_error!(self.logger, "Failed to send payment due to invalid invoice: {}", e);
				Err(Error::InvalidInvoice)
			}
			Err(payment::PaymentError::Sending(e)) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);

				outbound_payments_lock.insert(
					payment_hash,
					PaymentInfo {
						preimage: None,
						secret: payment_secret,
						status: PaymentStatus::Failed,
						amount_msat: Some(amount_msat),
					},
				);
				Err(Error::PaymentFailed)
			}
		}
	}

	/// Send a spontaneous, aka. "keysend", payment
	pub fn send_spontaneous_payment(
		&self, amount_msat: u64, node_id: &str,
	) -> Result<PaymentHash, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		let mut outbound_payments_lock = self.outbound_payments.lock().unwrap();

		let pubkey = hex_utils::to_compressed_pubkey(node_id).ok_or(Error::PeerInfoParseFailed)?;

		let payment_preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
		let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());

		let route_params = RouteParameters {
			payment_params: PaymentParameters::from_node_id(
				pubkey,
				self.config.default_cltv_expiry_delta,
			),
			final_value_msat: amount_msat,
		};

		match self.channel_manager.send_spontaneous_payment_with_retry(
			Some(payment_preimage),
			PaymentId(payment_hash.0),
			route_params,
			Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
		) {
			Ok(_payment_id) => {
				log_info!(self.logger, "Initiated sending {}msat to {}.", amount_msat, node_id);
				outbound_payments_lock.insert(
					payment_hash,
					PaymentInfo {
						preimage: None,
						secret: None,
						status: PaymentStatus::Pending,
						amount_msat: Some(amount_msat),
					},
				);
				Ok(payment_hash)
			}
			Err(e) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);
				outbound_payments_lock.insert(
					payment_hash,
					PaymentInfo {
						preimage: None,
						secret: None,
						status: PaymentStatus::Failed,
						amount_msat: Some(amount_msat),
					},
				);
				Err(Error::PaymentFailed)
			}
		}
	}

	/// Returns a payable invoice that can be used to request and receive a payment of the amount
	/// given.
	pub fn receive_payment(
		&self, amount_msat: u64, description: &str, expiry_secs: u32,
	) -> Result<Invoice, Error> {
		self.receive_payment_inner(Some(amount_msat), description, expiry_secs)
	}

	/// Returns a payable invoice that can be used to request and receive a payment for which the
	/// amount is to be determined by the user, also known as a "zero-amount" invoice.
	pub fn receive_variable_amount_payment(
		&self, description: &str, expiry_secs: u32,
	) -> Result<Invoice, Error> {
		self.receive_payment_inner(None, description, expiry_secs)
	}

	fn receive_payment_inner(
		&self, amount_msat: Option<u64>, description: &str, expiry_secs: u32,
	) -> Result<Invoice, Error> {
		let mut inbound_payments_lock = self.inbound_payments.lock().unwrap();

		let currency = match self.config.network {
			bitcoin::Network::Bitcoin => Currency::Bitcoin,
			bitcoin::Network::Testnet => Currency::BitcoinTestnet,
			bitcoin::Network::Regtest => Currency::Regtest,
			bitcoin::Network::Signet => Currency::Signet,
		};
		let keys_manager = Arc::clone(&self.keys_manager);
		let invoice = match lightning_invoice::utils::create_invoice_from_channelmanager(
			&self.channel_manager,
			keys_manager,
			Arc::clone(&self.logger),
			currency,
			amount_msat,
			description.to_string(),
			expiry_secs,
			None,
		) {
			Ok(inv) => {
				log_info!(self.logger, "Invoice created: {}", inv);
				inv
			}
			Err(e) => {
				log_error!(self.logger, "Failed to create invoice: {}", e);
				return Err(Error::InvoiceCreationFailed);
			}
		};

		let payment_hash = PaymentHash((*invoice.payment_hash()).into_inner());
		inbound_payments_lock.insert(
			payment_hash,
			PaymentInfo {
				preimage: None,
				secret: Some(*invoice.payment_secret()),
				status: PaymentStatus::Pending,
				amount_msat,
			},
		);
		Ok(invoice)
	}

	/// Query for information about the status of a specific payment.
	pub fn payment_info(&self, payment_hash: &[u8; 32]) -> Option<PaymentInfo> {
		let payment_hash = PaymentHash(*payment_hash);

		{
			let outbound_payments_lock = self.outbound_payments.lock().unwrap();
			if let Some(payment_info) = outbound_payments_lock.get(&payment_hash) {
				return Some((*payment_info).clone());
			}
		}

		{
			let inbound_payments_lock = self.inbound_payments.lock().unwrap();
			if let Some(payment_info) = inbound_payments_lock.get(&payment_hash) {
				return Some((*payment_info).clone());
			}
		}

		None
	}
}

async fn connect_peer_if_necessary(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
	logger: Arc<FilesystemLogger>,
) -> Result<(), Error> {
	for (node_pubkey, _addr) in peer_manager.get_peer_node_ids() {
		if node_pubkey == pubkey {
			return Ok(());
		}
	}

	do_connect_peer(pubkey, peer_addr, peer_manager, logger).await
}

async fn do_connect_peer(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
	logger: Arc<FilesystemLogger>,
) -> Result<(), Error> {
	log_info!(logger, "connecting to peer: {}@{}", pubkey, peer_addr);
	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), pubkey, peer_addr).await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				match futures::poll!(&mut connection_closed_future) {
					std::task::Poll::Ready(_) => {
						log_info!(logger, "peer connection closed: {}@{}", pubkey, peer_addr);
						return Err(Error::ConnectionFailed);
					}
					std::task::Poll::Pending => {}
				}
				// Avoid blocking the tokio context by sleeping a bit
				match peer_manager.get_peer_node_ids().iter().find(|(id, _addr)| *id == pubkey) {
					Some(_) => return Ok(()),
					None => tokio::time::sleep(Duration::from_millis(10)).await,
				}
			}
		}
		None => {
			log_error!(logger, "failed to connect to peer: {}@{}", pubkey, peer_addr);
			Err(Error::ConnectionFailed)
		}
	}
}
