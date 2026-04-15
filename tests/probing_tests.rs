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

mod common;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{
	expect_channel_ready_event, expect_event, generate_blocks_and_wait, open_channel,
	premine_and_distribute_funds, random_chain_source, random_config, setup_bitcoind_and_electrsd,
	setup_node, TestNode,
};

use ldk_node::bitcoin::Amount;
use ldk_node::probing::{ProbingConfigBuilder, ProbingStrategy};
use ldk_node::Event;

use lightning::routing::router::Path;

use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROBE_AMOUNT_MSAT: u64 = 1_000_000;
const PROBING_INTERVAL_MILLISECONDS: u64 = 100;

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
		ProbingConfigBuilder::custom(strategy.clone())
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
		ProbingConfigBuilder::custom(strategy.clone())
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

	let cleared = tokio::time::timeout(Duration::from_secs(180), async {
		loop {
			if node_a.prober().map_or(1, |p| p.locked_msat()) == 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(500)).await;
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
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
	})
	.await
	.is_ok();
	assert!(new_probe, "no new probe dispatched after budget was freed");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_c.stop().unwrap();
}
