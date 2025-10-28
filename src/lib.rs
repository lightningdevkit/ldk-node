// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

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
//! controlled via commands such as [`start`], [`stop`], [`open_channel`], [`send`], etc.:
//!
//! ```no_run
//! # #[cfg(not(feature = "uniffi"))]
//! # {
//! use std::str::FromStr;
//!
//! use ldk_node::bitcoin::secp256k1::PublicKey;
//! use ldk_node::bitcoin::Network;
//! use ldk_node::lightning::ln::msgs::SocketAddress;
//! use ldk_node::lightning_invoice::Bolt11Invoice;
//! use ldk_node::Builder;
//!
//! fn main() {
//! 	let mut builder = Builder::new();
//! 	builder.set_network(Network::Testnet);
//! 	builder.set_chain_source_esplora("https://blockstream.info/testnet/api".to_string(), None);
//! 	builder.set_gossip_source_rgs(
//! 		"https://rapidsync.lightningdevkit.org/testnet/snapshot".to_string(),
//! 	);
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
//! 	node.open_channel(node_id, node_addr, 10000, None, None).unwrap();
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
//! # }
//! ```
//!
//! [`build`]: Builder::build
//! [`start`]: Node::start
//! [`stop`]: Node::stop
//! [`open_channel`]: Node::open_channel
//! [`send`]: Bolt11Payment::send
#![cfg_attr(not(feature = "uniffi"), deny(missing_docs))]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![allow(bare_trait_objects)]
#![allow(ellipsis_inclusive_range_patterns)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod balance;
mod builder;
mod chain;
pub mod config;
mod connection;
mod data_store;
mod error;
mod event;
mod fee_estimator;
mod ffi;
mod gossip;
pub mod graph;
mod hex_utils;
pub mod io;
pub mod liquidity;
pub mod logger;
mod message_handler;
pub mod payment;
mod peer_store;
mod runtime;
mod scoring;
mod tx_broadcaster;
mod types;
mod wallet;

use std::default::Default;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::scoring::setup_background_pathfinding_scores_sync;
pub use balance::{BalanceDetails, LightningBalance, PendingSweepBalance};
use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
#[cfg(feature = "uniffi")]
pub use builder::ArcedNodeBuilder as Builder;
pub use builder::BuildError;
#[cfg(not(feature = "uniffi"))]
pub use builder::NodeBuilder as Builder;
use chain::ChainSource;
use config::{
	default_user_config, may_announce_channel, AsyncPaymentsRole, ChannelConfig, Config,
	NODE_ANN_BCAST_INTERVAL, PEER_RECONNECTION_INTERVAL, RGS_SYNC_INTERVAL,
};
use connection::ConnectionManager;
pub use error::Error as NodeError;
use error::Error;
pub use event::Event;
use event::{EventHandler, EventQueue};
#[cfg(feature = "uniffi")]
use ffi::*;
use gossip::GossipSource;
use graph::NetworkGraph;
pub use io::utils::generate_entropy_mnemonic;
use io::utils::write_node_metrics;
use lightning::chain::BestBlock;
use lightning::events::bump_transaction::{Input, Wallet as LdkWallet};
use lightning::impl_writeable_tlv_based;
use lightning::ln::chan_utils::{make_funding_redeemscript, FUNDING_TRANSACTION_WITNESS_WEIGHT};
use lightning::ln::channel_state::ChannelShutdownState;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::SpliceContribution;
use lightning::ln::msgs::SocketAddress;
use lightning::routing::gossip::NodeAlias;
use lightning::util::persist::KVStoreSync;
use lightning_background_processor::process_events_async;
use liquidity::{LSPS1Liquidity, LiquiditySource};
use logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use payment::asynchronous::om_mailbox::OnionMessageMailbox;
use payment::asynchronous::static_invoice_store::StaticInvoiceStore;
use payment::{
	Bolt11Payment, Bolt12Payment, OnchainPayment, PaymentDetails, SpontaneousPayment,
	UnifiedQrPayment,
};
use peer_store::{PeerInfo, PeerStore};
use rand::Rng;
use runtime::Runtime;
use types::{
	Broadcaster, BumpTransactionEventHandler, ChainMonitor, ChannelManager, Graph, KeysManager,
	OnionMessenger, PaymentStore, PeerManager, Router, Scorer, Sweeper, Wallet,
};
pub use types::{
	ChannelDetails, CustomTlvRecord, DynStore, PeerDetails, SyncAndAsyncKVStore, UserChannelId,
};

pub use {
	bip39, bitcoin, lightning, lightning_invoice, lightning_liquidity, lightning_types, tokio,
	vss_client,
};

#[cfg(feature = "uniffi")]
uniffi::include_scaffolding!("ldk_node");

/// The main interface object of LDK Node, wrapping the necessary LDK and BDK functionalities.
///
/// Needs to be initialized and instantiated through [`Builder::build`].
pub struct Node {
	runtime: Arc<Runtime>,
	stop_sender: tokio::sync::watch::Sender<()>,
	background_processor_stop_sender: tokio::sync::watch::Sender<()>,
	config: Arc<Config>,
	wallet: Arc<Wallet>,
	chain_source: Arc<ChainSource>,
	tx_broadcaster: Arc<Broadcaster>,
	event_queue: Arc<EventQueue<Arc<Logger>>>,
	channel_manager: Arc<ChannelManager>,
	chain_monitor: Arc<ChainMonitor>,
	output_sweeper: Arc<Sweeper>,
	peer_manager: Arc<PeerManager>,
	onion_messenger: Arc<OnionMessenger>,
	connection_manager: Arc<ConnectionManager<Arc<Logger>>>,
	keys_manager: Arc<KeysManager>,
	network_graph: Arc<Graph>,
	gossip_source: Arc<GossipSource>,
	pathfinding_scores_sync_url: Option<String>,
	liquidity_source: Option<Arc<LiquiditySource<Arc<Logger>>>>,
	kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
	_router: Arc<Router>,
	scorer: Arc<Mutex<Scorer>>,
	peer_store: Arc<PeerStore<Arc<Logger>>>,
	payment_store: Arc<PaymentStore>,
	is_running: Arc<RwLock<bool>>,
	node_metrics: Arc<RwLock<NodeMetrics>>,
	om_mailbox: Option<Arc<OnionMessageMailbox>>,
	async_payments_role: Option<AsyncPaymentsRole>,
}

impl Node {
	/// Starts the necessary background tasks, such as handling events coming from user input,
	/// LDK/BDK, and the peer-to-peer network.
	///
	/// This will try to auto-detect an outer pre-existing runtime, e.g., to avoid stacking Tokio
	/// runtime contexts. Note we require the outer runtime to be of the `multithreaded` flavor.
	///
	/// After this returns, the [`Node`] instance can be controlled via the provided API methods in
	/// a thread-safe manner.
	pub fn start(&self) -> Result<(), Error> {
		// Acquire a run lock and hold it until we're setup.
		let mut is_running_lock = self.is_running.write().unwrap();
		if *is_running_lock {
			return Err(Error::AlreadyRunning);
		}

		log_info!(
			self.logger,
			"Starting up LDK Node with node ID {} on network: {}",
			self.node_id(),
			self.config.network
		);

		// Start up any runtime-dependant chain sources (e.g. Electrum)
		self.chain_source.start(Arc::clone(&self.runtime)).map_err(|e| {
			log_error!(self.logger, "Failed to start chain syncing: {}", e);
			e
		})?;

		// Block to ensure we update our fee rate cache once on startup
		let chain_source = Arc::clone(&self.chain_source);
		self.runtime.block_on(async move { chain_source.update_fee_rate_estimates().await })?;

		// Spawn background task continuously syncing onchain, lightning, and fee rate cache.
		let stop_sync_receiver = self.stop_sender.subscribe();
		let chain_source = Arc::clone(&self.chain_source);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let sync_sweeper = Arc::clone(&self.output_sweeper);
		self.runtime.spawn_background_task(async move {
			chain_source
				.continuously_sync_wallets(stop_sync_receiver, sync_cman, sync_cmon, sync_sweeper)
				.await;
		});

		if self.gossip_source.is_rgs() {
			let gossip_source = Arc::clone(&self.gossip_source);
			let gossip_sync_store = Arc::clone(&self.kv_store);
			let gossip_sync_logger = Arc::clone(&self.logger);
			let gossip_node_metrics = Arc::clone(&self.node_metrics);
			let mut stop_gossip_sync = self.stop_sender.subscribe();
			self.runtime.spawn_cancellable_background_task(async move {
				let mut interval = tokio::time::interval(RGS_SYNC_INTERVAL);
				loop {
					tokio::select! {
						_ = stop_gossip_sync.changed() => {
							log_debug!(
								gossip_sync_logger,
								"Stopping background syncing RGS gossip data.",
							);
							return;
						}
						_ = interval.tick() => {
							let now = Instant::now();
							match gossip_source.update_rgs_snapshot().await {
								Ok(updated_timestamp) => {
									log_info!(
										gossip_sync_logger,
										"Background sync of RGS gossip data finished in {}ms.",
										now.elapsed().as_millis()
										);
									{
										let mut locked_node_metrics = gossip_node_metrics.write().unwrap();
										locked_node_metrics.latest_rgs_snapshot_timestamp = Some(updated_timestamp);
										write_node_metrics(&*locked_node_metrics, Arc::clone(&gossip_sync_store), Arc::clone(&gossip_sync_logger))
											.unwrap_or_else(|e| {
												log_error!(gossip_sync_logger, "Persistence failed: {}", e);
											});
									}
								}
								Err(e) => {
									log_error!(
										gossip_sync_logger,
										"Background sync of RGS gossip data failed: {}",
										e
									)
								}
							}
						}
					}
				}
			});
		}

		if let Some(pathfinding_scores_sync_url) = self.pathfinding_scores_sync_url.as_ref() {
			setup_background_pathfinding_scores_sync(
				pathfinding_scores_sync_url.clone(),
				Arc::clone(&self.scorer),
				Arc::clone(&self.node_metrics),
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
				Arc::clone(&self.runtime),
				self.stop_sender.subscribe(),
			);
		}

		if let Some(listening_addresses) = &self.config.listening_addresses {
			// Setup networking
			let peer_manager_connection_handler = Arc::clone(&self.peer_manager);
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

			let logger = Arc::clone(&listening_logger);
			let listeners = self.runtime.block_on(async move {
				let mut listeners = Vec::new();

				// Try to bind to all addresses
				for addr in &*bind_addrs {
					match tokio::net::TcpListener::bind(addr).await {
						Ok(listener) => {
							log_trace!(logger, "Listener bound to {}", addr);
							listeners.push(listener);
						},
						Err(e) => {
							log_error!(
								logger,
								"Failed to bind to {}: {} - is something else already listening?",
								addr,
								e
							);
							return Err(Error::InvalidSocketAddress);
						},
					}
				}

				Ok(listeners)
			})?;

			for listener in listeners {
				let logger = Arc::clone(&listening_logger);
				let peer_mgr = Arc::clone(&peer_manager_connection_handler);
				let mut stop_listen = self.stop_sender.subscribe();
				let runtime = Arc::clone(&self.runtime);
				self.runtime.spawn_cancellable_background_task(async move {
					loop {
						tokio::select! {
							_ = stop_listen.changed() => {
								log_debug!(
									logger,
									"Stopping listening to inbound connections."
								);
								break;
							}
							res = listener.accept() => {
								let tcp_stream = res.unwrap().0;
								let peer_mgr = Arc::clone(&peer_mgr);
								runtime.spawn_cancellable_background_task(async move {
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
		}

		// Regularly reconnect to persisted peers.
		let connect_cm = Arc::clone(&self.connection_manager);
		let connect_pm = Arc::clone(&self.peer_manager);
		let connect_logger = Arc::clone(&self.logger);
		let connect_peer_store = Arc::clone(&self.peer_store);
		let mut stop_connect = self.stop_sender.subscribe();
		self.runtime.spawn_cancellable_background_task(async move {
			let mut interval = tokio::time::interval(PEER_RECONNECTION_INTERVAL);
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
						_ = stop_connect.changed() => {
							log_debug!(
								connect_logger,
								"Stopping reconnecting known peers."
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
								let _ = connect_cm.do_connect_peer(
									peer_info.node_id,
									peer_info.address.clone(),
									).await;
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
		let bcast_node_metrics = Arc::clone(&self.node_metrics);
		let mut stop_bcast = self.stop_sender.subscribe();
		let node_alias = self.config.node_alias.clone();
		if may_announce_channel(&self.config).is_ok() {
			self.runtime.spawn_cancellable_background_task(async move {
				// We check every 30 secs whether our last broadcast is NODE_ANN_BCAST_INTERVAL away.
				#[cfg(not(test))]
				let mut interval = tokio::time::interval(Duration::from_secs(30));
				#[cfg(test)]
				let mut interval = tokio::time::interval(Duration::from_secs(5));
				loop {
					tokio::select! {
						_ = stop_bcast.changed() => {
							log_debug!(
								bcast_logger,
								"Stopping broadcasting node announcements.",
								);
							return;
						}
						_ = interval.tick() => {
							let skip_broadcast = match bcast_node_metrics.read().unwrap().latest_node_announcement_broadcast_timestamp {
								Some(latest_bcast_time_secs) => {
									// Skip if the time hasn't elapsed yet.
									let next_bcast_unix_time = SystemTime::UNIX_EPOCH + Duration::from_secs(latest_bcast_time_secs) + NODE_ANN_BCAST_INTERVAL;
									next_bcast_unix_time.elapsed().is_err()
								}
								None => {
									// Don't skip if we haven't broadcasted before.
									false
								}
							};

							if skip_broadcast {
								continue;
							}

							if !bcast_cm.list_channels().iter().any(|chan| chan.is_announced && chan.is_channel_ready) {
								// Skip if we don't have any public channels that are ready.
								continue;
							}

							if bcast_pm.list_peers().is_empty() {
								// Skip if we don't have any connected peers to gossip to.
								continue;
							}

							let addresses = if let Some(announcement_addresses) = bcast_config.announcement_addresses.clone() {
								announcement_addresses
							} else if let Some(listening_addresses) = bcast_config.listening_addresses.clone() {
								listening_addresses
							} else {
								debug_assert!(false, "We checked whether the node may announce, so listening addresses should always be set");
								continue;
							};

							if let Some(node_alias) = node_alias.as_ref() {
								bcast_pm.broadcast_node_announcement([0; 3], node_alias.0, addresses);

								let unix_time_secs_opt =
									SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
								{
									let mut locked_node_metrics = bcast_node_metrics.write().unwrap();
									locked_node_metrics.latest_node_announcement_broadcast_timestamp = unix_time_secs_opt;
									write_node_metrics(&*locked_node_metrics, Arc::clone(&bcast_store), Arc::clone(&bcast_logger))
										.unwrap_or_else(|e| {
											log_error!(bcast_logger, "Persistence failed: {}", e);
										});
								}
							} else {
								debug_assert!(false, "We checked whether the node may announce, so node alias should always be set");
								continue
							}
						}
					}
				}
			});
		}

		let stop_tx_bcast = self.stop_sender.subscribe();
		let chain_source = Arc::clone(&self.chain_source);
		self.runtime.spawn_cancellable_background_task(async move {
			chain_source.continuously_process_broadcast_queue(stop_tx_bcast).await
		});

		let bump_tx_event_handler = Arc::new(BumpTransactionEventHandler::new(
			Arc::clone(&self.tx_broadcaster),
			Arc::new(LdkWallet::new(Arc::clone(&self.wallet), Arc::clone(&self.logger))),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.logger),
		));

		let static_invoice_store = if let Some(AsyncPaymentsRole::Server) = self.async_payments_role
		{
			Some(StaticInvoiceStore::new(Arc::clone(&self.kv_store)))
		} else {
			None
		};

		let event_handler = Arc::new(EventHandler::new(
			Arc::clone(&self.event_queue),
			Arc::clone(&self.wallet),
			bump_tx_event_handler,
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.connection_manager),
			Arc::clone(&self.output_sweeper),
			Arc::clone(&self.network_graph),
			self.liquidity_source.clone(),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.peer_store),
			static_invoice_store,
			Arc::clone(&self.onion_messenger),
			self.om_mailbox.clone(),
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
		let background_liquidity_man_opt =
			self.liquidity_source.as_ref().map(|ls| ls.liquidity_manager());
		let background_sweeper = Arc::clone(&self.output_sweeper);
		let background_onion_messenger = Arc::clone(&self.onion_messenger);
		let background_logger = Arc::clone(&self.logger);
		let background_error_logger = Arc::clone(&self.logger);
		let background_scorer = Arc::clone(&self.scorer);
		let stop_bp = self.background_processor_stop_sender.subscribe();
		let sleeper_logger = Arc::clone(&self.logger);
		let sleeper = move |d| {
			let mut stop = stop_bp.clone();
			let sleeper_logger = Arc::clone(&sleeper_logger);
			Box::pin(async move {
				tokio::select! {
					_ = stop.changed() => {
						log_debug!(
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

		self.runtime.spawn_background_processor_task(async move {
			process_events_async(
				background_persister,
				|e| background_event_handler.handle_event(e),
				background_chain_mon,
				background_chan_man,
				Some(background_onion_messenger),
				background_gossip_sync,
				background_peer_man,
				background_liquidity_man_opt,
				Some(background_sweeper),
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
			let mut stop_liquidity_handler = self.stop_sender.subscribe();
			let liquidity_handler = Arc::clone(&liquidity_source);
			let liquidity_logger = Arc::clone(&self.logger);
			self.runtime.spawn_background_task(async move {
				loop {
					tokio::select! {
						_ = stop_liquidity_handler.changed() => {
							log_debug!(
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

		log_info!(self.logger, "Startup complete.");
		*is_running_lock = true;
		Ok(())
	}

	/// Disconnects all peers, stops all running background tasks, and shuts down [`Node`].
	///
	/// After this returns most API methods will return [`Error::NotRunning`].
	pub fn stop(&self) -> Result<(), Error> {
		let mut is_running_lock = self.is_running.write().unwrap();
		if !*is_running_lock {
			return Err(Error::NotRunning);
		}

		log_info!(self.logger, "Shutting down LDK Node with node ID {}...", self.node_id());

		// Stop background tasks.
		self.stop_sender
			.send(())
			.map(|_| {
				log_trace!(self.logger, "Sent shutdown signal to background tasks.");
			})
			.unwrap_or_else(|e| {
				log_error!(
					self.logger,
					"Failed to send shutdown signal. This should never happen: {}",
					e
				);
				debug_assert!(false);
			});

		// Cancel cancellable background tasks
		self.runtime.abort_cancellable_background_tasks();

		// Disconnect all peers.
		self.peer_manager.disconnect_all_peers();
		log_debug!(self.logger, "Disconnected all network peers.");

		// Wait until non-cancellable background tasks (mod LDK's background processor) are done.
		self.runtime.wait_on_background_tasks();

		// Stop any runtime-dependant chain sources.
		self.chain_source.stop();
		log_debug!(self.logger, "Stopped chain sources.");

		// Stop the background processor.
		self.background_processor_stop_sender
			.send(())
			.map(|_| {
				log_trace!(self.logger, "Sent shutdown signal to background processor.");
			})
			.unwrap_or_else(|e| {
				log_error!(
					self.logger,
					"Failed to send shutdown signal. This should never happen: {}",
					e
				);
				debug_assert!(false);
			});

		// Finally, wait until background processing stopped, at least until a timeout is reached.
		self.runtime.wait_on_background_processor_task();

		#[cfg(tokio_unstable)]
		self.runtime.log_metrics();

		log_info!(self.logger, "Shutdown complete.");
		*is_running_lock = false;
		Ok(())
	}

	/// Returns the status of the [`Node`].
	pub fn status(&self) -> NodeStatus {
		let is_running = *self.is_running.read().unwrap();
		let current_best_block = self.channel_manager.current_best_block().into();
		let locked_node_metrics = self.node_metrics.read().unwrap();
		let latest_lightning_wallet_sync_timestamp =
			locked_node_metrics.latest_lightning_wallet_sync_timestamp;
		let latest_onchain_wallet_sync_timestamp =
			locked_node_metrics.latest_onchain_wallet_sync_timestamp;
		let latest_fee_rate_cache_update_timestamp =
			locked_node_metrics.latest_fee_rate_cache_update_timestamp;
		let latest_rgs_snapshot_timestamp =
			locked_node_metrics.latest_rgs_snapshot_timestamp.map(|val| val as u64);
		let latest_pathfinding_scores_sync_timestamp =
			locked_node_metrics.latest_pathfinding_scores_sync_timestamp;
		let latest_node_announcement_broadcast_timestamp =
			locked_node_metrics.latest_node_announcement_broadcast_timestamp;
		let latest_channel_monitor_archival_height =
			locked_node_metrics.latest_channel_monitor_archival_height;

		NodeStatus {
			is_running,
			current_best_block,
			latest_lightning_wallet_sync_timestamp,
			latest_onchain_wallet_sync_timestamp,
			latest_fee_rate_cache_update_timestamp,
			latest_rgs_snapshot_timestamp,
			latest_pathfinding_scores_sync_timestamp,
			latest_node_announcement_broadcast_timestamp,
			latest_channel_monitor_archival_height,
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
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Node`].
	pub fn next_event(&self) -> Option<Event> {
		self.event_queue.next_event()
	}

	/// Returns the next event in the event queue.
	///
	/// Will asynchronously poll the event queue until the next event is ready.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Node::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Node`].
	pub async fn next_event_async(&self) -> Event {
		self.event_queue.next_event_async().await
	}

	/// Returns the next event in the event queue.
	///
	/// Will block the current thread until the next event is available.
	///
	/// **Note:** this will always return the same event until handling is confirmed via [`Node::event_handled`].
	///
	/// **Caution:** Users must handle events as quickly as possible to prevent a large event backlog,
	/// which can increase the memory footprint of [`Node`].
	pub fn wait_next_event(&self) -> Event {
		let fut = self.event_queue.next_event_async();
		// We use our runtime for the sync variant to ensure `tokio::task::block_in_place` is
		// always called if we'd ever hit this in an outer runtime context.
		self.runtime.block_on(fut)
	}

	/// Confirm the last retrieved event handled.
	///
	/// **Note:** This **MUST** be called after each event has been handled.
	pub fn event_handled(&self) -> Result<(), Error> {
		self.event_queue.event_handled().map_err(|e| {
			log_error!(
				self.logger,
				"Couldn't mark event handled due to persistence failure: {}",
				e
			);
			e
		})
	}

	/// Returns our own node id
	pub fn node_id(&self) -> PublicKey {
		self.channel_manager.get_our_node_id()
	}

	/// Returns our own listening addresses.
	pub fn listening_addresses(&self) -> Option<Vec<SocketAddress>> {
		self.config.listening_addresses.clone()
	}

	/// Returns the addresses that the node will announce to the network.
	pub fn announcement_addresses(&self) -> Option<Vec<SocketAddress>> {
		self.config
			.announcement_addresses
			.clone()
			.or_else(|| self.config.listening_addresses.clone())
	}

	/// Returns our node alias.
	pub fn node_alias(&self) -> Option<NodeAlias> {
		self.config.node_alias
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
			self.liquidity_source.clone(),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.peer_store),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
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
			self.liquidity_source.clone(),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.peer_store),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
		))
	}

	/// Returns a payment handler allowing to create and pay [BOLT 12] offers and refunds.
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	#[cfg(not(feature = "uniffi"))]
	pub fn bolt12_payment(&self) -> Bolt12Payment {
		Bolt12Payment::new(
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
			self.async_payments_role,
		)
	}

	/// Returns a payment handler allowing to create and pay [BOLT 12] offers and refunds.
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	#[cfg(feature = "uniffi")]
	pub fn bolt12_payment(&self) -> Arc<Bolt12Payment> {
		Arc::new(Bolt12Payment::new(
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
			self.async_payments_role,
		))
	}

	/// Returns a payment handler allowing to send spontaneous ("keysend") payments.
	#[cfg(not(feature = "uniffi"))]
	pub fn spontaneous_payment(&self) -> SpontaneousPayment {
		SpontaneousPayment::new(
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to send spontaneous ("keysend") payments.
	#[cfg(feature = "uniffi")]
	pub fn spontaneous_payment(&self) -> Arc<SpontaneousPayment> {
		Arc::new(SpontaneousPayment::new(
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.keys_manager),
			Arc::clone(&self.payment_store),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
		))
	}

	/// Returns a payment handler allowing to send and receive on-chain payments.
	#[cfg(not(feature = "uniffi"))]
	pub fn onchain_payment(&self) -> OnchainPayment {
		OnchainPayment::new(
			Arc::clone(&self.wallet),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a payment handler allowing to send and receive on-chain payments.
	#[cfg(feature = "uniffi")]
	pub fn onchain_payment(&self) -> Arc<OnchainPayment> {
		Arc::new(OnchainPayment::new(
			Arc::clone(&self.wallet),
			Arc::clone(&self.channel_manager),
			Arc::clone(&self.config),
			Arc::clone(&self.is_running),
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

	/// Returns a liquidity handler allowing to request channels via the [bLIP-51 / LSPS1] protocol.
	///
	/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
	#[cfg(not(feature = "uniffi"))]
	pub fn lsps1_liquidity(&self) -> LSPS1Liquidity {
		LSPS1Liquidity::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.wallet),
			Arc::clone(&self.connection_manager),
			self.liquidity_source.clone(),
			Arc::clone(&self.logger),
		)
	}

	/// Returns a liquidity handler allowing to request channels via the [bLIP-51 / LSPS1] protocol.
	///
	/// [bLIP-51 / LSPS1]: https://github.com/lightning/blips/blob/master/blip-0051.md
	#[cfg(feature = "uniffi")]
	pub fn lsps1_liquidity(&self) -> Arc<LSPS1Liquidity> {
		Arc::new(LSPS1Liquidity::new(
			Arc::clone(&self.runtime),
			Arc::clone(&self.wallet),
			Arc::clone(&self.connection_manager),
			self.liquidity_source.clone(),
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
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let peer_info = PeerInfo { node_id, address };

		let con_node_id = peer_info.node_id;
		let con_addr = peer_info.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
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
		if !*self.is_running.read().unwrap() {
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

	fn open_channel_inner(
		&self, node_id: PublicKey, address: SocketAddress, channel_amount_sats: u64,
		push_to_counterparty_msat: Option<u64>, channel_config: Option<ChannelConfig>,
		announce_for_forwarding: bool,
	) -> Result<UserChannelId, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let peer_info = PeerInfo { node_id, address };

		let con_node_id = peer_info.node_id;
		let con_addr = peer_info.address.clone();
		let con_cm = Arc::clone(&self.connection_manager);

		// We need to use our main runtime here as a local runtime might not be around to poll
		// connection futures going forward.
		self.runtime.block_on(async move {
			con_cm.connect_peer_if_necessary(con_node_id, con_addr).await
		})?;

		// Check funds availability after connection (includes anchor reserve calculation)
		self.check_sufficient_funds_for_channel(channel_amount_sats, &node_id)?;

		let mut user_config = default_user_config(&self.config);
		user_config.channel_handshake_config.announce_for_forwarding = announce_for_forwarding;
		user_config.channel_config = (channel_config.unwrap_or_default()).clone().into();
		// We set the max inflight to 100% for private channels.
		// FIXME: LDK will default to this behavior soon, too, at which point we should drop this
		// manual override.
		if !announce_for_forwarding {
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

	fn check_sufficient_funds_for_channel(
		&self, amount_sats: u64, peer_node_id: &PublicKey,
	) -> Result<(), Error> {
		let cur_anchor_reserve_sats =
			total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let spendable_amount_sats =
			self.wallet.get_spendable_amount_sats(cur_anchor_reserve_sats).unwrap_or(0);

		// Fail early if we have less than the channel value available.
		if spendable_amount_sats < amount_sats {
			log_error!(self.logger,
				"Unable to create channel due to insufficient funds. Available: {}sats, Required: {}sats",
				spendable_amount_sats, amount_sats
			);
			return Err(Error::InsufficientFunds);
		}

		// Fail if we have less than the channel value + anchor reserve available (if applicable).
		let init_features = self
			.peer_manager
			.peer_by_node_id(peer_node_id)
			.ok_or(Error::ConnectionFailed)?
			.init_features;
		let required_funds_sats = amount_sats
			+ self.config.anchor_channels_config.as_ref().map_or(0, |c| {
				if init_features.requires_anchors_zero_fee_htlc_tx()
					&& !c.trusted_peers_no_reserve.contains(peer_node_id)
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

		Ok(())
	}

	/// Connect to a node and open a new unannounced channel.
	///
	/// To open an announced channel, see [`Node::open_announced_channel`].
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
	///
	/// [`AnchorChannelsConfig::per_channel_reserve_sats`]: crate::config::AnchorChannelsConfig::per_channel_reserve_sats
	pub fn open_channel(
		&self, node_id: PublicKey, address: SocketAddress, channel_amount_sats: u64,
		push_to_counterparty_msat: Option<u64>, channel_config: Option<ChannelConfig>,
	) -> Result<UserChannelId, Error> {
		self.open_channel_inner(
			node_id,
			address,
			channel_amount_sats,
			push_to_counterparty_msat,
			channel_config,
			false,
		)
	}

	/// Connect to a node and open a new announced channel.
	///
	/// This will return an error if the node has not been sufficiently configured to operate as a
	/// forwarding node that can properly announce its existence to the publip network graph, i.e.,
	/// [`Config::listening_addresses`] and [`Config::node_alias`] are unset.
	///
	/// To open an unannounced channel, see [`Node::open_channel`].
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
	///
	/// [`AnchorChannelsConfig::per_channel_reserve_sats`]: crate::config::AnchorChannelsConfig::per_channel_reserve_sats
	pub fn open_announced_channel(
		&self, node_id: PublicKey, address: SocketAddress, channel_amount_sats: u64,
		push_to_counterparty_msat: Option<u64>, channel_config: Option<ChannelConfig>,
	) -> Result<UserChannelId, Error> {
		if let Err(err) = may_announce_channel(&self.config) {
			log_error!(self.logger, "Failed to open announced channel as the node hasn't been sufficiently configured to act as a forwarding node: {}", err);
			return Err(Error::ChannelCreationFailed);
		}

		self.open_channel_inner(
			node_id,
			address,
			channel_amount_sats,
			push_to_counterparty_msat,
			channel_config,
			true,
		)
	}

	/// Add funds from the on-chain wallet into an existing channel.
	///
	/// This provides for increasing a channel's outbound liquidity without re-balancing or closing
	/// it. Once negotiation with the counterparty is complete, the channel remains operational
	/// while waiting for a new funding transaction to confirm.
	pub fn splice_in(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
		splice_amount_sats: u64,
	) -> Result<(), Error> {
		let open_channels =
			self.channel_manager.list_channels_with_counterparty(&counterparty_node_id);
		if let Some(channel_details) =
			open_channels.iter().find(|c| c.user_channel_id == user_channel_id.0)
		{
			self.check_sufficient_funds_for_channel(splice_amount_sats, &counterparty_node_id)?;

			const EMPTY_SCRIPT_SIG_WEIGHT: u64 =
				1 /* empty script_sig */ * bitcoin::constants::WITNESS_SCALE_FACTOR as u64;
			let funding_txo = channel_details.funding_txo.ok_or_else(|| {
				log_error!(self.logger, "Failed to splice channel: channel not yet ready",);
				Error::ChannelSplicingFailed
			})?;
			let shared_input = Input {
				outpoint: funding_txo.into_bitcoin_outpoint(),
				previous_utxo: bitcoin::TxOut {
					value: Amount::from_sat(channel_details.channel_value_satoshis),
					script_pubkey: make_funding_redeemscript(
						&PublicKey::from_slice(&[2; 33]).unwrap(),
						&PublicKey::from_slice(&[2; 33]).unwrap(),
					)
					.to_p2wsh(),
				},
				satisfaction_weight: EMPTY_SCRIPT_SIG_WEIGHT + FUNDING_TRANSACTION_WITNESS_WEIGHT,
			};

			let shared_output = bitcoin::TxOut {
				value: shared_input.previous_utxo.value + Amount::from_sat(splice_amount_sats),
				script_pubkey: make_funding_redeemscript(
					&PublicKey::from_slice(&[2; 33]).unwrap(),
					&PublicKey::from_slice(&[2; 33]).unwrap(),
				)
				.to_p2wsh(),
			};

			let fee_rate = self.wallet.estimate_channel_funding_fee_rate();

			let inputs = self
				.wallet
				.select_confirmed_utxos(vec![shared_input], &[shared_output], fee_rate)
				.map_err(|()| {
					log_error!(
						self.logger,
						"Failed to splice channel: insufficient confirmed UTXOs",
					);
					Error::ChannelSplicingFailed
				})?;

			let contribution = SpliceContribution::SpliceIn {
				value: Amount::from_sat(splice_amount_sats),
				inputs,
				change_script: None,
			};

			let funding_feerate_per_kw = fee_rate.to_sat_per_kwu().try_into().unwrap_or(u32::MAX);

			self.channel_manager
				.splice_channel(
					&channel_details.channel_id,
					&counterparty_node_id,
					contribution,
					funding_feerate_per_kw,
					None,
				)
				.map_err(|e| {
					log_error!(self.logger, "Failed to splice channel: {:?}", e);
					Error::ChannelSplicingFailed
				})
		} else {
			log_error!(
				self.logger,
				"Channel not found for user_channel_id: {:?} and counterparty: {}",
				user_channel_id,
				counterparty_node_id
			);

			Err(Error::ChannelSplicingFailed)
		}
	}

	/// Manually sync the LDK and BDK wallets with the current chain state and update the fee rate
	/// cache.
	///
	/// **Note:** The wallets are regularly synced in the background if background syncing is enabled
	/// via [`EsploraSyncConfig::background_sync_config`]. Therefore, using this blocking sync method
	/// is almost always redundant when background syncing is enabled and should be avoided where possible.
	/// However, if background syncing is disabled (i.e., `background_sync_config` is set to `None`),
	/// this method must be called manually to keep wallets in sync with the chain state.
	///
	/// [`EsploraSyncConfig::background_sync_config`]: crate::config::EsploraSyncConfig::background_sync_config
	pub fn sync_wallets(&self) -> Result<(), Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let chain_source = Arc::clone(&self.chain_source);
		let sync_cman = Arc::clone(&self.channel_manager);
		let sync_cmon = Arc::clone(&self.chain_monitor);
		let sync_sweeper = Arc::clone(&self.output_sweeper);
		self.runtime.block_on(async move {
			if chain_source.is_transaction_based() {
				chain_source.update_fee_rate_estimates().await?;
				chain_source
					.sync_lightning_wallet(sync_cman, sync_cmon, Arc::clone(&sync_sweeper))
					.await?;
				chain_source.sync_onchain_wallet().await?;
			} else {
				chain_source.update_fee_rate_estimates().await?;
				chain_source
					.poll_and_update_listeners(sync_cman, sync_cmon, Arc::clone(&sync_sweeper))
					.await?;
			}
			let _ = sync_sweeper.regenerate_and_broadcast_spend_if_necessary().await;
			Ok(())
		})
	}

	/// Close a previously opened channel.
	///
	/// Will attempt to close a channel coopertively. If this fails, users might need to resort to
	/// [`Node::force_close_channel`].
	pub fn close_channel(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
	) -> Result<(), Error> {
		self.close_channel_internal(user_channel_id, counterparty_node_id, false, None)
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
	///
	/// [`AnchorChannelsConfig::trusted_peers_no_reserve`]: crate::config::AnchorChannelsConfig::trusted_peers_no_reserve
	pub fn force_close_channel(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey,
		reason: Option<String>,
	) -> Result<(), Error> {
		self.close_channel_internal(user_channel_id, counterparty_node_id, true, reason)
	}

	fn close_channel_internal(
		&self, user_channel_id: &UserChannelId, counterparty_node_id: PublicKey, force: bool,
		force_close_reason: Option<String>,
	) -> Result<(), Error> {
		debug_assert!(
			force_close_reason.is_none() || force,
			"Reason can only be set for force closures"
		);
		let open_channels =
			self.channel_manager.list_channels_with_counterparty(&counterparty_node_id);
		if let Some(channel_details) =
			open_channels.iter().find(|c| c.user_channel_id == user_channel_id.0)
		{
			if force {
				self.channel_manager
					.force_close_broadcasting_latest_txn(
						&channel_details.channel_id,
						&counterparty_node_id,
						force_close_reason.unwrap_or_default(),
					)
					.map_err(|e| {
						log_error!(self.logger, "Failed to force-close channel: {:?}", e);
						Error::ChannelClosingFailed
					})?;
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
		for channel_id in self.chain_monitor.list_monitors() {
			match self.chain_monitor.get_monitor(channel_id) {
				Ok(monitor) => {
					let counterparty_node_id = monitor.get_counterparty_node_id();
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
	/// # use ldk_node::Builder;
	/// # use ldk_node::config::Config;
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
	pub fn sign_message(&self, msg: &[u8]) -> String {
		self.keys_manager.sign_message(msg)
	}

	/// Verifies that the given ECDSA signature was created for the given message with the
	/// secret key corresponding to the given public key.
	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		self.keys_manager.verify_signature(msg, sig, pkey)
	}

	/// Exports the current state of the scorer. The result can be shared with and merged by light nodes that only have
	/// a limited view of the network.
	pub fn export_pathfinding_scores(&self) -> Result<Vec<u8>, Error> {
		KVStoreSync::read(
			&*self.kv_store,
			lightning::util::persist::SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			lightning::util::persist::SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			lightning::util::persist::SCORER_PERSISTENCE_KEY,
		)
		.map_err(|e| {
			log_error!(
				self.logger,
				"Failed to access store while exporting pathfinding scores: {}",
				e
			);
			Error::PersistenceFailed
		})
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
	/// The best block to which our Lightning wallet is currently synced.
	pub current_best_block: BestBlock,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully synced
	/// our Lightning wallet to the chain tip.
	///
	/// Will be `None` if the wallet hasn't been synced yet.
	pub latest_lightning_wallet_sync_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully synced
	/// our on-chain wallet to the chain tip.
	///
	/// Will be `None` if the wallet hasn't been synced yet.
	pub latest_onchain_wallet_sync_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully update
	/// our fee rate cache.
	///
	/// Will be `None` if the cache hasn't been updated yet.
	pub latest_fee_rate_cache_update_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when the last rapid gossip sync
	/// (RGS) snapshot we successfully applied was generated.
	///
	/// Will be `None` if RGS isn't configured or the snapshot hasn't been updated yet.
	pub latest_rgs_snapshot_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last successfully merged external scores.
	pub latest_pathfinding_scores_sync_timestamp: Option<u64>,
	/// The timestamp, in seconds since start of the UNIX epoch, when we last broadcasted a node
	/// announcement.
	///
	/// Will be `None` if we have no public channels or we haven't broadcasted yet.
	pub latest_node_announcement_broadcast_timestamp: Option<u64>,
	/// The block height when we last archived closed channel monitor data.
	///
	/// Will be `None` if we haven't archived any monitors of closed channels yet.
	pub latest_channel_monitor_archival_height: Option<u32>,
}

/// Status fields that are persisted across restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeMetrics {
	latest_lightning_wallet_sync_timestamp: Option<u64>,
	latest_onchain_wallet_sync_timestamp: Option<u64>,
	latest_fee_rate_cache_update_timestamp: Option<u64>,
	latest_rgs_snapshot_timestamp: Option<u32>,
	latest_pathfinding_scores_sync_timestamp: Option<u64>,
	latest_node_announcement_broadcast_timestamp: Option<u64>,
	latest_channel_monitor_archival_height: Option<u32>,
}

impl Default for NodeMetrics {
	fn default() -> Self {
		Self {
			latest_lightning_wallet_sync_timestamp: None,
			latest_onchain_wallet_sync_timestamp: None,
			latest_fee_rate_cache_update_timestamp: None,
			latest_rgs_snapshot_timestamp: None,
			latest_pathfinding_scores_sync_timestamp: None,
			latest_node_announcement_broadcast_timestamp: None,
			latest_channel_monitor_archival_height: None,
		}
	}
}

impl_writeable_tlv_based!(NodeMetrics, {
	(0, latest_lightning_wallet_sync_timestamp, option),
	(1, latest_pathfinding_scores_sync_timestamp, option),
	(2, latest_onchain_wallet_sync_timestamp, option),
	(4, latest_fee_rate_cache_update_timestamp, option),
	(6, latest_rgs_snapshot_timestamp, option),
	(8, latest_node_announcement_broadcast_timestamp, option),
	(10, latest_channel_monitor_archival_height, option),
});

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
