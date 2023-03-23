use crate::io::KVStoreUnpersister;
use crate::Config;
use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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
