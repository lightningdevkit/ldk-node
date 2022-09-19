use crate::tests::test_utils::expect_event;
use crate::{Builder, Config, Event};

use bitcoin::{Address, Amount};
use bitcoind::bitcoincore_rpc::RpcApi;
use electrsd::bitcoind::bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use electrsd::{bitcoind, bitcoind::BitcoinD, ElectrsD};
use electrum_client::ElectrumApi;

use once_cell::sync::OnceCell;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use std::env;
use std::sync::Mutex;
use std::time::Duration;

static BITCOIND: OnceCell<BitcoinD> = OnceCell::new();
static ELECTRSD: OnceCell<ElectrsD> = OnceCell::new();
static PREMINE: OnceCell<()> = OnceCell::new();
static MINER_LOCK: Mutex<()> = Mutex::new(());

fn get_bitcoind() -> &'static BitcoinD {
	BITCOIND.get_or_init(|| {
		let bitcoind_exe =
			env::var("BITCOIND_EXE").ok().or_else(|| bitcoind::downloaded_exe_path().ok()).expect(
				"you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
			);
		let mut conf = bitcoind::Conf::default();
		conf.network = "regtest";
		BitcoinD::with_conf(bitcoind_exe, &conf).unwrap()
	})
}

fn get_electrsd() -> &'static ElectrsD {
	ELECTRSD.get_or_init(|| {
		let bitcoind = get_bitcoind();
		let electrs_exe =
			env::var("ELECTRS_EXE").ok().or_else(electrsd::downloaded_exe_path).expect(
				"you need to provide env var ELECTRS_EXE or specify an electrsd version feature",
			);
		let mut conf = electrsd::Conf::default();
		conf.http_enabled = true;
		conf.network = "regtest";
		ElectrsD::with_conf(electrs_exe, &bitcoind, &conf).unwrap()
	})
}

fn generate_blocks_and_wait(num: usize) {
	let _miner = MINER_LOCK.lock().unwrap();
	let cur_height = get_bitcoind().client.get_block_count().unwrap();
	let address =
		get_bitcoind().client.get_new_address(Some("test"), Some(AddressType::Legacy)).unwrap();
	let _block_hashes = get_bitcoind().client.generate_to_address(num as u64, &address).unwrap();
	wait_for_block(cur_height as usize + num);
}

fn wait_for_block(min_height: usize) {
	let mut header = get_electrsd().client.block_headers_subscribe().unwrap();
	loop {
		if header.height >= min_height {
			break;
		}
		header = exponential_backoff_poll(|| {
			get_electrsd().trigger().unwrap();
			get_electrsd().client.ping().unwrap();
			get_electrsd().client.block_headers_pop().unwrap()
		});
	}
}

fn exponential_backoff_poll<T, F>(mut poll: F) -> T
where
	F: FnMut() -> Option<T>,
{
	let mut delay = Duration::from_millis(64);
	loop {
		match poll() {
			Some(data) => break data,
			None if delay.as_millis() < 512 => delay = delay.mul_f32(2.0),
			None => {}
		}

		std::thread::sleep(delay);
	}
}

fn premine_and_distribute_funds(addrs: Vec<Address>, amount: Amount) {
	PREMINE.get_or_init(|| {
		generate_blocks_and_wait(101);
		for addr in addrs {
			get_bitcoind()
				.client
				.send_to_address(&addr, amount, None, None, None, None, None, None)
				.unwrap();
		}
		generate_blocks_and_wait(1);
	});
}

fn rand_config() -> Config {
	let mut config = Config::default();

	let esplora_url = get_electrsd().esplora_url.as_ref().unwrap();

	println!("Setting esplora server URL: {}", esplora_url);
	config.esplora_server_url = format!("http://{}", esplora_url);

	let mut rng = thread_rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	let rand_path = format!("/tmp/{}", rand_dir);
	println!("Setting random LDK storage dir: {}", rand_dir);
	config.storage_dir_path = rand_path;

	let rand_port: u16 = rng.gen_range(5000..8000);
	println!("Setting random LDK listening port: {}", rand_port);
	let listening_address = format!("127.0.0.1:{}", rand_port);
	config.listening_address = Some(listening_address);

	config
}

#[test]
fn channel_full_cycle() {
	println!("== Node A ==");
	let config_a = rand_config();
	let node_a = Builder::from_config(config_a).build();
	node_a.start().unwrap();
	let addr_a = node_a.new_funding_address().unwrap();

	println!("\n== Node B ==");
	let config_b = rand_config();
	let node_b = Builder::from_config(config_b).build();
	node_b.start().unwrap();
	let addr_b = node_b.new_funding_address().unwrap();

	premine_and_distribute_funds(vec![addr_a, addr_b], Amount::from_sat(100000));
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.on_chain_balance().unwrap().get_spendable(), 100000);
	assert_eq!(node_b.on_chain_balance().unwrap().get_spendable(), 100000);

	println!("\nA -- connect_open_channel -> B");
	let node_b_addr =
		format!("{}@{}", node_b.node_id().unwrap(), node_b.listening_address().unwrap());
	node_a.connect_open_channel(&node_b_addr, 50000, true).unwrap();

	// Wait a sec so the funding tx can 'propagate' via EsploraD to BitcoinD.
	std::thread::sleep(Duration::from_secs(1));

	println!("\n .. generating blocks, syncing wallets .. ");
	generate_blocks_and_wait(6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let node_a_balance = node_a.on_chain_balance().unwrap();
	assert!(node_a_balance.get_spendable() < 50000);
	assert!(node_a_balance.get_spendable() > 40000);
	assert_eq!(node_b.on_chain_balance().unwrap().get_spendable(), 100000);

	expect_event!(node_a, ChannelReady);

	let channel_id = match node_b.next_event() {
		ref e @ Event::ChannelReady { ref channel_id, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			channel_id.clone()
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};

	println!("\nB receive_payment");
	let invoice = node_b.receive_payment(Some(1000), &"asdf", 9217).unwrap();

	println!("\nA send_payment");
	node_a.send_payment(invoice).unwrap();

	expect_event!(node_a, PaymentSuccessful);
	expect_event!(node_b, PaymentReceived);

	node_b.close_channel(&channel_id.0, &node_a.node_id().unwrap()).unwrap();
	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	// Wait a sec so the shutdown tx can 'propagate' via EsploraD to BitcoinD.
	std::thread::sleep(Duration::from_secs(1));

	generate_blocks_and_wait(1);
	node_a.sync_wallets().unwrap();

	assert!(node_a.on_chain_balance().unwrap().get_spendable() > 90000);
	assert_eq!(node_b.on_chain_balance().unwrap().get_spendable(), 100000);

	node_a.stop().unwrap();
	println!("\nA stopped");
	node_b.stop().unwrap();
	println!("\nB stopped");
}
