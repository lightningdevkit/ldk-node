#![cfg(any(test, cln_test, vss_test))]
#![allow(dead_code)]

use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::{
	Builder, Config, Event, LogLevel, Node, NodeError, PaymentDirection, PaymentStatus,
};

use lightning::ln::msgs::SocketAddress;
use lightning::util::persist::KVStore;
use lightning::util::test_utils::TestStore;
use lightning_persister::fs_store::FilesystemStore;

use bitcoin::{Address, Amount, Network, OutPoint, Txid};

use bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use bitcoincore_rpc::Client as BitcoindClient;
use bitcoincore_rpc::RpcApi;

use electrsd::{bitcoind, bitcoind::BitcoinD, ElectrsD};
use electrum_client::ElectrumApi;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use std::env;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

macro_rules! expect_event {
	($node: expr, $event_type: ident) => {{
		match $node.wait_next_event() {
			ref e @ Event::$event_type { .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				$node.event_handled();
			}
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			}
		}
	}};
}

pub(crate) use expect_event;

macro_rules! expect_channel_pending_event {
	($node: expr, $counterparty_node_id: expr) => {{
		match $node.wait_next_event() {
			ref e @ Event::ChannelPending { funding_txo, counterparty_node_id, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(counterparty_node_id, $counterparty_node_id);
				$node.event_handled();
				funding_txo
			}
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			}
		}
	}};
}

pub(crate) use expect_channel_pending_event;

macro_rules! expect_channel_ready_event {
	($node: expr, $counterparty_node_id: expr) => {{
		match $node.wait_next_event() {
			ref e @ Event::ChannelReady { user_channel_id, counterparty_node_id, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(counterparty_node_id, Some($counterparty_node_id));
				$node.event_handled();
				user_channel_id
			}
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			}
		}
	}};
}

pub(crate) use expect_channel_ready_event;

pub(crate) fn setup_bitcoind_and_electrsd() -> (BitcoinD, ElectrsD) {
	let bitcoind_exe =
		env::var("BITCOIND_EXE").ok().or_else(|| bitcoind::downloaded_exe_path().ok()).expect(
			"you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
		);
	let mut bitcoind_conf = bitcoind::Conf::default();
	bitcoind_conf.network = "regtest";
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
	let mut rng = thread_rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

pub(crate) fn random_port() -> u16 {
	let mut rng = thread_rng();
	rng.gen_range(5000..65535)
}

pub(crate) fn random_listening_addresses() -> Vec<SocketAddress> {
	let num_addresses = 2;
	let mut listening_addresses = Vec::with_capacity(num_addresses);

	for _ in 0..num_addresses {
		let rand_port = random_port();
		let address: SocketAddress = format!("127.0.0.1:{}", rand_port).parse().unwrap();
		listening_addresses.push(address);
	}

	listening_addresses
}

pub(crate) fn random_config() -> Config {
	let mut config = Config::default();

	config.network = Network::Regtest;
	println!("Setting network: {}", config.network);

	let rand_dir = random_storage_path();
	println!("Setting random LDK storage dir: {}", rand_dir.display());
	config.storage_dir_path = rand_dir.to_str().unwrap().to_owned();

	let rand_listening_addresses = random_listening_addresses();
	println!("Setting random LDK listening addresses: {:?}", rand_listening_addresses);
	config.listening_addresses = Some(rand_listening_addresses);

	config.log_level = LogLevel::Gossip;

	config
}

#[cfg(feature = "uniffi")]
type TestNode<K> = Arc<Node<K>>;
#[cfg(not(feature = "uniffi"))]
type TestNode<K> = Node<K>;

macro_rules! setup_builder {
	($builder: ident, $config: expr) => {
		#[cfg(feature = "uniffi")]
		let $builder = Builder::from_config($config.clone());
		#[cfg(not(feature = "uniffi"))]
		let mut $builder = Builder::from_config($config.clone());
	};
}

pub(crate) use setup_builder;

pub(crate) fn setup_two_nodes(
	electrsd: &ElectrsD, allow_0conf: bool,
) -> (TestNode<TestSyncStore>, TestNode<TestSyncStore>) {
	println!("== Node A ==");
	let config_a = random_config();
	let node_a = setup_node(electrsd, config_a);

	println!("\n== Node B ==");
	let mut config_b = random_config();
	if allow_0conf {
		config_b.trusted_peers_0conf.push(node_a.node_id());
	}
	let node_b = setup_node(electrsd, config_b);
	(node_a, node_b)
}

pub(crate) fn setup_node(electrsd: &ElectrsD, config: Config) -> TestNode<TestSyncStore> {
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	setup_builder!(builder, config);
	builder.set_esplora_server(esplora_url.clone());
	let test_sync_store = Arc::new(TestSyncStore::new(config.storage_dir_path.into()));
	let node = builder.build_with_store(test_sync_store).unwrap();
	node.start().unwrap();
	node
}

pub(crate) fn generate_blocks_and_wait<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, num: usize,
) {
	let _ = bitcoind.create_wallet("ldk_node_test", None, None, None, None);
	let _ = bitcoind.load_wallet("ldk_node_test");
	print!("Generating {} blocks...", num);
	let cur_height = bitcoind.get_block_count().expect("failed to get current block height");
	let address = bitcoind
		.get_new_address(Some("test"), Some(AddressType::Legacy))
		.expect("failed to get new address")
		.require_network(bitcoin::Network::Regtest)
		.expect("failed to get new address");
	// TODO: expect this Result once the WouldBlock issue is resolved upstream.
	let _block_hashes_res = bitcoind.generate_to_address(num as u64, &address);
	wait_for_block(electrs, cur_height as usize + num);
	print!(" Done!");
	println!("\n");
}

pub(crate) fn wait_for_block<E: ElectrumApi>(electrs: &E, min_height: usize) {
	let mut header = match electrs.block_headers_subscribe() {
		Ok(header) => header,
		Err(_) => {
			// While subscribing should succeed the first time around, we ran into some cases where
			// it didn't. Since we can't proceed without subscribing, we try again after a delay
			// and panic if it still fails.
			std::thread::sleep(Duration::from_secs(3));
			electrs.block_headers_subscribe().expect("failed to subscribe to block headers")
		}
	};
	loop {
		if header.height >= min_height {
			break;
		}
		header = exponential_backoff_poll(|| {
			electrs.ping().expect("failed to ping electrs");
			electrs.block_headers_pop().expect("failed to pop block header")
		});
	}
}

pub(crate) fn wait_for_tx<E: ElectrumApi>(electrs: &E, txid: Txid) {
	let mut tx_res = electrs.transaction_get(&txid);
	loop {
		if tx_res.is_ok() {
			break;
		}
		tx_res = exponential_backoff_poll(|| {
			electrs.ping().unwrap();
			Some(electrs.transaction_get(&txid))
		});
	}
}

pub(crate) fn wait_for_outpoint_spend<E: ElectrumApi>(electrs: &E, outpoint: OutPoint) {
	let tx = electrs.transaction_get(&outpoint.txid).unwrap();
	let txout_script = tx.output.get(outpoint.vout as usize).unwrap().clone().script_pubkey;
	let mut is_spent = !electrs.script_get_history(&txout_script).unwrap().is_empty();
	loop {
		if is_spent {
			break;
		}

		is_spent = exponential_backoff_poll(|| {
			electrs.ping().unwrap();
			Some(!electrs.script_get_history(&txout_script).unwrap().is_empty())
		});
	}
}

pub(crate) fn exponential_backoff_poll<T, F>(mut poll: F) -> T
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
			}

			None => {}
		}
		assert!(tries < 20, "Reached max tries.");
		tries += 1;
		std::thread::sleep(delay);
	}
}

pub(crate) fn premine_and_distribute_funds<E: ElectrumApi>(
	bitcoind: &BitcoindClient, electrs: &E, addrs: Vec<Address>, amount: Amount,
) {
	let _ = bitcoind.create_wallet("ldk_node_test", None, None, None, None);
	let _ = bitcoind.load_wallet("ldk_node_test");
	generate_blocks_and_wait(bitcoind, electrs, 101);

	for addr in addrs {
		let txid =
			bitcoind.send_to_address(&addr, amount, None, None, None, None, None, None).unwrap();
		wait_for_tx(electrs, txid);
	}

	generate_blocks_and_wait(bitcoind, electrs, 1);
}

pub fn open_channel<K: KVStore + Sync + Send>(
	node_a: &TestNode<K>, node_b: &TestNode<K>, funding_amount_sat: u64, announce: bool,
	electrsd: &ElectrsD,
) {
	node_a
		.connect_open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			funding_amount_sat,
			None,
			None,
			announce,
		)
		.unwrap();
	assert!(node_a.list_peers().iter().find(|c| { c.node_id == node_b.node_id() }).is_some());

	let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
	let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
	assert_eq!(funding_txo_a, funding_txo_b);
	wait_for_tx(&electrsd.client, funding_txo_a.txid);
}

pub(crate) fn do_channel_full_cycle<K: KVStore + Sync + Send, E: ElectrumApi>(
	node_a: TestNode<K>, node_b: TestNode<K>, bitcoind: &BitcoindClient, electrsd: &E,
	allow_0conf: bool,
) {
	let addr_a = node_a.new_onchain_address().unwrap();
	let addr_b = node_b.new_onchain_address().unwrap();

	let premine_amount_sat = 100_000;

	premine_and_distribute_funds(
		&bitcoind,
		electrsd,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.spendable_onchain_balance_sats().unwrap(), premine_amount_sat);
	assert_eq!(node_b.spendable_onchain_balance_sats().unwrap(), premine_amount_sat);

	// Check we haven't got any events yet
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_b.next_event(), None);

	println!("\nA -- connect_open_channel -> B");
	let funding_amount_sat = 80_000;
	let push_msat = (funding_amount_sat / 2) * 1000; // balance the channel
	node_a
		.connect_open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			funding_amount_sat,
			Some(push_msat),
			None,
			true,
		)
		.unwrap();

	assert_eq!(node_a.list_peers().first().unwrap().node_id, node_b.node_id());
	let funding_txo_a = expect_channel_pending_event!(node_a, node_b.node_id());
	let funding_txo_b = expect_channel_pending_event!(node_b, node_a.node_id());
	assert_eq!(funding_txo_a, funding_txo_b);

	wait_for_tx(electrsd, funding_txo_a.txid);

	if !allow_0conf {
		generate_blocks_and_wait(&bitcoind, electrsd, 6);
	}

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let onchain_fee_buffer_sat = 1500;
	let node_a_upper_bound_sat = premine_amount_sat - funding_amount_sat;
	let node_a_lower_bound_sat = premine_amount_sat - funding_amount_sat - onchain_fee_buffer_sat;
	assert!(node_a.spendable_onchain_balance_sats().unwrap() < node_a_upper_bound_sat);
	assert!(node_a.spendable_onchain_balance_sats().unwrap() > node_a_lower_bound_sat);
	assert_eq!(node_b.spendable_onchain_balance_sats().unwrap(), premine_amount_sat);

	expect_channel_ready_event!(node_a, node_b.node_id());

	let user_channel_id = expect_channel_ready_event!(node_b, node_a.node_id());

	println!("\nB receive_payment");
	let invoice_amount_1_msat = 2500_000;
	let invoice = node_b.receive_payment(invoice_amount_1_msat, &"asdf", 9217).unwrap();

	println!("\nA send_payment");
	let payment_hash = node_a.send_payment(&invoice).unwrap();
	assert_eq!(node_a.send_payment(&invoice), Err(NodeError::DuplicatePayment));

	assert_eq!(node_a.list_payments().first().unwrap().hash, payment_hash);

	let outbound_payments_a =
		node_a.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound);
	assert_eq!(outbound_payments_a.len(), 1);

	let inbound_payments_a =
		node_a.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound);
	assert_eq!(inbound_payments_a.len(), 0);

	let outbound_payments_b =
		node_b.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound);
	assert_eq!(outbound_payments_b.len(), 0);

	let inbound_payments_b =
		node_b.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound);
	assert_eq!(inbound_payments_b.len(), 1);

	expect_event!(node_a, PaymentSuccessful);
	expect_event!(node_b, PaymentReceived);
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(invoice_amount_1_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(invoice_amount_1_msat));

	// Assert we fail duplicate outbound payments and check the status hasn't changed.
	assert_eq!(Err(NodeError::DuplicatePayment), node_a.send_payment(&invoice));
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(invoice_amount_1_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(invoice_amount_1_msat));

	// Test under-/overpayment
	let invoice_amount_2_msat = 2500_000;
	let invoice = node_b.receive_payment(invoice_amount_2_msat, &"asdf", 9217).unwrap();

	let underpaid_amount = invoice_amount_2_msat - 1;
	assert_eq!(
		Err(NodeError::InvalidAmount),
		node_a.send_payment_using_amount(&invoice, underpaid_amount)
	);

	println!("\nB overpaid receive_payment");
	let invoice = node_b.receive_payment(invoice_amount_2_msat, &"asdf", 9217).unwrap();
	let overpaid_amount_msat = invoice_amount_2_msat + 100;

	println!("\nA overpaid send_payment");
	let payment_hash = node_a.send_payment_using_amount(&invoice, overpaid_amount_msat).unwrap();
	expect_event!(node_a, PaymentSuccessful);
	let received_amount = match node_b.wait_next_event() {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			amount_msat
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};
	assert_eq!(received_amount, overpaid_amount_msat);
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(overpaid_amount_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(overpaid_amount_msat));

	// Test "zero-amount" invoice payment
	println!("\nB receive_variable_amount_payment");
	let variable_amount_invoice = node_b.receive_variable_amount_payment(&"asdf", 9217).unwrap();
	let determined_amount_msat = 2345_678;
	assert_eq!(Err(NodeError::InvalidInvoice), node_a.send_payment(&variable_amount_invoice));
	println!("\nA send_payment_using_amount");
	let payment_hash =
		node_a.send_payment_using_amount(&variable_amount_invoice, determined_amount_msat).unwrap();

	expect_event!(node_a, PaymentSuccessful);
	let received_amount = match node_b.wait_next_event() {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			amount_msat
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};
	assert_eq!(received_amount, determined_amount_msat);
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(determined_amount_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(determined_amount_msat));

	// Test spontaneous/keysend payments
	println!("\nA send_spontaneous_payment");
	let keysend_amount_msat = 2500_000;
	let keysend_payment_hash =
		node_a.send_spontaneous_payment(keysend_amount_msat, node_b.node_id()).unwrap();
	expect_event!(node_a, PaymentSuccessful);
	let received_keysend_amount = match node_b.wait_next_event() {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			amount_msat
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};
	assert_eq!(received_keysend_amount, keysend_amount_msat);
	assert_eq!(node_a.payment(&keysend_payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(
		node_a.payment(&keysend_payment_hash).unwrap().direction,
		PaymentDirection::Outbound
	);
	assert_eq!(
		node_a.payment(&keysend_payment_hash).unwrap().amount_msat,
		Some(keysend_amount_msat)
	);
	assert_eq!(node_b.payment(&keysend_payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&keysend_payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(
		node_b.payment(&keysend_payment_hash).unwrap().amount_msat,
		Some(keysend_amount_msat)
	);

	println!("\nB close_channel");
	node_b.close_channel(&user_channel_id, node_a.node_id()).unwrap();
	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	wait_for_outpoint_spend(electrsd, funding_txo_b);

	generate_blocks_and_wait(&bitcoind, electrsd, 1);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let sum_of_all_payments_sat = (push_msat
		+ invoice_amount_1_msat
		+ overpaid_amount_msat
		+ determined_amount_msat
		+ keysend_amount_msat)
		/ 1000;
	let node_a_upper_bound_sat =
		(premine_amount_sat - funding_amount_sat) + (funding_amount_sat - sum_of_all_payments_sat);
	let node_a_lower_bound_sat = node_a_upper_bound_sat - onchain_fee_buffer_sat;
	assert!(node_a.spendable_onchain_balance_sats().unwrap() > node_a_lower_bound_sat);
	assert!(node_a.spendable_onchain_balance_sats().unwrap() < node_a_upper_bound_sat);
	let expected_final_amount_node_b_sat = premine_amount_sat + sum_of_all_payments_sat;
	assert_eq!(node_b.spendable_onchain_balance_sats().unwrap(), expected_final_amount_node_b_sat);

	// Check we handled all events
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_b.next_event(), None);

	node_a.stop().unwrap();
	println!("\nA stopped");
	node_b.stop().unwrap();
	println!("\nB stopped");
}

// A `KVStore` impl for testing purposes that wraps all our `KVStore`s and asserts their synchronicity.
pub(crate) struct TestSyncStore {
	serializer: RwLock<()>,
	test_store: TestStore,
	fs_store: FilesystemStore,
	sqlite_store: SqliteStore,
}

impl TestSyncStore {
	pub(crate) fn new(dest_dir: PathBuf) -> Self {
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
	) -> std::io::Result<Vec<String>> {
		let fs_res = self.fs_store.list(primary_namespace, secondary_namespace);
		let sqlite_res = self.sqlite_store.list(primary_namespace, secondary_namespace);
		let test_res = self.test_store.list(primary_namespace, secondary_namespace);

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
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert!(test_res.is_err());
				Err(e)
			}
		}
	}
}

impl KVStore for TestSyncStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> std::io::Result<Vec<u8>> {
		let _guard = self.serializer.read().unwrap();

		let fs_res = self.fs_store.read(primary_namespace, secondary_namespace, key);
		let sqlite_res = self.sqlite_store.read(primary_namespace, secondary_namespace, key);
		let test_res = self.test_store.read(primary_namespace, secondary_namespace, key);

		match fs_res {
			Ok(read) => {
				assert_eq!(read, sqlite_res.unwrap());
				assert_eq!(read, test_res.unwrap());
				Ok(read)
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert_eq!(e.kind(), unsafe { sqlite_res.unwrap_err_unchecked().kind() });
				assert!(test_res.is_err());
				assert_eq!(e.kind(), unsafe { test_res.unwrap_err_unchecked().kind() });
				Err(e)
			}
		}
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: &[u8],
	) -> std::io::Result<()> {
		let _guard = self.serializer.write().unwrap();
		let fs_res = self.fs_store.write(primary_namespace, secondary_namespace, key, buf);
		let sqlite_res = self.sqlite_store.write(primary_namespace, secondary_namespace, key, buf);
		let test_res = self.test_store.write(primary_namespace, secondary_namespace, key, buf);

		assert!(self
			.do_list(primary_namespace, secondary_namespace)
			.unwrap()
			.contains(&key.to_string()));

		match fs_res {
			Ok(()) => {
				assert!(sqlite_res.is_ok());
				assert!(test_res.is_ok());
				Ok(())
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert!(test_res.is_err());
				Err(e)
			}
		}
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> std::io::Result<()> {
		let _guard = self.serializer.write().unwrap();
		let fs_res = self.fs_store.remove(primary_namespace, secondary_namespace, key, lazy);
		let sqlite_res =
			self.sqlite_store.remove(primary_namespace, secondary_namespace, key, lazy);
		let test_res = self.test_store.remove(primary_namespace, secondary_namespace, key, lazy);

		assert!(!self
			.do_list(primary_namespace, secondary_namespace)
			.unwrap()
			.contains(&key.to_string()));

		match fs_res {
			Ok(()) => {
				assert!(sqlite_res.is_ok());
				assert!(test_res.is_ok());
				Ok(())
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				assert!(test_res.is_err());
				Err(e)
			}
		}
	}

	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> std::io::Result<Vec<String>> {
		let _guard = self.serializer.read().unwrap();
		self.do_list(primary_namespace, secondary_namespace)
	}
}
