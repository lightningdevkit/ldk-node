// This file is Copyright its original authors, visible in version contror
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#![crate_name = "ldk_node"]

//! A library providing a simplified API for the Lightning Dev Kit. While LDK itself provides a
//! highly configurable and adaptable interface, this API champions simplicity and ease of use over
//! configurability. To this end, it provides an opionated set of design choices and ready-to-go
//! default modules, while still enabling some configurability when dearly needed by the user:
//! - Chain data is accessed through an Esplora client.
//! - Wallet and channel states are persisted to disk.
//! - Gossip is retrieved over the P2P network.

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
mod wallet;

pub use error::Error as NodeError;
use error::Error;
pub use event::Event;
use event::{EventHandler, EventQueue};
use peer_store::{PeerInfo, PeerInfoStorage};
use wallet::{Wallet, WalletKeysManager};

use logger::{log_error, log_given_level, log_info, log_internal, FilesystemLogger, Logger};

use lightning::chain::keysinterface::{EntropySource, InMemorySigner, NodeSigner, Recipient};
use lightning::chain::{chainmonitor, Access, BestBlock, Confirm, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{ChainParameters, ChannelManagerReadArgs};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::gossip;
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::scoring::ProbabilisticScorer;

use lightning::util::config::{ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig};
use lightning::util::ser::ReadableArgs;

use lightning_background_processor::BackgroundProcessor;
use lightning_background_processor::GossipSync as BPGossipSync;
use lightning_persister::FilesystemPersister;

use lightning_transaction_sync::EsploraSyncClient;

use lightning_net_tokio::SocketDescriptor;

use lightning::routing::router::DefaultRouter;
use lightning_invoice::{payment, Currency, Invoice, SignedRawInvoice};

use bdk::bitcoin::secp256k1::Secp256k1;
use bdk::blockchain::esplora::EsploraBlockchain;
use bdk::sled;
use bdk::template::Bip84;

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Address, BlockHash};

use rand::Rng;

use core::str::FromStr;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::default::Default;
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

uniffi_macros::include_scaffolding!("ldk_node");

// The used 'stop gap' parameter used by BDK's wallet sync. This seems to configure the threshold
// number of blocks after which BDK stops looking for scripts belonging to the wallet.
const BDK_CLIENT_STOP_GAP: usize = 20;

// The number of concurrent requests made against the API provider.
const BDK_CLIENT_CONCURRENCY: u8 = 8;

// The timeout after which we abandon retrying failed payments.
const LDK_PAYMENT_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

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
		// Set the config defaults
		let storage_dir_path = "/tmp/ldk_node/".to_string();
		let esplora_server_url = "https://blockstream.info/api".to_string();
		let network = bitcoin::Network::Regtest;
		let listening_address = Some("0.0.0.0:9735".to_string());
		let default_cltv_expiry_delta = 144;

		Self {
			storage_dir_path,
			esplora_server_url,
			network,
			listening_address,
			default_cltv_expiry_delta,
		}
	}
}

/// A builder for an [`Node`] instance, allowing to set some configuration and module choices from
/// the getgo.
#[derive(Debug, Clone)]
pub struct Builder {
	config: Config,
}

impl Builder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let config = Config::default();

		Self { config }
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		Self { config }
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

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build(&self) -> Arc<Node> {
		let config = Arc::new(self.config.clone());

		let ldk_data_dir = format!("{}/ldk", &config.storage_dir_path.clone());
		fs::create_dir_all(ldk_data_dir.clone()).expect("Failed to create LDK data directory");

		let bdk_data_dir = format!("{}/bdk", config.storage_dir_path.clone());
		fs::create_dir_all(bdk_data_dir.clone()).expect("Failed to create BDK data directory");

		// Step 0: Initialize the Logger
		let log_file_path = format!("{}/ldk_node.log", config.storage_dir_path.clone());
		let logger = Arc::new(FilesystemLogger::new(log_file_path));

		// Step 1: Initialize the on-chain wallet and chain access
		let seed = io_utils::read_or_generate_seed_file(Arc::clone(&config));
		let xprv = bitcoin::util::bip32::ExtendedPrivKey::new_master(config.network, &seed)
			.expect("Failed to read wallet master key");

		let wallet_name = bdk::wallet::wallet_name_from_descriptor(
			Bip84(xprv.clone(), bdk::KeychainKind::External),
			Some(Bip84(xprv.clone(), bdk::KeychainKind::Internal)),
			config.network,
			&Secp256k1::new(),
		)
		.expect("Failed to derive on-chain wallet name");
		let database = sled::open(bdk_data_dir).expect("Failed to open BDK database");
		let database =
			database.open_tree(wallet_name.clone()).expect("Failed to open BDK database");

		let bdk_wallet = bdk::Wallet::new(
			Bip84(xprv.clone(), bdk::KeychainKind::External),
			Some(Bip84(xprv.clone(), bdk::KeychainKind::Internal)),
			config.network,
			database,
		)
		.expect("Failed to setup on-chain wallet");

		// TODO: Check that we can be sure that the Esplora client re-connects in case of failure
		// and and exits cleanly on drop. Otherwise we need to handle this/move it to the runtime?
		let blockchain = EsploraBlockchain::new(&config.esplora_server_url, BDK_CLIENT_STOP_GAP)
			.with_concurrency(BDK_CLIENT_CONCURRENCY);

		let wallet = Arc::new(Wallet::new(blockchain, bdk_wallet, Arc::clone(&logger)));

		let tx_sync = Arc::new(EsploraSyncClient::new(
			config.esplora_server_url.clone(),
			Arc::clone(&logger),
		));

		// Step 3: Initialize Persist
		let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

		// Step 4: Initialize the ChainMonitor
		let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
			Some(Arc::clone(&tx_sync)),
			Arc::clone(&wallet),
			Arc::clone(&logger),
			Arc::clone(&wallet),
			Arc::clone(&persister),
		));

		// Step 5: Initialize the KeysManager
		let cur_time = SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.expect("System time error: Clock may have gone backwards");
		let keys_manager = Arc::new(KeysManager::new(
			&seed,
			cur_time.as_secs(),
			cur_time.subsec_nanos(),
			Arc::clone(&wallet),
		));

		// Step 6: Read ChannelMonitor state from disk
		let mut channel_monitors = persister
			.read_channelmonitors(keys_manager.clone())
			.expect("Failed to read channel monitors from disk");

		// Step 7: Initialize the ChannelManager
		let mut user_config = UserConfig::default();
		user_config.channel_handshake_limits.force_announced_channel_preference = false;
		let channel_manager = {
			if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
				let mut channel_monitor_mut_references = Vec::new();
				for (_, channel_monitor) in channel_monitors.iter_mut() {
					channel_monitor_mut_references.push(channel_monitor);
				}
				let read_args = ChannelManagerReadArgs::new(
					Arc::clone(&keys_manager),
					Arc::clone(&wallet),
					Arc::clone(&chain_monitor),
					Arc::clone(&wallet),
					Arc::clone(&logger),
					user_config,
					channel_monitor_mut_references,
				);
				let (_hash, channel_manager) =
					<(BlockHash, ChannelManager)>::read(&mut f, read_args)
						.expect("Failed to read channel manager from disk");
				channel_manager
			} else {
				// We're starting a fresh node.
				let dummy_block_hash = bitcoin::blockdata::constants::genesis_block(config.network)
					.header
					.block_hash();

				let chain_params = ChainParameters {
					network: config.network,
					best_block: BestBlock::new(dummy_block_hash, 0),
				};
				let fresh_channel_manager = channelmanager::ChannelManager::new(
					Arc::clone(&wallet),
					Arc::clone(&chain_monitor),
					Arc::clone(&wallet),
					Arc::clone(&logger),
					Arc::clone(&keys_manager),
					user_config,
					chain_params,
				);
				fresh_channel_manager
			}
		};

		let channel_manager = Arc::new(channel_manager);

		// Step 8: Give ChannelMonitors to ChainMonitor
		for (_blockhash, channel_monitor) in channel_monitors.drain(..) {
			let funding_outpoint = channel_monitor.get_funding_txo().0;
			chain_monitor.watch_channel(funding_outpoint, channel_monitor);
		}

		// Step 10: Initialize the P2PGossipSync
		let network_graph = Arc::new(
			io_utils::read_network_graph(Arc::clone(&config), Arc::clone(&logger))
				.expect("Failed to read the network graph"),
		);
		let gossip_sync = Arc::new(P2PGossipSync::new(
			Arc::clone(&network_graph),
			None::<Arc<dyn Access + Send + Sync>>,
			Arc::clone(&logger),
		));

		//// Step 11: Initialize the PeerManager
		let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
			Arc::clone(&keys_manager),
			Arc::clone(&logger),
			IgnoringMessageHandler {},
		));
		let ephemeral_bytes: [u8; 32] = rand::thread_rng().gen();
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
			keys_manager.get_node_secret(Recipient::Node).unwrap(),
			cur_time.as_secs().try_into().expect("System time error"),
			&ephemeral_bytes,
			Arc::clone(&logger),
			IgnoringMessageHandler {},
		));

		// Step 12: Initialize routing ProbabilisticScorer
		let scorer = Arc::new(Mutex::new(io_utils::read_scorer(
			Arc::clone(&config),
			Arc::clone(&network_graph),
			Arc::clone(&logger),
		)));

		// Step 13: Init payment info storage
		// TODO: persist payment info to disk
		let inbound_payments = Arc::new(Mutex::new(HashMap::new()));
		let outbound_payments = Arc::new(Mutex::new(HashMap::new()));

		// Step 14: Restore event handler from disk or create a new one.
		let event_queue = if let Ok(mut f) =
			fs::File::open(format!("{}/{}", ldk_data_dir.clone(), event::EVENTS_PERSISTENCE_KEY))
		{
			Arc::new(
				EventQueue::read(&mut f, Arc::clone(&persister))
					.expect("Failed to read event queue from disk."),
			)
		} else {
			Arc::new(EventQueue::new(Arc::clone(&persister)))
		};

		let event_handler = Arc::new(EventHandler::new(
			Arc::clone(&wallet),
			Arc::clone(&event_queue),
			Arc::clone(&channel_manager),
			Arc::clone(&network_graph),
			Arc::clone(&keys_manager),
			Arc::clone(&inbound_payments),
			Arc::clone(&outbound_payments),
			Arc::clone(&logger),
			Arc::clone(&config),
		));

		//// Step 16: Create Router and InvoicePayer
		let router = DefaultRouter::new(
			Arc::clone(&network_graph),
			Arc::clone(&logger),
			keys_manager.get_secure_random_bytes(),
			Arc::clone(&scorer),
		);

		let invoice_payer = Arc::new(InvoicePayer::new(
			Arc::clone(&channel_manager),
			router,
			Arc::clone(&logger),
			Arc::clone(&event_handler),
			payment::Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
		));

		let peer_store = if let Ok(mut f) = fs::File::open(format!(
			"{}/{}",
			ldk_data_dir.clone(),
			peer_store::PEER_INFO_PERSISTENCE_KEY
		)) {
			Arc::new(
				PeerInfoStorage::read(&mut f, Arc::clone(&persister))
					.expect("Failed to read peer information from disk."),
			)
		} else {
			Arc::new(PeerInfoStorage::new(Arc::clone(&persister)))
		};

		let running = RwLock::new(None);

		Arc::new(Node {
			running,
			config,
			wallet,
			tx_sync,
			event_queue,
			event_handler,
			channel_manager,
			chain_monitor,
			peer_manager,
			keys_manager,
			gossip_sync,
			persister,
			logger,
			scorer,
			invoice_payer,
			inbound_payments,
			outbound_payments,
			peer_store,
		})
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

/// The main interface object of the simplified API, wrapping the necessary LDK and BDK functionalities.
///
/// Needs to be initialized and instantiated through [`Builder::build`].
pub struct Node {
	running: RwLock<Option<Runtime>>,
	config: Arc<Config>,
	wallet: Arc<Wallet<bdk::sled::Tree>>,
	tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	event_queue: Arc<EventQueue<Arc<FilesystemPersister>>>,
	event_handler: Arc<EventHandler<Arc<FilesystemPersister>, Arc<FilesystemLogger>>>,
	channel_manager: Arc<ChannelManager>,
	chain_monitor: Arc<ChainMonitor>,
	peer_manager: Arc<PeerManager>,
	keys_manager: Arc<KeysManager>,
	gossip_sync: Arc<GossipSync>,
	persister: Arc<FilesystemPersister>,
	logger: Arc<FilesystemLogger>,
	scorer: Arc<Mutex<Scorer>>,
	invoice_payer:
		Arc<InvoicePayer<Arc<EventHandler<Arc<FilesystemPersister>, Arc<FilesystemLogger>>>>>,
	inbound_payments: Arc<PaymentInfoStorage>,
	outbound_payments: Arc<PaymentInfoStorage>,
	peer_store: Arc<PeerInfoStorage<FilesystemPersister>>,
}

impl Node {
	/// Starts the necessary background tasks, such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network. After this returns, the [`Node`] instance can be
	/// controlled via the provided API methods in a thread-safe manner.
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
		self.event_handler.drop_runtime();

		// Drop the runtime, which stops the background processor and any possibly remaining tokio threads.
		*run_lock = None;
		Ok(())
	}

	fn setup_runtime(&self) -> Result<Runtime, Error> {
		let tokio_runtime =
			Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap());

		self.wallet.set_runtime(Arc::clone(&tokio_runtime));
		self.event_handler.set_runtime(Arc::clone(&tokio_runtime));

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
								"On-chain wallet sync finished in {}ms.",
								now.elapsed().as_millis()
							),
							Err(err) => {
								log_error!(sync_logger, "On-chain wallet sync failed: {}", err)
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
						"Lightning wallet sync finished in {}ms.",
						now.elapsed().as_millis()
					),
					Err(e) => {
						log_error!(sync_logger, "Lightning wallet sync failed: {}", e)
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
			let mut interval = tokio::time::interval(Duration::from_secs(1));
			loop {
				if stop_connect.load(Ordering::Acquire) {
					return;
				}
				interval.tick().await;
				let pm_peers = connect_pm.get_peer_node_ids();
				for node_id in connect_cm
					.list_channels()
					.iter()
					.map(|chan| chan.counterparty.node_id)
					.filter(|id| !pm_peers.contains(id))
				{
					for peer_info in connect_peer_store.peers() {
						if peer_info.pubkey == node_id {
							let _ = do_connect_peer(
								peer_info.pubkey,
								peer_info.address.clone(),
								Arc::clone(&connect_pm),
								Arc::clone(&connect_logger),
							)
							.await;
						}
					}
				}
			}
		});

		// Setup background processing
		let _background_processor = BackgroundProcessor::start(
			Arc::clone(&self.persister),
			Arc::clone(&self.invoice_payer),
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
	/// Note: this will always return the same event until handling is confirmed via [`Node::event_handled`].
	pub fn next_event(&self) -> Event {
		self.event_queue.next_event()
	}

	/// Confirm the last retrieved event handled.
	pub fn event_handled(&self) {
		self.event_queue.event_handled().unwrap();
	}

	/// Returns our own node id
	pub fn node_id(&self) -> Result<PublicKey, Error> {
		Ok(self.channel_manager.get_our_node_id())
	}

	/// Returns our own listening address and port.
	pub fn listening_address(&self) -> Option<String> {
		self.config.listening_address.clone()
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_funding_address(&self) -> Result<Address, Error> {
		let funding_address = self.wallet.get_new_address()?;
		log_info!(self.logger, "Generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	#[cfg(test)]
	/// Retrieve the current on-chain balance.
	pub fn on_chain_balance(&self) -> Result<bdk::Balance, Error> {
		self.wallet.get_balance()
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

		let peer_info = PeerInfo::try_from(node_pubkey_and_address.to_string())?;

		let runtime = runtime_lock.as_ref().unwrap();

		let con_peer_info = peer_info.clone();
		let con_success = Arc::new(AtomicBool::new(false));
		let con_success_cloned = Arc::clone(&con_success);
		let con_logger = Arc::clone(&self.logger);
		let con_pm = Arc::clone(&self.peer_manager);

		tokio::task::block_in_place(move || {
			runtime.tokio_runtime.block_on(async move {
				let res = connect_peer_if_necessary(
					con_peer_info.pubkey,
					con_peer_info.address,
					con_pm,
					con_logger,
				)
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

	#[cfg(test)]
	pub fn sync_wallets(&self) -> Result<(), Error> {
		let runtime_lock = self.running.read().unwrap();
		if runtime_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let wallet = Arc::clone(&self.wallet);
		let tx_sync = Arc::clone(&self.tx_sync);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let confirmables = vec![
			&*sync_cman as &(dyn Confirm + Sync + Send),
			&*sync_cmon as &(dyn Confirm + Sync + Send),
		];

		let runtime = runtime_lock.as_ref().unwrap();
		tokio::task::block_in_place(move || {
			runtime.tokio_runtime.block_on(async move { wallet.sync().await })
		})?;

		tokio::task::block_in_place(move || {
			runtime.tokio_runtime.block_on(async move { tx_sync.sync(confirmables).await })
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

		// TODO: ensure we never tried paying the given payment hash before
		let status = match self.invoice_payer.pay_invoice(&invoice) {
			Ok(_payment_id) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				// TODO: is this unwrap safe? Would a payment to an invoice with None amount ever
				// succeed? Should we allow to set the amount in the interface or via a dedicated
				// method?
				let amt_msat = invoice.amount_milli_satoshis().unwrap();
				log_info!(self.logger, "Initiated sending {} msats to {}", amt_msat, payee_pubkey);
				PaymentStatus::Pending
			}
			Err(payment::PaymentError::Invoice(e)) => {
				log_error!(self.logger, "Failed to send payment due to invalid invoice: {}", e);
				return Err(Error::InvoiceInvalid);
			}
			Err(payment::PaymentError::Routing(e)) => {
				log_error!(self.logger, "Failed to send payment due to routing failure: {}", e.err);
				return Err(Error::RoutingFailed);
			}
			Err(payment::PaymentError::Sending(e)) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);
				PaymentStatus::Failed
			}
		};

		let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
		let payment_secret = Some(invoice.payment_secret().clone());

		let mut outbound_payments_lock = self.outbound_payments.lock().unwrap();
		outbound_payments_lock.insert(
			payment_hash,
			PaymentInfo {
				preimage: None,
				secret: payment_secret,
				status,
				amount_msat: invoice.amount_milli_satoshis(),
			},
		);

		Ok(payment_hash)
	}

	/// Send a spontaneous, aka. "keysend", payment
	pub fn send_spontaneous_payment(
		&self, amount_msat: u64, node_id: &str,
	) -> Result<PaymentHash, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		let pubkey = hex_utils::to_compressed_pubkey(node_id).ok_or(Error::PeerInfoParseFailed)?;

		let payment_preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
		let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());

		let status = match self.invoice_payer.pay_pubkey(
			pubkey,
			payment_preimage,
			amount_msat,
			self.config.default_cltv_expiry_delta,
		) {
			Ok(_payment_id) => {
				log_info!(self.logger, "Initiated sending {} msats to {}.", amount_msat, node_id);
				PaymentStatus::Pending
			}
			Err(payment::PaymentError::Invoice(e)) => {
				log_error!(self.logger, "Failed to send payment due to invalid invoice: {}", e);
				return Err(Error::InvoiceInvalid);
			}
			Err(payment::PaymentError::Routing(e)) => {
				log_error!(self.logger, "Failed to send payment due to routing failure: {}", e.err);
				return Err(Error::RoutingFailed);
			}
			Err(payment::PaymentError::Sending(e)) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);
				PaymentStatus::Failed
			}
		};

		let mut outbound_payments_lock = self.outbound_payments.lock().unwrap();
		outbound_payments_lock.insert(
			payment_hash,
			PaymentInfo { preimage: None, secret: None, status, amount_msat: Some(amount_msat) },
		);

		Ok(payment_hash)
	}

	/// Returns a payable invoice that can be used to request and receive a payment.
	pub fn receive_payment(
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

		let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
		inbound_payments_lock.insert(
			payment_hash,
			PaymentInfo {
				preimage: None,
				secret: Some(invoice.payment_secret().clone()),
				status: PaymentStatus::Pending,
				amount_msat,
			},
		);
		Ok(invoice)
	}

	///	Query for information about the status of a specific payment.
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
	for node_pubkey in peer_manager.get_peer_node_ids() {
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
				match peer_manager.get_peer_node_ids().iter().find(|id| **id == pubkey) {
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

//
// Structs wrapping the particular information which should easily be
// understandable, parseable, and transformable, i.e., we'll try to avoid
// exposing too many technical detail here.
/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentInfo {
	/// The pre-image used by the payment.
	pub preimage: Option<PaymentPreimage>,
	/// The secret used by the payment.
	pub secret: Option<PaymentSecret>,
	/// The status of the payment.
	pub status: PaymentStatus,
	/// The amount transferred.
	pub amount_msat: Option<u64>,
}

/// Represents the current status of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
	/// The payment is still pending.
	Pending,
	/// The payment suceeded.
	Succeeded,
	/// The payment failed.
	Failed,
}

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	Arc<Wallet<bdk::sled::Tree>>,
	Arc<Wallet<bdk::sled::Tree>>,
	Arc<FilesystemLogger>,
	Arc<FilesystemPersister>,
>;

pub(crate) type PeerManager = lightning::ln::peer_handler::PeerManager<
	SocketDescriptor,
	Arc<ChannelManager>,
	Arc<GossipSync>,
	Arc<OnionMessenger>,
	Arc<FilesystemLogger>,
	IgnoringMessageHandler,
>;

pub(crate) type ChannelManager = lightning::ln::channelmanager::ChannelManager<
	Arc<ChainMonitor>,
	Arc<Wallet<bdk::sled::Tree>>,
	Arc<WalletKeysManager<bdk::sled::Tree>>,
	Arc<Wallet<bdk::sled::Tree>>,
	Arc<FilesystemLogger>,
>;

pub(crate) type KeysManager = WalletKeysManager<bdk::sled::Tree>;

type InvoicePayer<F> = payment::InvoicePayer<Arc<ChannelManager>, Router, Arc<FilesystemLogger>, F>;

type Router = DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>, Arc<Mutex<Scorer>>>;
type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

type GossipSync =
	P2PGossipSync<Arc<NetworkGraph>, Arc<dyn Access + Send + Sync>, Arc<FilesystemLogger>>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

pub(crate) type PaymentInfoStorage = Mutex<HashMap<PaymentHash, PaymentInfo>>;

pub(crate) type OnionMessenger = lightning::onion_message::OnionMessenger<
	Arc<WalletKeysManager<bdk::sled::Tree>>,
	Arc<FilesystemLogger>,
	IgnoringMessageHandler,
>;

impl UniffiCustomTypeConverter for PublicKey {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(key) = PublicKey::from_str(&val) {
			return Ok(key);
		}

		Err(Error::PublicKeyInvalid.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Address {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(addr) = Address::from_str(&val) {
			return Ok(addr);
		}

		Err(Error::AddressInvalid.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Invoice {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(signed) = val.parse::<SignedRawInvoice>() {
			if let Ok(invoice) = Invoice::from_signed(signed) {
				return Ok(invoice);
			}
		}

		Err(Error::InvoiceInvalid.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for PaymentHash {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(hash) = Sha256::from_str(&val) {
			Ok(PaymentHash(hash.into_inner()))
		} else {
			Err(Error::PaymentHashInvalid.into())
		}
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		Sha256::from_slice(&obj.0).unwrap().to_string()
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelId([u8; 32]);

impl UniffiCustomTypeConverter for ChannelId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(hex_vec) = hex_utils::to_vec(&val) {
			if hex_vec.len() == 32 {
				let mut channel_id = [0u8; 32];
				channel_id.copy_from_slice(&hex_vec[..]);
				return Ok(Self(channel_id));
			}
		}
		Err(Error::ChannelIdInvalid.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserChannelId(u128);

impl UniffiCustomTypeConverter for UserChannelId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(UserChannelId(u128::from_str(&val).map_err(|_| Error::ChannelIdInvalid)?))
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.0.to_string()
	}
}
