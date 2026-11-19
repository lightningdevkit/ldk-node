// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Tests that `ChannelMonitor` persistence which is in flight, or which is started, while
//! [`Node::stop`] runs still completes, so the channel makes progress once the same `Node` is
//! started again.
//!
//! [`Node::stop`]: ldk_node::Node::stop

mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::Amount;
use common::logging::{CollectingLogWriter, TestLogWriter};
use common::{
	expect_channel_ready_event, generate_blocks_and_wait, open_channel, open_channel_push_amt,
	premine_and_distribute_funds, random_config, setup_bitcoind_and_electrsd, setup_node,
	setup_node_with_store, wait_for_channel_ready_to_send, TestChainSource, TestNode,
};
use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::Event;
use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
use lightning_invoice::{Bolt11InvoiceDescription, Description};

const PAYMENT_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq)]
enum HeldWrite {
	/// The next `ChannelMonitorUpdate` write, held after it was committed.
	MonitorUpdateCommitted,
	/// The next `ChannelMonitorUpdate` write, held before it is handed to the store, as a store
	/// that only issues writes once their future is polled would leave it.
	MonitorUpdateUnissued,
	/// The next full `ChannelMonitor` write, held after it was committed.
	MonitorCommitted,
	/// The next `ChannelManager` write, held after it was committed.
	ChannelManagerCommitted,
}

/// A [`SqliteStore`] that can hold one write until it is released.
#[derive(Clone)]
struct HoldingStore {
	inner: Arc<SqliteStore>,
	armed: Arc<Mutex<Option<HeldWrite>>>,
	reached: Arc<tokio::sync::Notify>,
	release: Arc<tokio::sync::Semaphore>,
}

impl HoldingStore {
	fn new(storage_dir_path: &str) -> Self {
		let inner = Arc::new(
			SqliteStore::new(
				storage_dir_path.into(),
				Some("holding_store.sqlite".to_string()),
				Some("holding_kv".to_string()),
			)
			.unwrap(),
		);
		Self {
			inner,
			armed: Arc::new(Mutex::new(None)),
			reached: Arc::new(tokio::sync::Notify::new()),
			release: Arc::new(tokio::sync::Semaphore::new(0)),
		}
	}

	fn hold(&self, write: HeldWrite) {
		*self.armed.lock().unwrap() = Some(write);
	}

	async fn wait_until_held(&self) {
		tokio::time::timeout(Duration::from_secs(60), self.reached.notified())
			.await
			.expect("the held write was reached");
	}

	fn release(&self) {
		self.release.add_permits(1);
	}
}

impl KVStore for HoldingStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, lightning::io::Error>> + 'static + Send {
		KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), lightning::io::Error>> + 'static + Send {
		let held = {
			let mut armed = self.armed.lock().unwrap();
			let hit = match *armed {
				Some(HeldWrite::MonitorUpdateCommitted)
				| Some(HeldWrite::MonitorUpdateUnissued) => primary_namespace == "monitor_updates",
				Some(HeldWrite::MonitorCommitted) => primary_namespace == "monitors",
				Some(HeldWrite::ChannelManagerCommitted) => {
					primary_namespace.is_empty()
						&& secondary_namespace.is_empty()
						&& key == "manager"
				},
				None => false,
			};
			if hit {
				armed.take()
			} else {
				None
			}
		};
		// Like `SqliteStore` itself, issue the write when asked unless we model a store that
		// only issues it once polled.
		let (issued, unissued) = if held == Some(HeldWrite::MonitorUpdateUnissued) {
			(None, Some(buf))
		} else {
			let issued =
				KVStore::write(&*self.inner, primary_namespace, secondary_namespace, key, buf);
			(Some(issued), None)
		};
		let inner = Arc::clone(&self.inner);
		let reached = Arc::clone(&self.reached);
		let release = Arc::clone(&self.release);
		let (primary_namespace, secondary_namespace, key) =
			(primary_namespace.to_string(), secondary_namespace.to_string(), key.to_string());
		async move {
			if let Some(issued) = issued {
				issued.await?;
			}
			if held.is_some() {
				reached.notify_one();
				release.acquire().await.expect("semaphore open").forget();
			}
			match unissued {
				Some(buf) => {
					KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf)
						.await
				},
				None => Ok(()),
			}
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), lightning::io::Error>> + 'static + Send {
		KVStore::remove(&*self.inner, primary_namespace, secondary_namespace, key, lazy)
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, lightning::io::Error>> + 'static + Send {
		KVStore::list(&*self.inner, primary_namespace, secondary_namespace)
	}
}

impl PaginatedKVStore for HoldingStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, lightning::io::Error>> + 'static + Send
	{
		PaginatedKVStore::list_paginated(
			&*self.inner,
			primary_namespace,
			secondary_namespace,
			page_token,
		)
	}
}

/// Has `payer` pay a new invoice of `payee`.
fn send_payment(payer: &TestNode, payee: &TestNode, amount_msat: u64) -> PaymentId {
	let description =
		Bolt11InvoiceDescription::Direct(Description::new("monitor persistence".into()).unwrap());
	let invoice = payee.bolt11_payment().receive(amount_msat, &description.into(), 3600).unwrap();
	payer.bolt11_payment().send(&invoice, None).unwrap()
}

/// Handles events on `node` until `payment_id` succeeds or fails, or `PAYMENT_WAIT` passes.
async fn payment_succeeds(node: &TestNode, payment_id: PaymentId) -> bool {
	let deadline = tokio::time::Instant::now() + PAYMENT_WAIT;
	loop {
		let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
		let event = match tokio::time::timeout(remaining, node.next_event_async()).await {
			Ok(event) => event,
			Err(_) => return false,
		};
		node.event_handled().unwrap();
		match event {
			Event::PaymentSuccessful { payment_id: id, .. } if id == payment_id => return true,
			Event::PaymentFailed { payment_id: id, .. } if id == payment_id => return false,
			_ => {},
		}
	}
}

/// Calls `Node::stop` on another thread, as `stop` blocks on the runtime.
fn stop_in_thread(node: &Arc<TestNode>) -> std::thread::JoinHandle<()> {
	let node = Arc::clone(node);
	std::thread::spawn(move || node.stop().unwrap())
}

/// Holds A's next `held` write while a payment over an open channel needs it, stops A, lets the
/// write finish, restarts the same `Node` and checks that the payment and a new one settle.
async fn do_payment_across_restart(held: HeldWrite) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let logs = Arc::new(CollectingLogWriter::new());
	let mut config_a = random_config();
	config_a.log_writer = TestLogWriter::Custom(logs.clone());
	let store = HoldingStore::new(&config_a.node_config.storage_dir_path);
	let node_a = Arc::new(setup_node_with_store(&chain_source, config_a, store.clone()));
	let node_b = setup_node(&chain_source, random_config());

	let address_a = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(5_000_000),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	// A receives when its ChannelManager write is held: then A's peer handler, not its parked
	// background processor, starts the monitor update, which is only persisted once the
	// background processor flushes it while A is stopping.
	let a_receives = held == HeldWrite::ChannelManagerCommitted;
	if a_receives {
		open_channel_push_amt(&node_a, &node_b, 4_000_000, Some(1_500_000_000), false, &electrsd)
			.await;
	} else {
		open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;
	}
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());
	wait_for_channel_ready_to_send(&node_a, &node_b, 2_000_000).await;

	store.hold(held);
	let (payer, payee): (&TestNode, &TestNode) =
		if a_receives { (&node_b, &node_a) } else { (&node_a, &node_b) };
	let payment_id = send_payment(payer, payee, 1_000_000);
	store.wait_until_held().await;

	let stop = stop_in_thread(&node_a);
	if a_receives {
		// The background processor is parked on the held write, so `stop` waits for it. Only
		// release it once `stop` has cancelled the background tasks.
		assert!(logs.wait_for("Disconnected all network peers.").await);
		store.release();
		tokio::task::spawn_blocking(move || stop.join().unwrap()).await.unwrap();
	} else {
		// Only let the held write finish once `stop` has returned.
		tokio::task::spawn_blocking(move || stop.join().unwrap()).await.unwrap();
		store.release();
	}

	node_a.start().unwrap();
	let addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();
	let _ = node_a.connect(node_b.node_id(), addr_b, true);
	wait_for_channel_ready_to_send(&node_a, &node_b, 600_000).await;
	assert!(payment_succeeds(payer, payment_id).await, "payment started before stop");

	let payment_id = send_payment(&node_a, &node_b, 500_000);
	assert!(payment_succeeds(&node_a, payment_id).await, "payment after restart");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_with_monitor_update_write_in_flight() {
	do_payment_across_restart(HeldWrite::MonitorUpdateCommitted).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_with_monitor_update_write_not_yet_issued() {
	do_payment_across_restart(HeldWrite::MonitorUpdateUnissued).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_with_monitor_update_flushed_during_shutdown() {
	do_payment_across_restart(HeldWrite::ChannelManagerCommitted).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stop_with_initial_monitor_write_in_flight() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// B opens a channel to A, whose initial `ChannelMonitor` write is held.
	let config_a = random_config();
	let store = HoldingStore::new(&config_a.node_config.storage_dir_path);
	let node_a = Arc::new(setup_node_with_store(&chain_source, config_a, store.clone()));
	let node_b = setup_node(&chain_source, random_config());

	let address_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_b],
		Amount::from_sat(5_000_000),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	store.hold(HeldWrite::MonitorCommitted);
	let addr_a = node_a.listening_addresses().unwrap().first().unwrap().clone();
	node_b.open_channel(node_a.node_id(), addr_a, 4_000_000, None, None, None).unwrap();
	store.wait_until_held().await;

	// Only let the held write finish once `stop` has returned.
	let stop = stop_in_thread(&node_a);
	tokio::task::spawn_blocking(move || stop.join().unwrap()).await.unwrap();
	store.release();

	node_a.start().unwrap();
	let addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();
	let _ = node_a.connect(node_b.node_id(), addr_b, true);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	wait_for_channel_ready_to_send(&node_b, &node_a, 1_000_000).await;

	let payment_id = send_payment(&node_b, &node_a, 1_000_000);
	assert!(payment_succeeds(&node_b, payment_id).await, "payment over the new channel");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}
