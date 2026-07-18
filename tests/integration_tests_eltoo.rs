// This file is Copyright its authors and licensed under the Apache License, Version 2.0 or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option. You may not use this
// file except in accordance with one or both of these licenses.

//! Two-node eltoo integration test: two `Node`s talking over real TCP, backed by a
//! real regtest bitcoind, open an eltoo channel funded by the BDK wallet, pay over
//! it, and close it — cooperatively and by force (update → shared delay →
//! settlement), with every transaction accepted by real consensus.
//!
//! Requires `BITCOIND_EXE` to point at a bitcoind built from the BIP-448 branch
//! (https://github.com/bip448/bitcoin/pull/1); the test is skipped otherwise.

use ldk_node::entropy::{generate_entropy_mnemonic, NodeEntropy};
use ldk_node::lightning::ln::eltoo::channelmanager::EltooEvent;
use ldk_node::lightning::ln::types::ChannelId;
use ldk_node::{Builder, Node};

use electrsd::corepc_node::{Conf, Node as BitcoinD};

use bitcoin::{Address, Amount, Network, OutPoint, Transaction};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

const CAPACITY_SAT: u64 = 1_000_000;
static NEXT_PORT: AtomicU16 = AtomicU16::new(23_000);

struct Harness {
	bitcoind: BitcoinD,
}

impl Harness {
	fn start() -> Option<Self> {
		let exe = match std::env::var("BITCOIND_EXE") {
			Ok(exe) => exe,
			Err(_) => {
				eprintln!(
					"BITCOIND_EXE unset; skipping eltoo integration test (needs a BIP-448 bitcoind)"
				);
				return None;
			},
		};
		let conf = Conf::default();
		let bitcoind = BitcoinD::with_conf(exe, &conf).expect("failed to start bitcoind");
		let harness = Harness { bitcoind };
		harness.mine(101);
		Some(harness)
	}

	fn mine(&self, blocks: usize) {
		let addr = self.bitcoind.client.new_address().expect("new address");
		self.bitcoind.client.generate_to_address(blocks, &addr).expect("generate");
	}

	fn fund(&self, address: &Address, amount: Amount) {
		self.bitcoind.client.send_to_address(address, amount).expect("sendtoaddress");
		self.mine(1);
	}

	fn tip(&self) -> u32 {
		self.bitcoind.client.get_block_count().expect("getblockcount").0 as u32
	}

	/// Finds a confirmed transaction spending `outpoint`, scanning from `from_height`.
	fn find_spending_tx(&self, outpoint: OutPoint, from_height: u32) -> Option<Transaction> {
		for height in from_height..=self.tip() {
			let hash = self
				.bitcoind
				.client
				.get_block_hash(height as u64)
				.expect("getblockhash")
				.into_model()
				.expect("hash")
				.0;
			let block = self.bitcoind.client.get_block(hash).expect("getblock");
			for tx in block.txdata {
				if tx.input.iter().any(|input| input.previous_output == outpoint) {
					return Some(tx);
				}
			}
		}
		None
	}

	fn make_node(&self) -> Node {
		self.start_node(&self.new_node_setup())
	}

	/// Fresh, restart-stable node identity: a storage directory, wallet entropy and
	/// listening port that stay fixed across restarts (so the KVStore-backed state — the
	/// BDK wallet and now the eltoo channels/monitors — is reloaded).
	fn new_node_setup(&self) -> NodeSetup {
		let mut storage: PathBuf = std::env::temp_dir();
		storage.push(format!("eltoo-node-{}", rand_suffix()));
		let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
		let entropy = NodeEntropy::from_bip39_mnemonic(generate_entropy_mnemonic(None), None);
		NodeSetup { storage: storage.to_str().unwrap().to_string(), entropy, port }
	}

	/// Builds and starts a node for the given (possibly reused) setup.
	fn start_node(&self, setup: &NodeSetup) -> Node {
		let rpc_host = self.bitcoind.params.rpc_socket.ip().to_string();
		let rpc_port = self.bitcoind.params.rpc_socket.port();
		let cookie = self.bitcoind.params.get_cookie_values().unwrap().unwrap();

		let mut builder = Builder::new();
		builder.set_network(Network::Regtest);
		eprintln!("node storage: {}", setup.storage);
		builder.set_storage_dir_path(setup.storage.clone());
		builder.set_filesystem_logger(None, Some(ldk_node::logger::LogLevel::Trace));
		builder
			.set_listening_addresses(vec![format!("127.0.0.1:{}", setup.port).parse().unwrap()])
			.unwrap();
		builder.set_chain_source_bitcoind_rpc(
			rpc_host,
			rpc_port,
			cookie.user,
			cookie.password,
			None,
		);
		let node = builder.build(setup.entropy).unwrap();
		node.start().unwrap();
		node
	}

	/// Scans confirmed blocks for the channel funding output (the capacity-value output).
	fn find_funding_outpoint(&self) -> OutPoint {
		let mut found = None;
		for height in 100..=self.tip() {
			let hash = self
				.bitcoind
				.client
				.get_block_hash(height as u64)
				.expect("getblockhash")
				.into_model()
				.expect("hash")
				.0;
			let block = self.bitcoind.client.get_block(hash).expect("getblock");
			for tx in block.txdata {
				if let Some(vout) =
					tx.output.iter().position(|out| out.value == Amount::from_sat(CAPACITY_SAT))
				{
					found = Some(OutPoint { txid: tx.compute_txid(), vout: vout as u32 });
				}
			}
		}
		found.expect("funding tx confirmed")
	}
}

/// A restart-stable node identity (storage + entropy + port).
struct NodeSetup {
	storage: String,
	entropy: NodeEntropy,
	port: u16,
}

fn rand_suffix() -> u64 {
	use std::time::{SystemTime, UNIX_EPOCH};
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64
}

/// Syncs both nodes and lets the eltoo background driver tick.
fn settle(nodes: &[&Node]) {
	for node in nodes {
		node.sync_wallets().expect("sync");
	}
	std::thread::sleep(Duration::from_millis(400));
}

/// Drives sync/tick until `check` extracts a value from a node's eltoo events, or
/// panics after `tries` rounds. Events not consumed by `check` are dropped.
fn wait_for_event<T>(
	harness: &Harness, nodes: &[&Node], node: &Node, tries: usize,
	check: impl Fn(&EltooEvent) -> Option<T>,
) -> T {
	for _ in 0..tries {
		settle(nodes);
		for event in node.eltoo_events() {
			if let Some(value) = check(&event) {
				return value;
			}
		}
		harness.mine(1);
	}
	panic!("timed out waiting for eltoo event");
}

#[test]
fn eltoo_channel_full_cycle_over_tcp() {
	let harness = match Harness::start() {
		Some(harness) => harness,
		None => return,
	};
	let a = harness.make_node();
	let b = harness.make_node();

	// Fund a's on-chain wallet (it pays for funding txs and CPFP children).
	let addr_a = a.onchain_payment().new_address().expect("address");
	harness.fund(&addr_a, Amount::from_sat(10_000_000));
	settle(&[&a, &b]);

	// Open an eltoo channel over TCP; the wallet funds it automatically once the
	// peer accepts, and it becomes ready at funding depth.
	let b_addr = b.listening_addresses().unwrap()[0].clone();
	a.open_eltoo_channel(b.node_id(), b_addr, CAPACITY_SAT, 0).expect("open");
	let channel_id: ChannelId = wait_for_event(&harness, &[&a, &b], &a, 30, |event| match event {
		EltooEvent::ChannelReady { channel_id } => Some(*channel_id),
		_ => None,
	});
	wait_for_event(&harness, &[&a, &b], &b, 10, |event| match event {
		EltooEvent::ChannelReady { channel_id: id } if *id == channel_id => Some(()),
		_ => None,
	});

	// The funding tx is on-chain, paying the channel capacity.
	let funding_outpoint = {
		let txid_bytes: [u8; 32] = channel_id.0;
		// The channel id is funding_txid XOR vout (BOLT 2 v1); recover by scanning
		// recent blocks for a tx with a capacity-value output instead.
		let mut found = None;
		for height in 100..=harness.tip() {
			let hash = harness
				.bitcoind
				.client
				.get_block_hash(height as u64)
				.expect("getblockhash")
				.into_model()
				.expect("hash")
				.0;
			let block = harness.bitcoind.client.get_block(hash).expect("getblock");
			for tx in block.txdata {
				if let Some(vout) =
					tx.output.iter().position(|out| out.value == Amount::from_sat(CAPACITY_SAT))
				{
					found = Some(OutPoint { txid: tx.compute_txid(), vout: vout as u32 });
				}
			}
		}
		let _ = txid_bytes;
		found.expect("funding tx confirmed")
	};

	// A payment over real TCP: 6 messages, one round trip each way.
	let payment_hash = b.eltoo_receive_payment();
	a.send_eltoo_payment(channel_id, 100_000_000, payment_hash).expect("send");
	wait_for_event(&harness, &[&a, &b], &b, 10, |event| match event {
		EltooEvent::PaymentClaimed { payment_hash: hash, amount_msat }
			if *hash == payment_hash && *amount_msat == 100_000_000 =>
		{
			Some(())
		},
		_ => None,
	});
	wait_for_event(&harness, &[&a, &b], &a, 10, |event| match event {
		EltooEvent::PaymentFulfilled { payment_hash: hash } if *hash == payment_hash => Some(()),
		_ => None,
	});

	// Cooperative close: the MuSig2 keyspend confirms and spends the funding output.
	let close_from = harness.tip() + 1;
	a.close_eltoo_channel(channel_id).expect("close");
	settle(&[&a, &b]);
	harness.mine(1);
	settle(&[&a, &b]);
	let closing_tx = harness
		.find_spending_tx(funding_outpoint, close_from)
		.expect("cooperative close must confirm");
	assert_eq!(closing_tx.input.len(), 1);
	assert_eq!(closing_tx.input[0].witness.len(), 1, "keyspend: single signature");

	a.stop().unwrap();
	b.stop().unwrap();
}

#[test]
fn eltoo_force_close_settles_over_tcp() {
	let harness = match Harness::start() {
		Some(harness) => harness,
		None => return,
	};
	let a = harness.make_node();
	let b = harness.make_node();

	let addr_a = a.onchain_payment().new_address().expect("address");
	harness.fund(&addr_a, Amount::from_sat(10_000_000));
	settle(&[&a, &b]);

	let b_addr = b.listening_addresses().unwrap()[0].clone();
	a.open_eltoo_channel(b.node_id(), b_addr, CAPACITY_SAT, 0).expect("open");
	let channel_id: ChannelId = wait_for_event(&harness, &[&a, &b], &a, 30, |event| match event {
		EltooEvent::ChannelReady { channel_id } => Some(*channel_id),
		_ => None,
	});
	b.eltoo_events();

	// One payment so the broadcast state is not state 0.
	let payment_hash = b.eltoo_receive_payment();
	a.send_eltoo_payment(channel_id, 100_000_000, payment_hash).expect("send");
	wait_for_event(&harness, &[&a, &b], &a, 10, |event| match event {
		EltooEvent::PaymentFulfilled { .. } => Some(()),
		_ => None,
	});

	// Locate the funding outpoint (capacity-value output).
	let mut funding_outpoint = None;
	for height in 100..=harness.tip() {
		let hash = harness
			.bitcoind
			.client
			.get_block_hash(height as u64)
			.expect("getblockhash")
			.into_model()
			.expect("hash")
			.0;
		let block = harness.bitcoind.client.get_block(hash).expect("getblock");
		for tx in block.txdata {
			if let Some(vout) =
				tx.output.iter().position(|out| out.value == Amount::from_sat(CAPACITY_SAT))
			{
				funding_outpoint = Some(OutPoint { txid: tx.compute_txid(), vout: vout as u32 });
			}
		}
	}
	let funding_outpoint = funding_outpoint.expect("funding tx confirmed");

	// Force close: the zero-fee TRUC update tx enters the mempool as a package with
	// its wallet-funded P2A CPFP child.
	let close_from = harness.tip() + 1;
	a.force_close_eltoo_channel(channel_id);
	settle(&[&a, &b]);
	harness.mine(1);
	settle(&[&a, &b]);
	let update_tx =
		harness.find_spending_tx(funding_outpoint, close_from).expect("update tx must confirm");
	assert_eq!(update_tx.input[0].witness.len(), 3, "script-path: [sig, leaf, control]");

	// After the shared delay the signature-free settlement follows automatically.
	harness.mine(144);
	settle(&[&a, &b]);
	let settle_from = harness.tip() + 1;
	harness.mine(1);
	settle(&[&a, &b]);
	let state_outpoint = OutPoint { txid: update_tx.compute_txid(), vout: 2 };
	let settlement_tx =
		harness.find_spending_tx(state_outpoint, settle_from).expect("settlement must confirm");
	assert_eq!(settlement_tx.input[0].witness.len(), 2, "covenant spend: [leaf, control]");

	a.stop().unwrap();
	b.stop().unwrap();
}

#[test]
fn eltoo_channel_survives_restart_and_force_closes() {
	let harness = match Harness::start() {
		Some(harness) => harness,
		None => return,
	};
	// Both nodes get restart-stable identities so their KVStore-backed state (BDK wallet
	// + eltoo channels/monitors) reloads after a stop/start.
	let setup_a = harness.new_node_setup();
	let setup_b = harness.new_node_setup();
	let a = harness.start_node(&setup_a);
	let b = harness.start_node(&setup_b);

	let addr_a = a.onchain_payment().new_address().expect("address");
	harness.fund(&addr_a, Amount::from_sat(10_000_000));
	settle(&[&a, &b]);

	// Open an eltoo channel and make one payment, so the persisted tip is not state 0.
	let b_addr = b.listening_addresses().unwrap()[0].clone();
	a.open_eltoo_channel(b.node_id(), b_addr, CAPACITY_SAT, 0).expect("open");
	let channel_id: ChannelId = wait_for_event(&harness, &[&a, &b], &a, 30, |event| match event {
		EltooEvent::ChannelReady { channel_id } => Some(*channel_id),
		_ => None,
	});
	wait_for_event(&harness, &[&a, &b], &b, 10, |event| match event {
		EltooEvent::ChannelReady { channel_id: id } if *id == channel_id => Some(()),
		_ => None,
	});

	let payment_hash = b.eltoo_receive_payment();
	a.send_eltoo_payment(channel_id, 100_000_000, payment_hash).expect("send");
	wait_for_event(&harness, &[&a, &b], &a, 10, |event| match event {
		EltooEvent::PaymentFulfilled { .. } => Some(()),
		_ => None,
	});

	// Give the background driver a few passes to flush the per-update persistence.
	settle(&[&a, &b]);
	settle(&[&a, &b]);

	let funding_outpoint = harness.find_funding_outpoint();

	// Restart both nodes from their existing storage: the eltoo channels and monitors are
	// re-read from the KVStore (their signers re-derived from the persisted key ids).
	a.stop().unwrap();
	b.stop().unwrap();
	drop(a);
	drop(b);
	let a = harness.start_node(&setup_a);
	let b = harness.start_node(&setup_b);
	settle(&[&a, &b]);

	// The reloaded node force-closes: this only succeeds if the monitor's tip state — and
	// its aggregate signature — survived the restart. A lost monitor could not produce a
	// valid update transaction here.
	let close_from = harness.tip() + 1;
	a.force_close_eltoo_channel(channel_id);
	settle(&[&a, &b]);
	harness.mine(1);
	settle(&[&a, &b]);
	let update_tx = harness
		.find_spending_tx(funding_outpoint, close_from)
		.expect("update tx must confirm after restart");
	assert_eq!(update_tx.input[0].witness.len(), 3, "script-path: [sig, leaf, control]");

	// And its signature-free settlement follows after the shared delay, exactly as for a
	// channel that never restarted.
	harness.mine(144);
	let state_outpoint = OutPoint { txid: update_tx.compute_txid(), vout: 2 };
	let settle_from = harness.tip() + 1;
	// Poll: let the reloaded monitor broadcast the settlement once the delay matures, then
	// mine it in. (The broadcast lands a driver tick after the maturing block is synced.)
	let settlement_tx = {
		let mut found = None;
		for _ in 0..20 {
			settle(&[&a, &b]);
			harness.mine(1);
			settle(&[&a, &b]);
			if let Some(tx) = harness.find_spending_tx(state_outpoint, settle_from) {
				found = Some(tx);
				break;
			}
		}
		found.expect("settlement must confirm after restart")
	};
	assert_eq!(settlement_tx.input[0].witness.len(), 2, "covenant spend: [leaf, control]");

	a.stop().unwrap();
	b.stop().unwrap();
}
