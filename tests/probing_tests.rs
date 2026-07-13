// Integration tests for the probing service.
//
// Budget tests – linear A ──[1M sats]──▶ B ──[1M sats]──▶ C topology:
//
//   probe_budget_increments_and_decrements
//      Verifies locked_msat rises when a probe is dispatched and returns
//      to zero once the probe resolves.
//
//   locked_msat_accounts_for_routing_fees
//      Asserts the exact locked_msat (delivered amount + per-hop fee) for a single
//      in-flight probe, proving fees are tracked and not just the delivered amount.
//
//   exhausted_probe_budget_blocks_new_probes
//      Samples locked_msat across multiple probe cycles and asserts it never
//      exceeds the configured max_locked_msat budget cap.
//
//   probing_budget_restored_after_node_restart
//      Dispatches a probe, then stops node_b before the failure can propagate
//      back so the pending probe HTLC is preserved. Restarts node_a and asserts
//      the prober's locked_msat is rebuilt non-zero from list_recent_payments().

mod common;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{
	expect_channel_ready_event, expect_event, generate_blocks_and_wait, open_channel,
	premine_and_distribute_funds, random_chain_source, random_config, setup_bitcoind_and_electrsd,
	setup_node, wait_for_channel_ready_to_send, TestNode, TestStoreType,
};
use ldk_node::bitcoin::Amount;
use ldk_node::probing::{ProbingConfigBuilder, ProbingStrategy};
use ldk_node::Event;
use lightning::routing::router::Path;

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
		.find(|ch| ch.counterparty.node_id == node_b.node_id() && ch.short_channel_id.is_some())
		.expect("A→B channel not found");
	let ch_bc = node_b
		.list_channels()
		.into_iter()
		.find(|ch| ch.counterparty.node_id == node_c.node_id() && ch.short_channel_id.is_some())
		.expect("B→C channel not found");

	Path {
		hops: vec![
			RouteHop {
				pubkey: node_b.node_id(),
				node_features: NodeFeatures::empty(),
				short_channel_id: ch_ab.short_channel_id.unwrap(),
				channel_features: ChannelFeatures::empty(),
				fee_msat: 1000,
				cltv_expiry_delta: 144,
				maybe_announced_channel: true,
			},
			RouteHop {
				pubkey: node_c.node_id(),
				node_features: NodeFeatures::empty(),
				short_channel_id: ch_bc.short_channel_id.unwrap(),
				channel_features: ChannelFeatures::empty(),
				fee_msat: amount_msat,
				cltv_expiry_delta: 18,
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
	// First hop carries amount + per-hop fee; second hop carries just amount.
	wait_for_channel_ready_to_send(&node_a, &node_b, PROBE_AMOUNT_MSAT + 1000).await;
	wait_for_channel_ready_to_send(&node_b, &node_c, PROBE_AMOUNT_MSAT).await;
	strategy.start_probing();

	let went_up = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if node_a.prober().unwrap().locked_msat() > 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
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

/// Verifies that `locked_msat` accounts for routing fees, not just the delivered amount:
/// a probe along A→B→C locks `delivered amount + per-hop fee` on the first-hop channel.
///
/// The budget is sized to exactly one probe's worth, so at most one probe is in flight and
/// the observed `locked_msat` is deterministic. The existing budget test only checks that it
/// is non-zero; this asserts the precise value, which a fees-excluded accounting would miss.
#[tokio::test(flavor = "multi_thread")]
async fn locked_msat_accounts_for_routing_fees() {
	// First hop carries the delivered amount plus this per-hop fee (see `build_probe_path`).
	const FIRST_HOP_FEE_MSAT: u64 = 1000;
	const LOCKED_PER_PROBE_MSAT: u64 = PROBE_AMOUNT_MSAT + FIRST_HOP_FEE_MSAT;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let node_b = setup_node(&chain_source, random_config(false));
	let node_c = setup_node(&chain_source, random_config(false));

	let mut config_a = random_config(false);
	let strategy = FixedPathStrategy::new();
	config_a.probing = Some(
		ProbingConfigBuilder::custom(strategy.clone())
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			// Budget for exactly one in-flight probe so locked_msat is deterministic.
			.max_locked_msat(LOCKED_PER_PROBE_MSAT)
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

	strategy.set_path(build_probe_path(&node_a, &node_b, &node_c, PROBE_AMOUNT_MSAT));
	wait_for_channel_ready_to_send(&node_a, &node_b, LOCKED_PER_PROBE_MSAT).await;
	wait_for_channel_ready_to_send(&node_b, &node_c, PROBE_AMOUNT_MSAT).await;
	strategy.start_probing();

	// Capture locked_msat the moment the first probe goes in flight. With a single-probe
	// budget the value is only ever 0 or exactly one probe's worth, so the first non-zero
	// reading is the full first-hop HTLC.
	let locked = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let locked = node_a.prober().unwrap().locked_msat();
			if locked > 0 {
				break locked;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.expect("locked_msat never increased — no probe was dispatched");

	assert_eq!(
		locked, LOCKED_PER_PROBE_MSAT,
		"locked_msat must equal the delivered amount plus routing fees, not just the delivered amount"
	);

	strategy.stop_probing();
	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_c.stop().unwrap();
}

/// Verifies that `locked_msat` is restored after the node is stopped and restarted
/// while a probe is still in flight.
///
/// Race-sensitive: once a probe is dispatched, the failure round-trip
/// (`A→B→C → C fails back → B → A`) resolves it within milliseconds. To keep the
/// HTLC pending across the restart we observe `locked_msat > 0` and then *immediately*
/// call `node_a.disconnect(node_b)`, which closes A's socket to B in-process — much
/// faster than `node_b.stop()` — so any failure message from B is dropped before A
/// processes it. If the race is lost on a given probe (locked_msat drops back to 0
/// after the disconnect), we reconnect and let the next probe tick try again.
/// The pending Probe entry persists in `node_a`'s channel manager and must be
/// rebuilt by the prober's `locked_msat` on restart via `list_recent_payments()`.
#[tokio::test(flavor = "multi_thread")]
async fn probing_budget_restored_after_node_restart() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let node_b = setup_node(&chain_source, random_config(false));
	let node_c = setup_node(&chain_source, random_config(false));

	let mut config_a = random_config(false);
	// Use a pure on-disk store so state survives the restart.
	config_a.store_type = TestStoreType::Sqlite;
	let strategy = FixedPathStrategy::new();
	config_a.probing = Some(
		ProbingConfigBuilder::custom(strategy.clone())
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(10 * PROBE_AMOUNT_MSAT)
			.build(),
	);
	let restart_config = config_a.clone();
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

	strategy.set_path(build_probe_path(&node_a, &node_b, &node_c, PROBE_AMOUNT_MSAT));
	wait_for_channel_ready_to_send(&node_a, &node_b, PROBE_AMOUNT_MSAT + 1000).await;
	wait_for_channel_ready_to_send(&node_b, &node_c, PROBE_AMOUNT_MSAT).await;

	let node_b_id = node_b.node_id();
	let node_b_addr = node_b.listening_addresses().unwrap().into_iter().next().unwrap();

	strategy.start_probing();

	// Dispatch a probe and isolate node_a from node_b before the failure can
	// propagate back. Tight polling + in-process disconnect minimises the race
	// window; on a lost race we reconnect and let the prober's next tick try.
	let isolated = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if node_a.prober().unwrap().locked_msat() > 0 {
				node_a.disconnect(node_b_id).ok();
				if node_a.prober().unwrap().locked_msat() > 0 {
					return true;
				}
				node_a.connect(node_b_id, node_b_addr.clone(), false).ok();
			}
			tokio::time::sleep(Duration::from_millis(1)).await;
		}
	})
	.await
	.unwrap_or(false);
	assert!(isolated, "could not preserve in-flight probe long enough to restart");
	strategy.stop_probing();

	let locked_before = node_a.prober().unwrap().locked_msat();
	println!("Before restart: locked_msat = {}", locked_before);
	assert!(locked_before > 0, "probe resolved before we could isolate node_a — flaky timing");

	node_a.stop().unwrap();

	// Restart node_a from the same persisted state.
	let node_a = setup_node(&chain_source, restart_config);

	let locked_after = node_a.prober().unwrap().locked_msat();
	println!("After restart:  locked_msat = {}", locked_after);
	assert!(
		locked_after > 0,
		"locked_msat was not restored after restart (before={} after={})",
		locked_before,
		locked_after
	);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_c.stop().unwrap();
}

/// Verifies that `locked_msat` never exceeds `max_locked_msat` across multiple probe cycles.
#[tokio::test(flavor = "multi_thread")]
async fn exhausted_probe_budget_blocks_new_probes() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = random_chain_source(&bitcoind, &electrsd);

	let node_b = setup_node(&chain_source, random_config(false));
	let node_c = setup_node(&chain_source, random_config(false));

	let mut config_a = random_config(false);
	let strategy = FixedPathStrategy::new();
	let max_locked_msat = 2 * PROBE_AMOUNT_MSAT;
	config_a.probing = Some(
		ProbingConfigBuilder::custom(strategy.clone())
			.interval(Duration::from_millis(PROBING_INTERVAL_MILLISECONDS))
			.max_locked_msat(max_locked_msat)
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

	assert_eq!(node_a.prober().map_or(1, |p| p.locked_msat()), 0, "initial locked_msat is nonzero");

	strategy.set_path(build_probe_path(&node_a, &node_b, &node_c, PROBE_AMOUNT_MSAT));
	wait_for_channel_ready_to_send(&node_a, &node_b, PROBE_AMOUNT_MSAT + 1000).await;
	wait_for_channel_ready_to_send(&node_b, &node_c, PROBE_AMOUNT_MSAT).await;
	strategy.start_probing();

	// Sample locked_msat across multiple probe cycles and assert the budget cap is never exceeded
	let mut observed_locked = false;
	let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
	while tokio::time::Instant::now() < deadline {
		let msat = node_a.prober().map_or(0, |p| p.locked_msat());
		if msat > 0 {
			observed_locked = true;
		}
		assert!(
			msat <= max_locked_msat,
			"locked_msat {msat} exceeded budget cap {max_locked_msat}"
		);
		tokio::time::sleep(Duration::from_millis(25)).await;
	}

	assert!(observed_locked, "no probe was dispatched during the observation window");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_c.stop().unwrap();
}
