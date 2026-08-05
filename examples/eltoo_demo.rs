// This file is Copyright its authors and licensed under the Apache License, Version 2.0 or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option. You may not use this
// file except in accordance with one or both of these licenses.

//! Interactive demo driver for eltoo (LN-Symmetry over BIP-448) channels on regtest.
//!
//! Run one instance per node, each pointed at your own BIP-448-patched bitcoind
//! (<https://github.com/bip448/bitcoin/pull/1>; the opcodes are always active on
//! regtest). The demo never mines or queries bitcoind itself — you drive the chain
//! and inspect every transaction with `bitcoin-cli`.
//!
//! # Example session
//!
//! Terminal 0 — bitcoind:
//! ```text
//! $ bitcoind -regtest -fallbackfee=0.00001
//! $ bitcoin-cli -regtest createwallet mine
//! $ bitcoin-cli -regtest -generate 101
//! ```
//!
//! Terminals 1 and 2 — the two nodes:
//! ```text
//! $ cargo run --example eltoo_demo -- --storage /tmp/eltoo-a --listen 9735 \
//!       --rpc 127.0.0.1:18443 --rpc-cookie ~/.bitcoin/regtest/.cookie
//! $ cargo run --example eltoo_demo -- --storage /tmp/eltoo-b --listen 9736 \
//!       --rpc 127.0.0.1:18443 --rpc-cookie ~/.bitcoin/regtest/.cookie
//! ```
//!
//! Fund both wallets — CPFP fee-bumping needs confirmed UTXOs on *both* sides:
//! ```text
//! A> fundaddr
//! $ bitcoin-cli -regtest sendtoaddress <addr> 0.1 && bitcoin-cli -regtest -generate 1
//! (repeat for B, then `sync` + `balance` in each REPL)
//! ```
//!
//! Open a channel from A to B (B auto-accepts):
//! ```text
//! B> id
//! A> open <b_node_id> 127.0.0.1:9736 1000000
//! $ bitcoin-cli -regtest getrawmempool              # the funding tx
//! $ bitcoin-cli -regtest -generate 3                # minimum_depth = 3
//! A> events                                         # ChannelReady
//! A> channels                                       # funding outpoint, state 0
//! $ bitcoin-cli -regtest getrawtransaction <funding_txid> 2   # plain P2TR output
//! ```
//!
//! Pay (payment hashes travel out-of-band; the PoC has no invoices):
//! ```text
//! B> recv
//! A> pay <chan> 250000000 <hash>                    # amount in msat
//! A> channels                                       # state 2, balances moved
//! ```
//!
//! The cheat — snapshot a signed state, advance past it, publish the stale tx:
//! ```text
//! B> snapshot <chan>                                # held in REPL memory only
//! B> recv                                           # ...then A> pay ... again
//! B> cheat <chan>                                   # stale update + CPFP child
//! $ bitcoin-cli -regtest -generate 1
//! A> sync                                           # A sees the stale confirm and
//! A> events                                         #  rebinds its tip update onto it
//! $ bitcoin-cli -regtest getrawmempool              # rebound update: same signature,
//! $ bitcoin-cli -regtest -generate 1                #  new prevout
//! $ bitcoin-cli -regtest -generate 144              # shared_delay matures
//! A> sync                                           # settlement (no signature at all)
//! $ bitcoin-cli -regtest getrawmempool
//! ```
//! Both nodes rebind the same tip transaction — identical txid, harmless.
//!
//! Cooperative close (a single MuSig2 keyspend of the funding output):
//! ```text
//! A> close <chan>
//! $ bitcoin-cli -regtest getrawmempool && bitcoin-cli -regtest -generate 1
//! ```
//!
//! PoC caveats: keep both REPLs running for the life of a channel — peer
//! reconnect/reestablish is not wired, and after a restart channels are
//! force-close-only. Direct channels only; no invoices, routing or forwarding.

use ldk_node::bip39::Mnemonic;
use ldk_node::entropy::NodeEntropy;
use ldk_node::lightning::ln::eltoo::channelmanager::{EltooChannelDetails, EltooEvent};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::ln::types::ChannelId;
use ldk_node::lightning_types::payment::PaymentHash;
use ldk_node::{Builder, Node};

use bitcoin::secp256k1::PublicKey;
use bitcoin::{Network, OutPoint, Transaction};

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::str::FromStr;

const USAGE: &str = "usage: eltoo_demo --storage <dir> --listen <port> --rpc <host:port> \
                     (--rpc-cookie <path> | --rpc-user <user> --rpc-pass <pass>)";

const HELP: &str = "commands:
  id                                  node id, listening address, chain tip
  fundaddr                            new on-chain address (fund it via bitcoin-cli)
  balance                             on-chain wallet balance
  open <node_id> <host:port> <cap_sat> [push_msat]
                                      open an eltoo channel to a peer
  channels                            list eltoo channels
  recv                                register a payment; prints the payment hash
  pay <chan> <amount_msat> <hash>     pay over a channel
  snapshot <chan>                     hold the current signed update tx (for `cheat`)
  cheat <chan>                        broadcast the held (now stale) update tx
  close <chan>                        cooperative close
  forceclose <chan>                   unilateral close at the latest state
  events                              drain and print pending eltoo events
  sync                                sync wallets/chain now
  help                                this help
  quit                                stop the node and exit

<chan> may be any unique prefix of a channel id from `channels`.";

fn main() {
	let args = match Args::parse() {
		Ok(args) => args,
		Err(err) => {
			eprintln!("{}\n{}", err, USAGE);
			std::process::exit(1);
		},
	};

	std::fs::create_dir_all(&args.storage).expect("create storage dir");
	let entropy = load_or_generate_entropy(&args.storage);

	let mut builder = Builder::new();
	builder.set_network(Network::Regtest);
	builder.set_storage_dir_path(args.storage.clone());
	builder.set_filesystem_logger(None, None);
	builder
		.set_listening_addresses(vec![format!("0.0.0.0:{}", args.listen).parse().unwrap()])
		.unwrap();
	builder.set_chain_source_bitcoind_rpc(
		args.rpc_host,
		args.rpc_port,
		args.rpc_user,
		args.rpc_pass,
		None,
	);
	let node = builder.build(entropy).expect("build node");
	node.start().expect("start node");

	println!("eltoo demo node started");
	println!("  node id:   {}", node.node_id());
	println!("  listening: 127.0.0.1:{}", args.listen);
	println!("  storage:   {}", args.storage);
	println!("type `help` for commands\n");

	// Stale-state snapshots held for `cheat`, keyed by channel id.
	let mut snapshots: HashMap<ChannelId, (Transaction, OutPoint)> = HashMap::new();

	let stdin = std::io::stdin();
	loop {
		print!("> ");
		std::io::stdout().flush().unwrap();
		let mut line = String::new();
		if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
			break;
		}
		let parts: Vec<&str> = line.split_whitespace().collect();
		let (cmd, args) = match parts.split_first() {
			Some((cmd, args)) => (*cmd, args),
			None => continue,
		};
		match cmd {
			"id" => {
				let status = node.status();
				println!("node id:   {}", node.node_id());
				if let Some(addrs) = node.listening_addresses() {
					for addr in addrs {
						println!("listening: {}", addr);
					}
				}
				println!(
					"chain tip: {} ({})",
					status.current_best_block.height, status.current_best_block.block_hash
				);
			},
			"fundaddr" => match node.onchain_payment().new_address() {
				Ok(addr) => println!("{}", addr),
				Err(err) => println!("error: {}", err),
			},
			"balance" => {
				let balances = node.list_balances();
				println!(
					"on-chain: {} sat total, {} sat spendable",
					balances.total_onchain_balance_sats, balances.spendable_onchain_balance_sats
				);
			},
			"open" => {
				if args.len() < 3 {
					println!("usage: open <node_id> <host:port> <cap_sat> [push_msat]");
					continue;
				}
				let node_id = match PublicKey::from_str(args[0]) {
					Ok(id) => id,
					Err(err) => {
						println!("bad node id: {}", err);
						continue;
					},
				};
				let address = match SocketAddress::from_str(args[1]) {
					Ok(addr) => addr,
					Err(_) => {
						println!("bad address (want host:port)");
						continue;
					},
				};
				let cap_sat: u64 = match args[2].parse() {
					Ok(cap) => cap,
					Err(_) => {
						println!("bad capacity");
						continue;
					},
				};
				let push_msat: u64 = args.get(3).and_then(|arg| arg.parse().ok()).unwrap_or(0);
				match node.open_eltoo_channel(node_id, address, cap_sat, push_msat) {
					Ok(temp_id) => {
						println!("opening; temporary channel id {}", hex(&temp_id.0));
						println!("(funding broadcasts once the peer accepts; mine 3 blocks, then `events` for ChannelReady)");
					},
					Err(err) => println!("error: {}", err),
				}
			},
			"channels" => {
				let channels = node.list_eltoo_channels();
				if channels.is_empty() {
					println!("no eltoo channels");
				}
				for details in &channels {
					print_channel(details);
				}
			},
			"recv" => {
				let payment_hash = node.eltoo_receive_payment();
				println!("payment hash: {}", hex(&payment_hash.0));
				println!("(give this to the payer; it has no amount attached)");
			},
			"pay" => {
				if args.len() != 3 {
					println!("usage: pay <chan> <amount_msat> <hash>");
					continue;
				}
				let channel_id = match resolve_channel(&node, args[0]) {
					Some(id) => id,
					None => continue,
				};
				let amount_msat: u64 = match args[1].parse() {
					Ok(amount) => amount,
					Err(_) => {
						println!("bad amount (msat)");
						continue;
					},
				};
				let payment_hash = match parse_hash(args[2]) {
					Some(hash) => hash,
					None => {
						println!("bad payment hash (want 64 hex chars)");
						continue;
					},
				};
				match node.send_eltoo_payment(channel_id, amount_msat, payment_hash) {
					Ok(()) => println!("payment sent; `events` shows the outcome"),
					Err(err) => println!("error: {}", err),
				}
			},
			"snapshot" => {
				let channel_id = match args.first().and_then(|arg| resolve_channel(&node, arg)) {
					Some(id) => id,
					None => {
						println!("usage: snapshot <chan>");
						continue;
					},
				};
				match node.eltoo_signed_update_package(channel_id) {
					Ok((tx, anchor)) => {
						println!(
							"holding state-{} update tx {} (broadcast later with `cheat`)",
							state_of(&tx),
							tx.compute_txid()
						);
						snapshots.insert(channel_id, (tx, anchor));
					},
					Err(err) => println!("error: {}", err),
				}
			},
			"cheat" => {
				let channel_id = match args.first().and_then(|arg| resolve_channel(&node, arg)) {
					Some(id) => id,
					None => {
						println!("usage: cheat <chan>");
						continue;
					},
				};
				let (tx, anchor) =
					match snapshots.remove(&channel_id) {
						Some(snapshot) => snapshot,
						None => {
							println!("no snapshot held for that channel (take one with `snapshot` first)");
							continue;
						},
					};
				let state = state_of(&tx);
				let txid = tx.compute_txid();
				match node.broadcast_eltoo_package(tx, anchor) {
					Ok(()) => println!(
						"broadcast stale state-{} update {} — mine a block and watch the peer respond",
						state, txid
					),
					Err(err) => println!("error: {}", err),
				}
			},
			"close" => {
				let channel_id = match args.first().and_then(|arg| resolve_channel(&node, arg)) {
					Some(id) => id,
					None => {
						println!("usage: close <chan>");
						continue;
					},
				};
				match node.close_eltoo_channel(channel_id) {
					Ok(()) => {
						println!("cooperative close negotiated; `events` shows the closing tx")
					},
					Err(err) => println!("error: {}", err),
				}
			},
			"forceclose" => {
				let channel_id = match args.first().and_then(|arg| resolve_channel(&node, arg)) {
					Some(id) => id,
					None => {
						println!("usage: forceclose <chan>");
						continue;
					},
				};
				node.force_close_eltoo_channel(channel_id);
				println!("force-closing; `events` shows the update tx; settlement follows after shared_delay (144 blocks)");
			},
			"events" => {
				let events = node.eltoo_events();
				if events.is_empty() {
					println!("no pending events");
				}
				for event in &events {
					print_event(event);
				}
			},
			"sync" => match node.sync_wallets() {
				Ok(()) => println!("synced to height {}", node.status().current_best_block.height),
				Err(err) => println!("error: {}", err),
			},
			"help" => println!("{}", HELP),
			"quit" | "exit" => break,
			other => println!("unknown command `{}`; try `help`", other),
		}
	}

	println!("stopping node...");
	let _ = node.stop();
}

struct Args {
	storage: String,
	listen: u16,
	rpc_host: String,
	rpc_port: u16,
	rpc_user: String,
	rpc_pass: String,
}

impl Args {
	fn parse() -> Result<Self, String> {
		let mut storage = None;
		let mut listen = None;
		let mut rpc = None;
		let mut cookie = None;
		let mut user = None;
		let mut pass = None;
		let mut args = std::env::args().skip(1);
		while let Some(flag) = args.next() {
			let mut value = || args.next().ok_or_else(|| format!("{} needs a value", flag));
			match flag.as_str() {
				"--storage" => storage = Some(value()?),
				"--listen" => listen = Some(value()?),
				"--rpc" => rpc = Some(value()?),
				"--rpc-cookie" => cookie = Some(value()?),
				"--rpc-user" => user = Some(value()?),
				"--rpc-pass" => pass = Some(value()?),
				other => return Err(format!("unknown flag {}", other)),
			}
		}
		let storage = storage.ok_or("--storage is required")?;
		let listen =
			listen.ok_or("--listen is required")?.parse().map_err(|_| "bad --listen port")?;
		let rpc = rpc.ok_or("--rpc is required")?;
		let (rpc_host, rpc_port_str) = rpc.rsplit_once(':').ok_or("--rpc wants host:port")?;
		let rpc_port = rpc_port_str.parse().map_err(|_| "bad --rpc port")?;
		let (rpc_user, rpc_pass) = match (cookie, user, pass) {
			(Some(path), None, None) => {
				let contents = std::fs::read_to_string(&path)
					.map_err(|err| format!("cannot read cookie file {}: {}", path, err))?;
				let (user, pass) =
					contents.trim().split_once(':').ok_or("malformed cookie file")?;
				(user.to_string(), pass.to_string())
			},
			(None, Some(user), Some(pass)) => (user, pass),
			_ => return Err("need either --rpc-cookie or --rpc-user + --rpc-pass".to_string()),
		};
		Ok(Self { storage, listen, rpc_host: rpc_host.to_string(), rpc_port, rpc_user, rpc_pass })
	}
}

/// Loads the node's mnemonic from the storage dir, generating and saving one on first
/// run, so the node keeps its identity (and wallet) across restarts.
fn load_or_generate_entropy(storage: &str) -> NodeEntropy {
	let path = std::path::Path::new(storage).join("demo_mnemonic");
	let mnemonic = match std::fs::read_to_string(&path) {
		Ok(contents) => Mnemonic::from_str(contents.trim()).expect("valid saved mnemonic"),
		Err(_) => {
			let mnemonic = ldk_node::entropy::generate_entropy_mnemonic(None);
			std::fs::write(&path, mnemonic.to_string()).expect("write mnemonic");
			mnemonic
		},
	};
	NodeEntropy::from_bip39_mnemonic(mnemonic, None)
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|byte| format!("{:02x}", byte)).collect()
}

fn parse_hash(hash_hex: &str) -> Option<PaymentHash> {
	if hash_hex.len() != 64 || !hash_hex.chars().all(|c| c.is_ascii_hexdigit()) {
		return None;
	}
	let mut bytes = [0u8; 32];
	for (i, byte) in bytes.iter_mut().enumerate() {
		*byte = u8::from_str_radix(&hash_hex[2 * i..2 * i + 2], 16).ok()?;
	}
	Some(PaymentHash(bytes))
}

/// Resolves a (prefix of a) channel id against the node's eltoo channels.
fn resolve_channel(node: &Node, prefix: &str) -> Option<ChannelId> {
	let matches: Vec<ChannelId> = node
		.list_eltoo_channels()
		.iter()
		.map(|details| details.channel_id)
		.filter(|id| hex(&id.0).starts_with(prefix))
		.collect();
	match matches.as_slice() {
		[id] => Some(*id),
		[] => {
			println!("no channel matches `{}`", prefix);
			None
		},
		_ => {
			println!("`{}` is ambiguous; give more of the id", prefix);
			None
		},
	}
}

/// The state number an update transaction is pinned to, per `TL(n)` in `nLockTime`.
fn state_of(tx: &Transaction) -> u64 {
	tx.lock_time.to_consensus_u32().saturating_sub(500_000_000) as u64
}

fn print_channel(details: &EltooChannelDetails) {
	println!("channel {}", hex(&details.channel_id.0));
	println!("  peer:     {}", details.counterparty_node_id);
	match details.funding_outpoint {
		Some(outpoint) => println!("  funding:  {}", outpoint),
		None => println!("  funding:  (not yet negotiated)"),
	}
	match details.state_number {
		Some(state) => println!(
			"  state:    {} | ours {} msat / theirs {} msat / {} HTLC(s) in flight",
			state, details.our_balance_msat, details.their_balance_msat, details.pending_htlcs
		),
		None => println!("  state:    (state 0 not yet signed)"),
	}
	println!(
		"  ready:    {} | on-chain: {}",
		details.is_ready,
		details.onchain_status.unwrap_or("(no monitor yet)")
	);
}

fn print_event(event: &EltooEvent) {
	match event {
		EltooEvent::ChannelReady { channel_id } => {
			println!("* channel ready: {}", hex(&channel_id.0));
		},
		EltooEvent::BroadcastTransaction { tx, anchor_outpoint } => {
			// Update spends carry [sig, script, control]; settlement covenant spends
			// only [script, control]; everything else (closings, claims) varies.
			let kind = if tx.lock_time.to_consensus_u32() >= 500_000_000 {
				match tx.input[0].witness.len() {
					3 => format!("update tx (state {})", state_of(tx)),
					2 => format!("settlement tx (state {}, no signature)", state_of(tx)),
					_ => format!("claim tx (locktime {})", tx.lock_time),
				}
			} else if tx.input[0].witness.len() == 1 {
				"closing tx (MuSig2 keyspend)".to_string()
			} else {
				"transaction".to_string()
			};
			println!("* broadcast {}: {}", kind, tx.compute_txid());
			if anchor_outpoint.is_some() {
				println!("  (zero-fee parent; fees attached via CPFP child on its P2A anchor)");
			}
		},
		EltooEvent::HtlcReceived {
			channel_id,
			htlc_id,
			payment_hash,
			amount_msat,
			cltv_expiry,
		} => {
			println!(
				"* HTLC received on {}: id {}, {} msat, hash {}, expiry {}",
				hex(&channel_id.0),
				htlc_id,
				amount_msat,
				hex(&payment_hash.0),
				cltv_expiry
			);
		},
		EltooEvent::PaymentClaimed { payment_hash, amount_msat } => {
			println!("* payment claimed: {} msat, hash {}", amount_msat, hex(&payment_hash.0));
		},
		EltooEvent::PaymentFulfilled { payment_hash } => {
			println!("* payment fulfilled: hash {}", hex(&payment_hash.0));
		},
		EltooEvent::PaymentFailed { payment_hash } => {
			println!("* payment FAILED: hash {}", hex(&payment_hash.0));
		},
		EltooEvent::PreimageLearnedOnchain { payment_hash, .. } => {
			println!("* preimage learned on-chain for hash {}", hex(&payment_hash.0));
		},
		EltooEvent::HtlcTimedOutOnchain { payment_hash } => {
			println!("* HTLC timed out on-chain: hash {}", hex(&payment_hash.0));
		},
	}
}
