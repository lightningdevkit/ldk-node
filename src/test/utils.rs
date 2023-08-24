use crate::builder::NodeBuilder;
use crate::io::{FilesystemStore, KVStore, SqliteStore};
use crate::{Config, Node};
use lightning::util::logger::{Level, Logger, Record};
use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;

use bitcoin::{Address, Amount, Network, OutPoint, Txid};

use bitcoind::bitcoincore_rpc::RpcApi;
use electrsd::bitcoind::bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use electrsd::{bitcoind, bitcoind::BitcoinD, ElectrsD};
use electrum_client::ElectrumApi;

use regex;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::collections::hash_map;
use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

macro_rules! expect_event {
	($node: expr, $event_type: ident) => {{
		match $node.wait_next_event() {
			ref e @ Event::$event_type { .. } => {
				println!("{} got event {:?}", std::stringify!($node), e);
				$node.event_handled();
			}
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			}
		}
	}};
}

pub(crate) use expect_event;

pub(crate) struct TestStore {
	persisted_bytes: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
	did_persist: Arc<AtomicBool>,
}

impl TestStore {
	pub fn new() -> Self {
		let persisted_bytes = Mutex::new(HashMap::new());
		let did_persist = Arc::new(AtomicBool::new(false));
		Self { persisted_bytes, did_persist }
	}

	pub fn get_persisted_bytes(&self, namespace: &str, key: &str) -> Option<Vec<u8>> {
		let persisted_lock = self.persisted_bytes.lock().unwrap();
		persisted_lock.get(namespace).and_then(|e| e.get(key).cloned())
	}

	pub fn get_and_clear_did_persist(&self) -> bool {
		self.did_persist.swap(false, Ordering::Relaxed)
	}
}

impl KVStore for TestStore {
	type Reader = io::Cursor<Vec<u8>>;

	fn read(&self, namespace: &str, key: &str) -> io::Result<Self::Reader> {
		let persisted_lock = self.persisted_bytes.lock().unwrap();
		if let Some(outer_ref) = persisted_lock.get(namespace) {
			if let Some(inner_ref) = outer_ref.get(key) {
				let bytes = inner_ref.clone();
				Ok(io::Cursor::new(bytes))
			} else {
				Err(io::Error::new(io::ErrorKind::NotFound, "Key not found"))
			}
		} else {
			Err(io::Error::new(io::ErrorKind::NotFound, "Namespace not found"))
		}
	}

	fn write(&self, namespace: &str, key: &str, buf: &[u8]) -> io::Result<()> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();
		let outer_e = persisted_lock.entry(namespace.to_string()).or_insert(HashMap::new());
		let mut bytes = Vec::new();
		bytes.write_all(buf)?;
		outer_e.insert(key.to_string(), bytes);
		self.did_persist.store(true, Ordering::SeqCst);
		Ok(())
	}

	fn remove(&self, namespace: &str, key: &str) -> io::Result<()> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();
		if let Some(outer_ref) = persisted_lock.get_mut(namespace) {
			outer_ref.remove(&key.to_string());
			self.did_persist.store(true, Ordering::SeqCst);
		}

		Ok(())
	}

	fn list(&self, namespace: &str) -> io::Result<Vec<String>> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();
		match persisted_lock.entry(namespace.to_string()) {
			hash_map::Entry::Occupied(e) => Ok(e.get().keys().cloned().collect()),
			hash_map::Entry::Vacant(_) => Ok(Vec::new()),
		}
	}
}

impl KVStorePersister for TestStore {
	fn persist<W: Writeable>(&self, prefixed_key: &str, object: &W) -> std::io::Result<()> {
		let msg = format!("Could not persist file for key {}.", prefixed_key);
		let dest_file = PathBuf::from_str(prefixed_key).map_err(|_| {
			lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg.clone())
		})?;

		let parent_directory = dest_file.parent().ok_or(lightning::io::Error::new(
			lightning::io::ErrorKind::InvalidInput,
			msg.clone(),
		))?;
		let namespace = parent_directory.display().to_string();

		let dest_without_namespace = dest_file
			.strip_prefix(&namespace)
			.map_err(|_| lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg))?;
		let key = dest_without_namespace.display().to_string();

		let data = object.encode();
		self.write(&namespace, &key, &data)?;
		Ok(())
	}
}

// A `KVStore` impl for testing purposes that wraps all our `KVStore`s and asserts their synchronicity.
pub(crate) struct TestSyncStore {
	fs_store: FilesystemStore,
	sqlite_store: SqliteStore,
}

impl TestSyncStore {
	pub(crate) fn new(dest_dir: PathBuf) -> Self {
		let fs_store = FilesystemStore::new(dest_dir.clone());
		let sqlite_store = SqliteStore::new(dest_dir);
		Self { fs_store, sqlite_store }
	}
}

impl KVStore for TestSyncStore {
	type Reader = io::Cursor<Vec<u8>>;

	fn read(&self, namespace: &str, key: &str) -> std::io::Result<Self::Reader> {
		// For now, we only assert `Ok` with the `fs_reader` here, as it's too complicated to track
		// the read status of both seperately, however, the `Reader` concept is going away anyways
		// at which point we can assert on simply on the returned values of `KVStore::read`.
		let fs_res = self.fs_store.read(namespace, key);
		let sqlite_res = self.sqlite_store.read(namespace, key);

		match sqlite_res {
			Ok(read) => {
				assert!(fs_res.is_ok());
				Ok(read)
			}
			Err(e) => {
				assert!(fs_res.is_err());
				assert_eq!(e.kind(), unsafe { fs_res.unwrap_err_unchecked().kind() });
				Err(e)
			}
		}
	}

	fn write(&self, namespace: &str, key: &str, buf: &[u8]) -> std::io::Result<()> {
		let fs_res = self.fs_store.write(namespace, key, buf);
		let sqlite_res = self.sqlite_store.write(namespace, key, buf);

		assert!(self.list(namespace).unwrap().contains(&key.to_string()));

		match fs_res {
			Ok(()) => {
				assert!(sqlite_res.is_ok());
				Ok(())
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				Err(e)
			}
		}
	}

	fn remove(&self, namespace: &str, key: &str) -> std::io::Result<()> {
		let fs_res = self.fs_store.remove(namespace, key);
		let sqlite_res = self.sqlite_store.remove(namespace, key);

		match fs_res {
			Ok(()) => {
				assert!(sqlite_res.is_ok());
				Ok(())
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				Err(e)
			}
		}
	}

	fn list(&self, namespace: &str) -> std::io::Result<Vec<String>> {
		let fs_res = self.fs_store.list(namespace);
		let sqlite_res = self.sqlite_store.list(namespace);

		match fs_res {
			Ok(list) => {
				assert_eq!(list, sqlite_res.unwrap());
				Ok(list)
			}
			Err(e) => {
				assert!(sqlite_res.is_err());
				Err(e)
			}
		}
	}
}

impl KVStorePersister for TestSyncStore {
	fn persist<W: Writeable>(&self, prefixed_key: &str, object: &W) -> std::io::Result<()> {
		let msg = format!("Could not persist file for key {}.", prefixed_key);
		let dest_file = PathBuf::from_str(prefixed_key).map_err(|_| {
			lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg.clone())
		})?;

		let parent_directory = dest_file.parent().ok_or(lightning::io::Error::new(
			lightning::io::ErrorKind::InvalidInput,
			msg.clone(),
		))?;
		let namespace = parent_directory.display().to_string();

		let dest_without_namespace = dest_file
			.strip_prefix(&namespace)
			.map_err(|_| lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg))?;
		let key = dest_without_namespace.display().to_string();

		let data = object.encode();
		self.write(&namespace, &key, &data)?;
		Ok(())
	}
}

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
	fn log(&self, record: &Record) {
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

pub fn random_storage_path() -> String {
	let mut rng = thread_rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	format!("/tmp/{}", rand_dir)
}

pub fn random_port() -> u16 {
	let mut rng = thread_rng();
	rng.gen_range(5000..65535)
}

pub fn random_config() -> Config {
	let mut config = Config::default();

	config.network = Network::Regtest;
	println!("Setting network: {}", config.network);

	let rand_dir = random_storage_path();
	println!("Setting random LDK storage dir: {}", rand_dir);
	config.storage_dir_path = rand_dir;

	let rand_port = random_port();
	println!("Setting random LDK listening port: {}", rand_port);
	let listening_address_str = format!("127.0.0.1:{}", rand_port);
	config.listening_address = Some(listening_address_str.parse().unwrap());

	config.log_level = Level::Trace;

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
	let cur_height = bitcoind.client.get_block_count().expect("failed to get current block height");
	let address = bitcoind
		.client
		.get_new_address(Some("test"), Some(AddressType::Legacy))
		.expect("failed to get new address");
	// TODO: expect this Result once the WouldBlock issue is resolved upstream.
	let _block_hashes_res = bitcoind.client.generate_to_address(num as u64, &address);
	wait_for_block(electrsd, cur_height as usize + num);
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
		assert!(tries < 10, "Reached max tries.");
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
