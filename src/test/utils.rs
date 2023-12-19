use crate::builder::NodeBuilder;
use crate::io::test_utils::TestSyncStore;
use crate::{Config, Event, Network, Node};
use lightning::ln::msgs::SocketAddress;
use lightning::util::logger::{Level, Logger, Record};
use lightning::util::persist::KVStore;

use bitcoin::{Address, Amount, OutPoint, Txid};

use bitcoind::bitcoincore_rpc::RpcApi;
use electrsd::bitcoind::bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use electrsd::electrum_client::ElectrumApi;
use electrsd::{bitcoind, bitcoind::BitcoinD, ElectrsD};

use regex;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
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

// Copied over from upstream LDK
#[allow(dead_code)]
pub struct TestLogger {
	level: Level,
	pub(crate) id: String,
	pub lines: Mutex<HashMap<(String, String), usize>>,
}

impl TestLogger {
	#[allow(dead_code)]
	pub fn new() -> TestLogger {
		Self::with_id("".to_owned())
	}

	#[allow(dead_code)]
	pub fn with_id(id: String) -> TestLogger {
		TestLogger { level: Level::Trace, id, lines: Mutex::new(HashMap::new()) }
	}

	#[allow(dead_code)]
	pub fn enable(&mut self, level: Level) {
		self.level = level;
	}

	#[allow(dead_code)]
	pub fn assert_log(&self, module: String, line: String, count: usize) {
		let log_entries = self.lines.lock().unwrap();
		assert_eq!(log_entries.get(&(module, line)), Some(&count));
	}

	/// Search for the number of occurrence of the logged lines which
	/// 1. belongs to the specified module and
	/// 2. contains `line` in it.
	/// And asserts if the number of occurrences is the same with the given `count`
	#[allow(dead_code)]
	pub fn assert_log_contains(&self, module: &str, line: &str, count: usize) {
		let log_entries = self.lines.lock().unwrap();
		let l: usize = log_entries
			.iter()
			.filter(|&(&(ref m, ref l), _c)| m == module && l.contains(line))
			.map(|(_, c)| c)
			.sum();
		assert_eq!(l, count)
	}

	/// Search for the number of occurrences of logged lines which
	/// 1. belong to the specified module and
	/// 2. match the given regex pattern.
	/// Assert that the number of occurrences equals the given `count`
	#[allow(dead_code)]
	pub fn assert_log_regex(&self, module: &str, pattern: regex::Regex, count: usize) {
		let log_entries = self.lines.lock().unwrap();
		let l: usize = log_entries
			.iter()
			.filter(|&(&(ref m, ref l), _c)| m == module && pattern.is_match(&l))
			.map(|(_, c)| c)
			.sum();
		assert_eq!(l, count)
	}
}

impl Logger for TestLogger {
	fn log(&self, record: Record) {
		*self
			.lines
			.lock()
			.unwrap()
			.entry((record.module_path.to_string(), format!("{}", record.args)))
			.or_insert(0) += 1;
		if record.level >= self.level {
			#[cfg(feature = "std")]
			println!(
				"{:<5} {} [{} : {}, {}] {}",
				record.level.to_string(),
				self.id,
				record.module_path,
				record.file,
				record.line,
				record.args
			);
		}
	}
}

pub fn random_storage_path() -> PathBuf {
	let mut temp_path = std::env::temp_dir();
	let mut rng = thread_rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

pub fn random_port() -> u16 {
	let mut rng = thread_rng();
	rng.gen_range(5000..65535)
}

pub fn random_listening_addresses() -> Vec<SocketAddress> {
	let num_addresses = 2;
	let mut listening_addresses = Vec::with_capacity(num_addresses);

	for _ in 0..num_addresses {
		let rand_port = random_port();
		let address: SocketAddress = format!("127.0.0.1:{}", rand_port).parse().unwrap();
		listening_addresses.push(address);
	}

	listening_addresses
}

pub fn random_config() -> Config {
	let mut config = Config::default();

	config.network = Network::Regtest;
	println!("Setting network: {}", config.network);

	let rand_dir = random_storage_path();
	println!("Setting random LDK storage dir: {}", rand_dir.display());
	config.storage_dir_path = rand_dir.to_str().unwrap().to_owned();

	let rand_listening_addresses = random_listening_addresses();
	println!("Setting random LDK listening addresses: {:?}", rand_listening_addresses);
	config.listening_addresses = Some(rand_listening_addresses);

	config.log_level = Level::Gossip;

	config
}

pub fn setup_bitcoind_and_electrsd() -> (BitcoinD, ElectrsD) {
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

pub(crate) fn setup_two_nodes(
	electrsd: &ElectrsD, allow_0conf: bool,
) -> (Node<TestSyncStore>, Node<TestSyncStore>) {
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

pub(crate) fn setup_node(electrsd: &ElectrsD, config: Config) -> Node<TestSyncStore> {
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let mut builder = NodeBuilder::from_config(config.clone());
	builder.set_esplora_server(esplora_url.clone());
	let test_sync_store = Arc::new(TestSyncStore::new(config.storage_dir_path.into()));
	let node = builder.build_with_store(test_sync_store).unwrap();
	node.start().unwrap();
	node
}

pub fn generate_blocks_and_wait(bitcoind: &BitcoinD, electrsd: &ElectrsD, num: usize) {
	print!("Generating {} blocks...", num);
	let cur_height = bitcoind.client.get_block_count().expect("failed to get current block height");
	let address = bitcoind
		.client
		.get_new_address(Some("test"), Some(AddressType::Legacy))
		.expect("failed to get new address")
		.require_network(bitcoin::Network::Regtest)
		.expect("failed to get new address");
	// TODO: expect this Result once the WouldBlock issue is resolved upstream.
	let _block_hashes_res = bitcoind.client.generate_to_address(num as u64, &address);
	wait_for_block(electrsd, cur_height as usize + num);
	print!(" Done!");
	println!("\n");
}

pub fn wait_for_block(electrsd: &ElectrsD, min_height: usize) {
	let mut header = match electrsd.client.block_headers_subscribe() {
		Ok(header) => header,
		Err(_) => {
			// While subscribing should succeed the first time around, we ran into some cases where
			// it didn't. Since we can't proceed without subscribing, we try again after a delay
			// and panic if it still fails.
			std::thread::sleep(Duration::from_secs(1));
			electrsd.client.block_headers_subscribe().expect("failed to subscribe to block headers")
		}
	};
	loop {
		if header.height >= min_height {
			break;
		}
		header = exponential_backoff_poll(|| {
			electrsd.trigger().expect("failed to trigger electrsd");
			electrsd.client.ping().expect("failed to ping electrsd");
			electrsd.client.block_headers_pop().expect("failed to pop block header")
		});
	}
}

pub fn wait_for_tx(electrsd: &ElectrsD, txid: Txid) {
	let mut tx_res = electrsd.client.transaction_get(&txid);
	loop {
		if tx_res.is_ok() {
			break;
		}
		tx_res = exponential_backoff_poll(|| {
			electrsd.trigger().unwrap();
			electrsd.client.ping().unwrap();
			Some(electrsd.client.transaction_get(&txid))
		});
	}
}

pub fn wait_for_outpoint_spend(electrsd: &ElectrsD, outpoint: OutPoint) {
	let tx = electrsd.client.transaction_get(&outpoint.txid).unwrap();
	let txout_script = tx.output.get(outpoint.vout as usize).unwrap().clone().script_pubkey;
	let mut is_spent = !electrsd.client.script_get_history(&txout_script).unwrap().is_empty();
	loop {
		if is_spent {
			break;
		}

		is_spent = exponential_backoff_poll(|| {
			electrsd.trigger().unwrap();
			electrsd.client.ping().unwrap();
			Some(!electrsd.client.script_get_history(&txout_script).unwrap().is_empty())
		});
	}
}

pub fn exponential_backoff_poll<T, F>(mut poll: F) -> T
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

pub fn premine_and_distribute_funds(
	bitcoind: &BitcoinD, electrsd: &ElectrsD, addrs: Vec<Address>, amount: Amount,
) {
	generate_blocks_and_wait(bitcoind, electrsd, 101);

	for addr in addrs {
		let txid = bitcoind
			.client
			.send_to_address(&addr, amount, None, None, None, None, None, None)
			.unwrap();
		wait_for_tx(electrsd, txid);
	}

	generate_blocks_and_wait(bitcoind, electrsd, 1);
}

pub fn open_channel<K: KVStore + Sync + Send>(
	node_a: &Node<K>, node_b: &Node<K>, funding_amount_sat: u64, announce: bool,
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
	wait_for_tx(&electrsd, funding_txo_a.txid);
}
