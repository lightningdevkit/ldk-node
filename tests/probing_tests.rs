// Integration tests for the probing service.
//
// Budget tests – linear A ──[1M sats]──▶ B ──[1M sats]──▶ C topology:
//
//   probe_budget_increments_and_decrements
//      Verifies locked_msat rises when a probe is dispatched and returns
//      to zero once the probe resolves.
//
//   exhausted_probe_budget_blocks_new_probes
//      Stops B mid-flight so the HTLC cannot resolve; confirms the budget
//      stays exhausted and no further probes are sent. After B restarts
//      the probe fails, the budget clears, and new probes resume.
//
// Strategy tests:
//
//   probing_strategies_perfomance
//      Brings up a random mesh of nodes, fires random-walk probes via
//      RandomStrategy and high-degree probes via HighDegreeStrategy, then
//      runs payment rounds and prints probing perfomance tables.

mod common;
use std::sync::atomic::{AtomicBool, Ordering};

use lightning::routing::gossip::NodeAlias;
use lightning_invoice::{Bolt11InvoiceDescription, Description};

use common::{
	expect_channel_ready_event, expect_event, generate_blocks_and_wait, open_channel,
	open_channel_no_wait, premine_and_distribute_funds, random_chain_source, random_config,
	setup_bitcoind_and_electrsd, setup_node, TestChainSource, TestNode,
};

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Amount;
use ldk_node::probing::{ProbingConfig, ProbingStrategy};
use ldk_node::Event;

use lightning::routing::router::Path;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROBE_AMOUNT_MSAT: u64 = 1_000_000;
const MAX_LOCKED_MSAT: u64 = 100_000_000;
const PROBING_INTERVAL_MILLISECONDS: u64 = 100;
const PROBING_DIVERSITY_PENALTY: u64 = 50_000;

/// FixedPathStrategy — returns a fixed pre-built path; used by budget tests.
///
/// The path is set after node and channel setup via [`set_path`].
struct FixedPathStrategy {
	path: Mutex<Option<Path>>,
	ready_to_probe: AtomicBool,
}

impl FixedPathStrategy {
	fn new() -> Arc<Self> {
		Arc::new(Self { path: Mutex::new(None), ready_to_probe: AtomicBool::new(false) })
	}

	fn set_path(&self, path: Path) {
		*self.path.lock().unwrap() = Some(path);
	}

	fn start_probing(&self) {
		self.ready_to_probe.store(true, Ordering::Relaxed);
	}

	fn stop_probing(&self) {
		self.ready_to_probe.store(false, Ordering::Relaxed);
	}
}

impl ProbingStrategy for FixedPathStrategy {
	fn next_probe(&self) -> Option<Path> {
		if self.ready_to_probe.load(Ordering::Relaxed) {
			self.path.lock().unwrap().clone()
		} else {
			None
		}
	}
}

/// Builds a 2-hop probe path: node_a → node_b → node_c using live channel info.
fn build_probe_path(
	node_a: &TestNode, node_b: &TestNode, node_c: &TestNode, amount_msat: u64,
) -> Path {
	use lightning::routing::router::RouteHop;
	use lightning_types::features::{ChannelFeatures, NodeFeatures};

	let ch_ab = node_a
		.list_channels()
		.into_iter()
		.find(|ch| ch.counterparty_node_id == node_b.node_id() && ch.short_channel_id.is_some())
		.expect("A→B channel not found");
	let ch_bc = node_b
		.list_channels()
		.into_iter()
		.find(|ch| ch.counterparty_node_id == node_c.node_id() && ch.short_channel_id.is_some())
		.expect("B→C channel not found");

	Path {
		hops: vec![
			RouteHop {
				pubkey: node_b.node_id(),
				node_features: NodeFeatures::empty(),
				short_channel_id: ch_ab.short_channel_id.unwrap(),
				channel_features: ChannelFeatures::empty(),
				fee_msat: 0,
				cltv_expiry_delta: 40,
				maybe_announced_channel: true,
			},
			RouteHop {
				pubkey: node_c.node_id(),
				node_features: NodeFeatures::empty(),
				short_channel_id: ch_bc.short_channel_id.unwrap(),
				channel_features: ChannelFeatures::empty(),
				fee_msat: amount_msat,
				cltv_expiry_delta: 0,
				maybe_announced_channel: true,
			},
		],
		blinded_tail: None,
	}
}

fn config_with_label(label: &str) -> common::TestConfig {
	let mut config = random_config(false);
	let mut alias_bytes = [0u8; 32];
	let b = label.as_bytes();
	alias_bytes[..b.len()].copy_from_slice(b);
	config.node_config.node_alias = Some(NodeAlias(alias_bytes));
	config
}

fn build_node_random_probing(chain_source: &TestChainSource<'_>, max_hops: usize) -> TestNode {
	let mut config = config_with_label("Random");
	config.probing = Some(
		ProbingConfig::random_walk(max_hops)
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(MAX_LOCKED_MSAT)
			.build(),
	);
	setup_node(chain_source, config)
}

fn build_node_highdegree_probing(
	chain_source: &TestChainSource<'_>, top_node_count: usize,
) -> TestNode {
	let mut config = config_with_label("HiDeg");
	config.probing = Some(
		ProbingConfig::high_degree(top_node_count)
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(MAX_LOCKED_MSAT)
			.build(),
	);
	setup_node(chain_source, config)
}

fn build_node_z_highdegree_probing(
	chain_source: &TestChainSource<'_>, top_node_count: usize, diversity_penalty: u64,
) -> TestNode {
	let mut config = config_with_label("HiDeg+P");
	config.probing = Some(
		ProbingConfig::high_degree(top_node_count)
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(MAX_LOCKED_MSAT)
			.diversity_penalty_msat(diversity_penalty)
			.build(),
	);
	setup_node(chain_source, config)
}

// helpers, formatting
fn node_label(node: &TestNode) -> String {
	node.node_alias()
		.map(|alias| {
			let end = alias.0.iter().position(|&b| b == 0).unwrap_or(32);
			String::from_utf8_lossy(&alias.0[..end]).to_string()
		})
		.unwrap_or_else(|| format!("{:.8}", node.node_id()))
}

fn print_topology(all_nodes: &[&TestNode]) {
	let labels: HashMap<PublicKey, String> =
		all_nodes.iter().map(|n| (n.node_id(), node_label(n))).collect();
	let label_of = |pk: PublicKey| labels.get(&pk).cloned().unwrap_or_else(|| format!("{:.8}", pk));

	let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
	for node in all_nodes {
		let local = label_of(node.node_id());
		let mut peers: Vec<String> = node
			.list_channels()
			.into_iter()
			.filter(|ch| ch.short_channel_id.is_some())
			.map(|ch| label_of(ch.counterparty_node_id))
			.collect();
		peers.sort();
		peers.dedup();
		adjacency.entry(local).or_default().extend(peers);
	}

	println!("\n=== Topology ===");
	for (node, peers) in &adjacency {
		println!("  {node} ── {}", peers.join(", "));
	}
}

const LABEL_MAX: usize = 8;
const DIR_W: usize = LABEL_MAX * 2 + 1;
const SCORER_W: usize = 28;

fn thousands(n: u64) -> String {
	let s = n.to_string();
	let mut out = String::with_capacity(s.len() + s.len() / 3);
	for (i, c) in s.chars().rev().enumerate() {
		if i > 0 && i % 3 == 0 {
			out.push(' ');
		}
		out.push(c);
	}
	out.chars().rev().collect()
}

fn short_label(label: &str) -> String {
	label.chars().take(LABEL_MAX).collect()
}

fn fmt_est(est: Option<(u64, u64)>) -> String {
	match est {
		Some((lo, hi)) => format!("[{}, {}]", thousands(lo), thousands(hi)),
		None => "unknown".into(),
	}
}

fn print_probing_perfomance(observers: &[&TestNode], all_nodes: &[&TestNode]) {
	let labels: HashMap<PublicKey, String> =
		all_nodes.iter().chain(observers.iter()).map(|n| (n.node_id(), node_label(n))).collect();
	let label_of = |pk: PublicKey| {
		short_label(&labels.get(&pk).cloned().unwrap_or_else(|| format!("{:.8}", pk)))
	};

	let mut by_scid: BTreeMap<u64, Vec<(PublicKey, PublicKey, u64)>> = BTreeMap::new();
	for node in all_nodes {
		let local_pk = node.node_id();
		for ch in node.list_channels() {
			if let Some(scid) = ch.short_channel_id {
				by_scid.entry(scid).or_default().push((
					local_pk,
					ch.counterparty_node_id,
					ch.outbound_capacity_msat,
				));
			}
		}
	}

	print!("\n{:<15} {:<width$}", "SCID", "Direction", width = DIR_W);
	for obs in observers {
		print!(
			" {:<width$}",
			format!("Probing {}", short_label(&node_label(obs))),
			width = SCORER_W
		);
	}
	println!(" Real outbound msat");
	println!("{}", "-".repeat(15 + 1 + DIR_W + observers.len() * (SCORER_W + 1) + 20));

	let total_dirs: usize = by_scid.values().map(|v| v.len()).sum();
	let mut known_counts = vec![0usize; observers.len()];

	for (scid, entries) in &by_scid {
		for (from_pk, to_pk, outbound_msat) in entries {
			let dir = format!("{}→{}", label_of(*from_pk), label_of(*to_pk));
			print!("{:<15} {:<width$}", scid, dir, width = DIR_W);
			for (i, obs) in observers.iter().enumerate() {
				let est = obs.scorer_channel_liquidity(*scid, *to_pk);
				if est.is_some() {
					known_counts[i] += 1;
				}
				print!(" {:<width$}", fmt_est(est), width = SCORER_W);
			}
			println!(" {}", thousands(*outbound_msat));
		}
	}

	println!("{}", "-".repeat(15 + 1 + DIR_W + observers.len() * (SCORER_W + 1) + 20));
	print!("{:<15} {:<width$}", "Known directions", "", width = DIR_W);
	for count in &known_counts {
		print!(" {:<width$}", format!("{}/{}", count, total_dirs), width = SCORER_W);
	}
	println!();
}

/// Verifies that `locked_msat` increases when a probe is dispatched and returns
/// to zero once the probe resolves (succeeds or fails).
#[tokio::test(flavor = "multi_thread")]
async fn probe_budget_increments_and_decrements() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let node_b = setup_node(&chain_source, random_config(false));
	let node_c = setup_node(&chain_source, random_config(false));

	let mut config_a = random_config(false);
	let strategy = FixedPathStrategy::new();
	config_a.probing = Some(
		ProbingConfig::custom(strategy.clone())
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(10 * PROBE_AMOUNT_MSAT)
			.build(),
	);
	let node_a = setup_node(&chain_source, config_a);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(2_000_000),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_a, &node_b, 1_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node_b.sync_wallets().unwrap();
	open_channel(&node_b, &node_c, 1_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	node_c.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_event!(node_b, ChannelReady);
	expect_event!(node_b, ChannelReady);
	expect_event!(node_c, ChannelReady);

	// Build the probe path now that channels are ready, then enable probing.
	strategy.set_path(build_probe_path(&node_a, &node_b, &node_c, PROBE_AMOUNT_MSAT));
	tokio::time::sleep(Duration::from_secs(3)).await;
	strategy.start_probing();

	let went_up = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if node_a.prober().unwrap().locked_msat() > 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(1)).await;
		}
	})
	.await
	.is_ok();
	assert!(went_up, "locked_msat never increased — no probe was dispatched");
	println!("First probe dispatched; locked_msat = {}", node_a.prober().unwrap().locked_msat());

	strategy.stop_probing();
	let cleared = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if node_a.prober().unwrap().locked_msat() == 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.is_ok();
	assert!(cleared, "locked_msat never returned to zero after probe resolved");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_c.stop().unwrap();
}

/// Verifies that no new probes are dispatched once the in-flight budget is exhausted.
///
/// Exhaustion is triggered by stopping the intermediate node (B) while a probe HTLC
/// is in-flight, preventing resolution and keeping the budget locked. After B restarts
/// the HTLC fails, the budget clears, and probing resumes.
#[tokio::test(flavor = "multi_thread")]
async fn exhausted_probe_budget_blocks_new_probes() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let node_b = setup_node(&chain_source, random_config(false));
	let node_c = setup_node(&chain_source, random_config(false));

	let mut config_a = random_config(false);
	let strategy = FixedPathStrategy::new();
	config_a.probing = Some(
		ProbingConfig::custom(strategy.clone())
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(10 * PROBE_AMOUNT_MSAT)
			.build(),
	);
	let node_a = setup_node(&chain_source, config_a);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(2_000_000),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_a, &node_b, 1_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node_b.sync_wallets().unwrap();
	open_channel(&node_b, &node_c, 1_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	node_c.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_event!(node_b, ChannelReady);
	expect_event!(node_b, ChannelReady);
	expect_event!(node_c, ChannelReady);

	let capacity_at_open = node_a
		.list_channels()
		.iter()
		.find(|ch| ch.counterparty_node_id == node_b.node_id())
		.map(|ch| ch.outbound_capacity_msat)
		.expect("A→B channel not found");

	assert_eq!(node_a.prober().map_or(1, |p| p.locked_msat()), 0, "initial locked_msat is nonzero");

	strategy.set_path(build_probe_path(&node_a, &node_b, &node_c, PROBE_AMOUNT_MSAT));
	tokio::time::sleep(Duration::from_secs(3)).await;
	strategy.start_probing();

	// Wait for the first probe to be in-flight.
	let locked = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if node_a.prober().map_or(0, |p| p.locked_msat()) > 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(1)).await;
		}
	})
	.await
	.is_ok();
	assert!(locked, "no probe dispatched within 30 s");

	// Capacity should have decreased due to the in-flight probe HTLC.
	let capacity_with_probe = node_a
		.list_channels()
		.iter()
		.find(|ch| ch.counterparty_node_id == node_b.node_id())
		.map(|ch| ch.outbound_capacity_msat)
		.expect("A→B channel not found");
	assert!(
		capacity_with_probe < capacity_at_open,
		"HTLC not visible in channel state: capacity unchanged ({capacity_at_open} msat)"
	);

	// Stop B while the probe HTLC is in-flight.
	node_b.stop().unwrap();

	tokio::time::sleep(Duration::from_secs(5)).await;
	assert!(
		node_a.prober().map_or(0, |p| p.locked_msat()) > 0,
		"probe resolved unexpectedly while B was offline"
	);
	let capacity_after_wait = node_a
		.list_channels()
		.iter()
		.find(|ch| ch.counterparty_node_id == node_b.node_id())
		.map(|ch| ch.outbound_capacity_msat)
		.unwrap_or(u64::MAX);
	assert!(
		capacity_after_wait >= capacity_with_probe,
		"a new probe HTLC was sent despite budget being exhausted"
	);

	// Pause probing so the budget can clear without a new probe re-locking it.
	strategy.stop_probing();

	// Bring B back and explicitly reconnect to A and C so the stuck HTLC resolves
	// without waiting for the background reconnection backoff.
	node_b.start().unwrap();
	let node_a_addr = node_a.listening_addresses().unwrap().first().unwrap().clone();
	let node_c_addr = node_c.listening_addresses().unwrap().first().unwrap().clone();
	node_b.connect(node_a.node_id(), node_a_addr, false).unwrap();
	node_b.connect(node_c.node_id(), node_c_addr, false).unwrap();

	let cleared = tokio::time::timeout(Duration::from_secs(150), async {
		loop {
			if node_a.prober().map_or(1, |p| p.locked_msat()) == 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.is_ok();
	assert!(cleared, "locked_msat never cleared after B came back online");

	// Re-enable probing; a new probe should be dispatched within a few ticks.
	strategy.start_probing();
	let new_probe = tokio::time::timeout(Duration::from_secs(60), async {
		loop {
			if node_a.prober().map_or(0, |p| p.locked_msat()) > 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.is_ok();
	assert!(new_probe, "no new probe dispatched after budget was freed");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_c.stop().unwrap();
}

/// Builds a random mesh of nodes, runs `RandomStrategy` and `HighDegreeStrategy`
/// probers alongside payment rounds, then prints scorer liquidity estimates to
/// compare probing coverage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probing_strategies_perfomance() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let num_nodes = 5;
	let channel_capacity_sat = 1_000_000u64;
	// Each observer opens 1 channel; regular nodes open at most (num_nodes-1) each.
	// num_nodes UTXOs per node is a safe upper bound for funding.
	let utxos_per_node = num_nodes;
	let utxo_per_channel = Amount::from_sat(channel_capacity_sat + 50_000);

	let mut nodes: Vec<TestNode> = Vec::new();
	for i in 0..num_nodes {
		let label = char::from(b'B' + i as u8).to_string();
		let mut config = random_config(false);
		let mut alias_bytes = [0u8; 32];
		alias_bytes[..label.as_bytes().len()].copy_from_slice(label.as_bytes());
		config.node_config.node_alias = Some(NodeAlias(alias_bytes));
		nodes.push(setup_node(&chain_source, config));
	}
	let node_a = build_node_random_probing(&chain_source, 4);
	let node_x = setup_node(&chain_source, config_with_label("nostrat"));
	let node_y = build_node_highdegree_probing(&chain_source, 4);
	let node_z = build_node_z_highdegree_probing(&chain_source, 4, PROBING_DIVERSITY_PENALTY);

	let seed = std::env::var("TEST_SEED")
		.ok()
		.and_then(|s| s.parse::<u64>().ok())
		.unwrap_or_else(|| rand::rng().random());
	println!("RNG seed: {seed}  (re-run with TEST_SEED={seed} to reproduce)");
	let mut rng = StdRng::seed_from_u64(seed);
	let channels_per_node = rng.random_range(1..=num_nodes - 1);
	let channels_per_nodes: Vec<usize> =
		(0..num_nodes).map(|_| rng.random_range(1..=channels_per_node)).collect();

	let observer_nodes: [&TestNode; 4] = [&node_a, &node_y, &node_z, &node_x];

	let mut addresses = Vec::new();
	for node in observer_nodes {
		for _ in 0..utxos_per_node {
			addresses.push(node.onchain_payment().new_address().unwrap());
		}
	}
	for node in &nodes {
		for _ in 0..utxos_per_node {
			addresses.push(node.onchain_payment().new_address().unwrap());
		}
	}

	premine_and_distribute_funds(&bitcoind.client, &electrsd.client, addresses, utxo_per_channel)
		.await;

	println!("distributed initial sats");
	for node in nodes.iter().chain(observer_nodes) {
		node.sync_wallets().unwrap();
	}

	fn drain_events(node: &TestNode) {
		while let Some(_) = node.next_event() {
			node.event_handled().unwrap();
		}
	}

	println!("opening channels");
	for node in observer_nodes {
		let idx = rng.random_range(0..num_nodes);
		open_channel_no_wait(node, &nodes[idx], channel_capacity_sat, None, true).await;
	}
	for (i, &count) in channels_per_nodes.iter().enumerate() {
		let targets: Vec<usize> = (0..num_nodes).filter(|&j| j != i).take(count).collect();
		for j in targets {
			open_channel_no_wait(&nodes[i], &nodes[j], channel_capacity_sat, None, true).await;
		}
	}

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	for node in nodes.iter().chain(observer_nodes) {
		node.sync_wallets().unwrap();
	}
	for node in nodes.iter().chain(observer_nodes) {
		drain_events(node);
	}

	tokio::time::sleep(Duration::from_secs(3)).await;

	let mut node_map = HashMap::new();
	for (i, node) in nodes.iter().enumerate() {
		node_map.insert(node.node_id(), i);
	}

	let all_nodes: Vec<&TestNode> = nodes.iter().chain(observer_nodes).collect();

	print_topology(&all_nodes);

	println!("\n=== before random payments ===");
	print_probing_perfomance(&observer_nodes, &all_nodes);

	let desc = Bolt11InvoiceDescription::Direct(Description::new("test".to_string()).unwrap());
	for round in 0..10 {
		let mut sent = 0u32;
		for sender_idx in 0..num_nodes {
			let channels: Vec<_> = nodes[sender_idx]
				.list_channels()
				.into_iter()
				.filter(|ch| ch.is_channel_ready && ch.outbound_capacity_msat > 1_000)
				.collect();
			if channels.is_empty() {
				continue;
			}
			let ch = &channels[rng.random_range(0..channels.len())];
			let amount_msat = rng.random_range(1_000..=ch.outbound_capacity_msat.min(100_000_000));
			if let Some(&receiver_idx) = node_map.get(&ch.counterparty_node_id) {
				let invoice = nodes[receiver_idx]
					.bolt11_payment()
					.receive(amount_msat, &desc.clone().into(), 3600)
					.unwrap();
				if nodes[sender_idx].bolt11_payment().send(&invoice, None).is_ok() {
					sent += 1;
				}
			}
		}
		println!("round {round}: sent {sent} payments");
		tokio::time::sleep(Duration::from_millis(500)).await;
		for node in nodes.iter().chain(observer_nodes) {
			drain_events(node);
		}
	}

	tokio::time::sleep(Duration::from_secs(5)).await;
	println!("\n=== after random payments ===");
	print_probing_perfomance(&observer_nodes, &all_nodes);

	for node in nodes.iter().chain(observer_nodes) {
		node.stop().unwrap();
	}
}
