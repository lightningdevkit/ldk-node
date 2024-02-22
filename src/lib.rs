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
//! use ldk_node::lightning_invoice::Bolt11Invoice;
//! use ldk_node::lightning::ln::msgs::SocketAddress;
//! use ldk_node::bitcoin::Network;
//! use ldk_node::bitcoin::secp256k1::PublicKey;
//! use std::str::FromStr;
//!
//! fn main() {
//! 	let mut builder = Builder::new();
//! 	builder.set_network(Network::Testnet);
//! 	builder.set_esplora_server("https://blockstream.info/testnet/api".to_string());
//! 	builder.set_gossip_source_rgs("https://rapidsync.lightningdevkit.org/testnet/snapshot".to_string());
//!
//! 	let node = builder.build().unwrap();
//!
//! 	node.start().unwrap();
//!
//! 	let funding_address = node.new_onchain_address();
//!
//! 	// .. fund address ..
//!
//! 	let node_id = PublicKey::from_str("NODE_ID").unwrap();
//! 	let node_addr = SocketAddress::from_str("IP_ADDR:PORT").unwrap();
//! 	node.connect_open_channel(node_id, node_addr, 10000, None, None, false).unwrap();
//!
//! 	let event = node.wait_next_event();
//! 	println!("EVENT: {:?}", event);
//! 	node.event_handled();
//!
//! 	let invoice = Bolt11Invoice::from_str("INVOICE_STR").unwrap();
//! 	node.send_payment(&invoice).unwrap();
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
#![cfg_attr(not(feature = "uniffi"), deny(missing_docs))]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![allow(bare_trait_objects)]
#![allow(ellipsis_inclusive_range_patterns)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod builder;
mod error;
mod event;
mod fee_estimator;
mod gossip;
mod hex_utils;
pub mod io;
mod liquidity;
mod logger;
mod message_handler;
mod payment_store;
mod peer_store;
mod sweep;
mod tx_broadcaster;
mod types;
#[cfg(feature = "uniffi")]
mod uniffi_types;
mod wallet;

pub use bip39;
pub use bitcoin;
pub use lightning;
pub use lightning_invoice;

pub use error::Error as NodeError;
use error::Error;

pub use event::Event;
pub use types::ChannelConfig;

pub use io::utils::generate_entropy_mnemonic;

#[cfg(feature = "uniffi")]
use {
	bip39::Mnemonic,
	bitcoin::OutPoint,
	lightning::events::{ClosureReason, PaymentFailureReason},
	lightning::ln::ChannelId,
	lightning::ln::PaymentSecret,
	lightning::util::string::UntrustedString,
	uniffi_types::*,
};

#[cfg(feature = "uniffi")]
pub use builder::ArcedNodeBuilder as Builder;
pub use builder::BuildError;
#[cfg(not(feature = "uniffi"))]
pub use builder::NodeBuilder as Builder;

use event::{EventHandler, EventQueue};
use gossip::GossipSource;
use liquidity::LiquiditySource;
use payment_store::PaymentStore;
pub use payment_store::{LSPFeeLimits, PaymentDetails, PaymentDirection, PaymentStatus};
use peer_store::{PeerInfo, PeerStore};
use types::{
	Broadcaster, ChainMonitor, ChannelManager, FeeEstimator, KeysManager, NetworkGraph,
	PeerManager, Router, Scorer, Sweeper, Wallet,
};
pub use types::{ChannelDetails, PeerDetails, UserChannelId};

use logger::{log_error, log_info, log_trace, FilesystemLogger, Logger};

use lightning::chain::Confirm;
use lightning::ln::channelmanager::{self, PaymentId, RecipientOnionFields, Retry};
use lightning::ln::msgs::SocketAddress;
use lightning::ln::{PaymentHash, PaymentPreimage};

use lightning::sign::EntropySource;

use lightning::util::persist::KVStore;

use lightning::util::config::{ChannelHandshakeConfig, UserConfig};
pub use lightning::util::logger::Level as LogLevel;

use lightning_background_processor::process_events_async;

use lightning_transaction_sync::EsploraSyncClient;

use lightning::routing::router::{PaymentParameters, RouteParameters};
use lightning_invoice::{payment, Bolt11Invoice, Currency};

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;

use bitcoin::{Address, Network, Txid};

use rand::Rng;

use std::default::Default;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

#[cfg(feature = "uniffi")]
uniffi::include_scaffolding!("ldk_node");

// Config defaults
const DEFAULT_STORAGE_DIR_PATH: &str = "/tmp/ldk_node/";
const DEFAULT_NETWORK: Network = Network::Bitcoin;
const DEFAULT_CLTV_EXPIRY_DELTA: u32 = 144;
const DEFAULT_BDK_WALLET_SYNC_INTERVAL_SECS: u64 = 80;
const DEFAULT_LDK_WALLET_SYNC_INTERVAL_SECS: u64 = 30;
const DEFAULT_FEE_RATE_CACHE_UPDATE_INTERVAL_SECS: u64 = 60 * 10;
const DEFAULT_PROBING_LIQUIDITY_LIMIT_MULTIPLIER: u64 = 3;
const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Debug;

// The 'stop gap' parameter used by BDK's wallet sync. This seems to configure the threshold
// number of derivation indexes after which BDK stops looking for new scripts belonging to the wallet.
const BDK_CLIENT_STOP_GAP: usize = 20;

// The number of concurrent requests made against the API provider.
const BDK_CLIENT_CONCURRENCY: u8 = 4;

// The default Esplora server we're using.
const DEFAULT_ESPLORA_SERVER_URL: &str = "https://blockstream.info/api";

// The timeout after which we abandon retrying failed payments.
const LDK_PAYMENT_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

// The time in-between peer reconnection attempts.
const PEER_RECONNECTION_INTERVAL: Duration = Duration::from_secs(10);

// The time in-between RGS sync attempts.
const RGS_SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);

// The time in-between node announcement broadcast attempts.
const NODE_ANN_BCAST_INTERVAL: Duration = Duration::from_secs(60 * 60);

// The lower limit which we apply to any configured wallet sync intervals.
const WALLET_SYNC_INTERVAL_MINIMUM_SECS: u64 = 10;

// The length in bytes of our wallets' keys seed.
const WALLET_KEYS_SEED_LEN: usize = 64;

#[derive(Debug, Clone)]
/// Represents the configuration of an [`Node`] instance.
///
/// ### Defaults
///
/// | Parameter                              | Value              |
/// |----------------------------------------|--------------------|
/// | `storage_dir_path`                     | /tmp/ldk_node/     |
/// | `log_dir_path`                         | None               |
/// | `network`                              | Bitcoin            |
/// | `listening_addresses`                  | None               |
/// | `default_cltv_expiry_delta`            | 144                |
/// | `onchain_wallet_sync_interval_secs`    | 80                 |
/// | `wallet_sync_interval_secs`            | 30                 |
/// | `fee_rate_cache_update_interval_secs`  | 600                |
/// | `trusted_peers_0conf`                  | []                 |
/// | `probing_liquidity_limit_multiplier`   | 3                  |
/// | `log_level`                            | Debug              |
///
pub struct Config {
	/// The path where the underlying LDK and BDK persist their data.
	pub storage_dir_path: String,
	/// The path where logs are stored.
	///
	/// If set to `None`, logs can be found in the `logs` subdirectory in [`Config::storage_dir_path`].
	pub log_dir_path: Option<String>,
	/// The used Bitcoin network.
	pub network: Network,
	/// The addresses on which the node will listen for incoming connections.
	pub listening_addresses: Option<Vec<SocketAddress>>,
	/// The default CLTV expiry delta to be used for payments.
	pub default_cltv_expiry_delta: u32,
	/// The time in-between background sync attempts of the onchain wallet, in seconds.
	///
	/// **Note:** A minimum of 10 seconds is always enforced.
	pub onchain_wallet_sync_interval_secs: u64,
	/// The time in-between background sync attempts of the LDK wallet, in seconds.
	///
	/// **Note:** A minimum of 10 seconds is always enforced.
	pub wallet_sync_interval_secs: u64,
	/// The time in-between background update attempts to our fee rate cache, in seconds.
	///
	/// **Note:** A minimum of 10 seconds is always enforced.
	pub fee_rate_cache_update_interval_secs: u64,
	/// A list of peers that we allow to establish zero confirmation channels to us.
	///
	/// **Note:** Allowing payments via zero-confirmation channels is potentially insecure if the
	/// funding transaction ends up never being confirmed on-chain. Zero-confirmation channels
	/// should therefore only be accepted from trusted peers.
	pub trusted_peers_0conf: Vec<PublicKey>,
	/// The liquidity factor by which we filter the outgoing channels used for sending probes.
	///
	/// Channels with available liquidity less than the required amount times this value won't be
	/// used to send pre-flight probes.
	pub probing_liquidity_limit_multiplier: u64,
	/// The level at which we log messages.
	///
	/// Any messages below this level will be excluded from the logs.
	pub log_level: LogLevel,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			storage_dir_path: DEFAULT_STORAGE_DIR_PATH.to_string(),
			log_dir_path: None,
			network: DEFAULT_NETWORK,
			listening_addresses: None,
			default_cltv_expiry_delta: DEFAULT_CLTV_EXPIRY_DELTA,
			onchain_wallet_sync_interval_secs: DEFAULT_BDK_WALLET_SYNC_INTERVAL_SECS,
			wallet_sync_interval_secs: DEFAULT_LDK_WALLET_SYNC_INTERVAL_SECS,
			fee_rate_cache_update_interval_secs: DEFAULT_FEE_RATE_CACHE_UPDATE_INTERVAL_SECS,
			trusted_peers_0conf: Vec::new(),
			probing_liquidity_limit_multiplier: DEFAULT_PROBING_LIQUIDITY_LIMIT_MULTIPLIER,
			log_level: DEFAULT_LOG_LEVEL,
		}
	}
}

/// Returns a [`Config`] object populated with default values.
///
/// See the documentation of [`Config`] for more information on the used defaults.
///
/// This is mostly meant for use in bindings, in Rust this is synonymous with
/// [`Config::default()`].
pub fn default_config() -> Config {
	Config::default()
}

/// The main interface object of LDK Node, wrapping the necessary LDK and BDK functionalities.
///
/// Needs to be initialized and instantiated through [`Builder::build`].
pub struct Node<K: KVStore + Sync + Send + 'static> {
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
	stop_sender: tokio::sync::watch::Sender<()>,
	stop_receiver: tokio::sync::watch::Receiver<()>,
	config: Arc<Config>,
	wallet: Arc<Wallet>,
	tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	tx_broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<FeeEstimator>,
	event_queue: Arc<EventQueue<K, Arc<FilesystemLogger>>>,
	channel_manager: Arc<ChannelManager<K>>,
	chain_monitor: Arc<ChainMonitor<K>>,
	output_sweeper: Arc<Sweeper<K>>,
	peer_manager: Arc<PeerManager<K>>,
	keys_manager: Arc<KeysManager>,
	network_graph: Arc<NetworkGraph>,
	gossip_source: Arc<GossipSource>,
	liquidity_source: Option<Arc<LiquiditySource<K, Arc<FilesystemLogger>>>>,
	kv_store: Arc<K>,
	logger: Arc<FilesystemLogger>,
	_router: Arc<Router>,
	scorer: Arc<Mutex<Scorer>>,
	peer_store: Arc<PeerStore<K, Arc<FilesystemLogger>>>,
	payment_store: Arc<PaymentStore<K, Arc<FilesystemLogger>>>,
}

impl<K: KVStore + Sync + Send + 'static> Node<K> {
	/// Starts the necessary background tasks, such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network.
	///
	/// After this returns, the [`Node`] instance can be controlled via the provided API methods in
	/// a thread-safe manner.
	pub fn start(&self) -> Result<(), Error> {
		// Acquire a run lock and hold it until we're setup.
		let mut runtime_lock = self.runtime.write().unwrap();
		if runtime_lock.is_some() {
			// We're already running.
			return Err(Error::AlreadyRunning);
		}

		log_info!(self.logger, "Starting up LDK Node on network: {}", self.config.network);

		let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();

		// Block to ensure we update our fee rate cache once on startup
		let fee_estimator = Arc::clone(&self.fee_estimator);
		let sync_logger = Arc::clone(&self.logger);
		let runtime_ref = &runtime;
		tokio::task::block_in_place(move || {
			runtime_ref.block_on(async move {
				let now = Instant::now();
				match fee_estimator.update_fee_estimates().await {
					Ok(()) => {
						log_info!(
							sync_logger,
							"Initial fee rate cache update finished in {}ms.",
							now.elapsed().as_millis()
						);
						Ok(())
					}
					Err(e) => {
						log_error!(sync_logger, "Initial fee rate cache update failed: {}", e,);
						Err(e)
					}
				}
			})
		})?;

		// Setup wallet sync
		let wallet = Arc::clone(&self.wallet);
		let sync_logger = Arc::clone(&self.logger);
		let mut stop_sync = self.stop_receiver.clone();
		let onchain_wallet_sync_interval_secs =
			self.config.onchain_wallet_sync_interval_secs.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		std::thread::spawn(move || {
			tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
				async move {
					let mut onchain_wallet_sync_interval = tokio::time::interval(
						Duration::from_secs(onchain_wallet_sync_interval_secs),
					);
					onchain_wallet_sync_interval
						.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
					loop {
						tokio::select! {
							_ = stop_sync.changed() => {
								return;
							}
							_ = onchain_wallet_sync_interval.tick() => {
								let now = Instant::now();
								match wallet.sync().await {
									Ok(()) => log_trace!(
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
							}
						}
					}
				},
			);
		});

		let mut stop_fee_updates = self.stop_receiver.clone();
		let fee_update_logger = Arc::clone(&self.logger);
		let fee_estimator = Arc::clone(&self.fee_estimator);
		let fee_rate_cache_update_interval_secs =
			self.config.fee_rate_cache_update_interval_secs.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		runtime.spawn(async move {
			let mut fee_rate_update_interval =
				tokio::time::interval(Duration::from_secs(fee_rate_cache_update_interval_secs));
			// We just blocked on updating, so skip the first tick.
			fee_rate_update_interval.reset();
			fee_rate_update_interval
				.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					_ = stop_fee_updates.changed() => {
						return;
					}
					_ = fee_rate_update_interval.tick() => {
						let now = Instant::now();
						match fee_estimator.update_fee_estimates().await {
							Ok(()) => log_trace!(
								fee_update_logger,
								"Background update of fee rate cache finished in {}ms.",
								now.elapsed().as_millis()
								),
							Err(err) => {
								log_error!(
									fee_update_logger,
									"Background update of fee rate cache failed: {}",
									err
									)
							}
						}
					}
				}
			}
		});

		let tx_sync = Arc::clone(&self.tx_sync);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let sync_sweeper = Arc::clone(&self.output_sweeper);
		let sync_logger = Arc::clone(&self.logger);
		let mut stop_sync = self.stop_receiver.clone();
		let wallet_sync_interval_secs =
			self.config.wallet_sync_interval_secs.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		runtime.spawn(async move {
			let mut wallet_sync_interval =
				tokio::time::interval(Duration::from_secs(wallet_sync_interval_secs));
			wallet_sync_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					_ = stop_sync.changed() => {
						return;
					}
					_ = wallet_sync_interval.tick() => {
						let confirmables = vec![
							&*sync_cman as &(dyn Confirm + Sync + Send),
							&*sync_cmon as &(dyn Confirm + Sync + Send),
							&*sync_sweeper as &(dyn Confirm + Sync + Send),
						];
						let now = Instant::now();
						match tx_sync.sync(confirmables).await {
							Ok(()) => log_trace!(
								sync_logger,
								"Background sync of Lightning wallet finished in {}ms.",
								now.elapsed().as_millis()
								),
							Err(e) => {
								log_error!(sync_logger, "Background sync of Lightning wallet failed: {}", e)
							}
						}
					}
				}
			}
		});

		if self.gossip_source.is_rgs() {
			let gossip_source = Arc::clone(&self.gossip_source);
			let gossip_sync_store = Arc::clone(&self.kv_store);
			let gossip_sync_logger = Arc::clone(&self.logger);
			let mut stop_gossip_sync = self.stop_receiver.clone();
			runtime.spawn(async move {
				let mut interval = tokio::time::interval(RGS_SYNC_INTERVAL);
				loop {
					tokio::select! {
						_ = stop_gossip_sync.changed() => {
							return;
						}
						_ = interval.tick() => {
							let gossip_sync_logger = Arc::clone(&gossip_sync_logger);
							let now = Instant::now();
							match gossip_source.update_rgs_snapshot().await {
								Ok(updated_timestamp) => {
									log_trace!(
										gossip_sync_logger,
										"Background sync of RGS gossip data finished in {}ms.",
										now.elapsed().as_millis()
										);
									io::utils::write_latest_rgs_sync_timestamp(
										updated_timestamp,
										Arc::clone(&gossip_sync_store),
										Arc::clone(&gossip_sync_logger),
										)
										.unwrap_or_else(|e| {
											log_error!(gossip_sync_logger, "Persistence failed: {}", e);
											panic!("Persistence failed");
										});
								}
								Err(e) => log_error!(
									gossip_sync_logger,
									"Background sync of RGS gossip data failed: {}",
									e
									),
							}
						}
					}
				}
			});
		}

		if let Some(listening_addresses) = &self.config.listening_addresses {
			// Setup networking
			let peer_manager_connection_handler = Arc::clone(&self.peer_manager);
			let mut stop_listen = self.stop_receiver.clone();
			let listening_logger = Arc::clone(&self.logger);

			let mut bind_addrs = Vec::with_capacity(listening_addresses.len());

			for listening_addr in listening_addresses {
				let resolved_address = listening_addr.to_socket_addrs().map_err(|e| {
					log_error!(
						self.logger,
						"Unable to resolve listening address: {:?}. Error details: {}",
						listening_addr,
						e,
					);
					Error::InvalidSocketAddress
				})?;

				bind_addrs.extend(resolved_address);
			}

			runtime.spawn(async move {
				let listener =
					tokio::net::TcpListener::bind(&*bind_addrs).await
										.unwrap_or_else(|e| {
											log_error!(listening_logger, "Failed to bind to listen addresses/ports - is something else already listening on it?: {}", e);
											panic!(
												"Failed to bind to listen address/port - is something else already listening on it?",
												);
										});

				loop {
					let peer_mgr = Arc::clone(&peer_manager_connection_handler);
					tokio::select! {
						_ = stop_listen.changed() => {
							return;
						}
						res = listener.accept() => {
							let tcp_stream = res.unwrap().0;
							tokio::spawn(async move {
								lightning_net_tokio::setup_inbound(
									Arc::clone(&peer_mgr),
									tcp_stream.into_std().unwrap(),
									)
									.await;
							});
						}
					}
				}
			});
		}

		// Regularly reconnect to channel peers.
		let connect_cm = Arc::clone(&self.channel_manager);
		let connect_pm = Arc::clone(&self.peer_manager);
		let connect_logger = Arc::clone(&self.logger);
		let connect_peer_store = Arc::clone(&self.peer_store);
		let mut stop_connect = self.stop_receiver.clone();
		runtime.spawn(async move {
			let mut interval = tokio::time::interval(PEER_RECONNECTION_INTERVAL);
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
						_ = stop_connect.changed() => {
							return;
						}
						_ = interval.tick() => {
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
											let res = do_connect_peer(
												peer_info.node_id,
												peer_info.address,
												Arc::clone(&connect_pm),
												Arc::clone(&connect_logger),
												).await;
											match res {
												Ok(_) => {
													log_info!(connect_logger, "Successfully reconnected to peer {}", node_id);
												},
												Err(e) => {
													log_error!(connect_logger, "Failed to reconnect to peer {}: {}", node_id, e);
												}
											}
										}
									}
						}
				}
			}
		});

		// Regularly broadcast node announcements.
		let bcast_cm = Arc::clone(&self.channel_manager);
		let bcast_pm = Arc::clone(&self.peer_manager);
		let bcast_config = Arc::clone(&self.config);
		let bcast_store = Arc::clone(&self.kv_store);
		let bcast_logger = Arc::clone(&self.logger);
		let mut stop_bcast = self.stop_receiver.clone();
		runtime.spawn(async move {
			// We check every 30 secs whether our last broadcast is NODE_ANN_BCAST_INTERVAL away.
			let mut interval = tokio::time::interval(Duration::from_secs(30));
			loop {
				tokio::select! {
						_ = stop_bcast.changed() => {
							return;
						}
						_ = interval.tick() => {
							let skip_broadcast = match io::utils::read_latest_node_ann_bcast_timestamp(Arc::clone(&bcast_store), Arc::clone(&bcast_logger)) {
								Ok(latest_bcast_time_secs) => {
									// Skip if the time hasn't elapsed yet.
									let next_bcast_unix_time = SystemTime::UNIX_EPOCH + Duration::from_secs(latest_bcast_time_secs) + NODE_ANN_BCAST_INTERVAL;
									next_bcast_unix_time.elapsed().is_err()
								}
								Err(_) => {
									// Don't skip if we haven't broadcasted before.
									false
								}
							};

							if skip_broadcast {
								continue;
							}

							if !bcast_cm.list_channels().iter().any(|chan| chan.is_public) {
								// Skip if we don't have any public channels.
								continue;
							}

							if bcast_pm.get_peer_node_ids().is_empty() {
								// Skip if we don't have any connected peers to gossip to.
								continue;
							}

							let addresses = bcast_config.listening_addresses.clone().unwrap_or(Vec::new());

							if addresses.is_empty() {
								// Skip if we are not listening on any addresses.
								continue;
							}

							bcast_pm.broadcast_node_announcement([0; 3], [0; 32], addresses);

							let unix_time_secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
							io::utils::write_latest_node_ann_bcast_timestamp(unix_time_secs, Arc::clone(&bcast_store), Arc::clone(&bcast_logger))
								.unwrap_or_else(|e| {
									log_error!(bcast_logger, "Persistence failed: {}", e);
									panic!("Persistence failed");
								});
						}
				}
			}
		});

		let mut stop_tx_bcast = self.stop_receiver.clone();
		let tx_bcaster = Arc::clone(&self.tx_broadcaster);
		runtime.spawn(async move {
			// Every second we try to clear our broadcasting queue.
			let mut interval = tokio::time::interval(Duration::from_secs(1));
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
						_ = stop_tx_bcast.changed() => {
							return;
						}
						_ = interval.tick() => {
							tx_bcaster.process_queue().await;
						}
				}
			}
		});

		let event_handler = Arc::new(EventHandler::new(
			Arc::clone(&self.event_queue),
			Arc::clone(&self.wallet),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.output_sweeper),
			Arc::clone(&self.network_graph),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.peer_store),
			Arc::clone(&self.runtime),
			Arc::clone(&self.logger),
			Arc::clone(&self.config),
		));

		// Setup background processing
		let background_persister = Arc::clone(&self.kv_store);
		let background_event_handler = Arc::clone(&event_handler);
		let background_chain_mon = Arc::clone(&self.chain_monitor);
		let background_chan_man = Arc::clone(&self.channel_manager);
		let background_gossip_sync = self.gossip_source.as_gossip_sync();
		let background_peer_man = Arc::clone(&self.peer_manager);
		let background_logger = Arc::clone(&self.logger);
		let background_error_logger = Arc::clone(&self.logger);
		let background_scorer = Arc::clone(&self.scorer);
		let stop_bp = self.stop_receiver.clone();
		let sleeper = move |d| {
			let mut stop = stop_bp.clone();
			Box::pin(async move {
				tokio::select! {
					_ = stop.changed() => {
						true
					}
					_ = tokio::time::sleep(d) => {
						false
					}
				}
			})
		};

		runtime.spawn(async move {
			process_events_async(
				background_persister,
				|e| background_event_handler.handle_event(e),
				background_chain_mon,
				background_chan_man,
				background_gossip_sync,
				background_peer_man,
				background_logger,
				Some(background_scorer),
				sleeper,
				true,
				|| Some(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap()),
			)
			.await
			.unwrap_or_else(|e| {
				log_error!(background_error_logger, "Failed to process events: {}", e);
				panic!("Failed to process events");
			});
		});

		if let Some(liquidity_source) = self.liquidity_source.as_ref() {
			let mut stop_liquidity_handler = self.stop_receiver.clone();
			let liquidity_handler = Arc::clone(&liquidity_source);
			runtime.spawn(async move {
				loop {
					tokio::select! {
						_ = stop_liquidity_handler.changed() => {
							return;
						}
						_ = liquidity_handler.handle_next_event() => {}
					}
				}
			});
		}

		*runtime_lock = Some(runtime);

		log_info!(self.logger, "Startup complete.");
		Ok(())
	}

	/// Returns whether the [`Node`] is running.
	pub fn is_running(&self) -> bool {
		self.runtime.read().unwrap().is_some()
	}

	/// Disconnects all peers, stops all running background tasks, and shuts down [`Node`].
	///
	/// After this returns most API methods will return [`Error::NotRunning`].
	pub fn stop(&self) -> Result<(), Error> {
		let runtime = self.runtime.write().unwrap().take().ok_or(Error::NotRunning)?;

		log_info!(self.logger, "Shutting down LDK Node...");

		// Stop the runtime.
		match self.stop_sender.send(()) {
			Ok(_) => (),
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to send shutdown signal. This should never happen: {}",
					e
				);
				debug_assert!(false);
			}
		}

		// Stop disconnect peers.
		self.peer_manager.disconnect_all_peers();

		runtime.shutdown_timeout(Duration::from_secs(10));

		log_info!(self.logger, "Shutdown complete.");
		Ok(())
	}

	/// Returns the next event in the event queue, if currently available.
	///
	/// Will return `Some(..)` if an event is available and `None` otherwise.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Node::event_handled`].
	pub fn next_event(&self) -> Option<Event> {
		self.event_queue.next_event()
	}

	/// Returns the next event in the event queue.
	///
	/// Will asynchronously poll the event queue until the next event is ready.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Node::event_handled`].
	pub async fn next_event_async(&self) -> Event {
		self.event_queue.next_event_async().await
	}

	/// Returns the next event in the event queue.
	///
	/// Will block the current thread until the next event is available.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Node::event_handled`].
	pub fn wait_next_event(&self) -> Event {
		self.event_queue.wait_next_event()
	}

	/// Confirm the last retrieved event handled.
	///
	/// **Note:** This **MUST** be called after each event has been handled.
	pub fn event_handled(&self) {
		self.event_queue.event_handled().unwrap_or_else(|e| {
			log_error!(
				self.logger,
				"Couldn't mark event handled due to persistence failure: {}",
				e
			);
			panic!("Couldn't mark event handled due to persistence failure");
		});
	}

	/// Returns our own node id
	pub fn node_id(&self) -> PublicKey {
		self.channel_manager.get_our_node_id()
	}

	/// Returns our own listening addresses.
	pub fn listening_addresses(&self) -> Option<Vec<SocketAddress>> {
		self.config.listening_addresses.clone()
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_onchain_address(&self) -> Result<Address, Error> {
		let funding_address = self.wallet.get_new_address()?;
		log_info!(self.logger, "Generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	/// Retrieve the currently spendable on-chain balance in satoshis.
	pub fn spendable_onchain_balance_sats(&self) -> Result<u64, Error> {
		Ok(self.wallet.get_balance().map(|bal| bal.get_spendable())?)
	}

	/// Retrieve the current total on-chain balance in satoshis.
	pub fn total_onchain_balance_sats(&self) -> Result<u64, Error> {
		Ok(self.wallet.get_balance().map(|bal| bal.get_total())?)
	}

	/// Send an on-chain payment to the given address.
	pub fn send_to_onchain_address(
		&self, address: &bitcoin::Address, amount_sats: u64,
	) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let cur_balance = self.wallet.get_balance()?;
		if cur_balance.get_spendable() < amount_sats {
			log_error!(self.logger, "Unable to send payment due to insufficient funds.");
			return Err(Error::InsufficientFunds);
		}
		self.wallet.send_to_address(address, Some(amount_sats))
	}

	/// Send an on-chain payment to the given address, draining all the available funds.
	pub fn send_all_to_onchain_address(&self, address: &bitcoin::Address) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		self.wallet.send_to_address(address, None)
	}

	/// Retrieve a list of known channels.
	pub fn list_channels(&self) -> Vec<ChannelDetails> {
		self.channel_manager.list_channels().into_iter().map(|c| c.into()).collect()
	}

	/// Connect to a node on the peer-to-peer network.
	///
	/// If `persist` is set to `true`, we'll remember the peer and reconnect to it on restart.
	pub fn connect(
		&self, node_id: PublicKey, address: SocketAddress, persist: bool,
	) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let runtime = rt_lock.as_ref().unwrap();

		let peer_info = PeerInfo { node_id, address };

		let con_node_id = peer_info.node_id;
		let con_addr = peer_info.address.clone();
		let con_logger = Arc::clone(&self.logger);
		let con_pm = Arc::clone(&self.peer_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				connect_peer_if_necessary(con_node_id, con_addr, con_pm, con_logger).await
			})
		})?;

		log_info!(self.logger, "Connected to peer {}@{}. ", peer_info.node_id, peer_info.address);

		if persist {
			self.peer_store.add_peer(peer_info)?;
		}

		Ok(())
	}

	/// Disconnects the peer with the given node id.
	///
	/// Will also remove the peer from the peer store, i.e., after this has been called we won't
	/// try to reconnect on restart.
	pub fn disconnect(&self, counterparty_node_id: PublicKey) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		log_info!(self.logger, "Disconnecting peer {}..", counterparty_node_id);

		match self.peer_store.remove_peer(&counterparty_node_id) {
			Ok(()) => {}
			Err(e) => {
				log_error!(self.logger, "Failed to remove peer {}: {}", counterparty_node_id, e)
			}
		}

		self.peer_manager.disconnect_by_node_id(counterparty_node_id);
		Ok(())
	}

	/// Connect to a node and open a new channel. Disconnects and re-connects are handled automatically
	///
	/// Disconnects and reconnects are handled automatically.
	///
	/// If `push_to_counterparty_msat` is set, the given value will be pushed (read: sent) to the
	/// channel counterparty on channel open. This can be useful to start out with the balance not
	/// entirely shifted to one side, therefore allowing to receive payments from the getgo.
	///
	/// Returns a [`UserChannelId`] allowing to locally keep track of the channel.
	pub fn connect_open_channel(
		&self, node_id: PublicKey, address: SocketAddress, channel_amount_sats: u64,
		push_to_counterparty_msat: Option<u64>, channel_config: Option<Arc<ChannelConfig>>,
		announce_channel: bool,
	) -> Result<UserChannelId, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let runtime = rt_lock.as_ref().unwrap();

		let cur_balance = self.wallet.get_balance()?;
		if cur_balance.get_spendable() < channel_amount_sats {
			log_error!(self.logger, "Unable to create channel due to insufficient funds.");
			return Err(Error::InsufficientFunds);
		}

		let peer_info = PeerInfo { node_id, address };

		let con_node_id = peer_info.node_id;
		let con_addr = peer_info.address.clone();
		let con_logger = Arc::clone(&self.logger);
		let con_pm = Arc::clone(&self.peer_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				connect_peer_if_necessary(con_node_id, con_addr, con_pm, con_logger).await
			})
		})?;

		let channel_config = (*(channel_config.unwrap_or_default())).clone().into();
		let user_config = UserConfig {
			channel_handshake_limits: Default::default(),
			channel_handshake_config: ChannelHandshakeConfig {
				announced_channel: announce_channel,
				..Default::default()
			},
			channel_config,
			..Default::default()
		};

		let push_msat = push_to_counterparty_msat.unwrap_or(0);
		let user_channel_id: u128 = rand::thread_rng().gen::<u128>();

		match self.channel_manager.create_channel(
			peer_info.node_id,
			channel_amount_sats,
			push_msat,
			user_channel_id,
			None,
			Some(user_config),
		) {
			Ok(_) => {
				log_info!(
					self.logger,
					"Initiated channel creation with peer {}. ",
					peer_info.node_id
				);
				self.peer_store.add_peer(peer_info)?;
				Ok(UserChannelId(user_channel_id))
			}
			Err(e) => {
				log_error!(self.logger, "Failed to initiate channel creation: {:?}", e);
				Err(Error::ChannelCreationFailed)
			}
		}
	}

	/// Manually sync the LDK and BDK wallets with the current chain state.
	///
	/// **Note:** The wallets are regularly synced in the background, which is configurable via
	/// [`Config::onchain_wallet_sync_interval_secs`] and [`Config::wallet_sync_interval_secs`].
	/// Therefore, using this blocking sync method is almost always redundant and should be avoided
	/// where possible.
	pub fn sync_wallets(&self) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let wallet = Arc::clone(&self.wallet);
		let tx_sync = Arc::clone(&self.tx_sync);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let sync_sweeper = Arc::clone(&self.output_sweeper);
		let sync_logger = Arc::clone(&self.logger);
		let confirmables = vec![
			&*sync_cman as &(dyn Confirm + Sync + Send),
			&*sync_cmon as &(dyn Confirm + Sync + Send),
			&*sync_sweeper as &(dyn Confirm + Sync + Send),
		];

		tokio::task::block_in_place(move || {
			tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(
				async move {
					let now = Instant::now();
					match wallet.sync().await {
						Ok(()) => {
							log_info!(
								sync_logger,
								"Sync of on-chain wallet finished in {}ms.",
								now.elapsed().as_millis()
							);
						}
						Err(e) => {
							log_error!(sync_logger, "Sync of on-chain wallet failed: {}", e);
							return Err(e);
						}
					};

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
							Err(e.into())
						}
					}
				},
			)
		})
	}

	/// Close a previously opened channel.
	pub fn close_channel(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
	) -> Result<(), Error> {
		let open_channels =
			self.channel_manager.list_channels_with_counterparty(&counterparty_node_id);
		if let Some(channel_details) =
			open_channels.iter().find(|c| c.user_channel_id == user_channel_id.0)
		{
			match self
				.channel_manager
				.close_channel(&channel_details.channel_id, &counterparty_node_id)
			{
				Ok(_) => {
					// Check if this was the last open channel, if so, forget the peer.
					if open_channels.len() == 1 {
						self.peer_store.remove_peer(&counterparty_node_id)?;
					}
					Ok(())
				}
				Err(_) => Err(Error::ChannelClosingFailed),
			}
		} else {
			Ok(())
		}
	}

	/// Update the config for a previously opened channel.
	pub fn update_channel_config(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
		channel_config: Arc<ChannelConfig>,
	) -> Result<(), Error> {
		let open_channels =
			self.channel_manager.list_channels_with_counterparty(&counterparty_node_id);
		if let Some(channel_details) =
			open_channels.iter().find(|c| c.user_channel_id == user_channel_id.0)
		{
			self.channel_manager
				.update_channel_config(
					&counterparty_node_id,
					&[channel_details.channel_id],
					&(*channel_config).clone().into(),
				)
				.map_err(|_| Error::ChannelConfigUpdateFailed)
		} else {
			Err(Error::ChannelConfigUpdateFailed)
		}
	}

	/// Send a payment given an invoice.
	pub fn send_payment(&self, invoice: &Bolt11Invoice) -> Result<PaymentHash, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let (payment_hash, recipient_onion, route_params) = payment::payment_parameters_from_invoice(&invoice).map_err(|_| {
			log_error!(self.logger, "Failed to send payment due to the given invoice being \"zero-amount\". Please use send_payment_using_amount instead.");
			Error::InvalidInvoice
		})?;

		if let Some(payment) = self.payment_store.get(&payment_hash) {
			if payment.status == PaymentStatus::Pending
				|| payment.status == PaymentStatus::Succeeded
			{
				log_error!(self.logger, "Payment error: an invoice must not be paid twice.");
				return Err(Error::DuplicatePayment);
			}
		}

		let payment_secret = Some(*invoice.payment_secret());
		let payment_id = PaymentId(invoice.payment_hash().to_byte_array());
		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);

		match self.channel_manager.send_payment(
			payment_hash,
			recipient_onion,
			payment_id,
			route_params,
			retry_strategy,
		) {
			Ok(()) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				let amt_msat = invoice.amount_milli_satoshis().unwrap();
				log_info!(self.logger, "Initiated sending {}msat to {}", amt_msat, payee_pubkey);

				let payment = PaymentDetails {
					preimage: None,
					hash: payment_hash,
					secret: payment_secret,
					amount_msat: invoice.amount_milli_satoshis(),
					direction: PaymentDirection::Outbound,
					status: PaymentStatus::Pending,
					lsp_fee_limits: None,
				};
				self.payment_store.insert(payment)?;

				Ok(payment_hash)
			}
			Err(e) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);
				match e {
					channelmanager::RetryableSendFailure::DuplicatePayment => {
						Err(Error::DuplicatePayment)
					}
					_ => {
						let payment = PaymentDetails {
							preimage: None,
							hash: payment_hash,
							secret: payment_secret,
							amount_msat: invoice.amount_milli_satoshis(),
							direction: PaymentDirection::Outbound,
							status: PaymentStatus::Failed,
							lsp_fee_limits: None,
						};

						self.payment_store.insert(payment)?;
						Err(Error::PaymentSendingFailed)
					}
				}
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
		&self, invoice: &Bolt11Invoice, amount_msat: u64,
	) -> Result<PaymentHash, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		if let Some(invoice_amount_msat) = invoice.amount_milli_satoshis() {
			if amount_msat < invoice_amount_msat {
				log_error!(
					self.logger,
					"Failed to pay as the given amount needs to be at least the invoice amount: required {}msat, gave {}msat.", invoice_amount_msat, amount_msat);
				return Err(Error::InvalidAmount);
			}
		}

		let payment_hash = PaymentHash(invoice.payment_hash().to_byte_array());
		if let Some(payment) = self.payment_store.get(&payment_hash) {
			if payment.status == PaymentStatus::Pending
				|| payment.status == PaymentStatus::Succeeded
			{
				log_error!(self.logger, "Payment error: an invoice must not be paid twice.");
				return Err(Error::DuplicatePayment);
			}
		}

		let payment_id = PaymentId(invoice.payment_hash().to_byte_array());
		let payment_secret = invoice.payment_secret();
		let expiry_time = invoice.duration_since_epoch().saturating_add(invoice.expiry_time());
		let mut payment_params = PaymentParameters::from_node_id(
			invoice.recover_payee_pub_key(),
			invoice.min_final_cltv_expiry_delta() as u32,
		)
		.with_expiry_time(expiry_time.as_secs())
		.with_route_hints(invoice.route_hints())
		.map_err(|_| Error::InvalidInvoice)?;
		if let Some(features) = invoice.features() {
			payment_params = payment_params
				.with_bolt11_features(features.clone())
				.map_err(|_| Error::InvalidInvoice)?;
		}
		let route_params =
			RouteParameters::from_payment_params_and_value(payment_params, amount_msat);

		let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
		let recipient_fields = RecipientOnionFields::secret_only(*payment_secret);

		match self.channel_manager.send_payment(
			payment_hash,
			recipient_fields,
			payment_id,
			route_params,
			retry_strategy,
		) {
			Ok(_payment_id) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				log_info!(
					self.logger,
					"Initiated sending {} msat to {}",
					amount_msat,
					payee_pubkey
				);

				let payment = PaymentDetails {
					hash: payment_hash,
					preimage: None,
					secret: Some(*payment_secret),
					amount_msat: Some(amount_msat),
					direction: PaymentDirection::Outbound,
					status: PaymentStatus::Pending,
					lsp_fee_limits: None,
				};
				self.payment_store.insert(payment)?;

				Ok(payment_hash)
			}
			Err(e) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);

				match e {
					channelmanager::RetryableSendFailure::DuplicatePayment => {
						Err(Error::DuplicatePayment)
					}
					_ => {
						let payment = PaymentDetails {
							hash: payment_hash,
							preimage: None,
							secret: Some(*payment_secret),
							amount_msat: Some(amount_msat),
							direction: PaymentDirection::Outbound,
							status: PaymentStatus::Failed,
							lsp_fee_limits: None,
						};
						self.payment_store.insert(payment)?;

						Err(Error::PaymentSendingFailed)
					}
				}
			}
		}
	}

	/// Send a spontaneous, aka. "keysend", payment
	pub fn send_spontaneous_payment(
		&self, amount_msat: u64, node_id: PublicKey,
	) -> Result<PaymentHash, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let payment_preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
		let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0).to_byte_array());

		if let Some(payment) = self.payment_store.get(&payment_hash) {
			if payment.status == PaymentStatus::Pending
				|| payment.status == PaymentStatus::Succeeded
			{
				log_error!(self.logger, "Payment error: must not send duplicate payments.");
				return Err(Error::DuplicatePayment);
			}
		}

		let route_params = RouteParameters::from_payment_params_and_value(
			PaymentParameters::from_node_id(node_id, self.config.default_cltv_expiry_delta),
			amount_msat,
		);
		let recipient_fields = RecipientOnionFields::spontaneous_empty();

		match self.channel_manager.send_spontaneous_payment_with_retry(
			Some(payment_preimage),
			recipient_fields,
			PaymentId(payment_hash.0),
			route_params,
			Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT),
		) {
			Ok(_payment_id) => {
				log_info!(self.logger, "Initiated sending {}msat to {}.", amount_msat, node_id);

				let payment = PaymentDetails {
					hash: payment_hash,
					preimage: Some(payment_preimage),
					secret: None,
					status: PaymentStatus::Pending,
					direction: PaymentDirection::Outbound,
					amount_msat: Some(amount_msat),
					lsp_fee_limits: None,
				};
				self.payment_store.insert(payment)?;

				Ok(payment_hash)
			}
			Err(e) => {
				log_error!(self.logger, "Failed to send payment: {:?}", e);

				match e {
					channelmanager::RetryableSendFailure::DuplicatePayment => {
						Err(Error::DuplicatePayment)
					}
					_ => {
						let payment = PaymentDetails {
							hash: payment_hash,
							preimage: Some(payment_preimage),
							secret: None,
							status: PaymentStatus::Failed,
							direction: PaymentDirection::Outbound,
							amount_msat: Some(amount_msat),
							lsp_fee_limits: None,
						};

						self.payment_store.insert(payment)?;
						Err(Error::PaymentSendingFailed)
					}
				}
			}
		}
	}

	/// Sends payment probes over all paths of a route that would be used to pay the given invoice.
	///
	/// This may be used to send "pre-flight" probes, i.e., to train our scorer before conducting
	/// the actual payment. Note this is only useful if there likely is sufficient time for the
	/// probe to settle before sending out the actual payment, e.g., when waiting for user
	/// confirmation in a wallet UI.
	///
	/// Otherwise, there is a chance the probe could take up some liquidity needed to complete the
	/// actual payment. Users should therefore be cautious and might avoid sending probes if
	/// liquidity is scarce and/or they don't expect the probe to return before they send the
	/// payment. To mitigate this issue, channels with available liquidity less than the required
	/// amount times [`Config::probing_liquidity_limit_multiplier`] won't be used to send
	/// pre-flight probes.
	pub fn send_payment_probes(&self, invoice: &Bolt11Invoice) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let (_payment_hash, _recipient_onion, route_params) = payment::payment_parameters_from_invoice(&invoice).map_err(|_| {
			log_error!(self.logger, "Failed to send probes due to the given invoice being \"zero-amount\". Please use send_payment_probes_using_amount instead.");
			Error::InvalidInvoice
		})?;

		let liquidity_limit_multiplier = Some(self.config.probing_liquidity_limit_multiplier);

		self.channel_manager
			.send_preflight_probes(route_params, liquidity_limit_multiplier)
			.map_err(|e| {
				log_error!(self.logger, "Failed to send payment probes: {:?}", e);
				Error::ProbeSendingFailed
			})?;

		Ok(())
	}

	/// Sends payment probes over all paths of a route that would be used to pay the given
	/// amount to the given `node_id`.
	///
	/// See [`Self::send_payment_probes`] for more information.
	pub fn send_spontaneous_payment_probes(
		&self, amount_msat: u64, node_id: PublicKey,
	) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let liquidity_limit_multiplier = Some(self.config.probing_liquidity_limit_multiplier);
		let cltv_expiry_delta = self.config.default_cltv_expiry_delta;

		self.channel_manager
			.send_spontaneous_preflight_probes(
				node_id,
				amount_msat,
				cltv_expiry_delta,
				liquidity_limit_multiplier,
			)
			.map_err(|e| {
				log_error!(self.logger, "Failed to send payment probes: {:?}", e);
				Error::ProbeSendingFailed
			})?;

		Ok(())
	}

	/// Sends payment probes over all paths of a route that would be used to pay the given
	/// zero-value invoice using the given amount.
	///
	/// This can be used to send pre-flight probes for a so-called "zero-amount" invoice, i.e., an
	/// invoice that leaves the amount paid to be determined by the user.
	///
	/// See [`Self::send_payment_probes`] for more information.
	pub fn send_payment_probes_using_amount(
		&self, invoice: &Bolt11Invoice, amount_msat: u64,
	) -> Result<(), Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let (_payment_hash, _recipient_onion, route_params) = if let Some(invoice_amount_msat) =
			invoice.amount_milli_satoshis()
		{
			if amount_msat < invoice_amount_msat {
				log_error!(
					self.logger,
					"Failed to send probes as the given amount needs to be at least the invoice amount: required {}msat, gave {}msat.", invoice_amount_msat, amount_msat);
				return Err(Error::InvalidAmount);
			}

			payment::payment_parameters_from_invoice(&invoice).map_err(|_| {
				log_error!(self.logger, "Failed to send probes due to the given invoice unexpectedly being \"zero-amount\".");
				Error::InvalidInvoice
			})?
		} else {
			payment::payment_parameters_from_zero_amount_invoice(&invoice, amount_msat).map_err(|_| {
				log_error!(self.logger, "Failed to send probes due to the given invoice unexpectedly being not \"zero-amount\".");
				Error::InvalidInvoice
			})?
		};

		let liquidity_limit_multiplier = Some(self.config.probing_liquidity_limit_multiplier);

		self.channel_manager
			.send_preflight_probes(route_params, liquidity_limit_multiplier)
			.map_err(|e| {
				log_error!(self.logger, "Failed to send payment probes: {:?}", e);
				Error::ProbeSendingFailed
			})?;

		Ok(())
	}

	/// Returns a payable invoice that can be used to request and receive a payment of the amount
	/// given.
	pub fn receive_payment(
		&self, amount_msat: u64, description: &str, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		self.receive_payment_inner(Some(amount_msat), description, expiry_secs)
	}

	/// Returns a payable invoice that can be used to request and receive a payment for which the
	/// amount is to be determined by the user, also known as a "zero-amount" invoice.
	pub fn receive_variable_amount_payment(
		&self, description: &str, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		self.receive_payment_inner(None, description, expiry_secs)
	}

	fn receive_payment_inner(
		&self, amount_msat: Option<u64>, description: &str, expiry_secs: u32,
	) -> Result<Bolt11Invoice, Error> {
		let currency = Currency::from(self.config.network);
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

		let payment_hash = PaymentHash(invoice.payment_hash().to_byte_array());
		let payment = PaymentDetails {
			hash: payment_hash,
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			amount_msat,
			direction: PaymentDirection::Inbound,
			status: PaymentStatus::Pending,
			lsp_fee_limits: None,
		};

		self.payment_store.insert(payment)?;

		Ok(invoice)
	}

	/// Returns a payable invoice that can be used to request a payment of the amount given and
	/// receive it via a newly created just-in-time (JIT) channel.
	///
	/// When the returned invoice is paid, the configured [LSPS2]-compliant LSP will open a channel
	/// to us, supplying just-in-time inbound liquidity.
	///
	/// If set, `max_total_lsp_fee_limit_msat` will limit how much fee we allow the LSP to take for opening the
	/// channel to us. We'll use its cheapest offer otherwise.
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	pub fn receive_payment_via_jit_channel(
		&self, amount_msat: u64, description: &str, expiry_secs: u32,
		max_total_lsp_fee_limit_msat: Option<u64>,
	) -> Result<Bolt11Invoice, Error> {
		self.receive_payment_via_jit_channel_inner(
			Some(amount_msat),
			description,
			expiry_secs,
			max_total_lsp_fee_limit_msat,
			None,
		)
	}

	/// Returns a payable invoice that can be used to request a variable amount payment (also known
	/// as "zero-amount" invoice) and receive it via a newly created just-in-time (JIT) channel.
	///
	/// When the returned invoice is paid, the configured [LSPS2]-compliant LSP will open a channel
	/// to us, supplying just-in-time inbound liquidity.
	///
	/// If set, `max_proportional_lsp_fee_limit_ppm_msat` will limit how much proportional fee, in
	/// parts-per-million millisatoshis, we allow the LSP to take for opening the channel to us.
	/// We'll use its cheapest offer otherwise.
	///
	/// [LSPS2]: https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md
	pub fn receive_variable_amount_payment_via_jit_channel(
		&self, description: &str, expiry_secs: u32,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
	) -> Result<Bolt11Invoice, Error> {
		self.receive_payment_via_jit_channel_inner(
			None,
			description,
			expiry_secs,
			None,
			max_proportional_lsp_fee_limit_ppm_msat,
		)
	}

	fn receive_payment_via_jit_channel_inner(
		&self, amount_msat: Option<u64>, description: &str, expiry_secs: u32,
		max_total_lsp_fee_limit_msat: Option<u64>,
		max_proportional_lsp_fee_limit_ppm_msat: Option<u64>,
	) -> Result<Bolt11Invoice, Error> {
		let liquidity_source =
			self.liquidity_source.as_ref().ok_or(Error::LiquiditySourceUnavailable)?;

		let (node_id, address) = liquidity_source
			.get_liquidity_source_details()
			.ok_or(Error::LiquiditySourceUnavailable)?;

		let rt_lock = self.runtime.read().unwrap();
		let runtime = rt_lock.as_ref().unwrap();

		let peer_info = PeerInfo { node_id, address };

		let con_node_id = peer_info.node_id;
		let con_addr = peer_info.address.clone();
		let con_logger = Arc::clone(&self.logger);
		let con_pm = Arc::clone(&self.peer_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				connect_peer_if_necessary(con_node_id, con_addr, con_pm, con_logger).await
			})
		})?;

		log_info!(self.logger, "Connected to LSP {}@{}. ", peer_info.node_id, peer_info.address);

		let liquidity_source = Arc::clone(&liquidity_source);
		let (invoice, lsp_total_opening_fee, lsp_prop_opening_fee) =
			tokio::task::block_in_place(move || {
				runtime.block_on(async move {
					if let Some(amount_msat) = amount_msat {
						liquidity_source
							.lsps2_receive_to_jit_channel(
								amount_msat,
								description,
								expiry_secs,
								max_total_lsp_fee_limit_msat,
							)
							.await
							.map(|(invoice, total_fee)| (invoice, Some(total_fee), None))
					} else {
						liquidity_source
							.lsps2_receive_variable_amount_to_jit_channel(
								description,
								expiry_secs,
								max_proportional_lsp_fee_limit_ppm_msat,
							)
							.await
							.map(|(invoice, prop_fee)| (invoice, None, Some(prop_fee)))
					}
				})
			})?;

		// Register payment in payment store.
		let payment_hash = PaymentHash(invoice.payment_hash().to_byte_array());
		let lsp_fee_limits = Some(LSPFeeLimits {
			max_total_opening_fee_msat: lsp_total_opening_fee,
			max_proportional_opening_fee_ppm_msat: lsp_prop_opening_fee,
		});
		let payment = PaymentDetails {
			hash: payment_hash,
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			amount_msat,
			direction: PaymentDirection::Inbound,
			status: PaymentStatus::Pending,
			lsp_fee_limits,
		};

		self.payment_store.insert(payment)?;

		// Persist LSP peer to make sure we reconnect on restart.
		self.peer_store.add_peer(peer_info)?;

		Ok(invoice)
	}

	/// Retrieve the details of a specific payment with the given hash.
	///
	/// Returns `Some` if the payment was known and `None` otherwise.
	pub fn payment(&self, payment_hash: &PaymentHash) -> Option<PaymentDetails> {
		self.payment_store.get(payment_hash)
	}

	/// Remove the payment with the given hash from the store.
	pub fn remove_payment(&self, payment_hash: &PaymentHash) -> Result<(), Error> {
		self.payment_store.remove(&payment_hash)
	}

	/// Retrieves all payments that match the given predicate.
	///
	/// For example, you could retrieve all stored outbound payments as follows:
	/// ```
	/// # use ldk_node::{Builder, Config, PaymentDirection};
	/// # use ldk_node::bitcoin::Network;
	/// # let mut config = Config::default();
	/// # config.network = Network::Regtest;
	/// # config.storage_dir_path = "/tmp/ldk_node_test/".to_string();
	/// # let builder = Builder::from_config(config);
	/// # let node = builder.build().unwrap();
	/// node.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound);
	/// ```
	pub fn list_payments_with_filter<F: FnMut(&&PaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<PaymentDetails> {
		self.payment_store.list_filter(f)
	}

	/// Retrieves all payments.
	pub fn list_payments(&self) -> Vec<PaymentDetails> {
		self.payment_store.list_filter(|_| true)
	}

	/// Retrieves a list of known peers.
	pub fn list_peers(&self) -> Vec<PeerDetails> {
		let mut peers = Vec::new();

		// First add all connected peers, preferring to list the connected address if available.
		let connected_peers = self.peer_manager.get_peer_node_ids();
		let connected_peers_len = connected_peers.len();
		for (node_id, con_addr_opt) in connected_peers {
			let stored_peer = self.peer_store.get_peer(&node_id);
			let stored_addr_opt = stored_peer.as_ref().map(|p| p.address.clone());
			let address = match (con_addr_opt, stored_addr_opt) {
				(Some(con_addr), _) => con_addr,
				(None, Some(stored_addr)) => stored_addr,
				(None, None) => continue,
			};

			let is_persisted = stored_peer.is_some();
			let is_connected = true;
			let details = PeerDetails { node_id, address, is_persisted, is_connected };
			peers.push(details);
		}

		// Now add all known-but-offline peers, too.
		for p in self.peer_store.list_peers() {
			if peers.iter().take(connected_peers_len).find(|d| d.node_id == p.node_id).is_some() {
				continue;
			}

			let details = PeerDetails {
				node_id: p.node_id,
				address: p.address,
				is_persisted: true,
				is_connected: false,
			};

			peers.push(details);
		}

		peers
	}

	/// Creates a digital ECDSA signature of a message with the node's secret key.
	///
	/// A receiver knowing the corresponding `PublicKey` (e.g. the node’s id) and the message
	/// can be sure that the signature was generated by the caller.
	/// Signatures are EC recoverable, meaning that given the message and the
	/// signature the `PublicKey` of the signer can be extracted.
	pub fn sign_message(&self, msg: &[u8]) -> Result<String, Error> {
		self.keys_manager.sign_message(msg)
	}

	/// Verifies that the given ECDSA signature was created for the given message with the
	/// secret key corresponding to the given public key.
	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		self.keys_manager.verify_signature(msg, sig, pkey)
	}
}

impl<K: KVStore + Sync + Send + 'static> Drop for Node<K> {
	fn drop(&mut self) {
		let _ = self.stop();
	}
}

async fn connect_peer_if_necessary<K: KVStore + Sync + Send + 'static>(
	node_id: PublicKey, addr: SocketAddress, peer_manager: Arc<PeerManager<K>>,
	logger: Arc<FilesystemLogger>,
) -> Result<(), Error> {
	for (pman_node_id, _pman_addr) in peer_manager.get_peer_node_ids() {
		if node_id == pman_node_id {
			return Ok(());
		}
	}

	do_connect_peer(node_id, addr, peer_manager, logger).await
}

async fn do_connect_peer<K: KVStore + Sync + Send + 'static>(
	node_id: PublicKey, addr: SocketAddress, peer_manager: Arc<PeerManager<K>>,
	logger: Arc<FilesystemLogger>,
) -> Result<(), Error> {
	log_info!(logger, "Connecting to peer: {}@{}", node_id, addr);

	let socket_addr = addr
		.to_socket_addrs()
		.map_err(|e| {
			log_error!(logger, "Failed to resolve network address: {}", e);
			Error::InvalidSocketAddress
		})?
		.next()
		.ok_or(Error::ConnectionFailed)?;

	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), node_id, socket_addr)
		.await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				match futures::poll!(&mut connection_closed_future) {
					std::task::Poll::Ready(_) => {
						log_info!(logger, "Peer connection closed: {}@{}", node_id, addr);
						return Err(Error::ConnectionFailed);
					}
					std::task::Poll::Pending => {}
				}
				// Avoid blocking the tokio context by sleeping a bit
				match peer_manager.get_peer_node_ids().iter().find(|(id, _addr)| *id == node_id) {
					Some(_) => return Ok(()),
					None => tokio::time::sleep(Duration::from_millis(10)).await,
				}
			}
		}
		None => {
			log_error!(logger, "Failed to connect to peer: {}@{}", node_id, addr);
			Err(Error::ConnectionFailed)
		}
	}
}
