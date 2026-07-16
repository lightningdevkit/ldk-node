// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::hashes::Hash;
use bitcoin::{Address, Amount, ScriptBuf, Txid};
use common::logging::{init_log_logger, validate_log_entry, MultiNodeLogger, TestLogWriter};
use common::{
	bump_fee_and_broadcast, distribute_funds_unconfirmed, do_channel_full_cycle,
	expect_channel_pending_event, expect_channel_ready_event, expect_channel_ready_events,
	expect_event, expect_payment_claimable_event, expect_payment_received_event,
	expect_payment_successful_event, expect_splice_negotiated_event, generate_blocks_and_wait,
	generate_listening_addresses, invalidate_blocks, open_channel, open_channel_push_amt,
	open_channel_with_all, premine_and_distribute_funds, premine_blocks, prepare_rbf,
	random_chain_source, random_config, setup_bitcoind_and_electrsd, setup_builder, setup_node,
	setup_two_nodes, splice_in_with_all, wait_for_block, wait_for_tx, InMemoryStore,
	TestChainSource, TestConfig, TestStoreType, TestSyncStore,
};
use electrsd::corepc_node::{self, Node as BitcoinD};
use electrsd::ElectrsD;
use ldk_node::config::{AsyncPaymentsRole, EsploraSyncConfig, DEFAULT_FULL_SCAN_STOP_GAP};
use ldk_node::entropy::NodeEntropy;
use ldk_node::liquidity::LSPS2ServiceConfig;
use ldk_node::payment::{
	ConfirmationStatus, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
	TransactionType, UnifiedPaymentResult,
};
use ldk_node::{BuildError, Builder, Event, Node, NodeError, ReserveType};
use lightning::ln::channelmanager::PaymentId;
use lightning::routing::gossip::{NodeAlias, NodeId};
use lightning::routing::router::RouteParametersConfig;
use lightning::util::persist::{KVStore, PageToken, PaginatedKVStore, PaginatedListResponse};
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use lightning_types::payment::{PaymentHash, PaymentPreimage};
use log::LevelFilter;
use serde_json::json;

/// Waits until `node` has classified the funding broadcast `funding_txid` (a channel open or splice
/// candidate) into a payment record carrying a `tx_type`. Classification runs off the broadcaster's
/// queue, which can lag a `sync_wallets` call under load — and for a splice the counterparty also
/// broadcasts the same tx, so a racing sync can see it before this node classifies. Waiting here
/// keeps the next sync on the funding short-circuit instead of recording a generic on-chain payment
/// that clobbers the classification.
async fn wait_for_classified_funding_payment(node: &Node, funding_txid: Txid) {
	let poll = async {
		loop {
			let classified = node.list_payments().into_iter().any(|p| {
				matches!(
					p.kind,
					PaymentKind::Onchain { txid, tx_type: Some(_), .. } if txid == funding_txid
				)
			});
			if classified {
				return;
			}
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
		}
	};
	tokio::time::timeout(std::time::Duration::from_secs(common::INTEROP_TIMEOUT_SECS), poll)
		.await
		.unwrap_or_else(|_| {
			panic!("timed out waiting for funding broadcast {} to be classified", funding_txid)
		});
}

#[derive(Clone)]
struct ContendedStore {
	inner: Arc<InMemoryStore>,
	serializer: Arc<tokio::sync::RwLock<()>>,
	block_writes: Arc<AtomicBool>,
	wallet_write_started: Arc<tokio::sync::Notify>,
}

impl KVStore for ContendedStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, lightning::io::Error>> + 'static + Send {
		KVStore::read(&*self.inner, primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), lightning::io::Error>> + 'static + Send {
		let inner = Arc::clone(&self.inner);
		let serializer = Arc::clone(&self.serializer);
		let block_writes = Arc::clone(&self.block_writes);
		let wallet_write_started = Arc::clone(&self.wallet_write_started);
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		async move {
			if block_writes.load(Ordering::Acquire) {
				wallet_write_started.notify_one();
			}
			let _guard = serializer.read().await;
			KVStore::write(&*inner, &primary_namespace, &secondary_namespace, &key, buf).await
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

impl PaginatedKVStore for ContendedStore {
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

#[test]
fn wallet_store_contention_does_not_stall_runtime() {
	let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
	let (result_sender, result_receiver) = mpsc::sync_channel(1);
	std::thread::spawn(move || {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(1)
			.enable_all()
			.build()
			.expect("test runtime");
		let result = runtime.block_on(async move {
			let test_config = random_config();
			let builder = Builder::from_config(test_config.node_config.clone());
			let store = ContendedStore {
				inner: Arc::new(InMemoryStore::new()),
				serializer: Arc::new(tokio::sync::RwLock::new(())),
				block_writes: Arc::new(AtomicBool::new(false)),
				wallet_write_started: Arc::new(tokio::sync::Notify::new()),
			};
			let node = builder
				.build_with_store(test_config.node_entropy.into(), store.clone())
				.map_err(|e| format!("failed to build node: {e:?}"))?;
			#[cfg(not(feature = "uniffi"))]
			let node = Arc::new(node);

			let serializer = Arc::clone(&store.serializer);
			let release_store = Arc::new(tokio::sync::Notify::new());
			let release_store_task = Arc::clone(&release_store);
			let (store_locked_sender, store_locked_receiver) = tokio::sync::oneshot::channel();
			tokio::spawn(async move {
				let _guard = serializer.write().await;
				let _ = store_locked_sender.send(());
				release_store_task.notified().await;
			});
			store_locked_receiver.await.map_err(|e| format!("store lock task failed: {e}"))?;
			store.block_writes.store(true, Ordering::Release);
			let _ = ready_sender.send(());

			let address_node = Arc::clone(&node);
			let (address_sender, address_receiver) = tokio::sync::oneshot::channel();
			std::thread::spawn(move || {
				let result = address_node.onchain_payment().new_address().map(|_| ());
				let _ = address_sender.send(result);
			});
			store.wallet_write_started.notified().await;

			let balances_node = Arc::clone(&node);
			let (balances_started_sender, balances_started_receiver) =
				tokio::sync::oneshot::channel();
			let balances_task = tokio::spawn(async move {
				let _ = balances_started_sender.send(());
				balances_node.list_balances()
			});
			balances_started_receiver
				.await
				.map_err(|e| format!("balance task failed to start: {e}"))?;
			std::thread::spawn(move || {
				std::thread::sleep(Duration::from_millis(100));
				release_store.notify_one();
			});
			balances_task.await.map_err(|e| format!("balance task failed: {e}"))?;
			address_receiver
				.await
				.map_err(|e| format!("address task failed: {e}"))?
				.map_err(|e| format!("address generation failed: {e}"))
		});
		let _ = result_sender.send(result);
	});

	ready_receiver
		.recv_timeout(Duration::from_secs(30))
		.expect("failed to set up wallet contention test");
	let result = result_receiver
		.recv_timeout(Duration::from_secs(3))
		.expect("wallet contention stalled the single-thread runtime");
	result.unwrap_or_else(|e| panic!("wallet contention test failed: {e}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);
	do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		false,
		true,
		false,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_force_close() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);
	do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		false,
		true,
		true,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_force_close_trusted_no_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true);
	do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		false,
		true,
		true,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn peer_removed_when_counterparty_force_closes_last_channel() {
	// When we open a channel outbound, we persist the counterparty so the background
	// reconnection task can reach them. If the counterparty then force-closes what turns out
	// to be their last channel with us, the channel is terminal and there is nothing left for
	// `channel_reestablish` to recover, so the peer should be dropped from the store rather
	// than reconnected to forever.
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();

	// node_a opens the channel, so node_a persists node_b in its peer store.
	open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	assert!(
		node_a.list_peers().iter().any(|p| p.node_id == node_b.node_id() && p.is_persisted),
		"node_a should persist node_b after opening a channel to it"
	);

	// The counterparty force-closes their last channel with us.
	node_b.force_close_channel(&user_channel_id_b, node_a.node_id(), None).unwrap();

	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	// node_a should have dropped node_b from its peer store. We assert on `is_persisted` rather
	// than peer presence so a lingering transient TCP connection doesn't mask the removal.
	assert!(
		!node_a.list_peers().iter().any(|p| p.node_id == node_b.node_id() && p.is_persisted),
		"node_a should drop node_b from its peer store after node_b force-closed the last channel"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_0conf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, true, false);
	do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		true,
		false,
		true,
		false,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_0reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);
	do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		true,
		true,
		false,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_full_cycle_0conf_0reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, true, false);
	do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		true,
		true,
		true,
		false,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn channel_open_fails_when_funds_insufficient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 100_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	println!("\nA -- open_channel -> B");
	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			120000,
			None,
			None,
		)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn multi_hop_sending() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	// Setup and fund 5 nodes
	let mut nodes = Vec::new();
	for _ in 0..5 {
		let config = random_config();
		let mut sync_config = EsploraSyncConfig::default();
		sync_config.background_sync_config = None;
		setup_builder!(builder, config.node_config);
		builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
		let node = builder.build(config.node_entropy.into()).unwrap();
		node.start().unwrap();
		nodes.push(node);
	}

	let addresses = nodes.iter().map(|n| n.onchain_payment().new_address().unwrap()).collect();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		addresses,
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	for n in &nodes {
		n.sync_wallets().unwrap();
		assert_eq!(n.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
		assert_eq!(n.next_event(), None);
	}

	// Setup channel topology:
	//                    (1M:0)- N2 -(1M:0)
	//                   /                  \
	//  N0 -(100k:0)-> N1                    N4
	//                   \                  /
	//                    (1M:0)- N3 -(1M:0)

	open_channel(&nodes[0], &nodes[1], 100_000, true, &electrsd).await;
	open_channel(&nodes[1], &nodes[2], 1_000_000, true, &electrsd).await;
	// We need to sync wallets in-between back-to-back channel opens from the same node so BDK
	// wallet picks up on the broadcast funding tx and doesn't double-spend itself.
	//
	// TODO: Remove once fixed in BDK.
	nodes[1].sync_wallets().unwrap();
	open_channel(&nodes[1], &nodes[3], 1_000_000, true, &electrsd).await;
	open_channel(&nodes[2], &nodes[4], 1_000_000, true, &electrsd).await;
	open_channel(&nodes[3], &nodes[4], 1_000_000, true, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	for n in &nodes {
		n.sync_wallets().unwrap();
	}

	expect_event!(nodes[0], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[2], ChannelReady);
	expect_event!(nodes[2], ChannelReady);
	expect_event!(nodes[3], ChannelReady);
	expect_event!(nodes[3], ChannelReady);
	expect_event!(nodes[4], ChannelReady);
	expect_event!(nodes[4], ChannelReady);

	// Sleep a bit for gossip to propagate.
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let route_params = RouteParametersConfig {
		max_total_routing_fee_msat: Some(75_000),
		max_total_cltv_expiry_delta: 1000,
		max_path_count: 10,
		max_channel_saturation_power_of_half: 2,
	};

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let invoice = nodes[4]
		.bolt11_payment()
		.receive(2_500_000, &invoice_description.clone().into(), 9217)
		.unwrap();
	nodes[0].bolt11_payment().send(&invoice, Some(route_params)).unwrap();

	expect_event!(nodes[1], PaymentForwarded);

	// We expect that the payment goes through N2 or N3, so we check both for the PaymentForwarded event.
	let node_2_fwd_event = matches!(nodes[2].next_event(), Some(Event::PaymentForwarded { .. }));
	let node_3_fwd_event = matches!(nodes[3].next_event(), Some(Event::PaymentForwarded { .. }));
	assert!(node_2_fwd_event || node_3_fwd_event);

	let payment_id = expect_payment_received_event!(&nodes[4], 2_500_000);
	let fee_paid_msat = Some(2000);
	expect_payment_successful_event!(nodes[0], payment_id, Some(fee_paid_msat));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn split_underpaid_bolt11_payment() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);
	let node_c = setup_node(&chain_source, random_config());

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let addr_c = node_c.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b, addr_c],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	for node in [&node_a, &node_b, &node_c] {
		node.sync_wallets().unwrap();
		assert_eq!(node.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	}

	// The receiver opens both channels and pushes liquidity to both payers so each payer can send
	// half of the invoice back.
	let channel_amount_sat = 1_000_000;
	let push_amount_msat = Some(500_000_000);
	for payer in [&node_a, &node_b] {
		node_c
			.open_channel(
				payer.node_id(),
				payer.listening_addresses().unwrap().first().unwrap().clone(),
				channel_amount_sat,
				push_amount_msat,
				None,
			)
			.unwrap();

		let funding_txo_c = expect_channel_pending_event!(node_c, payer.node_id());
		let funding_txo_payer = expect_channel_pending_event!(payer, node_c.node_id());
		assert_eq!(funding_txo_c, funding_txo_payer);
		wait_for_tx(&electrsd.client, funding_txo_c.txid).await;

		node_c.sync_wallets().unwrap();
	}

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	for node in [&node_a, &node_b, &node_c] {
		node.sync_wallets().unwrap();
	}

	expect_channel_ready_events!(node_c, node_a.node_id(), node_b.node_id());
	expect_channel_ready_event!(node_a, node_c.node_id());
	expect_channel_ready_event!(node_b, node_c.node_id());

	let amount_msat = 100_000_000;
	let half_amount_msat = amount_msat / 2;
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("split")).unwrap());
	let invoice =
		node_c.bolt11_payment().receive(amount_msat, &invoice_description.into(), 3600).unwrap();

	// Each payer sends only half the invoice amount, while declaring the full invoice amount as
	// the total MPP value. The receiver should claim only once both HTLCs arrive.
	let payment_id_a = node_a
		.bolt11_payment()
		.send_using_amount_underpaying(&invoice, half_amount_msat, None)
		.unwrap();
	let payment_id_b = node_b
		.bolt11_payment()
		.send_using_amount_underpaying(&invoice, half_amount_msat, None)
		.unwrap();

	let receiver_payment_id = expect_payment_received_event!(node_c, amount_msat);
	assert_eq!(receiver_payment_id, Some(PaymentId(invoice.payment_hash().0)));
	expect_payment_successful_event!(node_a, Some(payment_id_a), None);
	expect_payment_successful_event!(node_b, Some(payment_id_b), None);

	// The receiver records the full invoice amount; each payer records only its own half.
	let receiver_payments =
		node_c.list_payments_with_filter(|p| p.id == receiver_payment_id.unwrap());
	assert_eq!(receiver_payments.len(), 1);
	assert_eq!(receiver_payments.first().unwrap().amount_msat, Some(amount_msat));

	let node_a_payments = node_a.list_payments_with_filter(|p| p.id == payment_id_a);
	assert_eq!(node_a_payments.len(), 1);
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(half_amount_msat));

	let node_b_payments = node_b.list_payments_with_filter(|p| p.id == payment_id_b);
	assert_eq!(node_b_payments.len(), 1);
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(half_amount_msat));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn start_stop_reinit() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let test_sync_store = TestSyncStore::new(config.node_config.storage_dir_path.clone().into());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;
	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

	let node =
		builder.build_with_store(config.node_entropy.into(), test_sync_store.clone()).unwrap();
	node.start().unwrap();

	let expected_node_id = node.node_id();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	let funding_address = node.onchain_payment().new_address().unwrap();

	assert_eq!(node.list_balances().total_onchain_balance_sats, 0);

	let expected_amount = Amount::from_sat(100000);
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![funding_address],
		expected_amount,
	)
	.await;

	node.sync_wallets().unwrap();
	assert_eq!(node.list_balances().spendable_onchain_balance_sats, expected_amount.to_sat());

	let log_file = format!("{}/ldk_node.log", config.node_config.clone().storage_dir_path);
	assert!(std::path::Path::new(&log_file).exists());

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));

	node.start().unwrap();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));
	drop(node);

	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

	let reinitialized_node =
		builder.build_with_store(config.node_entropy.into(), test_sync_store).unwrap();
	reinitialized_node.start().unwrap();
	assert_eq!(reinitialized_node.node_id(), expected_node_id);

	assert_eq!(
		reinitialized_node.list_balances().spendable_onchain_balance_sats,
		expected_amount.to_sat()
	);

	reinitialized_node.sync_wallets().unwrap();
	assert_eq!(
		reinitialized_node.list_balances().spendable_onchain_balance_sats,
		expected_amount.to_sat()
	);

	reinitialized_node.stop().unwrap();
}

// The scores-sync task runs alongside LDK's background processor, which claims the runtime's
// single background-processor slot; startup must not panic with both configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn start_stop_with_pathfinding_scores_sync() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;
	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	builder.set_pathfinding_scores_source(esplora_url);

	let node = builder.build(config.node_entropy.into()).unwrap();
	node.start().unwrap();
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	// This is a Bitcoin Testnet address. Sending funds to this address from the Regtest network will fail
	let static_address = "tb1q0d40e5rta4fty63z64gztf8c3v20cvet6v2jdh";
	let unchecked_address = Address::<NetworkUnchecked>::from_str(static_address).unwrap();
	let addr_c = unchecked_address.assume_checked();

	let premine_amount_sat = 1_100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let node_a_payments = node_a.list_payments();
	let node_b_payments = node_b.list_payments();
	for payments in [&node_a_payments, &node_b_payments] {
		assert_eq!(payments.len(), 1)
	}
	for p in [node_a_payments.first().unwrap(), node_b_payments.first().unwrap()] {
		assert_eq!(p.amount_msat, Some(premine_amount_sat * 1000));
		assert_eq!(p.direction, PaymentDirection::Inbound);
		// We got only 1-conf here, so we're only pending for now.
		assert_eq!(p.status, PaymentStatus::Pending);
		match p.kind {
			PaymentKind::Onchain { status, .. } => {
				assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			},
			_ => panic!("Unexpected payment kind"),
		}
	}

	let channel_amount_sat = 1_000_000;
	let reserve_amount_sat = 25_000;
	open_channel(&node_b, &node_a, channel_amount_sat, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 1);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 2);

	let onchain_fee_buffer_sat = 1000;
	let expected_node_a_balance = premine_amount_sat - reserve_amount_sat;
	let expected_node_b_balance_lower =
		premine_amount_sat - channel_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat;
	let expected_node_b_balance_upper =
		premine_amount_sat - channel_amount_sat - reserve_amount_sat;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.onchain_payment().send_to_address(&addr_b, expected_node_a_balance + 1, None)
	);

	assert_eq!(
		Err(NodeError::InvalidAddress),
		node_a.onchain_payment().send_to_address(&addr_c, expected_node_a_balance + 1, None)
	);

	assert_eq!(
		Err(NodeError::InvalidAddress),
		node_a.onchain_payment().send_all_to_address(&addr_c, true, None)
	);

	let amount_to_send_sats = 54321;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(txid.to_byte_array());
	let payment_a = node_a.payment(&payment_id).unwrap();
	assert_eq!(payment_a.status, PaymentStatus::Pending);
	match payment_a.kind {
		PaymentKind::Onchain { status, tx_type, .. } => {
			assert!(matches!(status, ConfirmationStatus::Unconfirmed));
			assert_eq!(tx_type, None);
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert!(payment_a.fee_paid_msat > Some(0));
	let payment_b = node_b.payment(&payment_id).unwrap();
	assert_eq!(payment_b.status, PaymentStatus::Pending);
	match payment_b.kind {
		PaymentKind::Onchain { status, tx_type, .. } => {
			assert!(matches!(status, ConfirmationStatus::Unconfirmed));
			assert_eq!(tx_type, None);
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert!(payment_b.fee_paid_msat > Some(0));
	assert_eq!(payment_a.amount_msat, Some(amount_to_send_sats * 1000));
	assert_eq!(payment_a.amount_msat, payment_b.amount_msat);
	assert_eq!(payment_a.fee_paid_msat, payment_b.fee_paid_msat);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_a_balance = expected_node_a_balance + amount_to_send_sats;
	let expected_node_b_balance_lower = expected_node_b_balance_lower - amount_to_send_sats;
	let expected_node_b_balance_upper = expected_node_b_balance_upper - amount_to_send_sats;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 2);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 3);

	let payment_a = node_a.payment(&payment_id).unwrap();
	match payment_a.kind {
		PaymentKind::Onchain { txid: _txid, status, tx_type } => {
			assert_eq!(_txid, txid);
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			assert_eq!(tx_type, None);
		},
		_ => panic!("Unexpected payment kind"),
	}

	let payment_b = node_b.payment(&payment_id).unwrap();
	match payment_b.kind {
		PaymentKind::Onchain { txid: _txid, status, tx_type } => {
			assert_eq!(_txid, txid);
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			assert_eq!(tx_type, None);
		},
		_ => panic!("Unexpected payment kind"),
	}

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, true, None).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	wait_for_tx(&electrsd.client, txid).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_b_balance_lower = expected_node_b_balance_lower + expected_node_a_balance;
	let expected_node_b_balance_upper = expected_node_b_balance_upper + expected_node_a_balance;
	let expected_node_a_balance = 0;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, reserve_amount_sat);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 3);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 4);

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, false, None).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	wait_for_tx(&electrsd.client, txid).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_b_balance_lower = expected_node_b_balance_lower + reserve_amount_sat;
	let expected_node_b_balance_upper = expected_node_b_balance_upper + reserve_amount_sat;
	let expected_node_a_balance = 0;

	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 4);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn reorged_onchain_payment_returns_to_unconfirmed() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 500_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let amount_to_send_sats = 100_000;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(txid.to_byte_array());
	for node in [&node_a, &node_b] {
		let payment = node.payment(&payment_id).unwrap();
		assert_eq!(payment.status, PaymentStatus::Pending);
		match payment.kind {
			PaymentKind::Onchain { status, .. } => {
				assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			},
			_ => panic!("Unexpected payment kind"),
		}
	}

	let original_height =
		bitcoind.client.get_blockchain_info().expect("failed to get blockchain info").blocks;
	invalidate_blocks(&bitcoind.client, 1);
	let replacement_address = bitcoind.client.new_address().expect("failed to get new address");
	for _ in 0..2 {
		let _res: serde_json::Value = bitcoind
			.client
			.call("generateblock", &[json!(replacement_address.to_string()), json!([])])
			.expect("failed to generate empty block");
	}
	wait_for_block(&bitcoind.client, &electrsd.client, original_height as usize + 1).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	for node in [&node_a, &node_b] {
		let payment = node.payment(&payment_id).unwrap();
		assert_eq!(payment.status, PaymentStatus::Pending);
		match payment.kind {
			PaymentKind::Onchain { status, .. } => {
				assert!(matches!(status, ConfirmationStatus::Unconfirmed));
			},
			_ => panic!("Unexpected payment kind"),
		}
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_send_all_retains_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	// Setup nodes
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;
	let reserve_amount_sat = 25_000;
	let onchain_fee_buffer_sat = 1000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	// Send all over, with 0 reserve as we don't have any channels open.
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, true, None).unwrap();

	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	// Check node a sent all and node b received it
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat * 2 - onchain_fee_buffer_sat)..=(premine_amount_sat * 2))
		.contains(&node_b.list_balances().spendable_onchain_balance_sats));

	// Refill to make sure we have enough reserve for the channel open.
	let txid = bitcoind
		.client
		.send_to_address(&addr_a, Amount::from_sat(reserve_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, reserve_amount_sat);

	// Open a channel.
	open_channel(&node_b, &node_a, premine_amount_sat, false, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Check node a sent all and node b received it
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat)
		..=premine_amount_sat)
		.contains(&node_b.list_balances().spendable_onchain_balance_sats));

	// Send all over again, this time ensuring the reserve is accounted for
	let txid = node_b.onchain_payment().send_all_to_address(&addr_a, true, None).unwrap();

	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Check node b sent all and node a received it
	assert_eq!(node_b.list_balances().total_onchain_balance_sats, reserve_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat)
		..=premine_amount_sat)
		.contains(&node_a.list_balances().spendable_onchain_balance_sats));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_wallet_recovery() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let original_config = random_config();
	let original_node_entropy = original_config.node_entropy;
	let original_node = setup_node(&chain_source, original_config);

	let premine_amount_sat = 100_000;

	let addr_1 = original_node.onchain_payment().new_address().unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_1],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	original_node.sync_wallets().unwrap();
	assert_eq!(original_node.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let addr_2 = original_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(&addr_2, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	original_node.sync_wallets().unwrap();
	assert_eq!(
		original_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	original_node.stop().unwrap();
	drop(original_node);

	// Now we start from scratch, only the seed remains the same.
	let mut recovered_config = random_config();
	recovered_config.node_entropy = original_node_entropy;
	recovered_config.wallet_rescan_from_height = Some(0);
	let recovered_node = setup_node(&chain_source, recovered_config);

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	// Check we sync even when skipping some addresses.
	let _addr_3 = recovered_node.onchain_payment().new_address().unwrap();
	let _addr_4 = recovered_node.onchain_payment().new_address().unwrap();
	let _addr_5 = recovered_node.onchain_payment().new_address().unwrap();
	let addr_6 = recovered_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(&addr_6, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 3
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_wallet_force_full_scan_rediscovers_esplora_funds() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	premine_blocks(&bitcoind.client, &electrsd.client).await;

	let address_source_config = random_config();
	let node_entropy = address_source_config.node_entropy;
	let address_source_node = setup_node(&chain_source, address_source_config);
	let addr_1 = address_source_node.onchain_payment().new_address().unwrap();
	let addr_2 = address_source_node.onchain_payment().new_address().unwrap();
	address_source_node.stop().unwrap();
	drop(address_source_node);

	let premine_amount_sat = 100_000;
	let mut stale_config = random_config();
	stale_config.node_entropy = node_entropy;
	stale_config.store_type = TestStoreType::Sqlite;
	let stale_node = setup_node(&chain_source, stale_config.clone());
	stale_node.sync_wallets().unwrap();
	assert_eq!(stale_node.list_balances().spendable_onchain_balance_sats, 0);
	stale_node.stop().unwrap();
	drop(stale_node);

	let txid_1 = bitcoind
		.client
		.send_to_address(&addr_1, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid_1).await;
	let txid_2 = bitcoind
		.client
		.send_to_address(&addr_2, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid_2).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	let normal_node = setup_node(&chain_source, stale_config.clone());
	normal_node.sync_wallets().unwrap();
	assert_eq!(
		normal_node.list_balances().spendable_onchain_balance_sats,
		0,
		"normal incremental sync should not rediscover previously-unknown addresses"
	);
	normal_node.stop().unwrap();
	drop(normal_node);

	stale_config.force_wallet_full_scan = true;
	let recovered_node = setup_node(&chain_source, stale_config);
	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2,
		"forced full scan should rediscover funds sent to previously-unknown addresses"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_wallet_full_scan_stop_gap_recovers_far_esplora_and_electrum_funds() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	premine_blocks(&bitcoind.client, &electrsd.client).await;

	do_onchain_wallet_full_scan_stop_gap_recovers_far_funds(
		TestChainSource::Esplora(&electrsd),
		&bitcoind,
		&electrsd,
	)
	.await;
	do_onchain_wallet_full_scan_stop_gap_recovers_far_funds(
		TestChainSource::Electrum(&electrsd),
		&bitcoind,
		&electrsd,
	)
	.await;
}

async fn do_onchain_wallet_full_scan_stop_gap_recovers_far_funds(
	chain_source: TestChainSource<'_>, bitcoind: &BitcoinD, electrsd: &ElectrsD,
) {
	let configured_stop_gap = DEFAULT_FULL_SCAN_STOP_GAP + 5;

	let address_source_config = random_config();
	let node_entropy = address_source_config.node_entropy;
	let address_source_node = setup_node(&chain_source, address_source_config);
	let mut far_address = None;
	for _ in 0..configured_stop_gap {
		far_address = Some(address_source_node.onchain_payment().new_address().unwrap());
	}
	address_source_node.stop().unwrap();
	drop(address_source_node);
	let far_address = far_address.unwrap();

	let premine_amount_sat = 100_000;
	let txid = bitcoind
		.client
		.send_to_address(&far_address, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	let mut default_gap_config = random_config();
	default_gap_config.node_entropy = node_entropy.clone();
	let default_gap_node = setup_node(&chain_source, default_gap_config);
	default_gap_node.sync_wallets().unwrap();
	assert_eq!(
		default_gap_node.list_balances().spendable_onchain_balance_sats,
		0,
		"default full-scan stop gap should not recover funds past its address gap"
	);
	default_gap_node.stop().unwrap();
	drop(default_gap_node);

	let mut configured_gap_config = random_config();
	configured_gap_config.node_entropy = node_entropy;
	configured_gap_config.full_scan_stop_gap = Some(configured_stop_gap);
	let configured_gap_node = setup_node(&chain_source, configured_gap_config);
	configured_gap_node.sync_wallets().unwrap();
	assert_eq!(
		configured_gap_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat,
		"configured full-scan stop gap should recover funds past the default address gap"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_wallet_recovery_rescans_from_birthday_height() {
	// End-to-end test for `wallet_rescan_from_height` against a bitcoind chain source. The
	// scenario:
	//
	// 1. Create a node at some "birthday" height and generate two receive addresses.
	// 2. Shut the node down and drop all persisted state except the seed.
	// 3. Advance the chain past the birthday.
	// 4. Send funds to the addresses generated at the birthday height and confirm them.
	// 5. Restart a fresh node with just the seed and no rescan height. Its wallet birthday
	//    is pinned at the current tip, which is above the blocks containing the funding
	//    transactions — so the node must not see the funds.
	// 6. Restart again with `wallet_rescan_from_height: Some(birthday)`. Now the wallet must
	//    find and report both funding transactions.
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	// We specifically exercise the bitcoind RPC backend because that's where
	// `rescan_from_height` is honored precisely (via `get_block_hash_by_height`).
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);

	// Mine the initial 101 blocks so bitcoind's wallet can fund our later sends.
	premine_blocks(&bitcoind.client, &electrsd.client).await;

	// Step 1: bring up an "original" node at the birthday height and generate addresses.
	let original_config = random_config();
	let original_node_entropy = original_config.node_entropy;
	let original_node = setup_node(&chain_source, original_config);

	let premine_amount_sat = 100_000;

	let addr_1 = original_node.onchain_payment().new_address().unwrap();
	let addr_2 = original_node.onchain_payment().new_address().unwrap();

	let birthday_height: u32 = bitcoind
		.client
		.get_blockchain_info()
		.expect("failed to get blockchain info")
		.blocks
		.try_into()
		.unwrap();

	// Step 2: shut the node down and drop its state.
	original_node.stop().unwrap();
	drop(original_node);

	// Step 3: advance the chain past the birthday, so a fresh node would otherwise pin its
	// wallet birthday at a height above the funding transactions in step 4.
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 10).await;

	// Step 4: fund both addresses and confirm them.
	let txid_1 = bitcoind
		.client
		.send_to_address(&addr_1, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid_1).await;
	let txid_2 = bitcoind
		.client
		.send_to_address(&addr_2, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid_2).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	// Step 5: restart a fresh node with only the seed and no rescan height. It must NOT see
	// the funds, because its wallet birthday sits above the funding transactions.
	let mut pinned_config = random_config();
	pinned_config.node_entropy = original_node_entropy;
	let pinned_node = setup_node(&chain_source, pinned_config);
	pinned_node.sync_wallets().unwrap();
	assert_eq!(
		pinned_node.list_balances().spendable_onchain_balance_sats,
		0,
		"fresh node without rescan height should not find funds below its wallet birthday"
	);
	pinned_node.stop().unwrap();
	drop(pinned_node);

	// Step 6: restart with a rescan height set to the birthday height. Funds must be
	// re-discovered.
	let mut recovered_config = random_config();
	recovered_config.node_entropy = original_node_entropy;
	recovered_config.wallet_rescan_from_height = Some(birthday_height);
	let recovered_node = setup_node(&chain_source, recovered_config);
	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2,
		"node recovered with rescan_from_height should see funds sent to pre-birthday addresses"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn build_fails_when_wallet_rescan_height_is_above_tip() {
	let (bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let current_tip_height: u32 = bitcoind
		.client
		.get_blockchain_info()
		.expect("failed to get blockchain info")
		.blocks
		.try_into()
		.unwrap();

	let config = random_config();
	let entropy = config.node_entropy;

	setup_builder!(builder, config.node_config);
	let values = bitcoind.params.get_cookie_values().unwrap().unwrap();
	builder.set_chain_source_bitcoind_rpc(
		bitcoind.params.rpc_socket.ip().to_string(),
		bitcoind.params.rpc_socket.port(),
		values.user,
		values.password,
		Some(current_tip_height + 1),
	);

	match builder.build(entropy.into()) {
		Err(err) => {
			assert_eq!(err, BuildError::WalletRescanHeightTooHigh);
			assert_eq!(err.to_string(), "Wallet rescan height is above the current chain tip.");
		},
		Ok(_) => panic!("expected build to fail for future wallet rescan height"),
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn build_aborts_on_first_startup_bitcoind_tip_fetch_failure() {
	// A fresh node pointed at an unreachable bitcoind RPC endpoint must not silently
	// fall back to genesis as the wallet birthday. The build must abort cleanly so the
	// misconfiguration surfaces immediately.
	let config = random_config();
	let entropy = config.node_entropy;

	setup_builder!(builder, config.node_config);
	// Pick a localhost port that is extremely unlikely to be bound. The kernel will
	// refuse the connection immediately so the test does not have to wait for the
	// chain-polling timeout.
	let unreachable_port: u16 = 1;
	builder.set_chain_source_bitcoind_rpc(
		"127.0.0.1".to_string(),
		unreachable_port,
		"user".to_string(),
		"password".to_string(),
		None,
	);

	let res = builder.build(entropy.into());
	match res {
		Err(BuildError::ChainTipFetchFailed) => {},
		other => panic!(
			"expected BuildError::ChainTipFetchFailed on fresh node with unreachable bitcoind, got {:?}",
			other.map(|_| "Ok(_)")
		),
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_via_mempool() {
	run_rbf_test(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_via_direct_block_insertion() {
	run_rbf_test(true).await;
}

// `is_insert_block`:
// - `true`: transaction is mined immediately (no mempool), testing confirmed-Tx handling.
// - `false`: transaction stays in mempool until confirmation, testing unconfirmed-Tx handling.
async fn run_rbf_test(is_insert_block: bool) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
	let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
	let chain_source_esplora = TestChainSource::Esplora(&electrsd);

	macro_rules! config_node {
		($chain_source:expr) => {{
			let config_a = random_config();
			let node = setup_node(&$chain_source, config_a);
			node
		}};
	}
	let nodes = vec![
		config_node!(chain_source_electrsd),
		config_node!(chain_source_bitcoind),
		config_node!(chain_source_esplora),
	];

	let (bitcoind, electrs) = (&bitcoind.client, &electrsd.client);
	premine_blocks(bitcoind, electrs).await;

	// Helpers declaration before starting the test
	let all_addrs =
		nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect::<Vec<_>>();
	let amount_sat = 2_100_000;
	let mut txid;
	macro_rules! distribute_funds_all_nodes {
		() => {
			txid = distribute_funds_unconfirmed(
				bitcoind,
				electrs,
				all_addrs.clone(),
				Amount::from_sat(amount_sat),
			)
			.await;
		};
	}
	macro_rules! validate_balances {
		($expected_balance_sat:expr, $is_spendable:expr) => {
			let spend_balance = if $is_spendable { $expected_balance_sat } else { 0 };
			for node in &nodes {
				node.sync_wallets().unwrap();
				let balances = node.list_balances();
				assert_eq!(balances.spendable_onchain_balance_sats, spend_balance);
				assert_eq!(balances.total_onchain_balance_sats, $expected_balance_sat);
			}
		};
	}

	let scripts_buf: HashSet<ScriptBuf> =
		all_addrs.iter().map(|addr| addr.script_pubkey()).collect();
	let mut tx;
	let mut fee_output_index;

	// Modify the output to the nodes
	distribute_funds_all_nodes!();
	validate_balances!(amount_sat, false);
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	tx.output.iter_mut().for_each(|output| {
		if scripts_buf.contains(&output.script_pubkey) {
			let new_addr = bitcoind.new_address().unwrap();
			output.script_pubkey = new_addr.script_pubkey();
		}
	});
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	validate_balances!(0, is_insert_block);

	// Not modifying the output scripts, but still bumping the fee.
	distribute_funds_all_nodes!();
	validate_balances!(amount_sat, false);
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	validate_balances!(amount_sat, is_insert_block);

	let mut final_amount_sat = amount_sat * 2;
	let value_sat = 21_000;

	// Increase the value of the nodes' outputs
	distribute_funds_all_nodes!();
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	tx.output.iter_mut().for_each(|output| {
		if scripts_buf.contains(&output.script_pubkey) {
			output.value = Amount::from_sat(output.value.to_sat() + value_sat);
		}
	});
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	final_amount_sat += value_sat;
	validate_balances!(final_amount_sat, is_insert_block);

	// Decreases the value of the nodes' outputs
	distribute_funds_all_nodes!();
	final_amount_sat += amount_sat;
	(tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
	tx.output.iter_mut().for_each(|output| {
		if scripts_buf.contains(&output.script_pubkey) {
			output.value = Amount::from_sat(output.value.to_sat() - value_sat);
		}
	});
	bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_insert_block).await;
	final_amount_sat -= value_sat;
	validate_balances!(final_amount_sat, is_insert_block);

	if !is_insert_block {
		generate_blocks_and_wait(bitcoind, electrs, 1).await;
		validate_balances!(final_amount_sat, true);
	}

	// Check if it is possible to send all funds from the node
	let mut txids = Vec::new();
	let addr = bitcoind.new_address().unwrap();
	nodes.iter().for_each(|node| {
		let txid = node.onchain_payment().send_all_to_address(&addr, true, None).unwrap();
		txids.push(txid);
	});
	for txid in txids {
		wait_for_tx(electrs, txid).await;
	}
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	validate_balances!(0, true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sign_verify_msg() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let node = setup_node(&chain_source, config);

	// Tests arbitrary message signing and later verification
	let msg = "OK computer".as_bytes();
	let sig = node.sign_message(msg);
	let pkey = node.node_id();
	assert!(node.verify_signature(msg, sig.as_str(), &pkey));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn connection_multi_listen() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let node_id_b = node_b.node_id();

	let node_addrs_b = node_b.listening_addresses().unwrap();
	for node_addr_b in &node_addrs_b {
		node_a.connect(node_id_b, node_addr_b.clone(), false).unwrap();
		node_a.disconnect(node_id_b).unwrap();
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn connection_restart_behavior() {
	do_connection_restart_behavior(true).await;
	do_connection_restart_behavior(false).await;
}

async fn do_connection_restart_behavior(persist: bool) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let node_id_a = node_a.node_id();
	let node_id_b = node_b.node_id();

	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();
	node_a.connect(node_id_b, node_addr_b, persist).unwrap();

	let peer_details_a = node_a.list_peers().first().unwrap().clone();
	assert_eq!(peer_details_a.node_id, node_id_b);
	assert_eq!(peer_details_a.is_persisted, persist);
	assert!(peer_details_a.is_connected);

	let peer_details_b = node_b.list_peers().first().unwrap().clone();
	assert_eq!(peer_details_b.node_id, node_id_a);
	assert_eq!(peer_details_b.is_persisted, false);
	assert!(peer_details_a.is_connected);

	// Restart nodes.
	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_b.start().unwrap();
	node_a.start().unwrap();

	// Sleep a bit to allow for the reconnect to happen.
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;

	if persist {
		let peer_details_a = node_a.list_peers().first().unwrap().clone();
		assert_eq!(peer_details_a.node_id, node_id_b);
		assert_eq!(peer_details_a.is_persisted, persist);
		assert!(peer_details_a.is_connected);

		let peer_details_b = node_b.list_peers().first().unwrap().clone();
		assert_eq!(peer_details_b.node_id, node_id_a);
		assert_eq!(peer_details_b.is_persisted, false);
		assert!(peer_details_a.is_connected);
	} else {
		assert!(node_a.list_peers().is_empty());
		assert!(node_b.list_peers().is_empty());
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn concurrent_connections_succeed() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let node_a = Arc::new(node_a);
	let node_b = Arc::new(node_b);

	let node_id_b = node_b.node_id();
	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();

	let mut handles = Vec::new();
	for _ in 0..10 {
		let thread_node = Arc::clone(&node_a);
		let thread_addr = node_addr_b.clone();
		let handle = std::thread::spawn(move || {
			thread_node.connect(node_id_b, thread_addr, false).unwrap();
		});
		handles.push(handle);
	}

	for h in handles {
		h.join().unwrap();
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn splice_channel() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let address_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a, address_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_a.list_balances().total_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().total_onchain_balance_sats, premine_amount_sat);

	open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;

	// Open a channel with Node A contributing the funding
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	let opening_transaction_fee_sat = 156;
	let zero_fee_commitments = node_a.list_channels()[0].feerate_sat_per_1000_weight == 0;
	let closing_transaction_fee_sat = if zero_fee_commitments { 0 } else { 614 };
	let anchor_output_sat = if zero_fee_commitments { 0 } else { 330 };

	assert_eq!(
		node_a.list_balances().total_onchain_balance_sats,
		premine_amount_sat - 4_000_000 - opening_transaction_fee_sat
	);
	assert_eq!(
		node_a.list_balances().total_lightning_balance_sats,
		4_000_000 - closing_transaction_fee_sat - anchor_output_sat
	);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 0);

	let address = node_a.onchain_payment().new_address().unwrap();
	let excessive_splice_out_sats = node_a.list_channels()[0].outbound_capacity_msat / 1000 + 1;
	assert_eq!(
		node_a.splice_out(
			&user_channel_id_a,
			node_b.node_id(),
			&address,
			excessive_splice_out_sats
		),
		Err(NodeError::ChannelSplicingFailed),
	);

	// Test that splicing and payments fail when there are insufficient funds
	let address = node_b.onchain_payment().new_address().unwrap();
	let amount_msat = 400_000_000;

	assert_eq!(
		node_b.splice_in(&user_channel_id_b, node_b.node_id(), 5_000_000),
		Err(NodeError::ChannelSplicingFailed),
	);
	assert_eq!(
		node_b.splice_out(&user_channel_id_b, node_b.node_id(), &address, amount_msat / 1000),
		Err(NodeError::ChannelSplicingFailed),
	);
	assert_eq!(
		node_b.spontaneous_payment().send(amount_msat, node_a.node_id(), None),
		Err(NodeError::PaymentSendingFailed)
	);

	// Splice-in funds for Node B so that it has outbound liquidity to make a payment
	node_b.splice_in(&user_channel_id_b, node_a.node_id(), 4_000_000).unwrap();

	let txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());

	// Node B contributed to this splice, so wait for its funding broadcast to be classified before
	// syncing — otherwise a sync racing the broadcaster's queue records a generic on-chain payment.
	wait_for_classified_funding_payment(&node_b, txo.txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let expected_splice_in_fee_sat = 251;
	let expected_splice_in_onchain_cost_sat = 253;

	// BDK 3.1.0 avoids the previous per-UTXO fee rounding during coin selection. Keep the
	// remaining 2-sat LDK/BDK fee-accounting drift explicit so a dependency change cannot silently
	// reintroduce the larger surplus. Rather than giving the extra sats to the miner, LDK sends
	// them to the channel balance since there may not be a change output.
	let expected_splice_in_lightning_balance_sat = 4_000_002;

	let payments = node_b.list_payments();
	let payment =
		payments.into_iter().find(|p| p.id == PaymentId(txo.txid.to_byte_array())).unwrap();
	assert_eq!(payment.fee_paid_msat, Some(expected_splice_in_fee_sat * 1_000));

	assert_eq!(
		node_b.list_balances().total_onchain_balance_sats,
		premine_amount_sat - 4_000_000 - expected_splice_in_onchain_cost_sat
	);
	assert_eq!(
		node_b.list_balances().total_lightning_balance_sats,
		expected_splice_in_lightning_balance_sat
	);

	let payment_id =
		node_b.spontaneous_payment().send(amount_msat, node_a.node_id(), None).unwrap();

	expect_payment_successful_event!(node_b, Some(payment_id), None);
	expect_payment_received_event!(node_a, amount_msat);

	// Mine a block to give time for the HTLC to resolve
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	assert_eq!(
		node_a.list_balances().total_lightning_balance_sats,
		4_000_000 - closing_transaction_fee_sat - anchor_output_sat + amount_msat / 1000
	);
	assert_eq!(
		node_b.list_balances().total_lightning_balance_sats,
		expected_splice_in_lightning_balance_sat - amount_msat / 1000
	);

	// Splice-out funds for Node A from the payment sent by Node B
	let address = node_a.onchain_payment().new_address().unwrap();
	node_a.splice_out(&user_channel_id_a, node_b.node_id(), &address, amount_msat / 1000).unwrap();

	let txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());

	// Node A contributed to this splice, so wait for its funding broadcast to be classified before
	// syncing — otherwise a sync racing the broadcaster's queue records a generic on-chain payment.
	wait_for_classified_funding_payment(&node_a, txo.txid).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let expected_splice_out_fee_sat = 183;

	let payments = node_a.list_payments();
	let payment =
		payments.into_iter().find(|p| p.id == PaymentId(txo.txid.to_byte_array())).unwrap();
	assert_eq!(payment.fee_paid_msat, Some(expected_splice_out_fee_sat * 1_000));
	// The splice-out graduated to a confirmed interactive-funding payment. Its `direction` is left
	// unasserted on purpose: the destination is our own address, so it is a self-transfer (channel
	// balance -> on-chain wallet) whose inbound/outbound sense is ambiguous.
	assert_eq!(payment.status, PaymentStatus::Succeeded);
	assert!(matches!(
		payment.kind,
		PaymentKind::Onchain {
			status: ConfirmationStatus::Confirmed { .. },
			tx_type: Some(TransactionType::InteractiveFunding { .. }),
			..
		}
	));

	assert_eq!(
		node_a.list_balances().total_onchain_balance_sats,
		premine_amount_sat - 4_000_000 - opening_transaction_fee_sat + amount_msat / 1000
	);
	assert_eq!(
		node_a.list_balances().total_lightning_balance_sats,
		4_000_000 - closing_transaction_fee_sat - anchor_output_sat - expected_splice_out_fee_sat
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rbf_splice_channel() {
	run_rbf_splice_channel_test(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rbf_splice_channel_original_candidate_confirms() {
	run_rbf_splice_channel_test(true).await;
}

async fn run_rbf_splice_channel_test(confirm_original: bool) {
	// Use a custom bitcoind config with a lower incrementalrelayfee so that the +25 sat/kwu
	// (0.1 sat/vB) RBF feerate bump satisfies BIP125's absolute fee increase requirement.
	let bitcoind_exe = std::env::var("BITCOIND_EXE")
		.ok()
		.or_else(|| corepc_node::downloaded_exe_path().ok())
		.expect(
			"you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
		);
	let mut bitcoind_conf = corepc_node::Conf::default();
	bitcoind_conf.network = "regtest";
	bitcoind_conf.args.push("-rest");
	bitcoind_conf.args.push("-incrementalrelayfee=0.00000100");
	let bitcoind = BitcoinD::with_conf(bitcoind_exe, &bitcoind_conf).unwrap();

	let electrs_exe = std::env::var("ELECTRS_EXE")
		.ok()
		.or_else(electrsd::downloaded_exe_path)
		.expect("you need to provide env var ELECTRS_EXE or specify an electrsd version feature");
	let mut electrsd_conf = electrsd::Conf::default();
	electrsd_conf.http_enabled = true;
	electrsd_conf.network = "regtest";
	let electrsd = ElectrsD::with_conf(electrs_exe, &bitcoind, &electrsd_conf).unwrap();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let address_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a, address_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	// bump_channel_funding_fee should fail when there's no pending splice
	assert_eq!(
		node_b.bump_channel_funding_fee(&user_channel_id_b, node_a.node_id()),
		Err(NodeError::ChannelSplicingFailed),
	);

	// Initiate a splice-in to create a pending splice
	node_b.splice_in(&user_channel_id_b, node_a.node_id(), 1_000_000).unwrap();

	let original_txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());

	// Sync so the original splice candidate is recorded as a canonical wallet transaction before
	// the RBF below replaces it. The post-RBF sync then observes the original candidate being
	// replaced (a `WalletEvent::TxReplaced`), which must not drop the payment's durable funding
	// classification — the `tx_type` assertion below catches a regression deterministically.
	wait_for_tx(&electrsd.client, original_txo.txid).await;
	// Node B contributed to this splice; wait for its classification before syncing so the sync
	// takes the funding short-circuit rather than racing the broadcaster's queue.
	wait_for_classified_funding_payment(&node_b, original_txo.txid).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// For `confirm_original`, capture the original candidate's fee and raw transaction now, before
	// the RBF replaces it, so it can be force-confirmed (instead of the RBF) further below.
	let original_candidate: Option<(Option<u64>, String)> = if confirm_original {
		let payment_id = PaymentId(original_txo.txid.to_byte_array());
		let fee = node_b.payment(&payment_id).expect("splice payment exists").fee_paid_msat;
		let raw_tx: String = bitcoind
			.client
			.call("getrawtransaction", &[json!(original_txo.txid.to_string())])
			.expect("failed to fetch the original splice transaction");
		Some((fee, raw_tx))
	} else {
		None
	};

	// Re-splicing the pending splice we already contributed to is rejected; the RBF guard points at
	// bump_channel_funding_fee instead.
	assert_eq!(
		node_b.splice_in(&user_channel_id_b, node_a.node_id(), 1_000_000),
		Err(NodeError::ChannelSplicingFailed),
	);

	// bump_channel_funding_fee should succeed when there's a pending splice
	node_b.bump_channel_funding_fee(&user_channel_id_b, node_a.node_id()).unwrap();

	let rbf_txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());

	assert_ne!(original_txo, rbf_txo, "RBF should produce a different funding txo");

	// Wait for the RBF transaction to replace the original in the mempool.
	wait_for_tx(&electrsd.client, rbf_txo.txid).await;
	// Wait for node_b's re-classification of the RBF candidate before syncing, so the recorded
	// candidate figures reflect the replacement rather than racing the broadcaster's queue.
	wait_for_classified_funding_payment(&node_b, rbf_txo.txid).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// After RBF but before confirmation, node_b (the initiator) should have a single on-chain
	// payment covering both candidates: id anchored to the first broadcast, `kind.txid` pointing
	// at the latest (RBF) candidate, and the durable interactive-funding `tx_type` preserved across
	// the replacement.
	let rbf_candidate_fee = {
		let payment_id = PaymentId(original_txo.txid.to_byte_array());
		let payment = node_b.payment(&payment_id).expect("splice payment exists");
		match payment.kind {
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(TransactionType::InteractiveFunding { .. }),
			} => {
				assert_eq!(txid, rbf_txo.txid);
			},
			ref other => {
				panic!("expected Onchain Unconfirmed interactive-funding, got {:?}", other)
			},
		}
		assert_eq!(payment.status, PaymentStatus::Pending);
		// Only one Onchain Pending payment for this splice attempt (not one per candidate).
		let splice_payments = node_b.list_payments_with_filter(|p| {
			p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Onchain { .. })
				&& p.status == PaymentStatus::Pending
		});
		assert_eq!(
			splice_payments.len(),
			1,
			"expected exactly one pending Onchain payment for the splice, got {}: {:#?}",
			splice_payments.len(),
			splice_payments,
		);

		// The fee recorded for the latest (RBF) candidate, which is the one that confirms below.
		assert!(payment.fee_paid_msat.is_some());
		payment.fee_paid_msat
	};

	// Confirm the splice. Normally the latest (RBF) candidate wins through the mempool; for
	// `confirm_original` we instead mine the original candidate directly into a block so an
	// earlier, lower-fee candidate is the one that confirms.
	let winning_txo = if confirm_original { original_txo } else { rbf_txo };
	if let Some((_, ref original_tx_hex)) = original_candidate {
		let address = bitcoind.client.new_address().expect("failed to get new address");
		let _: serde_json::Value = bitcoind
			.client
			.call("generateblock", &[json!(address.to_string()), json!([original_tx_hex])])
			.expect("failed to mine the original splice candidate");
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 5).await;
	} else {
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	}

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Verify the candidate that locked is the one that confirmed, not necessarily the last broadcast.
	match node_a.next_event_async().await {
		Event::ChannelReady { funding_txo, counterparty_node_id, .. } => {
			assert_eq!(counterparty_node_id, Some(node_b.node_id()));
			assert_eq!(funding_txo, Some(winning_txo));
			node_a.event_handled().unwrap();
		},
		ref e => panic!("node_a got unexpected event: {:?}", e),
	}
	match node_b.next_event_async().await {
		Event::ChannelReady { funding_txo, counterparty_node_id, .. } => {
			assert_eq!(counterparty_node_id, Some(node_a.node_id()));
			assert_eq!(funding_txo, Some(winning_txo));
			node_b.event_handled().unwrap();
		},
		ref e => panic!("node_b got unexpected event: {:?}", e),
	}

	// The splice payment graduates to `Succeeded` purely from wallet sync reaching
	// `ANTI_REORG_DELAY` confirmations — the `ChannelReady` events above are a separate
	// channel-lifecycle signal, not what drives payment status. Its `kind.txid` reflects the
	// winning RBF candidate, and `fee_paid_msat` carries this node's `FundingContribution` fee.
	{
		let payment_id = PaymentId(original_txo.txid.to_byte_array());
		let payment = node_b.payment(&payment_id).expect("splice payment graduated");
		assert_eq!(payment.status, PaymentStatus::Succeeded);
		match payment.kind {
			PaymentKind::Onchain { txid, status: ConfirmationStatus::Confirmed { .. }, .. } => {
				assert_eq!(txid, winning_txo.txid);
			},
			ref other => panic!("expected Onchain Confirmed, got {:?}", other),
		}
		// Graduation stamps the economics of the candidate that actually confirmed. For
		// `confirm_original` that is the earlier, lower-fee candidate, whose fee differs from the
		// last-broadcast (RBF) candidate's — so this would fail if the payment kept the
		// last-broadcast figures instead of the confirmed candidate's.
		let expected_fee = match original_candidate {
			Some((original_fee, _)) => {
				assert_ne!(original_fee, rbf_candidate_fee);
				original_fee
			},
			None => rbf_candidate_fee,
		};
		assert!(expected_fee.is_some());
		assert_eq!(payment.fee_paid_msat, expected_fee);
	}

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn funding_payment_graduates_without_channel_ready() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let address_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a, address_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// node_a funds the channel, so it holds the funding payment. `open_channel` drains only the
	// `ChannelPending` events, leaving any `ChannelReady` queued and undrained.
	let funding_txo = open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;

	// Mine past `ANTI_REORG_DELAY` and sync only node_a. node_b stays behind, so it cannot yet
	// send `channel_ready` and node_a therefore cannot have emitted a `ChannelReady` event — any
	// graduation below must come from wallet sync alone.
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();

	// The funding payment is `Succeeded` purely from wallet sync reaching `ANTI_REORG_DELAY`
	// confirmations, asserted before draining any LDK event — so graduation is not driven by the
	// Lightning `ChannelReady` signal.
	let payment_id = PaymentId(funding_txo.txid.to_byte_array());
	let payment = node_a.payment(&payment_id).expect("funding payment exists");
	assert_eq!(payment.status, PaymentStatus::Succeeded);
	match payment.kind {
		PaymentKind::Onchain {
			txid,
			status: ConfirmationStatus::Confirmed { .. },
			tx_type: Some(TransactionType::Funding { .. }),
		} => assert_eq!(txid, funding_txo.txid),
		ref other => panic!("expected Onchain Confirmed funding payment, got {:?}", other),
	}

	// Let node_b catch up so the channel completes; the `ChannelReady` events follow the
	// already-`Succeeded` payment rather than driving it.
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn splice_payment_reorged_to_unconfirmed() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let address_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a, address_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	// node_b splices in, recording a funding payment it contributed to.
	node_b.splice_in(&user_channel_id_b, node_a.node_id(), 1_000_000).unwrap();
	let splice_txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());
	wait_for_tx(&electrsd.client, splice_txo.txid).await;
	// Ensure node_b classified the splice before syncing so the test exercises a funding payment's
	// reorg rather than a generic on-chain payment's.
	wait_for_classified_funding_payment(&node_b, splice_txo.txid).await;

	// Confirm the splice with a single block — confirmed, but short of `ANTI_REORG_DELAY`, so the
	// payment is `Confirmed`/`Pending` rather than graduated.
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(splice_txo.txid.to_byte_array());
	let payment = node_b.payment(&payment_id).expect("splice payment exists");
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert!(matches!(
		payment.kind,
		PaymentKind::Onchain { status: ConfirmationStatus::Confirmed { .. }, .. }
	));

	// Reorg the splice transaction out by replacing its block with a longer, transaction-free chain.
	let original_height =
		bitcoind.client.get_blockchain_info().expect("failed to get blockchain info").blocks;
	invalidate_blocks(&bitcoind.client, 1);
	let replacement_address = bitcoind.client.new_address().expect("failed to get new address");
	for _ in 0..2 {
		let _res: serde_json::Value = bitcoind
			.client
			.call("generateblock", &[json!(replacement_address.to_string()), json!([])])
			.expect("failed to generate empty block");
	}
	wait_for_block(&bitcoind.client, &electrsd.client, original_height as usize + 1).await;
	node_b.sync_wallets().unwrap();

	// The funding payment returns to `Unconfirmed` and stays `Pending`, exercising the
	// `TxUnconfirmed` arm for a funding payment.
	let payment = node_b.payment(&payment_id).expect("splice payment still exists");
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert!(matches!(
		payment.kind,
		PaymentKind::Onchain { status: ConfirmationStatus::Unconfirmed, .. }
	));

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn splice_in_rbf_joins_counterparty_splice() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let address_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a, address_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_a, &node_b, 4_000_000, false, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	// node_b (which didn't fund the channel open, so holds the on-chain balance) initiates a
	// splice-in; node_a does not contribute to this first candidate.
	node_b.splice_in(&user_channel_id_b, node_a.node_id(), 1_000_000).unwrap();
	let counterparty_txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());
	wait_for_tx(&electrsd.client, counterparty_txo.txid).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// node_a contributes to the pending splice via RBF. Before honoring the funding template's RBF
	// minimum feerate, this was rejected with FeeRateBelowRbfMinimum because node_a's funding
	// feerate estimate sat below the minimum required to replace the in-flight transaction.
	node_a.splice_in(&user_channel_id_a, node_b.node_id(), 100_000).unwrap();
	let rbf_txo = expect_splice_negotiated_event!(node_a, node_b.node_id());
	expect_splice_negotiated_event!(node_b, node_a.node_id());
	assert_ne!(counterparty_txo, rbf_txo, "node_a's RBF should produce a different funding txo");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn simple_bolt12_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcasted a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}

	// Sleep one more sec to make sure the node announcement propagates.
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let expected_amount_msat = 100_000_000;
	let offer =
		node_b.bolt12_payment().receive(expected_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let payment_id = node_a
		.bolt12_payment()
		.send(&offer, expected_quantity, expected_payer_note.clone(), None)
		.unwrap();

	let event = node_a.next_event_async().await;
	match event {
		ref e @ Event::PaymentSuccessful { payment_id: ref evt_id, ref bolt12_invoice, .. } => {
			println!("{} got event {:?}", node_a.node_id(), e);
			assert_eq!(*evt_id, Some(payment_id));
			assert!(
				bolt12_invoice.is_some(),
				"bolt12_invoice should be present for BOLT12 payments"
			);
			node_a.event_handled().unwrap();
		},
		ref e => panic!("{} got unexpected event!: {:?}", "node_a", e),
	}
	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt12Offer { .. }));
	assert_eq!(node_a_payments.len(), 1);
	match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer {
			hash,
			preimage,
			secret: _,
			counterparty_skimmed_fee_msat,
			offer_id,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(counterparty_skimmed_fee_msat, None);
			assert_eq!(offer_id, offer.id());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0);
			// TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			// API currently doesn't allow to do that.
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	expect_payment_received_event!(node_b, expected_amount_msat);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt12Offer { .. }));
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer { hash, preimage, secret, offer_id, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
			assert_eq!(offer_id, offer.id());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	// Test send_using_amount
	let offer_amount_msat = 100_000_000;
	let less_than_offer_amount = offer_amount_msat - 10_000;
	let expected_amount_msat = offer_amount_msat + 10_000;
	let offer = node_b.bolt12_payment().receive(offer_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	assert!(node_a
		.bolt12_payment()
		.send_using_amount(&offer, less_than_offer_amount, None, None, None)
		.is_err());
	let payment_id = node_a
		.bolt12_payment()
		.send_using_amount(
			&offer,
			expected_amount_msat,
			expected_quantity,
			expected_payer_note.clone(),
			None,
		)
		.unwrap();

	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let node_a_payments = node_a.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Offer { .. }) && p.id == payment_id
	});
	assert_eq!(node_a_payments.len(), 1);
	let payment_hash = match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer {
			hash,
			preimage,
			secret: _,
			counterparty_skimmed_fee_msat,
			offer_id,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(counterparty_skimmed_fee_msat, None);
			assert_eq!(offer_id, offer.id());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0);
			// TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			// API currently doesn't allow to do that.
			hash.unwrap()
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	};
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	expect_payment_received_event!(node_b, expected_amount_msat);
	let node_b_payment_id = PaymentId(payment_hash.0);
	let node_b_payments = node_b.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Offer { .. }) && p.id == node_b_payment_id
	});
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer { hash, preimage, secret, offer_id, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
			assert_eq!(offer_id, offer.id());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	// Now node_b refunds the amount node_a just overpaid.
	let overpaid_amount = expected_amount_msat - offer_amount_msat;
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let refund = node_b
		.bolt12_payment()
		.initiate_refund(
			overpaid_amount,
			3600,
			expected_quantity,
			expected_payer_note.clone(),
			None,
		)
		.unwrap();
	let invoice = node_a.bolt12_payment().request_refund_payment(&refund).unwrap();
	expect_payment_received_event!(node_a, overpaid_amount);

	let node_b_payment_id = node_b
		.list_payments_with_filter(|p| {
			matches!(p.kind, PaymentKind::Bolt12Refund { .. })
				&& p.amount_msat == Some(overpaid_amount)
		})
		.first()
		.unwrap()
		.id;
	expect_payment_successful_event!(node_b, Some(node_b_payment_id), None);

	let node_b_payments = node_b.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Refund { .. }) && p.id == node_b_payment_id
	});
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Refund {
			hash,
			preimage,
			secret: _,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0)
			// TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			// API currently doesn't allow to do that.
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(overpaid_amount));

	let node_a_payment_id = PaymentId(invoice.payment_hash().0);
	let node_a_payments = node_a.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Refund { .. }) && p.id == node_a_payment_id
	});
	assert_eq!(node_a_payments.len(), 1);
	match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Refund { hash, preimage, secret, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(overpaid_amount));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn async_payment() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let mut config_sender = random_config();
	config_sender.node_config.listening_addresses = None;
	config_sender.node_config.node_alias = None;
	config_sender.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("sender      ".to_string())));
	config_sender.async_payments_role = Some(AsyncPaymentsRole::Client);
	let node_sender = setup_node(&chain_source, config_sender);

	let mut config_sender_lsp = random_config();
	config_sender_lsp.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("sender_lsp  ".to_string())));
	config_sender_lsp.async_payments_role = Some(AsyncPaymentsRole::Server);
	let node_sender_lsp = setup_node(&chain_source, config_sender_lsp);

	let mut config_receiver_lsp = random_config();
	config_receiver_lsp.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("receiver_lsp".to_string())));
	config_receiver_lsp.async_payments_role = Some(AsyncPaymentsRole::Server);

	let node_receiver_lsp = setup_node(&chain_source, config_receiver_lsp);

	let mut config_receiver = random_config();
	config_receiver.node_config.listening_addresses = None;
	config_receiver.node_config.node_alias = None;
	config_receiver.log_writer =
		TestLogWriter::Custom(Arc::new(MultiNodeLogger::new("receiver    ".to_string())));
	let node_receiver = setup_node(&chain_source, config_receiver);

	let address_sender = node_sender.onchain_payment().new_address().unwrap();
	let address_sender_lsp = node_sender_lsp.onchain_payment().new_address().unwrap();
	let address_receiver_lsp = node_receiver_lsp.onchain_payment().new_address().unwrap();
	let address_receiver = node_receiver.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 4_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_sender, address_sender_lsp, address_receiver_lsp, address_receiver],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_sender.sync_wallets().unwrap();
	node_sender_lsp.sync_wallets().unwrap();
	node_receiver_lsp.sync_wallets().unwrap();
	node_receiver.sync_wallets().unwrap();

	open_channel(&node_sender, &node_sender_lsp, 400_000, false, &electrsd).await;
	open_channel(&node_sender_lsp, &node_receiver_lsp, 400_000, true, &electrsd).await;
	open_channel_push_amt(
		&node_receiver,
		&node_receiver_lsp,
		400_000,
		Some(200_000_000),
		false,
		&electrsd,
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_sender.sync_wallets().unwrap();
	node_sender_lsp.sync_wallets().unwrap();
	node_receiver_lsp.sync_wallets().unwrap();
	node_receiver.sync_wallets().unwrap();

	expect_channel_ready_event!(node_sender, node_sender_lsp.node_id());
	expect_channel_ready_events!(
		node_sender_lsp,
		node_sender.node_id(),
		node_receiver_lsp.node_id()
	);
	expect_channel_ready_events!(
		node_receiver_lsp,
		node_sender_lsp.node_id(),
		node_receiver.node_id()
	);
	expect_channel_ready_event!(node_receiver, node_receiver_lsp.node_id());

	let has_node_announcements = |node: &ldk_node::Node| {
		node.network_graph()
			.list_nodes()
			.iter()
			.filter(|n| {
				node.network_graph().node(n).map_or(false, |info| info.announcement_info.is_some())
			})
			.count() >= 2
	};

	// Wait for everyone to see all channels and node announcements.
	while node_sender.network_graph().list_channels().len() < 1
		|| node_sender_lsp.network_graph().list_channels().len() < 1
		|| node_receiver_lsp.network_graph().list_channels().len() < 1
		|| node_receiver.network_graph().list_channels().len() < 1
		|| !has_node_announcements(&node_sender)
		|| !has_node_announcements(&node_sender_lsp)
		|| !has_node_announcements(&node_receiver_lsp)
		|| !has_node_announcements(&node_receiver)
	{
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	}

	let recipient_id = vec![1, 2, 3];
	let blinded_paths =
		node_receiver_lsp.bolt12_payment().blinded_paths_for_async_recipient(recipient_id).unwrap();
	node_receiver.bolt12_payment().set_paths_to_static_invoice_server(blinded_paths).unwrap();

	let offer = loop {
		if let Ok(offer) = node_receiver.bolt12_payment().receive_async() {
			break offer;
		}

		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	};

	node_receiver.stop().unwrap();

	let payment_id =
		node_sender.bolt12_payment().send_using_amount(&offer, 5_000, None, None, None).unwrap();

	// Sleep to allow the payment reach a state where the htlc is held and waiting for the receiver to come online.
	tokio::time::sleep(std::time::Duration::from_millis(3000)).await;

	node_receiver.start().unwrap();

	expect_payment_successful_event!(node_sender, Some(payment_id), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_node_announcement_propagation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	// Node A will use both listening and announcement addresses
	let mut config_a = random_config();
	let node_a_alias_string = "ldk-node-a".to_string();
	let mut node_a_alias_bytes = [0u8; 32];
	node_a_alias_bytes[..node_a_alias_string.as_bytes().len()]
		.copy_from_slice(node_a_alias_string.as_bytes());
	let node_a_node_alias = Some(NodeAlias(node_a_alias_bytes));
	let node_a_announcement_addresses = generate_listening_addresses();
	config_a.node_config.node_alias = node_a_node_alias.clone();
	config_a.node_config.listening_addresses = Some(generate_listening_addresses());
	config_a.node_config.announcement_addresses = Some(node_a_announcement_addresses.clone());

	// Node B will only use listening addresses
	let mut config_b = random_config();
	let node_b_alias_string = "ldk-node-b".to_string();
	let mut node_b_alias_bytes = [0u8; 32];
	node_b_alias_bytes[..node_b_alias_string.as_bytes().len()]
		.copy_from_slice(node_b_alias_string.as_bytes());
	let node_b_node_alias = Some(NodeAlias(node_b_alias_bytes));
	let node_b_listening_addresses = generate_listening_addresses();
	config_b.node_config.node_alias = node_b_node_alias.clone();
	config_b.node_config.listening_addresses = Some(node_b_listening_addresses.clone());
	config_b.node_config.announcement_addresses = None;

	let node_a = setup_node(&chain_source, config_a);
	let node_b = setup_node(&chain_source, config_b);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();

	// Open an announced channel from node_a to node_b
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let has_node_announcement = |node: &ldk_node::Node, node_id: bitcoin::secp256k1::PublicKey| {
		node.network_graph()
			.node(&NodeId::from_pubkey(&node_id))
			.map_or(false, |info| info.announcement_info.is_some())
	};

	while node_a.status().latest_node_announcement_broadcast_timestamp.is_none()
		|| node_b.status().latest_node_announcement_broadcast_timestamp.is_none()
		|| !has_node_announcement(&node_b, node_a.node_id())
		|| !has_node_announcement(&node_a, node_b.node_id())
	{
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}

	// Get node info from the other node's perspective
	let node_a_info = node_b.network_graph().node(&NodeId::from_pubkey(&node_a.node_id())).unwrap();
	let node_a_announcement_info = node_a_info.announcement_info.as_ref().unwrap();

	let node_b_info = node_a.network_graph().node(&NodeId::from_pubkey(&node_b.node_id())).unwrap();
	let node_b_announcement_info = node_b_info.announcement_info.as_ref().unwrap();

	// Assert that the aliases and addresses match the expected values
	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_a_announcement_info.alias(), &node_a_node_alias.unwrap());
	#[cfg(feature = "uniffi")]
	assert_eq!(node_a_announcement_info.alias, node_a_alias_string);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_a_announcement_info.addresses(), &node_a_announcement_addresses);
	#[cfg(feature = "uniffi")]
	assert_eq!(node_a_announcement_info.addresses, node_a_announcement_addresses);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_b_announcement_info.alias(), &node_b_node_alias.unwrap());
	#[cfg(feature = "uniffi")]
	assert_eq!(node_b_announcement_info.alias, node_b_alias_string);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_b_announcement_info.addresses(), &node_b_listening_addresses);
	#[cfg(feature = "uniffi")]
	assert_eq!(node_b_announcement_info.addresses, node_b_listening_addresses);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn generate_bip21_uri() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	// Test 1: Verify URI generation (on-chain + BOLT11) works
	// even before any channels are opened. This checks the graceful fallback behavior.
	let initial_uni_payment = node_b
		.unified_payment()
		.receive(expected_amount_sats, "asdf", expiry_sec)
		.expect("Failed to generate URI");
	println!("Initial URI (no channels): {}", initial_uni_payment);

	assert!(initial_uni_payment.contains("bitcoin:"));
	assert!(initial_uni_payment.contains("lightning="));
	assert!(!initial_uni_payment.contains("lno=")); // BOLT12 requires channels

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Test 2: Verify URI generation (on-chain + BOLT11 + BOLT12) works after channels are established.
	let uni_payment = node_b
		.unified_payment()
		.receive(expected_amount_sats, "asdf", expiry_sec)
		.expect("Failed to generate URI");

	println!("Generated URI: {}", uni_payment);
	assert!(uni_payment.contains("bitcoin:"));
	assert!(uni_payment.contains("lightning="));
	assert!(uni_payment.contains("lno="));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unified_receive_rejects_msat_overflow() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let node = setup_node(&chain_source, random_config());

	assert_eq!(
		Err(NodeError::InvalidAmount),
		node.unified_payment().receive(u64::MAX, "asdf", 4_000)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unified_send_receive_bip21_uri() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcast a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}

	// Sleep one more sec to make sure the node announcement propagates.
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	let uni_payment = node_b.unified_payment().receive(expected_amount_sats, "asdf", expiry_sec);
	let uri_str = uni_payment.clone().unwrap();
	let offer_payment_id: PaymentId =
		match node_a.unified_payment().send(&uri_str, None, None).await {
			Ok(UnifiedPaymentResult::Bolt12 { payment_id }) => {
				println!("\nBolt12 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(UnifiedPaymentResult::Bolt11 { payment_id: _ }) => {
				panic!("Expected Bolt12 payment but got Bolt11");
			},
			Ok(UnifiedPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt12 payment but got On-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt12 payment but got error: {:?}", e);
			},
		};

	expect_payment_successful_event!(node_a, Some(offer_payment_id), None);

	// Cut off the BOLT12 part to fallback to BOLT11.
	let uri_str_without_offer = uri_str.split("&lno=").next().unwrap();
	let invoice_payment_id: PaymentId =
		match node_a.unified_payment().send(uri_str_without_offer, None, None).await {
			Ok(UnifiedPaymentResult::Bolt12 { payment_id: _ }) => {
				panic!("Expected Bolt11 payment but got Bolt12");
			},
			Ok(UnifiedPaymentResult::Bolt11 { payment_id }) => {
				println!("\nBolt11 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(UnifiedPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt11 payment but got on-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt11 payment but got error: {:?}", e);
			},
		};
	expect_payment_successful_event!(node_a, Some(invoice_payment_id), None);

	let expect_onchain_amount_sats = 800_000;
	let onchain_uni_payment =
		node_b.unified_payment().receive(expect_onchain_amount_sats, "asdf", 4_000).unwrap();

	// Cut off any lightning part to fallback to on-chain only.
	let uri_str_without_lightning = onchain_uni_payment.split("&lightning=").next().unwrap();
	let txid = match node_a.unified_payment().send(&uri_str_without_lightning, None, None).await {
		Ok(UnifiedPaymentResult::Bolt12 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt12")
		},
		Ok(UnifiedPaymentResult::Bolt11 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt11");
		},
		Ok(UnifiedPaymentResult::Onchain { txid }) => {
			println!("\nOn-chain transaction successful with Txid: {}", txid);
			txid
		},
		Err(e) => {
			panic!("Expected on-chain payment but got error: {:?}", e);
		},
	};

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	wait_for_tx(&electrsd.client, txid).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_b.list_balances().total_onchain_balance_sats, 800_000);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 200_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_client_service_integration() {
	do_lsps2_client_service_integration(true).await;
	do_lsps2_client_service_integration(false).await;
}

async fn do_lsps2_client_service_integration(client_trusts_lsp: bool) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp,
		disable_client_reserve: false,
	};

	let service_config = random_config();
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.enable_liquidity_provider(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();

	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config();
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.add_liquidity_source(service_node_id, service_addr, None, true);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();

	let payer_config = random_config();
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let service_addr = service_node.onchain_payment().new_address().unwrap();
	let client_addr = client_node.onchain_payment().new_address().unwrap();
	let payer_addr = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr, client_addr, payer_addr],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();

	// Open a channel payer -> service that will allow paying the JIT invoice
	println!("Opening channel payer_node -> service_node!");
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel(jit_amount_msat, &invoice_description.into(), 1024)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_event!(service_node, PaymentForwarded);
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	let client_payment_id =
		expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();
	let client_payment = client_node.payment(&client_payment_id).unwrap();
	match client_payment.kind {
		PaymentKind::Bolt11 { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(service_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let expected_channel_overprovisioning_msat =
		(expected_received_amount_msat * channel_over_provisioning_ppm as u64) / 1_000_000;
	let expected_channel_size_sat =
		(expected_received_amount_msat + expected_channel_overprovisioning_msat) / 1000;
	let channel_value_sats = client_node.list_channels().first().unwrap().channel_value_sats;
	assert_eq!(channel_value_sats, expected_channel_size_sat);

	println!("Generating regular invoice!");
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap()).into();
	let amount_msat = 5_000_000;
	let invoice =
		client_node.bolt11_payment().receive(amount_msat, &invoice_description, 1024).unwrap();

	// Have the payer_node pay the invoice, to check regular forwards service_node -> client_node
	// are working as expected.
	println!("Paying regular invoice!");
	let payment_id = payer_node.bolt11_payment().send(&invoice, None).unwrap();
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	expect_event!(service_node, PaymentForwarded);
	expect_payment_received_event!(client_node, amount_msat);

	////////////////////////////////////////////////////////////////////////////
	// receive_via_jit_channel_for_hash and claim_for_hash
	////////////////////////////////////////////////////////////////////////////
	println!("Generating JIT invoice!");
	// Increase the amount to make sure it does not fit into the existing channels.
	let jit_amount_msat = 200_000_000;
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description,
			1024,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	let claimable_amount_msat = expect_payment_claimable_event!(
		client_node,
		payment_id,
		manual_payment_hash,
		expected_received_amount_msat
	);
	println!("Claiming payment!");
	client_node
		.bolt11_payment()
		.claim_for_hash(manual_payment_hash, claimable_amount_msat, manual_preimage)
		.unwrap();

	expect_event!(service_node, PaymentForwarded);
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	let client_payment_id =
		expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();
	let client_payment = client_node.payment(&client_payment_id).unwrap();
	match client_payment.kind {
		PaymentKind::Bolt11 { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(service_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	////////////////////////////////////////////////////////////////////////////
	// receive_via_jit_channel_for_hash and fail_for_hash
	////////////////////////////////////////////////////////////////////////////
	println!("Generating JIT invoice!");
	// Increase the amount to make sure it does not fit into the existing channels.
	let jit_amount_msat = 400_000_000;
	let manual_preimage = PaymentPreimage([43u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description,
			1024,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	expect_payment_claimable_event!(
		client_node,
		payment_id,
		manual_payment_hash,
		expected_received_amount_msat
	);
	println!("Failing payment!");
	client_node.bolt11_payment().fail_for_hash(manual_payment_hash).unwrap();

	expect_event!(payer_node, PaymentFailed);
	assert_eq!(client_node.payment(&payment_id).unwrap().status, PaymentStatus::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn bolt12_lsps2_client_service_integration() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	let channel_opening_fee_ppm = 10_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm: 100_000,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: true,
		disable_client_reserve: false,
	};

	let service_config = random_config();
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.enable_liquidity_provider(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();

	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config();
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.add_liquidity_source(service_node_id, service_addr, None, true);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();

	let payer_config = random_config();
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let client_addr = client_node.listening_addresses().unwrap().first().unwrap().clone();
	payer_node.connect(client_node.node_id(), client_addr, false).unwrap();

	let service_onchain_addr = service_node.onchain_payment().new_address().unwrap();
	let client_onchain_addr = client_node.onchain_payment().new_address().unwrap();
	let payer_onchain_addr = payer_node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_onchain_addr, client_onchain_addr, payer_onchain_addr],
		Amount::from_sat(10_000_000),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();

	open_channel(&payer_node, &service_node, 5_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());
	while payer_node.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let fixed_amount_msat = 100_000_000;
	// The recipient has no channel yet, but the connected payer gives the default message router a
	// peer through which it can construct an offer path. This keeps the long-lived offer addressed
	// by a blinded node path; the single-use intercept SCID is introduced only in the invoice.
	assert!(client_node.list_channels().is_empty());
	let fixed_offer =
		client_node.bolt12_payment().receive(fixed_amount_msat, "fixed", None, None).unwrap();
	assert!(!fixed_offer.paths().is_empty());
	assert_ne!(fixed_offer.issuer_signing_pubkey(), Some(client_node.node_id()));
	let fixed_payment_id =
		payer_node.bolt12_payment().send(&fixed_offer, None, None, None).unwrap();

	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_event!(service_node, PaymentForwarded);
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());
	expect_payment_successful_event!(payer_node, Some(fixed_payment_id), None);
	let fixed_fee_msat = fixed_amount_msat * channel_opening_fee_ppm as u64 / 1_000_000;
	let fixed_received_msat = fixed_amount_msat - fixed_fee_msat;
	let fixed_receiver_payment_id =
		expect_payment_received_event!(client_node, fixed_received_msat).unwrap();
	match client_node.payment(&fixed_receiver_payment_id).unwrap().kind {
		PaymentKind::Bolt12Offer { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(fixed_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	// The over-provisioned part of the first JIT channel can receive this payment in full. The
	// offers flow must therefore keep the ordinary blinded path and avoid negotiating or exposing a
	// second intercept SCID.
	let client_channel_count = client_node.list_channels().len();
	let service_channel_count = service_node.list_channels().len();
	let ordinary_amount_msat = 5_000_000;
	let ordinary_offer = client_node
		.bolt12_payment()
		.receive(ordinary_amount_msat, "existing inbound", None, None)
		.unwrap();
	let ordinary_payment_id =
		payer_node.bolt12_payment().send(&ordinary_offer, None, None, None).unwrap();

	expect_event!(service_node, PaymentForwarded);
	expect_payment_successful_event!(payer_node, Some(ordinary_payment_id), None);
	let ordinary_receiver_payment_id =
		expect_payment_received_event!(client_node, ordinary_amount_msat).unwrap();
	match client_node.payment(&ordinary_receiver_payment_id).unwrap().kind {
		PaymentKind::Bolt12Offer { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, None);
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert_eq!(client_node.list_channels().len(), client_channel_count);
	assert_eq!(service_node.list_channels().len(), service_channel_count);

	// The first JIT channel cannot carry this larger variable payment. The variable offer disables
	// MPP, so the payer must use the fresh JIT path rather than splitting across both paths.
	let variable_amount_msat = 200_000_000;
	let variable_offer =
		client_node.bolt12_payment().receive_variable_amount("variable", None).unwrap();
	let variable_payment_id = payer_node
		.bolt12_payment()
		.send_using_amount(&variable_offer, variable_amount_msat, None, None, None)
		.unwrap();

	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_event!(service_node, PaymentForwarded);
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());
	expect_payment_successful_event!(payer_node, Some(variable_payment_id), None);
	let variable_fee_msat = variable_amount_msat * channel_opening_fee_ppm as u64 / 1_000_000;
	let variable_received_msat = variable_amount_msat - variable_fee_msat;
	let variable_receiver_payment_id =
		expect_payment_received_event!(client_node, variable_received_msat).unwrap();
	match client_node.payment(&variable_receiver_payment_id).unwrap().kind {
		PaymentKind::Bolt12Offer { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(variable_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn facade_logging() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let logger = init_log_logger(LevelFilter::Trace);
	let mut config = random_config();
	config.log_writer = TestLogWriter::LogFacade;

	println!("== Facade logging starts ==");
	let _node = setup_node(&chain_source, config);

	assert!(!logger.retrieve_logs().is_empty());
	for (_, entry) in logger.retrieve_logs().iter().enumerate() {
		validate_log_entry(entry);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn spontaneous_send_with_custom_preimage() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_sat = 1_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 500_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let seed = b"test_payment_preimage";
	let bytes: Sha256Hash = Sha256Hash::hash(seed);
	let custom_bytes = bytes.to_byte_array();
	let custom_preimage = PaymentPreimage(custom_bytes);

	let amount_msat = 100_000;
	let payment_id = node_a
		.spontaneous_payment()
		.send_with_preimage(amount_msat, node_b.node_id(), custom_preimage, None)
		.unwrap();

	// check payment status and verify stored preimage
	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let details: PaymentDetails =
		node_a.list_payments_with_filter(|p| p.id == payment_id).first().unwrap().clone();
	assert_eq!(details.status, PaymentStatus::Succeeded);
	if let PaymentKind::Spontaneous { preimage: Some(pi), .. } = details.kind {
		assert_eq!(pi.0, custom_bytes);
	} else {
		panic!("Expected a spontaneous PaymentKind with a preimage");
	}

	// Verify receiver side (node_b)
	expect_payment_received_event!(node_b, amount_msat);
	let receiver_payments: Vec<PaymentDetails> = node_b.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Inbound
			&& matches!(p.kind, PaymentKind::Spontaneous { .. })
	});

	assert_eq!(receiver_payments.len(), 1);
	let receiver_details = &receiver_payments[0];
	assert_eq!(receiver_details.status, PaymentStatus::Succeeded);
	assert_eq!(receiver_details.amount_msat, Some(amount_msat));
	assert_eq!(receiver_details.direction, PaymentDirection::Inbound);

	// Verify receiver also has the same preimage
	if let PaymentKind::Spontaneous { preimage: Some(pi), .. } = &receiver_details.kind {
		assert_eq!(pi.0, custom_bytes);
	} else {
		panic!("Expected receiver to have spontaneous PaymentKind with preimage");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn drop_in_async_context() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let config = random_config();
	let node = setup_node(&chain_source, config);
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_client_trusts_lsp() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: true,
		disable_client_reserve: false,
	};

	let service_config = random_config();
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.enable_liquidity_provider(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();
	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config();
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.add_liquidity_source(service_node_id, service_addr.clone(), None, true);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();
	let client_node_id = client_node.node_id();

	let payer_config = random_config();
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let service_addr_onchain = service_node.onchain_payment().new_address().unwrap();
	let client_addr_onchain = client_node.onchain_payment().new_address().unwrap();
	let payer_addr_onchain = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr_onchain, client_addr_onchain, payer_addr_onchain],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	println!("Premine complete!");
	// Open a channel payer -> service that will allow paying the JIT invoice
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let res = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description.into(),
			1024,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&res, None).unwrap();
	println!("Payment ID: {:?}", payment_id);
	let funding_txo = expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	// Check the funding transaction hasn't been broadcasted yet and nodes aren't seeing it.
	println!("Try to find funding tx... It won't be found yet, as the client has not claimed it.");
	tokio::time::sleep(std::time::Duration::from_secs(3)).await;
	let mempool = bitcoind.client.get_raw_mempool().unwrap().into_model().unwrap();
	let funding_tx_found = mempool.0.iter().any(|txid| *txid == funding_txo.txid);
	assert!(!funding_tx_found, "Funding transaction should NOT be broadcast yet");

	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	assert_eq!(
		client_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty.node_id == service_node_id)
			.unwrap()
			.confirmations,
		Some(0)
	);
	assert_eq!(
		service_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty.node_id == client_node_id)
			.unwrap()
			.confirmations,
		Some(0)
	);

	// Now claim the JIT payment, which should release the funding transaction
	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;

	let _ = expect_payment_claimable_event!(
		client_node,
		payment_id,
		manual_payment_hash,
		expected_received_amount_msat
	);

	client_node
		.bolt11_payment()
		.claim_for_hash(manual_payment_hash, jit_amount_msat, manual_preimage)
		.unwrap();

	expect_payment_successful_event!(payer_node, Some(payment_id), None);

	let _ = expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();

	// Check the nodes pick up on the confirmed funding tx now.
	wait_for_tx(&electrsd.client, funding_txo.txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	assert_eq!(
		client_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty.node_id == service_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
	assert_eq!(
		service_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty.node_id == client_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_lsp_trusts_client_but_client_does_not_claim() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: false,
		disable_client_reserve: false,
	};

	let service_config = random_config();
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.enable_liquidity_provider(lsps2_service_config);
	let service_node = service_builder.build(service_config.node_entropy.into()).unwrap();
	service_node.start().unwrap();

	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config();
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.add_liquidity_source(service_node_id, service_addr.clone(), None, true);
	let client_node = client_builder.build(client_config.node_entropy.into()).unwrap();
	client_node.start().unwrap();

	let client_node_id = client_node.node_id();

	let payer_config = random_config();
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer_node.start().unwrap();

	let service_addr_onchain = service_node.onchain_payment().new_address().unwrap();
	let client_addr_onchain = client_node.onchain_payment().new_address().unwrap();
	let payer_addr_onchain = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr_onchain, client_addr_onchain, payer_addr_onchain],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	println!("Premine complete!");
	// Open a channel payer -> service that will allow paying the JIT invoice
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash: PaymentHash = manual_preimage.into();
	let res = client_node
		.bolt11_payment()
		.receive_via_jit_channel_for_hash(
			jit_amount_msat,
			&invoice_description.into(),
			1024,
			manual_payment_hash,
		)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let _payment_id = payer_node.bolt11_payment().send(&res, None).unwrap();
	let funding_txo = expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());
	println!("Waiting for funding transaction to be broadcast...");

	// Check the nodes pick up on the confirmed funding tx now.
	wait_for_tx(&electrsd.client, funding_txo.txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	assert_eq!(
		client_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty.node_id == service_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
	assert_eq!(
		service_node
			.list_channels()
			.iter()
			.find(|c| c.counterparty.node_id == client_node_id)
			.unwrap()
			.confirmations,
		Some(6)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn payment_persistence_after_restart() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	// Setup nodes manually so we can restart node_a with the same config
	println!("== Node A ==");
	let mut config_a = random_config();
	config_a.store_type = TestStoreType::Sqlite;

	let num_payments = 200;
	let payment_amount_msat = 1_000_000; // 1000 sats per payment

	{
		let node_a = setup_node(&chain_source, config_a.clone());

		println!("\n== Node B ==");
		let config_b = random_config();
		let node_b = setup_node(&chain_source, config_b);

		let addr_a = node_a.onchain_payment().new_address().unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		// Premine sufficient funds for a large channel and many payments
		let premine_amount_sat = 10_000_000;
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr_a, addr_b],
			Amount::from_sat(premine_amount_sat),
		)
		.await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
		assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

		// Open a large channel from node_a to node_b
		let channel_amount_sat = 5_000_000;
		open_channel(&node_a, &node_b, channel_amount_sat, true, &electrsd).await;
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		expect_channel_ready_event!(node_a, node_b.node_id());
		expect_channel_ready_event!(node_b, node_a.node_id());

		// Send 200 payments from node_a to node_b
		println!("\nSending {} payments from A to B...", num_payments);
		let invoice_description =
			Bolt11InvoiceDescription::Direct(Description::new(String::from("test")).unwrap());

		for i in 0..num_payments {
			let invoice = node_b
				.bolt11_payment()
				.receive(payment_amount_msat, &invoice_description.clone().into(), 3600)
				.unwrap();
			let payment_id = node_a.bolt11_payment().send(&invoice, None).unwrap();
			expect_event!(node_a, PaymentSuccessful);
			expect_event!(node_b, PaymentReceived);

			if (i + 1) % 50 == 0 {
				println!("Completed {} payments", i + 1);
			}

			// Verify payment succeeded
			assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
		}
		println!("All {} payments completed successfully", num_payments);

		// Verify node_a has 200 outbound Bolt11 payments before shutdown
		let outbound_payments_before = node_a.list_payments_with_filter(|p| {
			p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Bolt11 { .. })
		});
		assert_eq!(outbound_payments_before.len(), num_payments);

		// Shut down both nodes
		println!("\nShutting down nodes...");
		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	// Restart node_a with the same config
	println!("\nRestarting node A...");
	let restarted_node_a = setup_node(&chain_source, config_a);

	// Assert all 200 payments are still in the store
	let outbound_payments_after = restarted_node_a.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Outbound && matches!(p.kind, PaymentKind::Bolt11 { .. })
	});
	assert_eq!(
		outbound_payments_after.len(),
		num_payments,
		"Expected {} payments after restart, found {}",
		num_payments,
		outbound_payments_after.len()
	);

	// Verify all payments have the correct status
	for payment in &outbound_payments_after {
		assert_eq!(
			payment.status,
			PaymentStatus::Succeeded,
			"Payment {:?} has unexpected status {:?}",
			payment.id,
			payment.status
		);
		assert_eq!(payment.amount_msat, Some(payment_amount_msat));
	}

	println!(
		"Successfully verified {} payments persisted after restart",
		outbound_payments_after.len()
	);

	restarted_node_a.stop().unwrap();
}

enum OldLdkVersion {
	V0_6_2,
	V0_7_0,
}

async fn build_0_6_2_node(
	bitcoind: &BitcoinD, electrsd: &ElectrsD, storage_path: String, esplora_url: String,
	seed_bytes: [u8; 64],
) -> (u64, bitcoin::secp256k1::PublicKey) {
	let mut builder_old = ldk_node_062::Builder::new();
	builder_old.set_network(bitcoin::Network::Regtest);
	builder_old.set_storage_dir_path(storage_path);
	builder_old.set_entropy_seed_bytes(seed_bytes);
	builder_old.set_chain_source_esplora(esplora_url, None);
	let node_old = builder_old.build().unwrap();

	node_old.start().unwrap();
	let addr_old = node_old.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_old],
		Amount::from_sat(100_000),
	)
	.await;
	node_old.sync_wallets().unwrap();

	let balance = node_old.list_balances().spendable_onchain_balance_sats;
	assert!(balance > 0);
	let node_id = node_old.node_id();

	// Workaround necessary as v0.6.2's runtime wasn't dropsafe in a tokio context.
	tokio::task::block_in_place(move || {
		node_old.stop().unwrap();
		drop(node_old);
	});

	(balance, node_id)
}

async fn build_0_7_0_node(
	bitcoind: &BitcoinD, electrsd: &ElectrsD, esplora_url: String, seed_bytes: [u8; 64],
	config: &TestConfig,
) -> (u64, bitcoin::secp256k1::PublicKey) {
	let mut builder_old = ldk_node_070::Builder::new();
	builder_old.set_network(bitcoin::Network::Regtest);
	builder_old.set_storage_dir_path(config.node_config.storage_dir_path.clone());
	builder_old.set_entropy_seed_bytes(seed_bytes);
	builder_old.set_chain_source_esplora(esplora_url, None);
	let node_old = match config.store_type {
		TestStoreType::FilesystemStore => builder_old.build_with_fs_store().unwrap(),
		TestStoreType::Sqlite => builder_old.build().unwrap(),
		TestStoreType::TestSyncStore => panic!("TestSyncStore not supported in v0.7.0 builder"),
	};

	node_old.start().unwrap();
	let addr_old = node_old.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_old],
		Amount::from_sat(100_000),
	)
	.await;
	node_old.sync_wallets().unwrap();

	let balance = node_old.list_balances().spendable_onchain_balance_sats;
	assert!(balance > 0);
	let node_id = node_old.node_id();

	node_old.stop().unwrap();

	(balance, node_id)
}

async fn do_persistence_backwards_compatibility(version: OldLdkVersion) {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let storage_path = common::random_storage_path().to_str().unwrap().to_owned();
	let seed_bytes = [42u8; 64];

	let (old_balance, old_node_id) = match version {
		OldLdkVersion::V0_6_2 => {
			build_0_6_2_node(
				&bitcoind,
				&electrsd,
				storage_path.clone(),
				esplora_url.clone(),
				seed_bytes,
			)
			.await
		},
		OldLdkVersion::V0_7_0 => {
			let mut config = TestConfig::default();
			config.store_type = TestStoreType::Sqlite;
			config.node_config.storage_dir_path = storage_path.clone();
			build_0_7_0_node(&bitcoind, &electrsd, esplora_url.clone(), seed_bytes, &config).await
		},
	};

	// Now ensure we can still reinit from the same backend.
	#[cfg(feature = "uniffi")]
	let builder_new = Builder::new();
	#[cfg(not(feature = "uniffi"))]
	let mut builder_new = Builder::new();
	builder_new.set_network(bitcoin::Network::Regtest);
	builder_new.set_storage_dir_path(storage_path);
	builder_new.set_chain_source_esplora(esplora_url, None);

	#[cfg(feature = "uniffi")]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes.to_vec()).unwrap();
	#[cfg(not(feature = "uniffi"))]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes);
	let node_new = builder_new.build(node_entropy.into()).unwrap();

	node_new.start().unwrap();
	node_new.sync_wallets().unwrap();

	let new_balance = node_new.list_balances().spendable_onchain_balance_sats;
	let new_node_id = node_new.node_id();

	assert_eq!(old_node_id, new_node_id);
	assert_eq!(old_balance, new_balance);

	node_new.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn persistence_backwards_compatibility() {
	do_persistence_backwards_compatibility(OldLdkVersion::V0_6_2).await;
	do_persistence_backwards_compatibility(OldLdkVersion::V0_7_0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fs_store_persistence_backwards_compatibility() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let storage_path = common::random_storage_path().to_str().unwrap().to_owned();
	let seed_bytes = [42u8; 64];

	// Build a node using v0.7.0's build_with_fs_store (FilesystemStore v1).
	let mut config = TestConfig::default();
	config.node_config.storage_dir_path = storage_path.clone();
	config.store_type = TestStoreType::FilesystemStore;
	let (old_balance, old_node_id) =
		build_0_7_0_node(&bitcoind, &electrsd, esplora_url.clone(), seed_bytes, &config).await;

	// Now reopen with current code's build_with_fs_store, which should
	// auto-migrate from FilesystemStore v1 to FilesystemStoreV2.
	#[cfg(feature = "uniffi")]
	let builder_new = Builder::new();
	#[cfg(not(feature = "uniffi"))]
	let mut builder_new = Builder::new();
	builder_new.set_network(bitcoin::Network::Regtest);
	builder_new.set_storage_dir_path(storage_path);
	builder_new.set_chain_source_esplora(esplora_url, None);

	#[cfg(feature = "uniffi")]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes.to_vec()).unwrap();
	#[cfg(not(feature = "uniffi"))]
	let node_entropy = NodeEntropy::from_seed_bytes(seed_bytes);
	let node_new = builder_new.build_with_fs_store(node_entropy.into()).unwrap();

	node_new.start().unwrap();
	node_new.sync_wallets().unwrap();

	let new_balance = node_new.list_balances().spendable_onchain_balance_sats;
	let new_node_id = node_new.node_id();

	assert_eq!(old_node_id, new_node_id);
	assert_eq!(old_balance, new_balance);

	node_new.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_fee_bump_rbf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	// Fund both nodes
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 500_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Send a transaction from node_b to node_a that we'll later bump
	let amount_to_send_sats = 100_000;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	// Give the chain source time to index the unconfirmed transaction before syncing.
	// Without this, Esplora may not yet have the tx, causing sync to miss it and
	// leaving the BDK wallet graph empty.
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(txid.to_byte_array());
	let original_payment = node_b.payment(&payment_id).unwrap();
	let original_fee = original_payment.fee_paid_msat.unwrap();

	// Non-existent payment id
	let fake_txid =
		Txid::from_str("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
	let invalid_payment_id = PaymentId(fake_txid.to_byte_array());
	assert_eq!(
		Err(NodeError::InvalidPaymentId),
		node_b.onchain_payment().bump_fee_rbf(invalid_payment_id, None)
	);

	// Bump an inbound payment
	assert_eq!(
		Err(NodeError::InvalidPaymentId),
		node_a.onchain_payment().bump_fee_rbf(payment_id, None)
	);

	// Successful fee bump
	let new_txid = node_b.onchain_payment().bump_fee_rbf(payment_id, None).unwrap();
	wait_for_tx(&electrsd.client, new_txid).await;
	// Give the chain source time to index the unconfirmed transaction before syncing.
	// Without this, Esplora may not yet have the tx, causing sync to miss it and
	// leaving the BDK wallet graph empty.
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Verify fee increased and txid updated for node_b
	let new_payment = node_b.payment(&payment_id).unwrap();
	assert!(
		new_payment.fee_paid_msat > Some(original_fee),
		"Fee should increase after RBF bump. Original: {}, New: {}",
		original_fee,
		new_payment.fee_paid_msat.unwrap()
	);
	match &new_payment.kind {
		PaymentKind::Onchain { txid, .. } => {
			assert_eq!(
				*txid, new_txid,
				"node_b payment txid should be updated to the replacement txid"
			);
		},
		_ => panic!("Unexpected payment kind"),
	}

	// Multiple consecutive bumps
	let second_bump_txid = node_b.onchain_payment().bump_fee_rbf(payment_id, None).unwrap();
	wait_for_tx(&electrsd.client, second_bump_txid).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Verify second bump payment exists and txid updated for node_b
	let second_payment = node_b.payment(&payment_id).unwrap();
	assert!(
		second_payment.fee_paid_msat > new_payment.fee_paid_msat,
		"Second bump should have higher fee than first bump"
	);
	match &second_payment.kind {
		PaymentKind::Onchain { txid, .. } => {
			assert_eq!(
				*txid, second_bump_txid,
				"node_b payment txid should be updated to the second replacement txid"
			);
		},
		_ => panic!("Unexpected payment kind"),
	}

	// Confirm the transaction and try to bump again (should fail)
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(
		Err(NodeError::InvalidPaymentId),
		node_b.onchain_payment().bump_fee_rbf(payment_id, None)
	);

	// Verify final payment is confirmed
	let final_payment = node_b.payment(&payment_id).unwrap();
	assert_eq!(final_payment.status, PaymentStatus::Succeeded);
	match final_payment.kind {
		PaymentKind::Onchain { status, .. } => {
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
		},
		_ => panic!("Unexpected payment kind"),
	}

	// Verify node A received the funds correctly
	let node_a_received_payment = node_a.list_payments_with_filter(|p| {
		p.id == payment_id && matches!(p.kind, PaymentKind::Onchain { .. })
	});

	assert_eq!(node_a_received_payment.len(), 1);
	match &node_a_received_payment[0].kind {
		PaymentKind::Onchain { txid: inbound_txid, .. } => {
			assert_eq!(
				*inbound_txid, second_bump_txid,
				"node_a inbound payment txid should be updated to the second replacement txid"
			);
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert_eq!(node_a_received_payment[0].amount_msat, Some(amount_to_send_sats * 1000));
	assert_eq!(node_a_received_payment[0].status, PaymentStatus::Succeeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn onchain_fee_bump_rbf_respects_anchor_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_b, &node_a, 200_000, false, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_b, node_a.node_id());

	let balances_before = node_b.list_balances();
	let reserve = balances_before.total_anchor_channels_reserve_sats;
	assert!(reserve > 0, "Anchor reserve should be non-zero after channel open");
	let spendable_before = balances_before.spendable_onchain_balance_sats;

	let buffer_sats = 5_000;
	assert!(spendable_before > buffer_sats);
	let amount_to_send_sats = spendable_before - buffer_sats;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(txid.to_byte_array());
	let high_fee_rate = bitcoin::FeeRate::from_sat_per_kwu(20_000);
	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_b.onchain_payment().bump_fee_rbf(payment_id, Some(high_fee_rate.into()))
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn open_channel_with_all_with_anchors() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let funding_txo = open_channel_with_all(&node_a, &node_b, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let _user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	// After opening a channel with all balance, the remaining on-chain balance should only
	// be the anchor reserve (25k sats by default) plus a small margin for change
	let anchor_reserve_sat = 25_000;
	let remaining_balance = node_a.list_balances().spendable_onchain_balance_sats;
	assert!(
		remaining_balance < anchor_reserve_sat + 500,
		"Remaining balance {remaining_balance} should be close to the anchor reserve {anchor_reserve_sat}"
	);

	// Verify a channel was opened with most of the funds
	let channels = node_a.list_channels();
	assert_eq!(channels.len(), 1);
	let channel = &channels[0];
	assert!(channel.channel_value_sats > premine_amount_sat - anchor_reserve_sat - 500);
	assert_eq!(channel.counterparty.node_id, node_b.node_id());
	assert_eq!(channel.funding_txo.unwrap(), funding_txo);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[derive(Clone, Copy)]
enum OpenChannelVariant {
	Standard,
	Announced,
	ZeroReserve,
	StandardWithAll,
	AnnouncedWithAll,
	ZeroReserveWithAll,
}

impl OpenChannelVariant {
	fn label(&self) -> &'static str {
		match self {
			Self::Standard => "open_channel",
			Self::Announced => "open_announced_channel",
			Self::ZeroReserve => "open_0reserve_channel",
			Self::StandardWithAll => "open_channel_with_all",
			Self::AnnouncedWithAll => "open_announced_channel_with_all",
			Self::ZeroReserveWithAll => "open_0reserve_channel_with_all",
		}
	}
}

fn open_channel_variant(
	variant: OpenChannelVariant, node_a: &Node, node_b: &Node, channel_amount_sats: u64,
) -> Result<(), NodeError> {
	let address = node_b.listening_addresses().unwrap().first().unwrap().clone();
	match variant {
		OpenChannelVariant::Standard => node_a
			.open_channel(node_b.node_id(), address, channel_amount_sats, None, None)
			.map(|_| ()),
		OpenChannelVariant::Announced => node_a
			.open_announced_channel(node_b.node_id(), address, channel_amount_sats, None, None)
			.map(|_| ()),
		OpenChannelVariant::ZeroReserve => node_a
			.open_0reserve_channel(node_b.node_id(), address, channel_amount_sats, None, None)
			.map(|_| ()),
		OpenChannelVariant::StandardWithAll => {
			node_a.open_channel_with_all(node_b.node_id(), address, None, None).map(|_| ())
		},
		OpenChannelVariant::AnnouncedWithAll => node_a
			.open_announced_channel_with_all(node_b.node_id(), address, None, None)
			.map(|_| ()),
		OpenChannelVariant::ZeroReserveWithAll => {
			node_a.open_0reserve_channel_with_all(node_b.node_id(), address, None, None).map(|_| ())
		},
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn open_channel_variants_reserve_funds_for_anchor_peers() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let exact_variants = [
		OpenChannelVariant::Standard,
		OpenChannelVariant::Announced,
		OpenChannelVariant::ZeroReserve,
	];
	let with_all_variants = [
		OpenChannelVariant::StandardWithAll,
		OpenChannelVariant::AnnouncedWithAll,
		OpenChannelVariant::ZeroReserveWithAll,
	];

	let premine_amount_sat = 1_000_000;
	let exact_channel_amount_sat = premine_amount_sat - 10_000;
	let anchor_reserve_sat = 25_000;

	let mut addresses = Vec::new();
	let mut exact_cases = Vec::new();
	for variant in exact_variants {
		let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);
		addresses.push(node_a.onchain_payment().new_address().unwrap());
		addresses.push(node_b.onchain_payment().new_address().unwrap());
		exact_cases.push((variant, node_a, node_b));
	}

	let mut with_all_cases = Vec::new();
	for variant in with_all_variants {
		let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);
		addresses.push(node_a.onchain_payment().new_address().unwrap());
		addresses.push(node_b.onchain_payment().new_address().unwrap());
		with_all_cases.push((variant, node_a, node_b));
	}

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		addresses,
		Amount::from_sat(premine_amount_sat),
	)
	.await;

	for (_, node_a, node_b) in exact_cases.iter().chain(with_all_cases.iter()) {
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
		assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	}

	for (variant, node_a, node_b) in exact_cases {
		assert_eq!(
			Err(NodeError::InsufficientFunds),
			open_channel_variant(variant, &node_a, &node_b, exact_channel_amount_sat),
			"{} should require funds for the channel amount plus anchor reserve",
			variant.label()
		);
		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	let mut opened_with_all_cases = Vec::new();
	for (variant, node_a, node_b) in with_all_cases {
		open_channel_variant(variant, &node_a, &node_b, 0)
			.unwrap_or_else(|e| panic!("{} failed: {e:?}", variant.label()));

		let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
		let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
		assert_eq!(funding_txo_a, funding_txo_b, "{} funding txo mismatch", variant.label());
		wait_for_tx(&electrsd.client, funding_txo_a.txid).await;

		opened_with_all_cases.push((variant, node_a, node_b, funding_txo_a));
	}

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	for (variant, node_a, node_b, funding_txo) in opened_with_all_cases {
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();

		let _user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
		let _user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

		let balances = node_a.list_balances();
		assert_eq!(balances.total_onchain_balance_sats, anchor_reserve_sat - 1);
		assert_eq!(balances.total_anchor_channels_reserve_sats, anchor_reserve_sat - 1);
		assert_eq!(balances.spendable_onchain_balance_sats, 0);

		let channels = node_a.list_channels();
		assert_eq!(channels.len(), 1, "{} should have one channel", variant.label());
		let channel = &channels[0];
		// Also subtract the fees spent to open the channel
		assert_eq!(channel.channel_value_sats, premine_amount_sat - anchor_reserve_sat - 155);
		assert_eq!(channel.counterparty.node_id, node_b.node_id());
		assert!(channel.counterparty.features.supports_anchors_zero_fee_htlc_tx());
		assert!(!channel.counterparty.features.requires_anchors_zero_fee_htlc_tx());
		assert_eq!(channel.funding_txo.unwrap(), funding_txo);
		assert_eq!(channel.reserve_type, Some(ReserveType::Adaptive));

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn splice_in_with_all_balance() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 5_000_000;
	let channel_amount_sat = 1_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	// Open a channel with a fixed amount first
	let funding_txo = open_channel(&node_a, &node_b, channel_amount_sat, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let _user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	let channels = node_a.list_channels();
	assert_eq!(channels.len(), 1);
	assert_eq!(channels[0].channel_value_sats, channel_amount_sat);
	assert_eq!(channels[0].funding_txo.unwrap(), funding_txo);

	let balance_before_splice = node_a.list_balances().spendable_onchain_balance_sats;
	assert!(balance_before_splice > 0);

	// Splice in with all remaining on-chain funds
	splice_in_with_all(&node_a, &node_b, &user_channel_id_a, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let _user_channel_id_a2 = expect_channel_ready_event!(node_a, node_b.node_id());
	let _user_channel_id_b2 = expect_channel_ready_event!(node_b, node_a.node_id());

	// After splicing with all balance, channel value should be close to the premined amount
	// minus fees and anchor reserve
	let anchor_reserve_sat = 25_000;
	let channels = node_a.list_channels();
	assert_eq!(channels.len(), 1);
	let channel = &channels[0];
	assert!(
		channel.channel_value_sats > premine_amount_sat - anchor_reserve_sat - 1000,
		"Channel value {} should be close to premined amount {} minus anchor reserve {} and fees",
		channel.channel_value_sats,
		premine_amount_sat,
		anchor_reserve_sat,
	);

	// Remaining on-chain balance should be close to just the anchor reserve
	let remaining_balance = node_a.list_balances().spendable_onchain_balance_sats;
	assert!(
		remaining_balance < anchor_reserve_sat + 500,
		"Remaining balance {remaining_balance} should be close to the anchor reserve {anchor_reserve_sat}"
	);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_multi_lsp_picks_cheapest() {
	do_lsps2_multi_lsp_picks_cheapest(false, None).await;
	do_lsps2_multi_lsp_picks_cheapest(true, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn lsps2_multi_lsp_rejects_fees_above_limit() {
	do_lsps2_multi_lsp_picks_cheapest(false, Some(999_999)).await;
}

async fn do_lsps2_multi_lsp_picks_cheapest(
	reverse_order: bool, max_total_lsp_fee_limit_msat: Option<u64>,
) {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let mut sync_config = EsploraSyncConfig::default();
	sync_config.background_sync_config = None;

	// Cheap LSP: 10_000 ppm.
	let cheap_opening_fee_ppm = 10_000;
	let cheap_cfg = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm: cheap_opening_fee_ppm,
		channel_over_provisioning_ppm: 100_000,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 10,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: true,
		disable_client_reserve: false,
	};
	let cheap_node_config = random_config();
	setup_builder!(cheap_builder, cheap_node_config.node_config);
	cheap_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	cheap_builder.enable_liquidity_provider(cheap_cfg);
	let cheap = cheap_builder.build(cheap_node_config.node_entropy.into()).unwrap();
	cheap.start().unwrap();
	let cheap_id = cheap.node_id();
	let cheap_addr = cheap.listening_addresses().unwrap().first().unwrap().clone();

	// Expensive LSP: 20_000 ppm.
	let expensive_cfg = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm: 20_000,
		channel_over_provisioning_ppm: 200_000,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 5,
		max_client_to_self_delay: 1024,
		client_trusts_lsp: true,
		disable_client_reserve: false,
	};
	let expensive_node_config = random_config();
	setup_builder!(expensive_builder, expensive_node_config.node_config);
	expensive_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	expensive_builder.enable_liquidity_provider(expensive_cfg);
	let expensive = expensive_builder.build(expensive_node_config.node_entropy.into()).unwrap();
	expensive.start().unwrap();
	let expensive_id = expensive.node_id();
	let expensive_addr = expensive.listening_addresses().unwrap().first().unwrap().clone();

	// Client knows both LSPs. Registration order is varied to confirm selection isn't order-based.
	let mut client_config = random_config();
	client_config.node_config.lsps2_max_total_lsp_fee_limit_msat = max_total_lsp_fee_limit_msat;
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	if reverse_order {
		client_builder.add_liquidity_source(expensive_id, expensive_addr, None, true);
		client_builder.add_liquidity_source(cheap_id, cheap_addr, None, true);
	} else {
		client_builder.add_liquidity_source(cheap_id, cheap_addr, None, true);
		client_builder.add_liquidity_source(expensive_id, expensive_addr, None, true);
	}
	let client = client_builder.build(client_config.node_entropy.into()).unwrap();
	client.start().unwrap();

	let payer_config = random_config();
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer = payer_builder.build(payer_config.node_entropy.into()).unwrap();
	payer.start().unwrap();

	let client_addr = client.listening_addresses().unwrap().first().unwrap().clone();
	payer.connect(client.node_id(), client_addr, false).unwrap();

	let cheap_onchain_addr = cheap.onchain_payment().new_address().unwrap();
	let client_onchain_addr = client.onchain_payment().new_address().unwrap();
	let payer_onchain_addr = payer.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![cheap_onchain_addr, client_onchain_addr, payer_onchain_addr],
		Amount::from_sat(10_000_000),
	)
	.await;
	cheap.sync_wallets().unwrap();
	client.sync_wallets().unwrap();
	payer.sync_wallets().unwrap();

	open_channel(&payer, &cheap, 5_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	cheap.sync_wallets().unwrap();
	payer.sync_wallets().unwrap();
	expect_channel_ready_event!(payer, cheap.node_id());
	expect_channel_ready_event!(cheap, payer.node_id());
	while payer.status().latest_node_announcement_broadcast_timestamp.is_none() {
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	tokio::time::sleep(std::time::Duration::from_secs(1)).await;

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap()).into();
	let jit_invoice_result =
		client.bolt11_payment().receive_via_jit_channel(100_000_000, &invoice_description, 1024);
	if max_total_lsp_fee_limit_msat.is_some() {
		assert!(matches!(jit_invoice_result, Err(NodeError::LiquidityFeeTooHigh)));
		payer.stop().unwrap();
		client.stop().unwrap();
		cheap.stop().unwrap();
		expensive.stop().unwrap();
		return;
	}
	let jit_invoice = jit_invoice_result.unwrap();

	// The route hint's src_node_id is the LSP the client picked.
	let route_hints = jit_invoice.route_hints();
	let first_hint = route_hints.first().expect("JIT invoice should have a route hint");
	#[cfg(feature = "uniffi")]
	let first_hop = first_hint.first();
	#[cfg(not(feature = "uniffi"))]
	let first_hop = first_hint.0.first();
	let route_hint_src = first_hop.expect("route hint should have at least one hop").src_node_id;
	assert_eq!(route_hint_src, cheap_id, "expected cheaper LSP to be selected.");

	// Consuming the BOLT11 lease schedules a replacement. The BOLT12 flow shares that cache and
	// must retain the same cheapest-LSP selection regardless of registration order.
	let payment_amount_msat = 100_000_000;
	let offer =
		client.bolt12_payment().receive(payment_amount_msat, "multi LSP", None, None).unwrap();
	let payment_id = payer.bolt12_payment().send(&offer, None, None, None).unwrap();

	expect_channel_pending_event!(cheap, client.node_id());
	expect_channel_ready_event!(cheap, client.node_id());
	expect_event!(cheap, PaymentForwarded);
	expect_channel_pending_event!(client, cheap.node_id());
	expect_channel_ready_event!(client, cheap.node_id());
	expect_payment_successful_event!(payer, Some(payment_id), None);
	let fee_msat = payment_amount_msat * cheap_opening_fee_ppm as u64 / 1_000_000;
	let receiver_payment_id =
		expect_payment_received_event!(client, payment_amount_msat - fee_msat).unwrap();
	match client.payment(&receiver_payment_id).unwrap().kind {
		PaymentKind::Bolt12Offer { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	payer.stop().unwrap();
	client.stop().unwrap();
	cheap.stop().unwrap();
	expensive.stop().unwrap();
}
