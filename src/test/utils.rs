use crate::io::KVStoreUnpersister;
use crate::Config;
use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;

use bitcoin::{Address, Amount, OutPoint, Txid};

use bitcoind::bitcoincore_rpc::RpcApi;
use electrsd::bitcoind::bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use electrsd::{bitcoind, bitcoind::BitcoinD, ElectrsD};
use electrum_client::ElectrumApi;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

macro_rules! expect_event {
	($node: expr, $event_type: ident) => {{
		match $node.next_event() {
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

pub(crate) struct TestPersister {
	persisted_bytes: Mutex<HashMap<String, Vec<u8>>>,
	did_persist: AtomicBool,
}

impl TestPersister {
	pub fn new() -> Self {
		let persisted_bytes = Mutex::new(HashMap::new());
		let did_persist = AtomicBool::new(false);
		Self { persisted_bytes, did_persist }
	}

	pub fn get_persisted_bytes(&self, key: &str) -> Option<Vec<u8>> {
		let persisted_bytes_lock = self.persisted_bytes.lock().unwrap();
		persisted_bytes_lock.get(key).cloned()
	}

	pub fn get_and_clear_did_persist(&self) -> bool {
		self.did_persist.swap(false, Ordering::SeqCst)
	}
}

impl KVStorePersister for TestPersister {
	fn persist<W: Writeable>(&self, key: &str, object: &W) -> std::io::Result<()> {
		let mut persisted_bytes_lock = self.persisted_bytes.lock().unwrap();
		let mut bytes = Vec::new();
		object.write(&mut bytes)?;
		persisted_bytes_lock.insert(key.to_owned(), bytes);
		self.did_persist.store(true, Ordering::SeqCst);
		Ok(())
	}
}

impl KVStoreUnpersister for TestPersister {
	fn unpersist(&self, key: &str) -> std::io::Result<bool> {
		let mut persisted_bytes_lock = self.persisted_bytes.lock().unwrap();
		self.did_persist.store(true, Ordering::SeqCst);
		Ok(persisted_bytes_lock.remove(key).is_some())
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

pub fn random_config(esplora_url: &str) -> Config {
	let mut config = Config::default();

	println!("Setting esplora server URL: {}", esplora_url);
	config.esplora_server_url = format!("http://{}", esplora_url);

	let rand_dir = random_storage_path();
	println!("Setting random LDK storage dir: {}", rand_dir);
	config.storage_dir_path = rand_dir;

	let rand_port = random_port();
	println!("Setting random LDK listening port: {}", rand_port);
	let listening_address = format!("127.0.0.1:{}", rand_port);
	config.listening_address = Some(listening_address);

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
