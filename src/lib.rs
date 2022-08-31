// This file is Copyright its original authors, visible in version contror
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#![crate_name = "ldk_lite"]

//! A library providing a simplified API for the Lightning Dev Kit. While LDK itself provides a
//! highly configurable and adaptable interface, this API champions simplicity and ease of use over
//! configurability. To this end, it provides an opionated set of design choices and ready-to-go
//! default modules, while still enabling some configurability when dearly needed by the user:
//! - Chain data is accessed through an Esplora client.
//! - Wallet and channel states are persisted to disk.
//! - Gossip is retrieved over the P2P network.

#![deny(missing_docs)]
#![deny(broken_intra_doc_links)]
#![deny(private_intra_doc_links)]
#![allow(bare_trait_objects)]
#![allow(ellipsis_inclusive_range_patterns)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod access;
mod error;
mod event;
mod hex;
mod io;
mod logger;

use access::LdkLiteChainAccess;
pub use error::LdkLiteError as Error;
pub use event::LdkLiteEvent;
use event::LdkLiteEventHandler;

#[allow(unused_imports)]
use logger::{
	log_error, log_given_level, log_info, log_internal, log_trace, log_warn, FilesystemLogger,
	Logger,
};

use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager, Recipient};
use lightning::chain::{chainmonitor, Access, BestBlock, Confirm, Filter, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{
	ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::gossip;
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::scoring::ProbabilisticScorer;

use lightning::util::config::UserConfig;
use lightning::util::ser::ReadableArgs;

use lightning_background_processor::BackgroundProcessor;
use lightning_background_processor::GossipSync as BPGossipSync;
use lightning_persister::FilesystemPersister;

use lightning_net_tokio::SocketDescriptor;

use lightning_invoice::utils::DefaultRouter;
use lightning_invoice::{payment, Currency, Invoice};

use bdk::blockchain::esplora::EsploraBlockchain;
use bdk::blockchain::{GetBlockHash, GetHeight};
use bdk::database::MemoryDatabase;

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::BlockHash;

use rand::Rng;

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

// TODO: Is MemoryDatabase okay to use?

// The number of messages we buffer in the used channels.
const CHANNEL_BUF_SIZE: usize = 1000;

// The used 'stop gap' parameter used by BDK's wallet sync. This seems to configure the threshold
// number of blocks after which BDK stops looking for scripts belonging to the wallet.
const BDK_CLIENT_STOP_GAP: usize = 20;

// The number of concurrent requests made against the API provider.
const BDK_CLIENT_CONCURRENCY: u8 = 8;

// The timeout after which we abandon retrying failed payments.
const LDK_PAYMENT_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
/// Represents the configuration of an `LdkLite` instance.
pub struct LdkLiteConfig {
	/// The path where the underlying LDK and BDK persist their data.
	pub storage_dir_path: String,
	/// The URL of the utilized Esplora server.
	pub esplora_server_url: String,
	/// The used Bitcoin network.
	pub network: bitcoin::Network,
	/// The TCP port the network node will listen on.
	pub listening_port: u16,
	/// The default CLTV expiry delta to be used for payments.
	pub default_cltv_expiry_delta: u32,
}

/// A builder for an [`LdkLite`] instance, allowing to set some configuration and module choices from
/// the getgo.
#[derive(Debug, Clone)]
pub struct LdkLiteBuilder {
	config: LdkLiteConfig,
}

impl LdkLiteBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		// Set the config defaults
		let storage_dir_path = "/tmp/ldk_lite/".to_string();
		let esplora_server_url = "https://blockstream.info/api".to_string();
		let network = bitcoin::Network::Testnet;
		let listening_port = 9735;
		let default_cltv_expiry_delta = 144;

		let config = LdkLiteConfig {
			storage_dir_path,
			esplora_server_url,
			network,
			listening_port,
			default_cltv_expiry_delta,
		};

		Self { config }
	}

	/// Creates a new builder instance from an [`LdkLiteConfig`].
	pub fn from_config(config: LdkLiteConfig) -> Self {
		Self { config }
	}

	/// Sets the used storage directory path.
	///
	/// Default: `/tmp/ldk_lite/`
	pub fn storage_dir_path(&mut self, storage_dir_path: String) -> &mut Self {
		self.config.storage_dir_path = storage_dir_path;
		self
	}

	/// Sets the Esplora server URL.
	///
	/// Default: `https://blockstream.info/api`
	pub fn esplora_server_url(&mut self, esplora_server_url: String) -> &mut Self {
		self.config.esplora_server_url = esplora_server_url;
		self
	}

	/// Sets the Bitcoin network used.
	///
	/// Options: `mainnet`/`bitcoin`, `testnet`, `regtest`, `signet`
	///
	/// Default: `testnet`
	pub fn network(&mut self, network: &str) -> &mut Self {
		self.config.network = match network {
			"mainnet" => bitcoin::Network::Bitcoin,
			"bitcoin" => bitcoin::Network::Bitcoin,
			"testnet" => bitcoin::Network::Testnet,
			"regtest" => bitcoin::Network::Regtest,
			"signet" => bitcoin::Network::Signet,
			_ => bitcoin::Network::Testnet,
		};
		self
	}

	/// Sets the port on which [`LdkLite`] will listen for incoming network connections.
	///
	/// Default: `9735`
	pub fn listening_port(&mut self, listening_port: u16) -> &mut Self {
		self.config.listening_port = listening_port;
		self
	}

	/// Builds an [`LdkLite`] instance according to the options previously configured.
	pub fn build(self) -> Result<LdkLite, Error> {
		let config = Arc::new(self.config);

		let ldk_data_dir = format!("{}/ldk", &config.storage_dir_path.clone());
		fs::create_dir_all(ldk_data_dir.clone())?;

		let bdk_data_dir = format!("{}/bdk", config.storage_dir_path.clone());
		fs::create_dir_all(bdk_data_dir.clone())?;

		// Step 0: Initialize the Logger
		let log_file_path = format!("{}/ldk_lite.log", config.storage_dir_path.clone());
		let logger = Arc::new(FilesystemLogger::new(log_file_path));

		// Step 1: Initialize the on-chain wallet and chain access
		let seed = io::read_or_generate_seed_file(Arc::clone(&config))?;
		let xprv = bitcoin::util::bip32::ExtendedPrivKey::new_master(config.network, &seed)?;

		let bdk_wallet = bdk::Wallet::new(
			bdk::template::Bip84(xprv.clone(), bdk::KeychainKind::External),
			Some(bdk::template::Bip84(xprv.clone(), bdk::KeychainKind::Internal)),
			config.network,
			MemoryDatabase::default(),
		)?;

		// TODO: Check that we can be sure that the Esplora client re-connects in case of failure
		// and and exits cleanly on drop. Otherwise we need to handle this/move it to the runtime?
		let blockchain = EsploraBlockchain::new(&config.esplora_server_url, BDK_CLIENT_STOP_GAP)
			.with_concurrency(BDK_CLIENT_CONCURRENCY);

		let chain_access = Arc::new(LdkLiteChainAccess::new(blockchain, bdk_wallet));

		// Step 3: Initialize Persist
		let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

		// Step 4: Initialize the ChainMonitor
		let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
			None,
			Arc::clone(&chain_access),
			Arc::clone(&logger),
			Arc::clone(&chain_access),
			Arc::clone(&persister),
		));

		// Step 5: Initialize the KeysManager
		let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
		let keys_manager = Arc::new(KeysManager::new(&seed, cur.as_secs(), cur.subsec_nanos()));

		// Step 6: Read ChannelMonitor state from disk
		let mut channel_monitors = persister.read_channelmonitors(keys_manager.clone())?;

		// Step 7: Initialize the ChannelManager
		let mut user_config = UserConfig::default();
		user_config.channel_handshake_limits.force_announced_channel_preference = false;
		let (_channel_manager_blockhash, channel_manager) = {
			if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
				let mut channel_monitor_mut_references = Vec::new();
				for (_, channel_monitor) in channel_monitors.iter_mut() {
					channel_monitor_mut_references.push(channel_monitor);
				}
				let read_args = ChannelManagerReadArgs::new(
					Arc::clone(&keys_manager),
					Arc::clone(&chain_access),
					Arc::clone(&chain_monitor),
					Arc::clone(&chain_access),
					Arc::clone(&logger),
					user_config,
					channel_monitor_mut_references,
				);
				<(BlockHash, ChannelManager)>::read(&mut f, read_args)?
			} else {
				// We're starting a fresh node.
				let latest_block_height = chain_access.get_height()?;
				let latest_block_hash = chain_access.get_block_hash(latest_block_height as u64)?;

				let chain_params = ChainParameters {
					network: config.network,
					best_block: BestBlock::new(latest_block_hash, latest_block_height),
				};
				let fresh_channel_manager = channelmanager::ChannelManager::new(
					Arc::clone(&chain_access),
					Arc::clone(&chain_monitor),
					Arc::clone(&chain_access),
					Arc::clone(&logger),
					Arc::clone(&keys_manager),
					user_config,
					chain_params,
				);
				(latest_block_hash, fresh_channel_manager)
			}
		};

		let channel_manager = Arc::new(channel_manager);

		// Step 8: Give ChannelMonitors to ChainMonitor
		for (_blockhash, channel_monitor) in channel_monitors.drain(..) {
			let funding_outpoint = channel_monitor.get_funding_txo().0;
			chain_monitor.watch_channel(funding_outpoint, channel_monitor).unwrap();
		}

		// TODO: Use RGS on first start, if configured
		// Step 10: Initialize the P2PGossipSync
		let network_graph =
			Arc::new(io::read_network_graph(Arc::clone(&config), Arc::clone(&logger))?);
		let gossip_sync = Arc::new(P2PGossipSync::new(
			Arc::clone(&network_graph),
			None::<Arc<dyn Access + Send + Sync>>,
			Arc::clone(&logger),
		));

		//// Step 11: Initialize the PeerManager
		let ephemeral_bytes: [u8; 32] = rand::thread_rng().gen();
		let lightning_msg_handler = MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::clone(&gossip_sync),
		};

		let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
			lightning_msg_handler,
			keys_manager.get_node_secret(Recipient::Node).unwrap(),
			&ephemeral_bytes,
			Arc::clone(&logger),
			Arc::new(IgnoringMessageHandler {}),
		));

		// Step 12: Initialize routing ProbabilisticScorer
		let scorer = Arc::new(Mutex::new(io::read_scorer(
			Arc::clone(&config),
			Arc::clone(&network_graph),
			Arc::clone(&logger),
		)));

		// Step 13: Init payment info storage
		// TODO: persist payment info to disk
		let inbound_payments = Arc::new(Mutex::new(HashMap::new()));
		let outbound_payments = Arc::new(Mutex::new(HashMap::new()));

		// Step 14: Handle LDK Events
		let event_queue = mpsc::sync_channel(CHANNEL_BUF_SIZE);
		let event_sender = event_queue.0.clone();
		let event_handler = LdkLiteEventHandler::new(
			Arc::clone(&chain_access),
			Arc::clone(&channel_manager),
			Arc::clone(&network_graph),
			Arc::clone(&keys_manager),
			Arc::clone(&inbound_payments),
			Arc::clone(&outbound_payments),
			event_sender,
			Arc::clone(&logger),
			Arc::clone(&config),
		);

		//// Step 16: Create Router and InvoicePayer
		let router = DefaultRouter::new(
			Arc::clone(&network_graph),
			Arc::clone(&logger),
			keys_manager.get_secure_random_bytes(),
		);

		let invoice_payer = Arc::new(InvoicePayer::new(
			Arc::clone(&channel_manager),
			router,
			Arc::clone(&scorer),
			Arc::clone(&logger),
			event_handler,
			payment::Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
		));

		let running = RwLock::new(None);

		Ok(LdkLite {
			running,
			config,
			chain_access,
			channel_manager,
			chain_monitor,
			peer_manager,
			keys_manager,
			gossip_sync,
			persister,
			logger,
			network_graph,
			scorer,
			invoice_payer,
			inbound_payments,
			outbound_payments,
			event_queue,
		})
	}
}

/// Wraps all objects that need to be preserved during the run time of `LdkLite`. Will be dropped
/// upon [`LdkLite::stop()`].
struct LdkLiteRuntime {
	_tokio_runtime: tokio::runtime::Runtime,
	_background_processor: BackgroundProcessor,
	stop_networking: Arc<AtomicBool>,
	stop_wallet_sync: Arc<AtomicBool>,
}

/// The main interface object of the simplified API, wrapping the necessary LDK and BDK functionalities.
///
/// Needs to be initialized and instantiated through [`LdkLiteBuilder::build`].
pub struct LdkLite {
	running: RwLock<Option<LdkLiteRuntime>>,
	config: Arc<LdkLiteConfig>,
	chain_access: Arc<LdkLiteChainAccess<MemoryDatabase>>,
	channel_manager: Arc<ChannelManager>,
	chain_monitor: Arc<ChainMonitor>,
	peer_manager: Arc<PeerManager>,
	keys_manager: Arc<KeysManager>,
	gossip_sync: Arc<GossipSync>,
	persister: Arc<FilesystemPersister>,
	logger: Arc<FilesystemLogger>,
	scorer: Arc<Mutex<Scorer>>,
	network_graph: Arc<NetworkGraph>,
	invoice_payer: Arc<InvoicePayer<LdkLiteEventHandler>>,
	inbound_payments: Arc<PaymentInfoStorage>,
	outbound_payments: Arc<PaymentInfoStorage>,
	event_queue: (EventSender, EventReceiver),
}

impl LdkLite {
	/// Starts the necessary background tasks, such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network. After this returns, the [`LdkLite`] instance can be
	/// controlled via the provided API methods in a thread-safe manner.
	pub fn start(&mut self) -> Result<(), Error> {
		// Acquire a run lock and hold it until we're setup.
		// TODO: maybe this could be done cleaner with a Condvar?
		let mut run_lock = self.running.write().unwrap();
		if run_lock.is_some() {
			// We're already running.
			return Err(Error::AlreadyRunning);
		}

		let runtime = self.setup_runtime()?;
		*run_lock = Some(runtime);
		Ok(())
	}

	/// Disconnects all peers, stops all running background tasks, and shuts down [`LdkLite`].
	pub fn stop(&mut self) -> Result<(), Error> {
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

		// Drop the runtime, which stops the background processor and any possibly remaining tokio threads.
		*run_lock = None;
		Ok(())
	}

	fn setup_runtime(&self) -> Result<LdkLiteRuntime, Error> {
		let _tokio_runtime =
			tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();

		// Setup wallet sync
		let chain_access = Arc::clone(&self.chain_access);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let stop_wallet_sync = Arc::new(AtomicBool::new(false));
		let stop_sync = Arc::clone(&stop_wallet_sync);

		_tokio_runtime.spawn(async move {
			let mut rounds = 0;
			loop {
				if stop_sync.load(Ordering::Acquire) {
					return;
				}
				// As syncing the on-chain wallet is much more time-intesive, we only sync every
				// fifth round.
				// TODO: make this superfluous by integrating LDK's sync with the BDK wallet sync
				// properly.
				if rounds == 0 {
					chain_access.sync_wallet().unwrap();
				}
				rounds = (rounds + 1) % 5;

				let confirmables = vec![&*sync_cman as &dyn Confirm, &*sync_cmon as &dyn Confirm];
				chain_access.sync(confirmables).unwrap();
				thread::sleep(Duration::from_secs(5));
			}
		});

		// Setup networking
		let peer_manager_connection_handler = Arc::clone(&self.peer_manager);
		let listening_port = self.config.listening_port;
		let stop_networking = Arc::new(AtomicBool::new(false));
		let stop_listen = Arc::clone(&stop_networking);

		_tokio_runtime.spawn(async move {
			let listener =
				tokio::net::TcpListener::bind(format!("0.0.0.0:{}", listening_port)).await.expect(
					"Failed to bind to listen port - is something else already listening on it?",
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

		// Regularly reconnect to channel peers.
		let connect_cm = Arc::clone(&self.channel_manager);
		let connect_pm = Arc::clone(&self.peer_manager);
		let connect_config = Arc::clone(&self.config);
		let connect_logger = Arc::clone(&self.logger);
		let stop_connect = Arc::clone(&stop_networking);
		_tokio_runtime.spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_secs(1));
			loop {
				if stop_connect.load(Ordering::Acquire) {
					return;
				}
				interval.tick().await;
				match io::read_channel_peer_data(Arc::clone(&connect_config)) {
					Ok(info) => {
						let peers = connect_pm.get_peer_node_ids();
						for node_id in connect_cm
							.list_channels()
							.iter()
							.map(|chan| chan.counterparty.node_id)
							.filter(|id| !peers.contains(id))
						{
							for (pubkey, peer_addr) in info.iter() {
								if *pubkey == node_id {
									let _ = do_connect_peer(
										*pubkey,
										peer_addr.clone(),
										Arc::clone(&connect_pm),
										Arc::clone(&connect_logger),
									)
									.await;
								}
							}
						}
					}
					Err(e) => {
						log_error!(
							connect_logger,
							"failure reading channel peer info from disk: {:?}",
							e
						)
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

		Ok(LdkLiteRuntime {
			_tokio_runtime,
			_background_processor,
			stop_networking,
			stop_wallet_sync,
		})
	}

	/// Returns the next event from the event queue. Blocks until a new event is available.
	pub fn next_event(&self) -> Result<LdkLiteEvent, Error> {
		Ok(self.event_queue.1.recv()?)
	}

	/// Returns our own node id
	pub fn my_node_id(&self) -> Result<PublicKey, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		Ok(self.channel_manager.get_our_node_id())
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_funding_address(&mut self) -> Result<bitcoin::Address, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		let funding_address = self.chain_access.get_new_address()?;
		log_info!(self.logger, "generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	// Connect to a node and open a new channel. Disconnects and re-connects should be handled automatically
	//pub fn connect_open_channel(&mut self, node_id: PublicKey, node_address: NetAddress) -> Result<u64> {
	//}
	//	// Close a previously opened channel
	//	pub close_channel(&mut self, channel_id: u64) -> Result<()>;
	//
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
				log_info!(self.logger, "initiated sending {} msats to {}", amt_msat, payee_pubkey);
				PaymentStatus::Pending
			}
			Err(payment::PaymentError::Invoice(e)) => {
				log_error!(self.logger, "invalid invoice: {}", e);
				return Err(Error::LdkPayment(payment::PaymentError::Invoice(e)));
			}
			Err(payment::PaymentError::Routing(e)) => {
				log_error!(self.logger, "failed to find route: {}", e.err);
				return Err(Error::LdkPayment(payment::PaymentError::Routing(e)));
			}
			Err(payment::PaymentError::Sending(e)) => {
				log_error!(self.logger, "failed to send payment: {:?}", e);
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
		&self, amount_msat: u64, node_id: PublicKey,
	) -> Result<PaymentHash, Error> {
		if self.running.read().unwrap().is_none() {
			return Err(Error::NotRunning);
		}

		let payment_preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
		let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());

		let status = match self.invoice_payer.pay_pubkey(
			node_id,
			payment_preimage,
			amount_msat,
			self.config.default_cltv_expiry_delta,
		) {
			Ok(_payment_id) => {
				log_info!(self.logger, "initiated sending {} msats to {}", amount_msat, node_id);
				PaymentStatus::Pending
			}
			Err(payment::PaymentError::Invoice(e)) => {
				log_error!(self.logger, "invalid invoice: {}", e);
				return Err(Error::LdkPayment(payment::PaymentError::Invoice(e)));
			}
			Err(payment::PaymentError::Routing(e)) => {
				log_error!(self.logger, "failed to find route: {}", e.err);
				return Err(Error::LdkPayment(payment::PaymentError::Routing(e)));
			}
			Err(payment::PaymentError::Sending(e)) => {
				log_error!(self.logger, "failed to send payment: {:?}", e);
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

	// TODO: Should we provide a configurable default for the expiry, or force the user to supply it on every call?
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
			currency,
			amount_msat,
			description.to_string(),
			expiry_secs,
		) {
			Ok(inv) => {
				log_info!(self.logger, "generated invoice: {}", inv);
				inv
			}
			Err(e) => {
				let err_str = &e.to_string();
				log_error!(self.logger, "failed to create invoice: {:?}", err_str);
				return Err(Error::LdkInvoiceCreation(e));
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

	//
	//	// Query for information about our channels
	//	pub channel_info(&self) -> ChannelInfo;
	//
	//	// Query for information about our on-chain/funding status.
	//	pub funding_info(&self) -> FundingInfo;
	//}
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
						return Err(Error::ConnectionClosed);
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
			Err(Error::ConnectionClosed)
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
	Arc<dyn Filter + Send + Sync>,
	Arc<LdkLiteChainAccess<MemoryDatabase>>,
	Arc<LdkLiteChainAccess<MemoryDatabase>>,
	Arc<FilesystemLogger>,
	Arc<FilesystemPersister>,
>;

type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	LdkLiteChainAccess<MemoryDatabase>,
	LdkLiteChainAccess<MemoryDatabase>,
	dyn Access + Send + Sync,
	FilesystemLogger,
>;

pub(crate) type ChannelManager = SimpleArcChannelManager<
	ChainMonitor,
	LdkLiteChainAccess<MemoryDatabase>,
	LdkLiteChainAccess<MemoryDatabase>,
	FilesystemLogger,
>;

type InvoicePayer<F> = payment::InvoicePayer<
	Arc<ChannelManager>,
	Router,
	Arc<Mutex<Scorer>>,
	Arc<FilesystemLogger>,
	F,
>;

type Router = DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>>;
type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>;

type GossipSync =
	P2PGossipSync<Arc<NetworkGraph>, Arc<dyn Access + Send + Sync>, Arc<FilesystemLogger>>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

pub(crate) type PaymentInfoStorage = Mutex<HashMap<PaymentHash, PaymentInfo>>;

pub(crate) type EventSender = mpsc::SyncSender<LdkLiteEvent>;
pub(crate) type EventReceiver = mpsc::Receiver<LdkLiteEvent>;

#[cfg(test)]
mod tests {}
