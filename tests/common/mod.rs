// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(any(test, cln_test, lnd_test, vss_test))]
#![allow(dead_code)]

pub(crate) mod logging;

use std::collections::{HashMap, HashSet};
use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::{
	Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, Txid, Witness,
};
use electrsd::corepc_node::{Client as BitcoindClient, Node as BitcoinD};
use electrsd::{corepc_node, ElectrsD};
use electrum_client::ElectrumApi;
use ldk_node::config::{AsyncPaymentsRole, Config, ElectrumSyncConfig, EsploraSyncConfig};
use ldk_node::entropy::{generate_entropy_mnemonic, NodeEntropy};
use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::payment::{PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::{
	Builder, CustomTlvRecord, Event, LightningBalance, Node, NodeError, PendingSweepBalance,
};
use lightning::io;
use lightning::ln::msgs::SocketAddress;
use lightning::routing::gossip::NodeAlias;
use lightning::util::persist::{KVStore, KVStoreSync};
use lightning::util::test_utils::TestStore;
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use lightning_persister::fs_store::FilesystemStore;
use lightning_types::payment::{PaymentHash, PaymentPreimage};
use logging::TestLogWriter;
use rand::distr::Alphanumeric;
use rand::{rng, Rng};
use serde_json::{json, Value};

macro_rules! expect_event {
	($node:expr, $event_type:ident) => {{
		match $node.next_event_async().await {
			ref e @ Event::$event_type { .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				$node.event_handled().unwrap();
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			},
		}
	}};
}

pub(crate) use expect_event;

macro_rules! expect_channel_pending_event {
	($node:expr, $counterparty_node_id:expr) => {{
		match $node.next_event_async().await {
			ref e @ Event::ChannelPending { funding_txo, counterparty_node_id, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(counterparty_node_id, $counterparty_node_id);
				$node.event_handled().unwrap();
				funding_txo
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			},
		}
	}};
}

pub(crate) use expect_channel_pending_event;

macro_rules! expect_channel_ready_event {
	($node:expr, $counterparty_node_id:expr) => {{
		match $node.next_event_async().await {
			ref e @ Event::ChannelReady { user_channel_id, counterparty_node_id, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(counterparty_node_id, Some($counterparty_node_id));
				$node.event_handled().unwrap();
				user_channel_id
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			},
		}
	}};
}

pub(crate) use expect_channel_ready_event;

macro_rules! expect_splice_pending_event {
	($node:expr, $counterparty_node_id:expr) => {{
		match $node.next_event_async().await {
			ref e @ Event::SplicePending { new_funding_txo, counterparty_node_id, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(counterparty_node_id, $counterparty_node_id);
				$node.event_handled().unwrap();
				new_funding_txo
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			},
		}
	}};
}

pub(crate) use expect_splice_pending_event;

macro_rules! expect_payment_received_event {
	($node:expr, $amount_msat:expr) => {{
		match $node.next_event_async().await {
			ref e @ Event::PaymentReceived { payment_id, amount_msat, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(amount_msat, $amount_msat);
				let payment = $node.payment(&payment_id.unwrap()).unwrap();
				if !matches!(payment.kind, PaymentKind::Onchain { .. }) {
					assert_eq!(payment.fee_paid_msat, None);
				}
				$node.event_handled().unwrap();
				payment_id
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
			},
		}
	}};
}

pub(crate) use expect_payment_received_event;

macro_rules! expect_payment_claimable_event {
	($node:expr, $payment_id:expr, $payment_hash:expr, $claimable_amount_msat:expr) => {{
		match $node.next_event_async().await {
			ref e @ Event::PaymentClaimable {
				payment_id,
				payment_hash,
				claimable_amount_msat,
				..
			} => {
				println!("{} got event {:?}", std::stringify!($node), e);
				assert_eq!(payment_hash, $payment_hash);
				assert_eq!(payment_id, $payment_id);
				assert_eq!(claimable_amount_msat, $claimable_amount_msat);
				$node.event_handled().unwrap();
				claimable_amount_msat
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			},
		}
	}};
}

pub(crate) use expect_payment_claimable_event;

macro_rules! expect_payment_successful_event {
	($node:expr, $payment_id:expr, $fee_paid_msat:expr) => {{
		match $node.next_event_async().await {
			ref e @ Event::PaymentSuccessful { payment_id, fee_paid_msat, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				if let Some(fee_msat) = $fee_paid_msat {
					assert_eq!(fee_paid_msat, fee_msat);
				}
				let payment = $node.payment(&$payment_id.unwrap()).unwrap();
				assert_eq!(payment.fee_paid_msat, fee_paid_msat);
				assert_eq!(payment_id, $payment_id);
				$node.event_handled().unwrap();
			},
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
			},
		}
	}};
}

pub(crate) use expect_payment_successful_event;

pub(crate) fn setup_bitcoind_and_electrsd() -> (BitcoinD, ElectrsD) {
	let bitcoind_exe =
		env::var("BITCOIND_EXE").ok().or_else(|| corepc_node::downloaded_exe_path().ok()).expect(
			"you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
		);
	let mut bitcoind_conf = corepc_node::Conf::default();
	bitcoind_conf.network = "regtest";
	bitcoind_conf.args.push("-rest");
	let bitcoind = BitcoinD::with_conf(bitcoind_exe, &bitcoind_conf).unwrap();

	let electrs_exe = env::var("ELECTRS_EXE")
		.ok()
		.or_else(electrsd::downloaded_exe_path)
		.expect("you need to provide env var ELECTRS_EXE or specify an electrsd version feature");
	let mut electrsd_conf = electrsd::Conf::default();
	electrsd_conf.http_enabled = true;
	electrsd_conf.network = "regtest";
	let electrsd = ElectrsD::with_conf(electrs_exe, &bitcoind, &electrsd_conf).unwrap();
	(bitcoind, electrsd)
}

pub(crate) fn random_storage_path() -> PathBuf {
	let mut temp_path = std::env::temp_dir();
	let mut rng = rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

pub(crate) fn random_listening_addresses() -> Vec<SocketAddress> {
	let num_addresses = 2;
	let mut listening_addresses = HashSet::new();

	while listening_addresses.len() < num_addresses {
		let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address: SocketAddress = socket.local_addr().unwrap().into();
		listening_addresses.insert(address);
	}

	listening_addresses.into_iter().collect()
}

pub(crate) fn random_node_alias() -> Option<NodeAlias> {
	let mut rng = rng();
	let rand_val = rng.random_range(0..1000);
	let alias = format!("ldk-node-{}", rand_val);
	let mut bytes = [0u8; 32];
	bytes[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());
	Some(NodeAlias(bytes))
}

pub(crate) fn random_config(anchor_channels: bool) -> TestConfig {
	let mut node_config = Config::default();

	if !anchor_channels {
		node_config.anchor_channels_config = None;
	}

	node_config.network = Network::Regtest;
	println!("Setting network: {}", node_config.network);

	let rand_dir = random_storage_path();
	println!("Setting random LDK storage dir: {}", rand_dir.display());
	node_config.storage_dir_path = rand_dir.to_str().unwrap().to_owned();

	let rand_listening_addresses = random_listening_addresses();
	println!("Setting random LDK listening addresses: {:?}", rand_listening_addresses);
	node_config.listening_addresses = Some(rand_listening_addresses);

	let alias = random_node_alias();
	println!("Setting random LDK node alias: {:?}", alias);
	node_config.node_alias = alias;

	TestConfig { node_config, ..Default::default() }
}

#[cfg(feature = "uniffi")]
type TestNode = Arc<Node>;
#[cfg(not(feature = "uniffi"))]
type TestNode = Node;

#[derive(Clone)]
pub(crate) enum TestChainSource<'a> {
	Esplora(&'a ElectrsD),
	Electrum(&'a ElectrsD),
	BitcoindRpcSync(&'a BitcoinD),
	BitcoindRestSync(&'a BitcoinD),
}

#[derive(Clone, Copy)]
pub(crate) enum TestStoreType {
	TestSyncStore,
	Sqlite,
}

impl Default for TestStoreType {
	fn default() -> Self {
		TestStoreType::TestSyncStore
	}
}

#[derive(Clone)]
pub(crate) struct TestConfig {
	pub node_config: Config,
	pub log_writer: TestLogWriter,
	pub store_type: TestStoreType,
	pub node_entropy: NodeEntropy,
}

impl Default for TestConfig {
	fn default() -> Self {
		let node_config = Default::default();
		let log_writer = Default::default();
		let store_type = Default::default();

		let mnemonic = generate_entropy_mnemonic(None);
		let node_entropy = NodeEntropy::from_bip39_mnemonic(mnemonic, None);
		TestConfig { node_config, log_writer, store_type, node_entropy }
	}
}

macro_rules! setup_builder {
	($builder:ident, $config:expr) => {
		#[cfg(feature = "uniffi")]
		let $builder = Builder::from_config($config.clone());
		#[cfg(not(feature = "uniffi"))]
		let mut $builder = Builder::from_config($config.clone());
	};
}

pub(crate) use setup_builder;

pub(crate) fn setup_two_nodes(
	chain_source: &TestChainSource, allow_0conf: bool, anchor_channels: bool,
	anchors_trusted_no_reserve: bool,
) -> (TestNode, TestNode) {
	setup_two_nodes_with_store(
		chain_source,
		allow_0conf,
		anchor_channels,
		anchors_trusted_no_reserve,
		TestStoreType::TestSyncStore,
	)
}

pub(crate) fn setup_two_nodes_with_store(
	chain_source: &TestChainSource, allow_0conf: bool, anchor_channels: bool,
	anchors_trusted_no_reserve: bool, store_type: TestStoreType,
) -> (TestNode, TestNode) {
	println!("== Node A ==");
	let mut config_a = random_config(anchor_channels);
	config_a.store_type = store_type;
	let node_a = setup_node(chain_source, config_a);

	println!("\n== Node B ==");
	let mut config_b = random_config(anchor_channels);
	config_b.store_type = store_type;
	if allow_0conf {
		config_b.node_config.trusted_peers_0conf.push(node_a.node_id());
	}
	if anchor_channels && anchors_trusted_no_reserve {
		config_b
			.node_config
			.anchor_channels_config
			.as_mut()
			.unwrap()
			.trusted_peers_no_reserve
			.push(node_a.node_id());
	}
	let node_b = setup_node(chain_source, config_b);
	(node_a, node_b)
}

pub(crate) fn setup_node(chain_source: &TestChainSource, config: TestConfig) -> TestNode {
	setup_node_for_async_payments(chain_source, config, None)
}

pub(crate) fn setup_node_for_async_payments(
	chain_source: &TestChainSource, config: TestConfig,
	async_payments_role: Option<AsyncPaymentsRole>,
) -> TestNode {
	setup_builder!(builder, config.node_config);
	match chain_source {
		TestChainSource::Esplora(electrsd) => {
			let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
			let mut sync_config = EsploraSyncConfig::default();
			sync_config.background_sync_config = None;
			builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
		},
		TestChainSource::Electrum(electrsd) => {
			let electrum_url = format!("tcp://{}", electrsd.electrum_url);
			let mut sync_config = ElectrumSyncConfig::default();
			sync_config.background_sync_config = None;
			builder.set_chain_source_electrum(electrum_url.clone(), Some(sync_config));
		},
		TestChainSource::BitcoindRpcSync(bitcoind) => {
			let rpc_host = bitcoind.params.rpc_socket.ip().to_string();
			let rpc_port = bitcoind.params.rpc_socket.port();
			let values = bitcoind.params.get_cookie_values().unwrap().unwrap();
			let rpc_user = values.user;
			let rpc_password = values.password;
			builder.set_chain_source_bitcoind_rpc(rpc_host, rpc_port, rpc_user, rpc_password);
		},
		TestChainSource::BitcoindRestSync(bitcoind) => {
			let rpc_host = bitcoind.params.rpc_socket.ip().to_string();
			let rpc_port = bitcoind.params.rpc_socket.port();
			let values = bitcoind.params.get_cookie_values().unwrap().unwrap();
			let rpc_user = values.user;
			let rpc_password = values.password;
			let rest_host = bitcoind.params.rpc_socket.ip().to_string();
			let rest_port = bitcoind.params.rpc_socket.port();
			builder.set_chain_source_bitcoind_rest(
				rest_host,
				rest_port,
				rpc_host,
				rpc_port,
				rpc_user,
				rpc_password,
			);
		},
	}

	match &config.log_writer {
		TestLogWriter::FileWriter => {
			builder.set_filesystem_logger(None, None);
		},
		TestLogWriter::LogFacade => {
			builder.set_log_facade_logger();
		},
		TestLogWriter::Custom(custom_log_writer) => {
			builder.set_custom_logger(Arc::clone(custom_log_writer));
		},
	}

	builder.set_async_payments_role(async_payments_role).unwrap();

	let node = match config.store_type {
		TestStoreType::TestSyncStore => {
			let kv_store = TestSyncStore::new(config.node_config.storage_dir_path.into());
			builder.build_with_store(config.node_entropy.into(), kv_store).unwrap()
		},
		TestStoreType::Sqlite => builder.build(config.node_entropy.into()).unwrap(),
	};

	node.start().unwrap();
	assert!(node.status().is_running);
	assert!(node.status().latest_fee_rate_cache_update_timestamp.is_some());
	node
}

pub(crate) async fn generate_blocks_and_wait<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, num: usize,
) {
	let _ = bitcoind.create_wallet("ldk_node_test");
	let _ = bitcoind.load_wallet("ldk_node_test");
	print!("Generating {} blocks...", num);
	let blockchain_info = bitcoind.get_blockchain_info().expect("failed to get blockchain info");
	let cur_height = blockchain_info.blocks;
	let address = bitcoind.new_address().expect("failed to get new address");
	// TODO: expect this Result once the WouldBlock issue is resolved upstream.
	let _block_hashes_res = bitcoind.generate_to_address(num, &address);
	wait_for_block(electrs, cur_height as usize + num).await;
	print!(" Done!");
	println!("\n");
}

pub(crate) fn invalidate_blocks(bitcoind: &BitcoindClient, num_blocks: usize) {
	let blockchain_info = bitcoind.get_blockchain_info().expect("failed to get blockchain info");
	let cur_height = blockchain_info.blocks as usize;
	let target_height = cur_height - num_blocks + 1;
	let block_hash = bitcoind
		.get_block_hash(target_height as u64)
		.expect("failed to get block hash")
		.block_hash()
		.expect("block hash should be present");
	bitcoind.invalidate_block(block_hash).expect("failed to invalidate block");
	let blockchain_info = bitcoind.get_blockchain_info().expect("failed to get blockchain info");
	let new_cur_height = blockchain_info.blocks as usize;
	assert!(new_cur_height + num_blocks == cur_height);
}

pub(crate) async fn wait_for_block<E: ElectrumApi>(electrs: &E, min_height: usize) {
	let mut header = match electrs.block_headers_subscribe() {
		Ok(header) => header,
		Err(_) => {
			// While subscribing should succeed the first time around, we ran into some cases where
			// it didn't. Since we can't proceed without subscribing, we try again after a delay
			// and panic if it still fails.
			tokio::time::sleep(Duration::from_secs(3)).await;
			electrs.block_headers_subscribe().expect("failed to subscribe to block headers")
		},
	};
	loop {
		if header.height >= min_height {
			break;
		}
		header = exponential_backoff_poll(|| {
			electrs.ping().expect("failed to ping electrs");
			electrs.block_headers_pop().expect("failed to pop block header")
		})
		.await;
	}
}

pub(crate) async fn wait_for_tx<E: ElectrumApi>(electrs: &E, txid: Txid) {
	if electrs.transaction_get(&txid).is_ok() {
		return;
	}

	exponential_backoff_poll(|| {
		electrs.ping().unwrap();
		electrs.transaction_get(&txid).ok()
	})
	.await;
}

pub(crate) async fn wait_for_outpoint_spend<E: ElectrumApi>(electrs: &E, outpoint: OutPoint) {
	let tx = electrs.transaction_get(&outpoint.txid).unwrap();
	let txout_script = tx.output.get(outpoint.vout as usize).unwrap().clone().script_pubkey;

	let is_spent = !electrs.script_get_history(&txout_script).unwrap().is_empty();
	if is_spent {
		return;
	}

	exponential_backoff_poll(|| {
		electrs.ping().unwrap();

		let is_spent = !electrs.script_get_history(&txout_script).unwrap().is_empty();
		is_spent.then_some(())
	})
	.await;
}

pub(crate) async fn exponential_backoff_poll<T, F>(mut poll: F) -> T
where
	F: FnMut() -> Option<T>,
{
	let mut delay = Duration::from_millis(64);
	let mut tries = 0;
	loop {
		match poll() {
			Some(data) => break data,
			None if delay.as_millis() < 512 => {
				delay = delay.mul_f32(2.0);
			},

			None => {},
		}
		assert!(tries < 20, "Reached max tries.");
		tries += 1;
		tokio::time::sleep(delay).await;
	}
}

pub(crate) async fn premine_and_distribute_funds<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, addrs: Vec<Address>, amount: Amount,
) {
	premine_blocks(bitcoind, electrs).await;

	distribute_funds_unconfirmed(bitcoind, electrs, addrs, amount).await;
	generate_blocks_and_wait(bitcoind, electrs, 1).await;
}

pub(crate) async fn premine_blocks<E: ElectrumApi>(bitcoind: &BitcoindClient, electrs: &E) {
	let _ = bitcoind.create_wallet("ldk_node_test");
	let _ = bitcoind.load_wallet("ldk_node_test");
	generate_blocks_and_wait(bitcoind, electrs, 101).await;
}

pub(crate) async fn distribute_funds_unconfirmed<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, addrs: Vec<Address>, amount: Amount,
) -> Txid {
	let mut amounts = HashMap::<String, f64>::new();
	for addr in &addrs {
		amounts.insert(addr.to_string(), amount.to_btc());
	}

	let empty_account = json!("");
	let amounts_json = json!(amounts);
	let txid = bitcoind
		.call::<Value>("sendmany", &[empty_account, amounts_json])
		.unwrap()
		.as_str()
		.unwrap()
		.parse()
		.unwrap();

	wait_for_tx(electrs, txid).await;

	txid
}

pub(crate) fn prepare_rbf<E: ElectrumApi>(
	electrs: &E, txid: Txid, scripts_buf: &HashSet<ScriptBuf>,
) -> (Transaction, usize) {
	let tx = electrs.transaction_get(&txid).unwrap();

	let fee_output_index = tx
		.output
		.iter()
		.position(|output| !scripts_buf.contains(&output.script_pubkey))
		.expect("No output available for fee bumping");

	(tx, fee_output_index)
}

pub(crate) async fn bump_fee_and_broadcast<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, mut tx: Transaction, fee_output_index: usize,
	is_insert_block: bool,
) -> Transaction {
	let mut bump_fee_amount_sat = tx.vsize() as u64;
	let attempts = 5;

	for _ in 0..attempts {
		let fee_output = &mut tx.output[fee_output_index];
		let new_fee_value = fee_output.value.to_sat().saturating_sub(bump_fee_amount_sat);
		if new_fee_value < 546 {
			panic!("Warning: Fee output approaching dust limit ({} sats)", new_fee_value);
		}
		fee_output.value = Amount::from_sat(new_fee_value);

		for input in &mut tx.input {
			input.sequence = Sequence::ENABLE_RBF_NO_LOCKTIME;
			input.script_sig = ScriptBuf::new();
			input.witness = Witness::new();
		}

		let signed_result = bitcoind.sign_raw_transaction_with_wallet(&tx).unwrap();
		assert!(signed_result.complete, "Failed to sign RBF transaction");

		let tx_bytes = Vec::<u8>::from_hex(&signed_result.hex).unwrap();
		tx = bitcoin::consensus::encode::deserialize::<Transaction>(&tx_bytes).unwrap();

		match bitcoind.send_raw_transaction(&tx) {
			Ok(res) => {
				if is_insert_block {
					generate_blocks_and_wait(bitcoind, electrs, 1).await;
				}
				let new_txid: Txid = res.0.parse().unwrap();
				wait_for_tx(electrs, new_txid).await;
				return tx;
			},
			Err(_) => {
				bump_fee_amount_sat += bump_fee_amount_sat * 5;
				if tx.output[fee_output_index].value.to_sat() < bump_fee_amount_sat {
					panic!("Insufficient funds to increase fee");
				}
			},
		}
	}

	panic!("Failed to bump fee after {} attempts", attempts);
}

pub async fn open_channel(
	node_a: &TestNode, node_b: &TestNode, funding_amount_sat: u64, should_announce: bool,
	electrsd: &ElectrsD,
) -> OutPoint {
	open_channel_push_amt(node_a, node_b, funding_amount_sat, None, should_announce, electrsd).await
}

pub async fn open_channel_push_amt(
	node_a: &TestNode, node_b: &TestNode, funding_amount_sat: u64, push_amount_msat: Option<u64>,
	should_announce: bool, electrsd: &ElectrsD,
) -> OutPoint {
	if should_announce {
		node_a
			.open_announced_channel(
				node_b.node_id(),
				node_b.listening_addresses().unwrap().first().unwrap().clone(),
				funding_amount_sat,
				push_amount_msat,
				None,
			)
			.unwrap();
	} else {
		node_a
			.open_channel(
				node_b.node_id(),
				node_b.listening_addresses().unwrap().first().unwrap().clone(),
				funding_amount_sat,
				push_amount_msat,
				None,
			)
			.unwrap();
	}
	assert!(node_a.list_peers().iter().find(|c| { c.node_id == node_b.node_id() }).is_some());

	let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
	let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
	assert_eq!(funding_txo_a, funding_txo_b);
	wait_for_tx(&electrsd.client, funding_txo_a.txid).await;

	funding_txo_a
}

pub async fn open_channel_with_all(
	node_a: &TestNode, node_b: &TestNode, should_announce: bool, electrsd: &ElectrsD,
) -> OutPoint {
	if should_announce {
		node_a
			.open_announced_channel_with_all(
				node_b.node_id(),
				node_b.listening_addresses().unwrap().first().unwrap().clone(),
				None,
				None,
			)
			.unwrap();
	} else {
		node_a
			.open_channel_with_all(
				node_b.node_id(),
				node_b.listening_addresses().unwrap().first().unwrap().clone(),
				None,
				None,
			)
			.unwrap();
	}
	assert!(node_a.list_peers().iter().find(|c| { c.node_id == node_b.node_id() }).is_some());

	let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
	let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
	assert_eq!(funding_txo_a, funding_txo_b);
	wait_for_tx(&electrsd.client, funding_txo_a.txid).await;

	funding_txo_a
}

pub(crate) async fn do_channel_full_cycle<E: ElectrumApi>(
	node_a: TestNode, node_b: TestNode, bitcoind: &BitcoindClient, electrsd: &E, allow_0conf: bool,
	expect_anchor_channel: bool, force_close: bool,
) {
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = if expect_anchor_channel { 2_125_000 } else { 2_100_000 };

	premine_and_distribute_funds(
		&bitcoind,
		electrsd,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	// Check we saw the node funding transactions.
	assert_eq!(
		node_a
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		1
	);
	assert_eq!(
		node_a
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		0
	);
	assert_eq!(
		node_b
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		1
	);
	assert_eq!(
		node_b
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		0
	);

	// Check we haven't got any events yet
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_b.next_event(), None);

	println!("\nA -- open_channel -> B");
	let funding_amount_sat = 2_080_000;
	let push_msat = (funding_amount_sat / 2) * 1000; // balance the channel
	node_a
		.open_announced_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			funding_amount_sat,
			Some(push_msat),
			None,
		)
		.unwrap();

	assert_eq!(node_a.list_peers().first().unwrap().node_id, node_b.node_id());
	assert!(node_a.list_peers().first().unwrap().is_persisted);
	let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
	let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
	assert_eq!(funding_txo_a, funding_txo_b);

	wait_for_tx(electrsd, funding_txo_a.txid).await;

	if !allow_0conf {
		generate_blocks_and_wait(&bitcoind, electrsd, 6).await;
	}

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Check we now see the channel funding transaction as outbound.
	assert_eq!(
		node_a
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		1
	);

	let onchain_fee_buffer_sat = 5000;
	let node_a_anchor_reserve_sat = if expect_anchor_channel { 25_000 } else { 0 };
	let node_a_upper_bound_sat =
		premine_amount_sat - node_a_anchor_reserve_sat - funding_amount_sat;
	let node_a_lower_bound_sat = premine_amount_sat
		- node_a_anchor_reserve_sat
		- funding_amount_sat
		- onchain_fee_buffer_sat;
	assert!(node_a.list_balances().spendable_onchain_balance_sats < node_a_upper_bound_sat);
	assert!(node_a.list_balances().spendable_onchain_balance_sats > node_a_lower_bound_sat);
	assert_eq!(
		node_a.list_balances().total_anchor_channels_reserve_sats,
		node_a_anchor_reserve_sat
	);

	let node_b_anchor_reserve_sat = if node_b
		.config()
		.anchor_channels_config
		.map_or(true, |acc| acc.trusted_peers_no_reserve.contains(&node_a.node_id()))
	{
		0
	} else {
		25_000
	};
	assert_eq!(
		node_b.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat - node_b_anchor_reserve_sat
	);
	assert_eq!(
		node_b.list_balances().total_anchor_channels_reserve_sats,
		node_b_anchor_reserve_sat
	);

	let user_channel_id_a = expect_channel_ready_event!(node_a, node_b.node_id());
	let user_channel_id_b = expect_channel_ready_event!(node_b, node_a.node_id());

	println!("\nB receive");
	let invoice_amount_1_msat = 2500_000;
	let invoice_description: Bolt11InvoiceDescription =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let invoice = node_b
		.bolt11_payment()
		.receive(invoice_amount_1_msat, &invoice_description.clone().into(), 9217)
		.unwrap();

	println!("\nA send");
	let payment_id = node_a.bolt11_payment().send(&invoice, None).unwrap();
	assert_eq!(node_a.bolt11_payment().send(&invoice, None), Err(NodeError::DuplicatePayment));

	assert!(!node_a.list_payments_with_filter(|p| p.id == payment_id).is_empty());

	let outbound_payments_a = node_a.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Outbound && matches!(p.kind, PaymentKind::Bolt11 { .. })
	});
	assert_eq!(outbound_payments_a.len(), 1);

	let inbound_payments_a = node_a.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Inbound && matches!(p.kind, PaymentKind::Bolt11 { .. })
	});
	assert_eq!(inbound_payments_a.len(), 0);

	let outbound_payments_b = node_b.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Outbound && matches!(p.kind, PaymentKind::Bolt11 { .. })
	});
	assert_eq!(outbound_payments_b.len(), 0);

	let inbound_payments_b = node_b.list_payments_with_filter(|p| {
		p.direction == PaymentDirection::Inbound && matches!(p.kind, PaymentKind::Bolt11 { .. })
	});
	assert_eq!(inbound_payments_b.len(), 1);

	expect_event!(node_a, PaymentSuccessful);
	expect_event!(node_b, PaymentReceived);
	assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_id).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_id).unwrap().amount_msat, Some(invoice_amount_1_msat));
	assert!(matches!(node_a.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));
	assert_eq!(node_b.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_id).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_id).unwrap().amount_msat, Some(invoice_amount_1_msat));
	assert!(matches!(node_b.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));

	// Assert we fail duplicate outbound payments and check the status hasn't changed.
	assert_eq!(Err(NodeError::DuplicatePayment), node_a.bolt11_payment().send(&invoice, None));
	assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_id).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_id).unwrap().amount_msat, Some(invoice_amount_1_msat));
	assert_eq!(node_b.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_id).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_id).unwrap().amount_msat, Some(invoice_amount_1_msat));

	// Test under-/overpayment
	let invoice_amount_2_msat = 2500_000;
	let invoice = node_b
		.bolt11_payment()
		.receive(invoice_amount_2_msat, &invoice_description.clone().into(), 9217)
		.unwrap();

	let underpaid_amount = invoice_amount_2_msat - 1;
	assert_eq!(
		Err(NodeError::InvalidAmount),
		node_a.bolt11_payment().send_using_amount(&invoice, underpaid_amount, None)
	);

	println!("\nB overpaid receive");
	let invoice = node_b
		.bolt11_payment()
		.receive(invoice_amount_2_msat, &invoice_description.clone().into(), 9217)
		.unwrap();
	let overpaid_amount_msat = invoice_amount_2_msat + 100;

	println!("\nA overpaid send");
	let payment_id =
		node_a.bolt11_payment().send_using_amount(&invoice, overpaid_amount_msat, None).unwrap();
	expect_event!(node_a, PaymentSuccessful);
	let received_amount = match node_b.next_event_async().await {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled().unwrap();
			amount_msat
		},
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		},
	};
	assert_eq!(received_amount, overpaid_amount_msat);
	assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_id).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_id).unwrap().amount_msat, Some(overpaid_amount_msat));
	assert!(matches!(node_a.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));
	assert_eq!(node_b.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_id).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_id).unwrap().amount_msat, Some(overpaid_amount_msat));
	assert!(matches!(node_b.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));

	// Test "zero-amount" invoice payment
	println!("\nB receive_variable_amount_payment");
	let variable_amount_invoice = node_b
		.bolt11_payment()
		.receive_variable_amount(&invoice_description.clone().into(), 9217)
		.unwrap();
	let determined_amount_msat = 2345_678;
	assert_eq!(
		Err(NodeError::InvalidInvoice),
		node_a.bolt11_payment().send(&variable_amount_invoice, None)
	);
	println!("\nA send_using_amount");
	let payment_id = node_a
		.bolt11_payment()
		.send_using_amount(&variable_amount_invoice, determined_amount_msat, None)
		.unwrap();

	expect_event!(node_a, PaymentSuccessful);
	let received_amount = match node_b.next_event_async().await {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled().unwrap();
			amount_msat
		},
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		},
	};
	assert_eq!(received_amount, determined_amount_msat);
	assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_id).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_id).unwrap().amount_msat, Some(determined_amount_msat));
	assert!(matches!(node_a.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));
	assert_eq!(node_b.payment(&payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_id).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_id).unwrap().amount_msat, Some(determined_amount_msat));
	assert!(matches!(node_b.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));

	// Test claiming manually registered payments.
	let invoice_amount_3_msat = 5_532_000;
	let manual_preimage = PaymentPreimage([42u8; 32]);
	let manual_payment_hash = PaymentHash(Sha256::hash(&manual_preimage.0).to_byte_array());
	let manual_invoice = node_b
		.bolt11_payment()
		.receive_for_hash(
			invoice_amount_3_msat,
			&invoice_description.clone().into(),
			9217,
			manual_payment_hash,
		)
		.unwrap();
	let manual_payment_id = node_a.bolt11_payment().send(&manual_invoice, None).unwrap();

	let claimable_amount_msat = expect_payment_claimable_event!(
		node_b,
		manual_payment_id,
		manual_payment_hash,
		invoice_amount_3_msat
	);
	node_b
		.bolt11_payment()
		.claim_for_hash(manual_payment_hash, claimable_amount_msat, manual_preimage)
		.unwrap();
	expect_payment_received_event!(node_b, claimable_amount_msat);
	expect_payment_successful_event!(node_a, Some(manual_payment_id), None);
	assert_eq!(node_a.payment(&manual_payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&manual_payment_id).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(
		node_a.payment(&manual_payment_id).unwrap().amount_msat,
		Some(invoice_amount_3_msat)
	);
	assert!(matches!(node_a.payment(&manual_payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));
	assert_eq!(node_b.payment(&manual_payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&manual_payment_id).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(
		node_b.payment(&manual_payment_id).unwrap().amount_msat,
		Some(invoice_amount_3_msat)
	);
	assert!(matches!(node_b.payment(&manual_payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }));

	// Test failing manually registered payments.
	let invoice_amount_4_msat = 5_532_000;
	let manual_fail_preimage = PaymentPreimage([43u8; 32]);
	let manual_fail_payment_hash =
		PaymentHash(Sha256::hash(&manual_fail_preimage.0).to_byte_array());
	let manual_fail_invoice = node_b
		.bolt11_payment()
		.receive_for_hash(
			invoice_amount_3_msat,
			&invoice_description.into(),
			9217,
			manual_fail_payment_hash,
		)
		.unwrap();
	let manual_fail_payment_id = node_a.bolt11_payment().send(&manual_fail_invoice, None).unwrap();

	expect_payment_claimable_event!(
		node_b,
		manual_fail_payment_id,
		manual_fail_payment_hash,
		invoice_amount_4_msat
	);
	node_b.bolt11_payment().fail_for_hash(manual_fail_payment_hash).unwrap();
	expect_event!(node_a, PaymentFailed);
	assert_eq!(node_a.payment(&manual_fail_payment_id).unwrap().status, PaymentStatus::Failed);
	assert_eq!(
		node_a.payment(&manual_fail_payment_id).unwrap().direction,
		PaymentDirection::Outbound
	);
	assert_eq!(
		node_a.payment(&manual_fail_payment_id).unwrap().amount_msat,
		Some(invoice_amount_4_msat)
	);
	assert!(matches!(
		node_a.payment(&manual_fail_payment_id).unwrap().kind,
		PaymentKind::Bolt11 { .. }
	));
	assert_eq!(node_b.payment(&manual_fail_payment_id).unwrap().status, PaymentStatus::Failed);
	assert_eq!(
		node_b.payment(&manual_fail_payment_id).unwrap().direction,
		PaymentDirection::Inbound
	);
	assert_eq!(
		node_b.payment(&manual_fail_payment_id).unwrap().amount_msat,
		Some(invoice_amount_4_msat)
	);
	assert!(matches!(
		node_b.payment(&manual_fail_payment_id).unwrap().kind,
		PaymentKind::Bolt11 { .. }
	));

	// Test spontaneous/keysend payments
	println!("\nA send_spontaneous_payment");
	let keysend_amount_msat = 2500_000;
	let custom_tlvs = vec![CustomTlvRecord { type_num: 13377331, value: vec![1, 2, 3] }];
	let keysend_payment_id = node_a
		.spontaneous_payment()
		.send_with_custom_tlvs(keysend_amount_msat, node_b.node_id(), None, custom_tlvs.clone())
		.unwrap();
	expect_event!(node_a, PaymentSuccessful);
	let next_event = node_b.next_event_async().await;
	let (received_keysend_amount, received_custom_records) = match next_event {
		ref e @ Event::PaymentReceived { amount_msat, ref custom_records, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled().unwrap();
			(amount_msat, custom_records)
		},
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		},
	};
	assert_eq!(received_keysend_amount, keysend_amount_msat);
	assert_eq!(node_a.payment(&keysend_payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&keysend_payment_id).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&keysend_payment_id).unwrap().amount_msat, Some(keysend_amount_msat));
	assert!(matches!(
		node_a.payment(&keysend_payment_id).unwrap().kind,
		PaymentKind::Spontaneous { .. }
	));
	assert_eq!(received_custom_records, &custom_tlvs);
	assert_eq!(node_b.payment(&keysend_payment_id).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&keysend_payment_id).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&keysend_payment_id).unwrap().amount_msat, Some(keysend_amount_msat));
	assert!(matches!(
		node_b.payment(&keysend_payment_id).unwrap().kind,
		PaymentKind::Spontaneous { .. }
	));
	assert_eq!(
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt11 { .. })).len(),
		5
	);
	assert_eq!(
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt11 { .. })).len(),
		6
	);
	assert_eq!(
		node_a
			.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Spontaneous { .. }))
			.len(),
		1
	);
	assert_eq!(
		node_b
			.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Spontaneous { .. }))
			.len(),
		1
	);

	// Mine a block to give time for the HTLC to resolve
	generate_blocks_and_wait(&bitcoind, electrsd, 1).await;

	println!("\nB splices out to pay A");
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let splice_out_sat = funding_amount_sat / 2;
	node_b.splice_out(&user_channel_id_b, node_a.node_id(), &addr_a, splice_out_sat).unwrap();

	expect_splice_pending_event!(node_a, node_b.node_id());
	expect_splice_pending_event!(node_b, node_a.node_id());

	generate_blocks_and_wait(&bitcoind, electrsd, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	assert_eq!(
		node_a
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		2
	);

	println!("\nA splices in the splice-out payment from B");
	let splice_in_sat = splice_out_sat;
	node_a.splice_in(&user_channel_id_a, node_b.node_id(), splice_in_sat).unwrap();

	expect_splice_pending_event!(node_a, node_b.node_id());
	expect_splice_pending_event!(node_b, node_a.node_id());

	generate_blocks_and_wait(&bitcoind, electrsd, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	assert_eq!(
		node_a
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		2
	);

	println!("\nB close_channel (force: {})", force_close);
	if force_close {
		tokio::time::sleep(Duration::from_secs(1)).await;
		node_a.force_close_channel(&user_channel_id_a, node_b.node_id(), None).unwrap();
	} else {
		node_a.close_channel(&user_channel_id_a, node_b.node_id()).unwrap();
	}

	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	wait_for_outpoint_spend(electrsd, funding_txo_b).await;

	generate_blocks_and_wait(&bitcoind, electrsd, 1).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	if force_close {
		// Check node_b properly sees all balances and sweeps them.
		assert_eq!(node_b.list_balances().lightning_balances.len(), 1);
		match node_b.list_balances().lightning_balances[0] {
			LightningBalance::ClaimableAwaitingConfirmations {
				counterparty_node_id,
				confirmation_height,
				..
			} => {
				assert_eq!(counterparty_node_id, node_a.node_id());
				let cur_height = node_b.status().current_best_block.height;
				let blocks_to_go = confirmation_height - cur_height;
				generate_blocks_and_wait(&bitcoind, electrsd, blocks_to_go as usize).await;
				node_b.sync_wallets().unwrap();
				node_a.sync_wallets().unwrap();
			},
			_ => panic!("Unexpected balance state!"),
		}

		assert!(node_b.list_balances().lightning_balances.is_empty());
		assert_eq!(node_b.list_balances().pending_balances_from_channel_closures.len(), 1);
		match node_b.list_balances().pending_balances_from_channel_closures[0] {
			PendingSweepBalance::BroadcastAwaitingConfirmation { .. } => {},
			_ => panic!("Unexpected balance state!"),
		}
		generate_blocks_and_wait(&bitcoind, electrsd, 1).await;
		node_b.sync_wallets().unwrap();
		node_a.sync_wallets().unwrap();

		assert!(node_b.list_balances().lightning_balances.is_empty());
		assert_eq!(node_b.list_balances().pending_balances_from_channel_closures.len(), 1);
		match node_b.list_balances().pending_balances_from_channel_closures[0] {
			PendingSweepBalance::AwaitingThresholdConfirmations { .. } => {},
			_ => panic!("Unexpected balance state!"),
		}
		generate_blocks_and_wait(&bitcoind, electrsd, 5).await;
		node_b.sync_wallets().unwrap();
		node_a.sync_wallets().unwrap();

		assert!(node_b.list_balances().lightning_balances.is_empty());
		assert_eq!(node_b.list_balances().pending_balances_from_channel_closures.len(), 1);

		// Check node_a properly sees all balances and sweeps them.
		assert_eq!(node_a.list_balances().lightning_balances.len(), 1);
		match node_a.list_balances().lightning_balances[0] {
			LightningBalance::ClaimableAwaitingConfirmations {
				counterparty_node_id,
				confirmation_height,
				..
			} => {
				assert_eq!(counterparty_node_id, node_b.node_id());
				let cur_height = node_a.status().current_best_block.height;
				let blocks_to_go = confirmation_height - cur_height;
				generate_blocks_and_wait(&bitcoind, electrsd, blocks_to_go as usize).await;
				node_a.sync_wallets().unwrap();
				node_b.sync_wallets().unwrap();
			},
			_ => panic!("Unexpected balance state!"),
		}

		assert!(node_a.list_balances().lightning_balances.is_empty());
		assert_eq!(node_a.list_balances().pending_balances_from_channel_closures.len(), 1);
		match node_a.list_balances().pending_balances_from_channel_closures[0] {
			PendingSweepBalance::BroadcastAwaitingConfirmation { .. } => {},
			_ => panic!("Unexpected balance state!"),
		}
		generate_blocks_and_wait(&bitcoind, electrsd, 1).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();

		assert!(node_a.list_balances().lightning_balances.is_empty());
		assert_eq!(node_a.list_balances().pending_balances_from_channel_closures.len(), 1);
		match node_a.list_balances().pending_balances_from_channel_closures[0] {
			PendingSweepBalance::AwaitingThresholdConfirmations { .. } => {},
			_ => panic!("Unexpected balance state!"),
		}
		generate_blocks_and_wait(&bitcoind, electrsd, 5).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
	}

	let sum_of_all_payments_sat = (push_msat
		+ invoice_amount_1_msat
		+ overpaid_amount_msat
		+ invoice_amount_3_msat
		+ determined_amount_msat
		+ keysend_amount_msat)
		/ 1000 - splice_out_sat;
	let node_a_upper_bound_sat =
		(premine_amount_sat - funding_amount_sat) + (funding_amount_sat - sum_of_all_payments_sat);
	let node_a_lower_bound_sat = node_a_upper_bound_sat - onchain_fee_buffer_sat;
	assert!(node_a.list_balances().spendable_onchain_balance_sats > node_a_lower_bound_sat);
	assert!(node_a.list_balances().spendable_onchain_balance_sats < node_a_upper_bound_sat);

	let node_b_upper_bound_sat = premine_amount_sat + sum_of_all_payments_sat;
	let node_b_lower_bound_sat = node_b_upper_bound_sat - onchain_fee_buffer_sat;
	assert!(node_b.list_balances().spendable_onchain_balance_sats > node_b_lower_bound_sat);
	assert!(node_b.list_balances().spendable_onchain_balance_sats <= node_b_upper_bound_sat);

	assert_eq!(node_a.list_balances().total_anchor_channels_reserve_sats, 0);
	assert_eq!(node_b.list_balances().total_anchor_channels_reserve_sats, 0);

	// Now we should have seen the channel closing transaction on-chain.
	assert_eq!(
		node_a
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		3
	);
	assert_eq!(
		node_b
			.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
				&& matches!(p.kind, PaymentKind::Onchain { .. }))
			.len(),
		2
	);

	// Check we handled all events
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_b.next_event(), None);

	node_a.stop().unwrap();
	println!("\nA stopped");
	node_b.stop().unwrap();
	println!("\nB stopped");
}

// A `KVStore` impl for testing purposes that wraps all our `KVStore`s and asserts their synchronicity.
#[derive(Clone)]
pub(crate) struct TestSyncStore {
	inner: Arc<TestSyncStoreInner>,
}

impl TestSyncStore {
	pub(crate) fn new(dest_dir: PathBuf) -> Self {
		let inner = Arc::new(TestSyncStoreInner::new(dest_dir));
		Self { inner }
	}
}

impl KVStore for TestSyncStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.read_internal(&primary_namespace, &secondary_namespace, &key)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.write_internal(&primary_namespace, &secondary_namespace, &key, buf)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.remove_internal(&primary_namespace, &secondary_namespace, &key, lazy)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = tokio::task::spawn_blocking(move || {
			inner.list_internal(&primary_namespace, &secondary_namespace)
		});
		async move {
			fut.await.unwrap_or_else(|e| {
				let msg = format!("Failed to IO operation due join error: {}", e);
				Err(io::Error::new(io::ErrorKind::Other, msg))
			})
		}
	}
}

impl KVStoreSync for TestSyncStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> lightning::io::Result<Vec<u8>> {
		self.inner.read_internal(primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> lightning::io::Result<()> {
		self.inner.write_internal(primary_namespace, secondary_namespace, key, buf)
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> lightning::io::Result<()> {
		self.inner.remove_internal(primary_namespace, secondary_namespace, key, lazy)
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> lightning::io::Result<Vec<String>> {
		self.inner.list_internal(primary_namespace, secondary_namespace)
	}
}

struct TestSyncStoreInner {
	serializer: RwLock<()>,
	test_store: TestStore,
	fs_store: FilesystemStore,
	sqlite_store: SqliteStore,
}

impl TestSyncStoreInner {
	fn new(dest_dir: PathBuf) -> Self {
		let serializer = RwLock::new(());
		let mut fs_dir = dest_dir.clone();
		fs_dir.push("fs_store");
		let fs_store = FilesystemStore::new(fs_dir);
		let mut sql_dir = dest_dir.clone();
		sql_dir.push("sqlite_store");
		let sqlite_store = SqliteStore::new(
			sql_dir,
			Some("test_sync_db".to_string()),
			Some("test_sync_table".to_string()),
		)
		.unwrap();
		let test_store = TestStore::new(false);
		Self { serializer, fs_store, sqlite_store, test_store }
	}

	fn do_list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> lightning::io::Result<Vec<String>> {
		let fs_res = KVStoreSync::list(&self.fs_store, primary_namespace, secondary_namespace);
		let sqlite_res =
			KVStoreSync::list(&self.sqlite_store, primary_namespace, secondary_namespace);
		let test_res = KVStoreSync::list(&self.test_store, primary_namespace, secondary_namespace);

		match fs_res {
			Ok(mut list) => {
				list.sort();

				let mut sqlite_list = sqlite_res.unwrap();
				sqlite_list.sort();
				assert_eq!(list, sqlite_list);

				let mut test_list = test_res.unwrap();
				test_list.sort();
				assert_eq!(list, test_list);

				Ok(list)
			},
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert!(test_res.is_err());
				Err(e)
			},
		}
	}

	fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> lightning::io::Result<Vec<u8>> {
		let _guard = self.serializer.read().unwrap();

		let fs_res = KVStoreSync::read(&self.fs_store, primary_namespace, secondary_namespace, key);
		let sqlite_res =
			KVStoreSync::read(&self.sqlite_store, primary_namespace, secondary_namespace, key);
		let test_res =
			KVStoreSync::read(&self.test_store, primary_namespace, secondary_namespace, key);

		match fs_res {
			Ok(read) => {
				assert_eq!(read, sqlite_res.unwrap());
				assert_eq!(read, test_res.unwrap());
				Ok(read)
			},
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert_eq!(e.kind(), unsafe { sqlite_res.unwrap_err_unchecked().kind() });
				assert!(test_res.is_err());
				assert_eq!(e.kind(), unsafe { test_res.unwrap_err_unchecked().kind() });
				Err(e)
			},
		}
	}

	fn write_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> lightning::io::Result<()> {
		let _guard = self.serializer.write().unwrap();
		let fs_res = KVStoreSync::write(
			&self.fs_store,
			primary_namespace,
			secondary_namespace,
			key,
			buf.clone(),
		);
		let sqlite_res = KVStoreSync::write(
			&self.sqlite_store,
			primary_namespace,
			secondary_namespace,
			key,
			buf.clone(),
		);
		let test_res = KVStoreSync::write(
			&self.test_store,
			primary_namespace,
			secondary_namespace,
			key,
			buf.clone(),
		);

		assert!(self
			.do_list(primary_namespace, secondary_namespace)
			.unwrap()
			.contains(&key.to_string()));

		match fs_res {
			Ok(()) => {
				assert!(sqlite_res.is_ok());
				assert!(test_res.is_ok());
				Ok(())
			},
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert!(test_res.is_err());
				Err(e)
			},
		}
	}

	fn remove_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> lightning::io::Result<()> {
		let _guard = self.serializer.write().unwrap();
		let fs_res =
			KVStoreSync::remove(&self.fs_store, primary_namespace, secondary_namespace, key, lazy);
		let sqlite_res = KVStoreSync::remove(
			&self.sqlite_store,
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		);
		let test_res = KVStoreSync::remove(
			&self.test_store,
			primary_namespace,
			secondary_namespace,
			key,
			lazy,
		);

		assert!(!self
			.do_list(primary_namespace, secondary_namespace)
			.unwrap()
			.contains(&key.to_string()));

		match fs_res {
			Ok(()) => {
				assert!(sqlite_res.is_ok());
				assert!(test_res.is_ok());
				Ok(())
			},
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert!(test_res.is_err());
				Err(e)
			},
		}
	}

	fn list_internal(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> lightning::io::Result<Vec<String>> {
		let _guard = self.serializer.read().unwrap();
		self.do_list(primary_namespace, secondary_namespace)
	}
}
