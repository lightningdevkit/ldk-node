#![cfg(cln_test)]

use ldk_node::{Config, LogLevel};

use lightning::ln::msgs::SocketAddress;

use bitcoin::{Address, Amount, Network, OutPoint, Txid};

use bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use bitcoincore_rpc::Client as BitcoindClient;
use bitcoincore_rpc::RpcApi;

use electrum_client::Client as ElectrumClient;
use electrum_client::ElectrumApi;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use std::path::PathBuf;
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
			ref e @ Event::ChannelReady { channel_id, counterparty_node_id, .. } => {
				println!("{} got event {:?}", $node.node_id(), e);
				assert_eq!(counterparty_node_id, Some($counterparty_node_id));
				$node.event_handled();
				channel_id
			}
			ref e => {
				panic!("{} got unexpected event!: {:?}", std::stringify!($node), e);
			}
		}
	}};
}

pub(crate) use expect_channel_ready_event;

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

pub(crate) fn generate_blocks_and_wait(
	bitcoind: &BitcoindClient, electrs: &ElectrumClient, num: usize,
) {
	let _ = bitcoind.create_wallet("ldk_node_test", None, None, None, None);
	let _ = bitcoind.load_wallet("ldk_node_test");
	print!("Generating {} blocks...", num);
	let cur_height = bitcoind.get_block_count().expect("failed to get current block height");
	let address = bitcoind
		.get_new_address(Some("test"), Some(AddressType::Legacy))
		.expect("failed to get new address");
	// TODO: expect this Result once the WouldBlock issue is resolved upstream.
	let _block_hashes_res = bitcoind.generate_to_address(num as u64, &address);
	wait_for_block(electrs, cur_height as usize + num);
	print!(" Done!");
	println!("\n");
}

pub(crate) fn wait_for_block(electrs: &ElectrumClient, min_height: usize) {
	let mut header = match electrs.block_headers_subscribe() {
		Ok(header) => header,
		Err(_) => {
			// While subscribing should succeed the first time around, we ran into some cases where
			// it didn't. Since we can't proceed without subscribing, we try again after a delay
			// and panic if it still fails.
			std::thread::sleep(Duration::from_secs(1));
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

pub(crate) fn wait_for_tx(electrs: &ElectrumClient, txid: Txid) {
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

pub(crate) fn wait_for_outpoint_spend(electrs: &ElectrumClient, outpoint: OutPoint) {
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

pub(crate) fn premine_and_distribute_funds(
	bitcoind: &BitcoindClient, electrs: &ElectrumClient, addrs: Vec<Address>, amount: Amount,
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
