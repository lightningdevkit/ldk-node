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
//! [`send`], etc.:
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
//! 	let funding_address = node.onchain_payment().new_address();
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
//! 	node.bolt11_payment().send(&invoice, None).unwrap();
//!
//! 	node.stop().unwrap();
//! }
//! ```
//!
//! [`build`]: Builder::build
//! [`start`]: Node::start
//! [`stop`]: Node::stop
//! [`connect_open_channel`]: Node::connect_open_channel
//! [`send`]: Bolt11Payment::send
//!
#![cfg_attr(not(feature = "uniffi"), deny(missing_docs))]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![allow(bare_trait_objects)]
#![allow(ellipsis_inclusive_range_patterns)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod balance;
mod builder;
mod config;
mod connection;
mod error;
mod event;
mod fee_estimator;
mod gossip;
pub mod graph;
mod hex_utils;
pub mod io;
mod liquidity;
mod logger;
mod message_handler;
pub mod payment;
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

pub use balance::{BalanceDetails, LightningBalance, PendingSweepBalance};
pub use config::{default_config, AnchorChannelsConfig, Config};
pub use error::Error as NodeError;
use error::Error;

pub use event::Event;
pub use types::{ChannelConfig, MaxDustHTLCExposure};

pub use io::utils::generate_entropy_mnemonic;

#[cfg(feature = "uniffi")]
use uniffi_types::*;

#[cfg(feature = "uniffi")]
pub use builder::ArcedNodeBuilder as Builder;
pub use builder::BuildError;
#[cfg(not(feature = "uniffi"))]
pub use builder::NodeBuilder as Builder;

use config::{
	default_user_config, LDK_WALLET_SYNC_TIMEOUT_SECS, NODE_ANN_BCAST_INTERVAL,
	PEER_RECONNECTION_INTERVAL, RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL, RGS_SYNC_INTERVAL,
	WALLET_SYNC_INTERVAL_MINIMUM_SECS,
};
use connection::ConnectionManager;
use event::{EventHandler, EventQueue};
use gossip::GossipSource;
use graph::NetworkGraph;
use liquidity::LiquiditySource;
use payment::store::PaymentStore;
use payment::{
	Bolt11Payment, Bolt12Payment, OnchainPayment, PaymentDetails, SpontaneousPayment,
	UnifiedQrPayment,
};
use peer_store::{PeerInfo, PeerStore};
use types::{
	Broadcaster, BumpTransactionEventHandler, ChainMonitor, ChannelManager, DynStore, FeeEstimator,
	Graph, KeysManager, PeerManager, Router, Scorer, Sweeper, Wallet,
};
pub use types::{ChannelDetails, PeerDetails, UserChannelId};

use logger::{log_error, log_info, log_trace, FilesystemLogger, Logger};

use lightning::chain::{BestBlock, Confirm};
use lightning::events::bump_transaction::Wallet as LdkWallet;
use lightning::ln::channelmanager::{ChannelShutdownState, PaymentId};
use lightning::ln::msgs::SocketAddress;

pub use lightning::util::logger::Level as LogLevel;

use lightning_background_processor::process_events_async;

use lightning_transaction_sync::EsploraSyncClient;

use bitcoin::secp256k1::PublicKey;

use rand::Rng;

use std::default::Default;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "uniffi")]
uniffi::include_scaffolding!("ldk_node");

/// The main interface object of LDK Node, wrapping the necessary LDK and BDK functionalities.
///
/// Needs to be initialized and instantiated through [`Builder::build`].
pub struct Node {
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	stop_sender: tokio::sync::watch::Sender<()>,
	event_handling_stopped_sender: tokio::sync::watch::Sender<()>,
	config: Arc<Config>,
	wallet: Arc<Wallet>,
	tx_sync: Arc<EsploraSyncClient<Arc<FilesystemLogger>>>,
	tx_broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<FeeEstimator>,
	event_queue: Arc<EventQueue<Arc<FilesystemLogger>>>,
	channel_manager: Arc<ChannelManager>,
	chain_monitor: Arc<ChainMonitor>,
	output_sweeper: Arc<Sweeper>,
	peer_manager: Arc<PeerManager>,
	connection_manager: Arc<ConnectionManager<Arc<FilesystemLogger>>>,
	keys_manager: Arc<KeysManager>,
	network_graph: Arc<Graph>,
	gossip_source: Arc<GossipSource>,
	liquidity_source: Option<Arc<LiquiditySource<Arc<FilesystemLogger>>>>,
	kv_store: Arc<DynStore>,
	logger: Arc<FilesystemLogger>,
	_router: Arc<Router>,
	scorer: Arc<Mutex<Scorer>>,
	peer_store: Arc<PeerStore<Arc<FilesystemLogger>>>,
	payment_store: Arc<PaymentStore<Arc<FilesystemLogger>>>,
	is_listening: Arc<AtomicBool>,
	latest_wallet_sync_timestamp: Arc<RwLock<Option<u64>>>,
	latest_onchain_wallet_sync_timestamp: Arc<RwLock<Option<u64>>>,
	latest_fee_rate_cache_update_timestamp: Arc<RwLock<Option<u64>>>,
	latest_rgs_snapshot_timestamp: Arc<RwLock<Option<u64>>>,
	latest_node_announcement_broadcast_timestamp: Arc<RwLock<Option<u64>>>,
	latest_channel_monitor_archival_height: Arc<RwLock<Option<u32>>>,
}

impl Node {
	/// Starts the necessary background tasks, such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network.
	///
	/// After this returns, the [`Node`] instance can be controlled via the provided API methods in
	/// a thread-safe manner.
	pub fn start(&self) -> Result<(), Error> {
		let runtime =
			Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap());
		self.start_with_runtime(runtime)
	}

	/// Starts the necessary background tasks (such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network) on the the given `runtime`.
	///
	/// This allows to have LDK Node reuse an outer pre-existing runtime, e.g., to avoid stacking Tokio
	/// runtime contexts.
	///
	/// After this returns, the [`Node`] instance can be controlled via the provided API methods in
	/// a thread-safe manner.
	pub fn start_with_runtime(&self, runtime: Arc<tokio::runtime::Runtime>) -> Result<(), Error> {
		// Acquire a run lock and hold it until we're setup.
		let mut runtime_lock = self.runtime.write().unwrap();
		if runtime_lock.is_some() {
			// We're already running.
			return Err(Error::AlreadyRunning);
		}

		log_info!(
			self.logger,
			"Starting up LDK Node with node ID {} on network: {}",
			self.node_id(),
			self.config.network
		);

		// Block to ensure we update our fee rate cache once on startup
		let fee_estimator = Arc::clone(&self.fee_estimator);
		let sync_logger = Arc::clone(&self.logger);
		let sync_fee_rate_update_timestamp =
			Arc::clone(&self.latest_fee_rate_cache_update_timestamp);
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
						let unix_time_secs_opt =
							SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
						*sync_fee_rate_update_timestamp.write().unwrap() = unix_time_secs_opt;
						Ok(())
					},
					Err(e) => {
						log_error!(sync_logger, "Initial fee rate cache update failed: {}", e,);
						Err(e)
					},
				}
			})
		})?;

		// Setup wallet sync
		let wallet = Arc::clone(&self.wallet);
		let sync_logger = Arc::clone(&self.logger);
		let sync_onchain_wallet_timestamp = Arc::clone(&self.latest_onchain_wallet_sync_timestamp);
		let mut stop_sync = self.stop_sender.subscribe();
		let onchain_wallet_sync_interval_secs = self
			.config
			.onchain_wallet_sync_interval_secs
			.max(config::WALLET_SYNC_INTERVAL_MINIMUM_SECS);
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
								log_trace!(
									sync_logger,
									"Stopping background syncing on-chain wallet.",
									);
								return;
							}
							_ = onchain_wallet_sync_interval.tick() => {
								let now = Instant::now();
								match wallet.sync().await {
									Ok(()) => {
										log_trace!(
										sync_logger,
										"Background sync of on-chain wallet finished in {}ms.",
										now.elapsed().as_millis()
										);
										let unix_time_secs_opt =
											SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
										*sync_onchain_wallet_timestamp.write().unwrap() = unix_time_secs_opt;
									}
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

		let mut stop_fee_updates = self.stop_sender.subscribe();
		let fee_update_logger = Arc::clone(&self.logger);
		let fee_update_timestamp = Arc::clone(&self.latest_fee_rate_cache_update_timestamp);
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
						log_trace!(
							fee_update_logger,
							"Stopping background updates of fee rate cache.",
						);
						return;
					}
					_ = fee_rate_update_interval.tick() => {
						let now = Instant::now();
						match fee_estimator.update_fee_estimates().await {
							Ok(()) => {
								log_trace!(
								fee_update_logger,
								"Background update of fee rate cache finished in {}ms.",
								now.elapsed().as_millis()
								);
								let unix_time_secs_opt =
									SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
								*fee_update_timestamp.write().unwrap() = unix_time_secs_opt;
							}
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
		let archive_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let archive_cmon = Arc::clone(&self.chain_monitor);
		let sync_sweeper = Arc::clone(&self.output_sweeper);
		let sync_logger = Arc::clone(&self.logger);
		let sync_wallet_timestamp = Arc::clone(&self.latest_wallet_sync_timestamp);
		let sync_monitor_archival_height = Arc::clone(&self.latest_channel_monitor_archival_height);
		let mut stop_sync = self.stop_sender.subscribe();
		let wallet_sync_interval_secs =
			self.config.wallet_sync_interval_secs.max(WALLET_SYNC_INTERVAL_MINIMUM_SECS);
		runtime.spawn(async move {
			let mut wallet_sync_interval =
				tokio::time::interval(Duration::from_secs(wallet_sync_interval_secs));
			wallet_sync_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					_ = stop_sync.changed() => {
						log_trace!(
							sync_logger,
							"Stopping background syncing Lightning wallet.",
						);
						return;
					}
					_ = wallet_sync_interval.tick() => {
						let confirmables = vec![
							&*sync_cman as &(dyn Confirm + Sync + Send),
							&*sync_cmon as &(dyn Confirm + Sync + Send),
							&*sync_sweeper as &(dyn Confirm + Sync + Send),
						];
						let now = Instant::now();
						let timeout_fut = tokio::time::timeout(Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS), tx_sync.sync(confirmables));
						match timeout_fut.await {
							Ok(res) => match res {
								Ok(()) => {
									log_trace!(
										sync_logger,
										"Background sync of Lightning wallet finished in {}ms.",
										now.elapsed().as_millis()
										);
									let unix_time_secs_opt =
										SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
									*sync_wallet_timestamp.write().unwrap() = unix_time_secs_opt;

									periodically_archive_fully_resolved_monitors(
										Arc::clone(&archive_cman),
										Arc::clone(&archive_cmon),
										Arc::clone(&sync_monitor_archival_height)
									);
								}
								Err(e) => {
									log_error!(sync_logger, "Background sync of Lightning wallet failed: {}", e)
								}
							}
							Err(e) => {
								log_error!(sync_logger, "Background sync of Lightning wallet timed out: {}", e)
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
			let gossip_rgs_sync_timestamp = Arc::clone(&self.latest_rgs_snapshot_timestamp);
			let mut stop_gossip_sync = self.stop_sender.subscribe();
			runtime.spawn(async move {
				let mut interval = tokio::time::interval(RGS_SYNC_INTERVAL);
				loop {
					tokio::select! {
						_ = stop_gossip_sync.changed() => {
							log_trace!(
								gossip_sync_logger,
								"Stopping background syncing RGS gossip data.",
							);
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
									*gossip_rgs_sync_timestamp.write().unwrap() = Some(updated_timestamp as u64);
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
			let mut stop_listen = self.stop_sender.subscribe();
			let listening_logger = Arc::clone(&self.logger);
			let listening_indicator = Arc::clone(&self.is_listening);

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
				{
				let listener =
					tokio::net::TcpListener::bind(&*bind_addrs).await
										.unwrap_or_else(|e| {
											log_error!(listening_logger, "Failed to bind to listen addresses/ports - is something else already listening on it?: {}", e);
											panic!(
												"Failed to bind to listen address/port - is something else already listening on it?",
												);
										});

				listening_indicator.store(true, Ordering::Release);

				loop {
					let peer_mgr = Arc::clone(&peer_manager_connection_handler);
					tokio::select! {
						_ = stop_listen.changed() => {
							log_trace!(
								listening_logger,
								"Stopping listening to inbound connections.",
							);
							break;
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
				}

				listening_indicator.store(false, Ordering::Release);
			});
		}

		// Regularly reconnect to persisted peers.
		let connect_cm = Arc::clone(&self.connection_manager);
		let connect_pm = Arc::clone(&self.peer_manager);
		let connect_logger = Arc::clone(&self.logger);
		let connect_peer_store = Arc::clone(&self.peer_store);
		let mut stop_connect = self.stop_sender.subscribe();
		runtime.spawn(async move {
			let mut interval = tokio::time::interval(PEER_RECONNECTION_INTERVAL);
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
						_ = stop_connect.changed() => {
							log_trace!(
								connect_logger,
								"Stopping reconnecting known peers.",
							);
							return;
						}
						_ = interval.tick() => {
							let pm_peers = connect_pm
								.list_peers()
								.iter()
								.map(|peer| peer.counterparty_node_id)
								.collect::<Vec<_>>();

							for peer_info in connect_peer_store.list_peers().iter().filter(|info| !pm_peers.contains(&info.node_id)) {
								let res = connect_cm.do_connect_peer(
									peer_info.node_id,
									peer_info.address.clone(),
									).await;
								match res {
									Ok(_) => {
										log_info!(connect_logger, "Successfully reconnected to peer {}", peer_info.node_id);
									},
									Err(e) => {
										log_error!(connect_logger, "Failed to reconnect to peer {}: {}", peer_info.node_id, e);
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
		let bcast_ann_timestamp = Arc::clone(&self.latest_node_announcement_broadcast_timestamp);
		let mut stop_bcast = self.stop_sender.subscribe();
		runtime.spawn(async move {
			// We check every 30 secs whether our last broadcast is NODE_ANN_BCAST_INTERVAL away.
			#[cfg(not(test))]
			let mut interval = tokio::time::interval(Duration::from_secs(30));
			#[cfg(test)]
			let mut interval = tokio::time::interval(Duration::from_secs(5));
			loop {
				tokio::select! {
						_ = stop_bcast.changed() => {
							log_trace!(
								bcast_logger,
								"Stopping broadcasting node announcements.",
							);
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

							if !bcast_cm.list_channels().iter().any(|chan| chan.is_public && chan.is_channel_ready) {
								// Skip if we don't have any public channels that are ready.
								continue;
							}

							if bcast_pm.list_peers().is_empty() {
								// Skip if we don't have any connected peers to gossip to.
								continue;
							}

							let addresses = bcast_config.listening_addresses.clone().unwrap_or(Vec::new());

							if addresses.is_empty() {
								// Skip if we are not listening on any addresses.
								continue;
							}

							bcast_pm.broadcast_node_announcement([0; 3], [0; 32], addresses);

							let unix_time_secs_opt =
								SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
							*bcast_ann_timestamp.write().unwrap() = unix_time_secs_opt;

							if let Some(unix_time_secs) = unix_time_secs_opt {
								io::utils::write_latest_node_ann_bcast_timestamp(unix_time_secs, Arc::clone(&bcast_store), Arc::clone(&bcast_logger))
									.unwrap_or_else(|e| {
										log_error!(bcast_logger, "Persistence failed: {}", e);
										panic!("Persistence failed");
									});
							}
						}
				}
			}
		});

		let mut stop_tx_bcast = self.stop_sender.subscribe();
		let tx_bcaster = Arc::clone(&self.tx_broadcaster);
		let tx_bcast_logger = Arc::clone(&self.logger);
		runtime.spawn(async move {
			// Every second we try to clear our broadcasting queue.
			let mut interval = tokio::time::interval(Duration::from_secs(1));
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
						_ = stop_tx_bcast.changed() => {
							log_trace!(
								tx_bcast_logger,
								"Stopping broadcasting transactions.",
							);
							return;
						}
						_ = interval.tick() => {
							tx_bcaster.process_queue().await;
						}
				}
			}
		});

		let bump_tx_event_handler = Arc::new(BumpTransactionEventHandler::new(
			Arc::clone(&self.tx_broadcaster),
			Arc::new(LdkWallet::new(Arc::clone(&self.wallet), Arc::clone(&self.logger))),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.logger),
		));

		let event_handler = Arc::new(EventHandler::new(
			Arc::clone(&self.event_queue),
			Arc::clone(&self.wallet),
			bump_tx_event_handler,
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.connection_manager),
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
		let stop_bp = self.stop_sender.subscribe();
		let sleeper_logger = Arc::clone(&self.logger);
		let sleeper = move |d| {
			let mut stop = stop_bp.clone();
			let sleeper_logger = Arc::clone(&sleeper_logger);
			Box::pin(async move {
				tokio::select! {
					_ = stop.changed() => {
						log_trace!(
							sleeper_logger,
							"Stopping processing events.",
						);
						true
					}
					_ = tokio::time::sleep(d) => {
						false
					}
				}
			})
		};

		let background_stop_logger = Arc::clone(&self.logger);
		let event_handling_stopped_sender = self.event_handling_stopped_sender.clone();
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
			log_trace!(background_stop_logger, "Events processing stopped.",);

			match event_handling_stopped_sender.send(()) {
				Ok(_) => (),
				Err(e) => {
					log_error!(
						background_stop_logger,
						"Failed to send 'events handling stopped' signal. This should never happen: {}",
						e
						);
					debug_assert!(false);
				},
			}
		});

		if let Some(liquidity_source) = self.liquidity_source.as_ref() {
			let mut stop_liquidity_handler = self.stop_sender.subscribe();
			let liquidity_handler = Arc::clone(&liquidity_source);
			let liquidity_logger = Arc::clone(&self.logger);
			runtime.spawn(async move {
				loop {
					tokio::select! {
						_ = stop_liquidity_handler.changed() => {
							log_trace!(
								liquidity_logger,
								"Stopping processing liquidity events.",
							);
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

	/// Disconnects all peers, stops all running background tasks, and shuts down [`Node`].
	///
	/// After this returns most API methods will return [`Error::NotRunning`].
	pub fn stop(&self) -> Result<(), Error> {
		let runtime = self.runtime.write().unwrap().take().ok_or(Error::NotRunning)?;

		log_info!(self.logger, "Shutting down LDK Node with node ID {}...", self.node_id());

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
			},
		}

		// Disconnect all peers.
		self.peer_manager.disconnect_all_peers();

		// Wait until event handling stopped, at least until a timeout is reached.
		let event_handling_stopped_logger = Arc::clone(&self.logger);
		let mut event_handling_stopped_receiver = self.event_handling_stopped_sender.subscribe();

		// FIXME: For now, we wait up to 100 secs (BDK_WALLET_SYNC_TIMEOUT_SECS + 10) to allow
		// event handling to exit gracefully even if it was blocked on the BDK wallet syncing. We
		// should drop this considerably post upgrading to BDK 1.0.
		let timeout_res = runtime.block_on(async {
			tokio::time::timeout(
				Duration::from_secs(100),
				event_handling_stopped_receiver.changed(),
			)
			.await
		});

		match timeout_res {
			Ok(stop_res) => match stop_res {
				Ok(()) => {},
				Err(e) => {
					log_error!(
						event_handling_stopped_logger,
						"Stopping event handling failed. This should never happen: {}",
						e
					);
					panic!("Stopping event handling failed. This should never happen.");
				},
			},
			Err(e) => {
				log_error!(
					event_handling_stopped_logger,
					"Stopping event handling timed out: {}",
					e
				);
			},
		}

		#[cfg(tokio_unstable)]
		{
			log_trace!(
				self.logger,
				"Active runtime tasks left prior to shutdown: {}",
				runtime.metrics().active_tasks_count()
			);
		}

		log_info!(self.logger, "Shutdown complete.");
		Ok(())
	}

	/// Returns the status of the [`Node`].
	pub fn status(&self) -> NodeStatus {
		let is_running = self.runtime.read().unwrap().is_some();
		let is_listening = self.is_listening.load(Ordering::Acquire);
		let current_best_block = self.channel_manager.current_best_block().into();
		let latest_wallet_sync_timestamp = *self.latest_wallet_sync_timestamp.read().unwrap();
		let latest_onchain_wallet_sync_timestamp =
			*self.latest_onchain_wallet_sync_timestamp.read().unwrap();
		let latest_fee_rate_cache_update_timestamp =
			*self.latest_fee_rate_cache_update_timestamp.read().unwrap();
		let latest_rgs_snapshot_timestamp = *self.latest_rgs_snapshot_timestamp.read().unwrap();
		let latest_node_announcement_broadcast_timestamp =
			*self.latest_node_announcement_broadcast_timestamp.read().unwrap();

		NodeStatus {
			is_running,
			is_listening,
			current_best_block,
			latest_wallet_sync_timestamp,
			latest_onchain_wallet_sync_timestamp,
			latest_fee_rate_cache_update_timestamp,
			latest_rgs_snapshot_timestamp,
			latest_node_announcement_broadcast_timestamp,
		}
	}

	/// Returns the config with which the [`Node`] was initialized.
	pub fn config(&self) -> Config {
		self.config.as_ref().clone()
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

	/// Returns a payment handler allowing to create and pay [BOLT 11] invoices.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	#[cfg(not(feature = "uniffi"))]
	pub fn bolt11_payment(&self) -> Bolt11Payment {
		Bolt11Payment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.connection_manager),
			Arc::clone(&self.keys_manager),
			self.liquidity_source.clone(),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.peer_store),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to create and pay [BOLT 11] invoices.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	#[cfg(feature = "uniffi")]
	pub fn bolt11_payment(&self) -> Arc<Bolt11Payment> {
		Arc::new(Bolt11Payment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.connection_manager),
			Arc::clone(&self.keys_manager),
			self.liquidity_source.clone(),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.peer_store),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		))
	}

	/// Returns a payment handler allowing to create and pay [BOLT 12] offers and refunds.
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	#[cfg(not(feature = "uniffi"))]
	pub fn bolt12_payment(&self) -> Bolt12Payment {
		Bolt12Payment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to create and pay [BOLT 12] offers and refunds.
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	#[cfg(feature = "uniffi")]
	pub fn bolt12_payment(&self) -> Arc<Bolt12Payment> {
		Arc::new(Bolt12Payment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.logger),
		))
	}

	/// Returns a payment handler allowing to send spontaneous ("keysend") payments.
	#[cfg(not(feature = "uniffi"))]
	pub fn spontaneous_payment(&self) -> SpontaneousPayment {
		SpontaneousPayment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to send spontaneous ("keysend") payments.
	#[cfg(feature = "uniffi")]
	pub fn spontaneous_payment(&self) -> Arc<SpontaneousPayment> {
		Arc::new(SpontaneousPayment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		))
	}

	/// Returns a payment handler allowing to send and receive on-chain payments.
	#[cfg(not(feature = "uniffi"))]
	pub fn onchain_payment(&self) -> OnchainPayment {
		OnchainPayment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.wallet),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to send and receive on-chain payments.
	#[cfg(feature = "uniffi")]
	pub fn onchain_payment(&self) -> Arc<OnchainPayment> {
		Arc::new(OnchainPayment::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.wallet),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		))
	}

	/// Returns a payment handler allowing to create [BIP 21] URIs with an on-chain, [BOLT 11],
	/// and [BOLT 12] payment options.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	#[cfg(not(feature = "uniffi"))]
	pub fn unified_qr_payment(&self) -> UnifiedQrPayment {
		UnifiedQrPayment::new(
			self.onchain_payment().into(),
			self.bolt11_payment().into(),
			self.bolt12_payment().into(),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to create [BIP 21] URIs with an on-chain, [BOLT 11],
	/// and [BOLT 12] payment options.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	#[cfg(feature = "uniffi")]
	pub fn unified_qr_payment(&self) -> Arc<UnifiedQrPayment> {
		Arc::new(UnifiedQrPayment::new(
			self.onchain_payment(),
			self.bolt11_payment(),
			self.bolt12_payment(),
			Arc::clone(&self.config),
			Arc::clone(&self.logger),
		))
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
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
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
			Ok(()) => {},
			Err(e) => {
				log_error!(self.logger, "Failed to remove peer {}: {}", counterparty_node_id, e)
			},
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
	/// If Anchor channels are enabled, this will ensure the configured
	/// [`AnchorChannelsConfig::per_channel_reserve_sats`] is available and will be retained before
	/// opening the channel.
	///
	/// Returns a [`UserChannelId`] allowing to locally keep track of the channel.
	pub fn connect_open_channel(
		&self, node_id: PublicKey, address: SocketAddress, channel_amount_sats: u64,
		push_to_counterparty_msat: Option<u64>, channel_config: Option<ChannelConfig>,
		announce_channel: bool,
	) -> Result<UserChannelId, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}
		let runtime = rt_lock.as_ref().unwrap();

		let peer_info = PeerInfo { node_id, address };

		let con_node_id = peer_info.node_id;
		let con_addr = peer_info.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		let cur_anchor_reserve_sats =
			total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let spendable_amount_sats =
			self.wallet.get_spendable_amount_sats(cur_anchor_reserve_sats).unwrap_or(0);

		// Fail early if we have less than the channel value available.
		if spendable_amount_sats < channel_amount_sats {
			log_error!(self.logger,
				"Unable to create channel due to insufficient funds. Available: {}sats, Required: {}sats",
				spendable_amount_sats, channel_amount_sats
			);
			return Err(Error::InsufficientFunds);
		}

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		tokio::task::block_in_place(move || {
			runtime.block_on(async move {
				con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
			})
		})?;

		// Fail if we have less than the channel value + anchor reserve available (if applicable).
		let init_features = self
			.peer_manager
			.peer_by_node_id(&node_id)
			.ok_or(Error::ConnectionFailed)?
			.init_features;
		let required_funds_sats = channel_amount_sats
			+ self.config.anchor_channels_config.as_ref().map_or(0, |c| {
				if init_features.requires_anchors_zero_fee_htlc_tx()
					&& !c.trusted_peers_no_reserve.contains(&node_id)
				{
					c.per_channel_reserve_sats
				} else {
					0
				}
			});

		if spendable_amount_sats < required_funds_sats {
			log_error!(self.logger,
				"Unable to create channel due to insufficient funds. Available: {}sats, Required: {}sats",
				spendable_amount_sats, required_funds_sats
			);
			return Err(Error::InsufficientFunds);
		}

		let mut user_config = default_user_config(&self.config);
		user_config.channel_handshake_config.announced_channel = announce_channel;
		user_config.channel_config = (channel_config.unwrap_or_default()).clone().into();
		// We set the max inflight to 100% for private channels.
		// FIXME: LDK will default to this behavior soon, too, at which point we should drop this
		// manual override.
		if !announce_channel {
			user_config
				.channel_handshake_config
				.max_inbound_htlc_value_in_flight_percent_of_channel = 100;
		}

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
			},
			Err(e) => {
				log_error!(self.logger, "Failed to initiate channel creation: {:?}", e);
				Err(Error::ChannelCreationFailed)
			},
		}
	}

	/// Manually sync the LDK and BDK wallets with the current chain state and update the fee rate
	/// cache.
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
		let archive_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let archive_cmon = Arc::clone(&self.chain_monitor);
		let fee_estimator = Arc::clone(&self.fee_estimator);
		let sync_sweeper = Arc::clone(&self.output_sweeper);
		let sync_logger = Arc::clone(&self.logger);
		let confirmables = vec![
			&*sync_cman as &(dyn Confirm + Sync + Send),
			&*sync_cmon as &(dyn Confirm + Sync + Send),
			&*sync_sweeper as &(dyn Confirm + Sync + Send),
		];
		let sync_wallet_timestamp = Arc::clone(&self.latest_wallet_sync_timestamp);
		let sync_fee_rate_update_timestamp =
			Arc::clone(&self.latest_fee_rate_cache_update_timestamp);
		let sync_onchain_wallet_timestamp = Arc::clone(&self.latest_onchain_wallet_sync_timestamp);
		let sync_monitor_archival_height = Arc::clone(&self.latest_channel_monitor_archival_height);

		tokio::task::block_in_place(move || {
			tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(
				async move {
					let now = Instant::now();
					// We don't add an additional timeout here, as `Wallet::sync` already returns
					// after a timeout.
					match wallet.sync().await {
						Ok(()) => {
							log_info!(
								sync_logger,
								"Sync of on-chain wallet finished in {}ms.",
								now.elapsed().as_millis()
							);
							let unix_time_secs_opt = SystemTime::now()
								.duration_since(UNIX_EPOCH)
								.ok()
								.map(|d| d.as_secs());
							*sync_onchain_wallet_timestamp.write().unwrap() = unix_time_secs_opt;
						},
						Err(e) => {
							log_error!(sync_logger, "Sync of on-chain wallet failed: {}", e);
							return Err(e);
						},
					};

					let now = Instant::now();
					// We don't add an additional timeout here, as
					// `FeeEstimator::update_fee_estimates` already returns after a timeout.
					match fee_estimator.update_fee_estimates().await {
						Ok(()) => {
							log_info!(
								sync_logger,
								"Fee rate cache update finished in {}ms.",
								now.elapsed().as_millis()
							);
							let unix_time_secs_opt = SystemTime::now()
								.duration_since(UNIX_EPOCH)
								.ok()
								.map(|d| d.as_secs());
							*sync_fee_rate_update_timestamp.write().unwrap() = unix_time_secs_opt;
						},
						Err(e) => {
							log_error!(sync_logger, "Fee rate cache update failed: {}", e,);
							return Err(e);
						},
					}

					let now = Instant::now();
					let tx_sync_timeout_fut = tokio::time::timeout(
						Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS),
						tx_sync.sync(confirmables),
					);
					match tx_sync_timeout_fut.await {
						Ok(res) => match res {
							Ok(()) => {
								log_info!(
									sync_logger,
									"Sync of Lightning wallet finished in {}ms.",
									now.elapsed().as_millis()
								);

								let unix_time_secs_opt = SystemTime::now()
									.duration_since(UNIX_EPOCH)
									.ok()
									.map(|d| d.as_secs());
								*sync_wallet_timestamp.write().unwrap() = unix_time_secs_opt;

								periodically_archive_fully_resolved_monitors(
									archive_cman,
									archive_cmon,
									sync_monitor_archival_height,
								);
								Ok(())
							},
							Err(e) => {
								log_error!(sync_logger, "Sync of Lightning wallet failed: {}", e);
								Err(e.into())
							},
						},
						Err(e) => {
							log_error!(sync_logger, "Sync of Lightning wallet timed out: {}", e);
							Err(Error::TxSyncTimeout)
						},
					}
				},
			)
		})
	}

	/// Close a previously opened channel.
	///
	/// Will attempt to close a channel coopertively. If this fails, users might need to resort to
	/// [`Node::force_close_channel`].
	pub fn close_channel(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
	) -> Result<(), Error> {
		self.close_channel_internal(user_channel_id, counterparty_node_id, false)
	}

	/// Force-close a previously opened channel.
	///
	/// Will force-close the channel, potentially broadcasting our latest state. Note that in
	/// contrast to cooperative closure, force-closing will have the channel funds time-locked,
	/// i.e., they will only be available after the counterparty had time to contest our claim.
	/// Force-closing channels also more costly in terms of on-chain fees. So cooperative closure
	/// should always be preferred (and tried first).
	///
	/// Broadcasting the closing transactions will be omitted for Anchor channels if we trust the
	/// counterparty to broadcast for us (see [`AnchorChannelsConfig::trusted_peers_no_reserve`]
	/// for more information).
	pub fn force_close_channel(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
	) -> Result<(), Error> {
		self.close_channel_internal(user_channel_id, counterparty_node_id, true)
	}

	fn close_channel_internal(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey, force: bool,
	) -> Result<(), Error> {
		let open_channels =
			self.channel_manager.list_channels_with_counterparty(&counterparty_node_id);
		if let Some(channel_details) =
			open_channels.iter().find(|c| c.user_channel_id == user_channel_id.0)
		{
			if force {
				if self.config.anchor_channels_config.as_ref().map_or(false, |acc| {
					acc.trusted_peers_no_reserve.contains(&counterparty_node_id)
				}) {
					self.channel_manager
						.force_close_without_broadcasting_txn(
							&channel_details.channel_id,
							&counterparty_node_id,
						)
						.map_err(|e| {
							log_error!(
								self.logger,
								"Failed to force-close channel to trusted peer: {:?}",
								e
							);
							Error::ChannelClosingFailed
						})?;
				} else {
					self.channel_manager
						.force_close_broadcasting_latest_txn(
							&channel_details.channel_id,
							&counterparty_node_id,
						)
						.map_err(|e| {
							log_error!(self.logger, "Failed to force-close channel: {:?}", e);
							Error::ChannelClosingFailed
						})?;
				}
			} else {
				self.channel_manager
					.close_channel(&channel_details.channel_id, &counterparty_node_id)
					.map_err(|e| {
						log_error!(self.logger, "Failed to close channel: {:?}", e);
						Error::ChannelClosingFailed
					})?;
			}

			// Check if this was the last open channel, if so, forget the peer.
			if open_channels.len() == 1 {
				self.peer_store.remove_peer(&counterparty_node_id)?;
			}
		}

		Ok(())
	}

	/// Update the config for a previously opened channel.
	pub fn update_channel_config(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
		channel_config: ChannelConfig,
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
					&(channel_config).clone().into(),
				)
				.map_err(|_| Error::ChannelConfigUpdateFailed)
		} else {
			Err(Error::ChannelConfigUpdateFailed)
		}
	}

	/// Retrieve the details of a specific payment with the given id.
	///
	/// Returns `Some` if the payment was known and `None` otherwise.
	pub fn payment(&self, payment_id: &PaymentId) -> Option<PaymentDetails> {
		self.payment_store.get(payment_id)
	}

	/// Remove the payment with the given id from the store.
	pub fn remove_payment(&self, payment_id: &PaymentId) -> Result<(), Error> {
		self.payment_store.remove(&payment_id)
	}

	/// Retrieves an overview of all known balances.
	pub fn list_balances(&self) -> BalanceDetails {
		let cur_anchor_reserve_sats =
			total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let (total_onchain_balance_sats, spendable_onchain_balance_sats) =
			self.wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

		let total_anchor_channels_reserve_sats =
			std::cmp::min(cur_anchor_reserve_sats, total_onchain_balance_sats);

		let mut total_lightning_balance_sats = 0;
		let mut lightning_balances = Vec::new();
		for (funding_txo, channel_id) in self.chain_monitor.list_monitors() {
			match self.chain_monitor.get_monitor(funding_txo) {
				Ok(monitor) => {
					// unwrap safety: `get_counterparty_node_id` will always be `Some` after 0.0.110 and
					// LDK Node 0.1 depended on 0.0.115 already.
					let counterparty_node_id = monitor.get_counterparty_node_id().unwrap();
					for ldk_balance in monitor.get_claimable_balances() {
						total_lightning_balance_sats += ldk_balance.claimable_amount_satoshis();
						lightning_balances.push(LightningBalance::from_ldk_balance(
							channel_id,
							counterparty_node_id,
							ldk_balance,
						));
					}
				},
				Err(()) => {
					continue;
				},
			}
		}

		let pending_balances_from_channel_closures = self
			.output_sweeper
			.tracked_spendable_outputs()
			.into_iter()
			.map(PendingSweepBalance::from_tracked_spendable_output)
			.collect();

		BalanceDetails {
			total_onchain_balance_sats,
			spendable_onchain_balance_sats,
			total_anchor_channels_reserve_sats,
			total_lightning_balance_sats,
			lightning_balances,
			pending_balances_from_channel_closures,
		}
	}

	/// Retrieves all payments that match the given predicate.
	///
	/// For example, you could retrieve all stored outbound payments as follows:
	/// ```
	/// # use ldk_node::{Builder, Config};
	/// # use ldk_node::payment::PaymentDirection;
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
		let connected_peers = self.peer_manager.list_peers();
		let connected_peers_len = connected_peers.len();
		for connected_peer in connected_peers {
			let node_id = connected_peer.counterparty_node_id;
			let stored_peer = self.peer_store.get_peer(&node_id);
			let stored_addr_opt = stored_peer.as_ref().map(|p| p.address.clone());
			let address = match (connected_peer.socket_address, stored_addr_opt) {
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
			if peers.iter().take(connected_peers_len).any(|d| d.node_id == p.node_id) {
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

	/// Returns a handler allowing to query the network graph.
	#[cfg(not(feature = "uniffi"))]
	pub fn network_graph(&self) -> NetworkGraph {
		NetworkGraph::new(Arc::clone(&self.network_graph))
	}

	/// Returns a handler allowing to query the network graph.
	#[cfg(feature = "uniffi")]
	pub fn network_graph(&self) -> Arc<NetworkGraph> {
		Arc::new(NetworkGraph::new(Arc::clone(&self.network_graph)))
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

impl Drop for Node {
	fn drop(&mut self) {
		let _ = self.stop();
	}
}

/// Represents the status of the [`Node`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeStatus {
	/// Indicates whether the [`Node`] is running.
	pub is_running: bool,
	/// Indicates whether the [`Node`] is listening for incoming connections on the addresses
	/// configured via [`Config::listening_addresses`].
	pub is_listening: bool,
	/// The best block to which our Lightning wallet is currently synced.
	pub current_best_block: BestBlock,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully synced
	/// our Lightning wallet to the chain tip.
	///
	/// Will be `None` if the wallet hasn't been synced since the [`Node`] was initialized.
	pub latest_wallet_sync_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully synced
	/// our on-chain wallet to the chain tip.
	///
	/// Will be `None` if the wallet hasn't been synced since the [`Node`] was initialized.
	pub latest_onchain_wallet_sync_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully update
	/// our fee rate cache.
	///
	/// Will be `None` if the cache hasn't been updated since the [`Node`] was initialized.
	pub latest_fee_rate_cache_update_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when the last rapid gossip sync
	/// (RGS) snapshot we successfully applied was generated.
	///
	/// Will be `None` if RGS isn't configured or the snapshot hasn't been updated since the [`Node`] was initialized.
	pub latest_rgs_snapshot_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last broadcasted a node
	/// announcement.
	///
	/// Will be `None` if we have no public channels or we haven't broadcasted since the [`Node`] was initialized.
	pub latest_node_announcement_broadcast_timestamp: Option<u64>,
}

pub(crate) fn total_anchor_channels_reserve_sats(
	channel_manager: &ChannelManager, config: &Config,
) -> u64 {
	config.anchor_channels_config.as_ref().map_or(0, |anchor_channels_config| {
		channel_manager
			.list_channels()
			.into_iter()
			.filter(|c| {
				!anchor_channels_config.trusted_peers_no_reserve.contains(&c.counterparty.node_id)
					&& c.channel_shutdown_state
						.map_or(true, |s| s != ChannelShutdownState::ShutdownComplete)
					&& c.channel_type
						.as_ref()
						.map_or(false, |t| t.requires_anchors_zero_fee_htlc_tx())
			})
			.count() as u64
			* anchor_channels_config.per_channel_reserve_sats
	})
}

fn periodically_archive_fully_resolved_monitors(
	channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
	latest_channel_monitor_archival_height: Arc<RwLock<Option<u32>>>,
) {
	let mut latest_archival_height_lock = latest_channel_monitor_archival_height.write().unwrap();
	let cur_height = channel_manager.current_best_block().height;
	let should_archive = latest_archival_height_lock
		.as_ref()
		.map_or(true, |h| cur_height >= h + RESOLVED_CHANNEL_MONITOR_ARCHIVAL_INTERVAL);

	if should_archive {
		chain_monitor.archive_fully_resolved_channel_monitors();
		*latest_archival_height_lock = Some(cur_height);
	}
}
