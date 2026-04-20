// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Background probing strategies for training the payment scorer.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "uniffi")]
use std::sync::RwLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitcoin::secp256k1::PublicKey;
use lightning::routing::gossip::NodeId;
use lightning::routing::router::{
	Path, PaymentParameters, RouteHop, RouteParameters, MAX_PATH_LENGTH_ESTIMATE,
};
use lightning_invoice::DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning_types::features::{ChannelFeatures, NodeFeatures};

use crate::config::{
	DEFAULT_MAX_PROBE_LOCKED_MSAT, DEFAULT_PROBED_NODE_COOLDOWN_SECS, DEFAULT_PROBING_INTERVAL_SECS,
};
use crate::logger::{log_debug, LdkLogger, Logger};
use crate::types::{ChannelManager, Graph, Router};
use crate::util::random_range;

use lightning::routing::router::Router as LdkRouter;

/// Which built-in probing strategy to use, or a custom one.
#[derive(Clone)]
pub(crate) enum ProbingStrategyKind {
	HighDegree { top_node_count: usize },
	Random { max_hops: usize },
	Custom(Arc<dyn ProbingStrategy>),
}

/// Configuration for the background probing subsystem.
///
/// Construct via [`ProbingConfigBuilder`]. Pick a strategy with
/// [`ProbingConfigBuilder::high_degree`], [`ProbingConfigBuilder::random_walk`], or
/// [`ProbingConfigBuilder::custom`], chain optional setters, and finalize with
/// [`ProbingConfigBuilder::build`].
///
/// # Example
/// ```ignore
/// let config = ProbingConfigBuilder::high_degree(100)
///     .interval(Duration::from_secs(30))
///     .max_locked_msat(500_000)
///     .diversity_penalty_msat(250)
///     .build();
/// builder.set_probing_config(config);
/// ```
#[derive(Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct ProbingConfig {
	pub(crate) kind: ProbingStrategyKind,
	pub(crate) interval: Duration,
	pub(crate) max_locked_msat: u64,
	pub(crate) diversity_penalty_msat: Option<u64>,
	pub(crate) cooldown: Duration,
}

impl fmt::Debug for ProbingConfig {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let kind_str = match &self.kind {
			ProbingStrategyKind::HighDegree { top_node_count } => {
				format!("HighDegree {{ top_node_count: {} }}", top_node_count)
			},
			ProbingStrategyKind::Random { max_hops } => {
				format!("Random {{ max_hops: {} }}", max_hops)
			},
			ProbingStrategyKind::Custom(_) => "Custom(<probing strategy>)".to_string(),
		};
		f.debug_struct("ProbingConfig")
			.field("kind", &kind_str)
			.field("interval", &self.interval)
			.field("max_locked_msat", &self.max_locked_msat)
			.field("diversity_penalty_msat", &self.diversity_penalty_msat)
			.field("cooldown", &self.cooldown)
			.finish()
	}
}

/// Builder for [`ProbingConfig`].
///
/// Pick a strategy with [`high_degree`], [`random_walk`], or [`custom`], chain optional
/// setters, and call [`build`] to finalize.
///
/// [`high_degree`]: Self::high_degree
/// [`random_walk`]: Self::random_walk
/// [`custom`]: Self::custom
/// [`build`]: Self::build
pub struct ProbingConfigBuilder {
	kind: ProbingStrategyKind,
	interval: Duration,
	max_locked_msat: u64,
	diversity_penalty_msat: Option<u64>,
	cooldown: Duration,
}

impl ProbingConfigBuilder {
	fn with_kind(kind: ProbingStrategyKind) -> Self {
		Self {
			kind,
			interval: Duration::from_secs(DEFAULT_PROBING_INTERVAL_SECS),
			max_locked_msat: DEFAULT_MAX_PROBE_LOCKED_MSAT,
			diversity_penalty_msat: None,
			cooldown: Duration::from_secs(DEFAULT_PROBED_NODE_COOLDOWN_SECS),
		}
	}

	/// Start building a config that probes toward the highest-degree nodes in the graph.
	///
	/// `top_node_count` controls how many of the most-connected nodes are cycled through.
	pub fn high_degree(top_node_count: usize) -> Self {
		Self::with_kind(ProbingStrategyKind::HighDegree { top_node_count })
	}

	/// Start building a config that probes via random graph walks.
	///
	/// `max_hops` is the upper bound on the number of hops in a randomly constructed path.
	pub fn random_walk(max_hops: usize) -> Self {
		Self::with_kind(ProbingStrategyKind::Random { max_hops })
	}

	/// Start building a config with a custom [`ProbingStrategy`] implementation.
	pub fn custom(strategy: Arc<dyn ProbingStrategy>) -> Self {
		Self::with_kind(ProbingStrategyKind::Custom(strategy))
	}

	/// Overrides the interval between probe attempts.
	///
	/// Defaults to 10 seconds.
	pub fn interval(&mut self, interval: Duration) -> &mut Self {
		self.interval = interval;
		self
	}

	/// Overrides the maximum millisatoshis that may be locked in in-flight probes at any time.
	///
	/// Defaults to 100 000 000 msat (100k sats).
	pub fn max_locked_msat(&mut self, max_msat: u64) -> &mut Self {
		self.max_locked_msat = max_msat;
		self
	}

	/// Sets the probing diversity penalty applied by the probabilistic scorer.
	///
	/// When set, the scorer will penalize channels that have been recently probed,
	/// encouraging path diversity during background probing. The penalty decays
	/// quadratically over 24 hours.
	///
	/// This is only useful for probing strategies that route through the scorer
	/// (e.g., [`HighDegreeStrategy`]). Strategies that build paths manually
	/// (e.g., [`RandomStrategy`]) bypass the scorer entirely.
	///
	/// If unset, LDK's default of `0` (no penalty) is used.
	pub fn diversity_penalty_msat(&mut self, penalty_msat: u64) -> &mut Self {
		self.diversity_penalty_msat = Some(penalty_msat);
		self
	}

	/// Sets how long a probed node stays ineligible before being probed again.
	///
	/// Only applies to [`HighDegreeStrategy`]. Defaults to 1 hour.
	pub fn cooldown(&mut self, cooldown: Duration) -> &mut Self {
		self.cooldown = cooldown;
		self
	}

	/// Builds the [`ProbingConfig`].
	pub fn build(&self) -> ProbingConfig {
		ProbingConfig {
			kind: self.kind.clone(),
			interval: self.interval,
			max_locked_msat: self.max_locked_msat,
			diversity_penalty_msat: self.diversity_penalty_msat,
			cooldown: self.cooldown,
		}
	}
}

/// A UniFFI-compatible wrapper around [`ProbingConfigBuilder`] that uses interior mutability
/// so it can be shared behind an `Arc` as required by the FFI object model.
///
/// Obtain one via the constructors [`new_high_degree`] or [`new_random_walk`], configure it
/// with the `set_*` methods, then call [`build`] to produce a [`ProbingConfig`].
///
/// [`new_high_degree`]: Self::new_high_degree
/// [`new_random_walk`]: Self::new_random_walk
/// [`build`]: Self::build
#[cfg(feature = "uniffi")]
#[derive(uniffi::Object)]
pub struct ArcedProbingConfigBuilder {
	inner: RwLock<ProbingConfigBuilder>,
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl ArcedProbingConfigBuilder {
	/// Creates a builder configured to probe toward the highest-degree nodes in the graph.
	///
	/// `top_node_count` controls how many of the most-connected nodes are cycled through.
	#[uniffi::constructor]
	pub fn new_high_degree(top_node_count: u64) -> Arc<Self> {
		Arc::new(Self {
			inner: RwLock::new(ProbingConfigBuilder::high_degree(top_node_count as usize)),
		})
	}

	/// Creates a builder configured to probe via random graph walks.
	///
	/// `max_hops` is the upper bound on the number of hops in a randomly constructed path.
	#[uniffi::constructor]
	pub fn new_random_walk(max_hops: u64) -> Arc<Self> {
		Arc::new(Self { inner: RwLock::new(ProbingConfigBuilder::random_walk(max_hops as usize)) })
	}

	/// Overrides the interval between probe attempts. Defaults to 10 seconds.
	pub fn set_interval(&self, secs: u64) {
		self.inner.write().unwrap().interval(Duration::from_secs(secs));
	}

	/// Overrides the maximum millisatoshis that may be locked in in-flight probes at any time.
	///
	/// Defaults to 100 000 000 msat (100k sats).
	pub fn set_max_locked_msat(&self, max_msat: u64) {
		self.inner.write().unwrap().max_locked_msat(max_msat);
	}

	/// Sets the probing diversity penalty applied by the probabilistic scorer.
	///
	/// When set, the scorer will penalize channels that have been recently probed,
	/// encouraging path diversity during background probing. The penalty decays
	/// quadratically over 24 hours.
	///
	/// If unset, LDK's default of `0` (no penalty) is used.
	pub fn set_diversity_penalty_msat(&self, penalty_msat: u64) {
		self.inner.write().unwrap().diversity_penalty_msat(penalty_msat);
	}

	/// Sets how long a probed node stays ineligible before being probed again.
	///
	/// Only applies to the high-degree strategy. Defaults to 1 hour.
	pub fn set_cooldown(&self, secs: u64) {
		self.inner.write().unwrap().cooldown(Duration::from_secs(secs));
	}

	/// Builds the [`ProbingConfig`].
	pub fn build(&self) -> Arc<ProbingConfig> {
		Arc::new(self.inner.read().unwrap().build())
	}
}

/// Strategy can be used for determining the next target and amount for probing.
pub trait ProbingStrategy: Send + Sync + 'static {
	/// Returns the next probe path to run, or `None` to skip this tick.
	fn next_probe(&self) -> Option<Path>;
}

/// Probes toward the most-connected nodes in the graph.
///
/// On each tick the strategy reads the current gossip graph, sorts nodes by
/// channel count, and picks the highest-degree node from the top
/// `top_node_count` that has not been probed within `cooldown`.
/// Nodes probed more recently are skipped so that the strategy
/// naturally spreads across the top nodes and picks up graph changes.
/// If all top nodes are on cooldown, the cooldown map is cleared and a new cycle begins
/// immediately.
///
/// The probe amount is chosen uniformly at random from
/// `[min_amount_msat, max_amount_msat]`.
pub struct HighDegreeStrategy {
	network_graph: Arc<Graph>,
	channel_manager: Arc<ChannelManager>,
	router: Arc<Router>,
	/// How many of the highest-degree nodes to cycle through.
	pub top_node_count: usize,
	/// Lower bound for the randomly chosen probe amount.
	pub min_amount_msat: u64,
	/// Upper bound for the randomly chosen probe amount.
	pub max_amount_msat: u64,
	/// How long a node stays ineligible after being probed.
	pub cooldown: Duration,
	/// Skip a path when the first-hop outbound liquidity is less than
	/// `path_value * liquidity_limit_multiplier`.
	pub liquidity_limit_multiplier: u64,
	/// Nodes probed recently, with the time they were last probed.
	recently_probed: Mutex<HashMap<PublicKey, Instant>>,
}

impl HighDegreeStrategy {
	/// Creates a new high-degree probing strategy.
	pub(crate) fn new(
		network_graph: Arc<Graph>, channel_manager: Arc<ChannelManager>, router: Arc<Router>,
		top_node_count: usize, min_amount_msat: u64, max_amount_msat: u64, cooldown: Duration,
		liquidity_limit_multiplier: u64,
	) -> Self {
		assert!(
			min_amount_msat <= max_amount_msat,
			"min_amount_msat must not exceed max_amount_msat"
		);
		Self {
			network_graph,
			channel_manager,
			router,
			top_node_count,
			min_amount_msat,
			max_amount_msat,
			cooldown,
			liquidity_limit_multiplier,
			recently_probed: Mutex::new(HashMap::new()),
		}
	}
}

impl ProbingStrategy for HighDegreeStrategy {
	fn next_probe(&self) -> Option<Path> {
		let graph = self.network_graph.read_only();

		let mut nodes_by_degree: Vec<(PublicKey, usize)> = graph
			.nodes()
			.unordered_iter()
			.filter_map(|(id, info)| {
				PublicKey::try_from(*id).ok().map(|pubkey| (pubkey, info.channels.len()))
			})
			.collect();

		if nodes_by_degree.is_empty() {
			return None;
		}

		nodes_by_degree.sort_unstable_by(|a, b| b.1.cmp(&a.1));

		let top_node_count = self.top_node_count.min(nodes_by_degree.len());
		let now = Instant::now();

		let mut probed = self.recently_probed.lock().unwrap_or_else(|e| e.into_inner());

		// We could check staleness when we use the entry, but that way we'd not clear cache at
		// all. For hundreds of top nodes it's okay to call retain each tick.
		probed.retain(|_, probed_at| now.duration_since(*probed_at) < self.cooldown);

		// If all top nodes are on cooldown, reset and start a new cycle.
		let final_node = match nodes_by_degree[..top_node_count]
			.iter()
			.find(|(pubkey, _)| !probed.contains_key(pubkey))
		{
			Some((pubkey, _)) => *pubkey,
			None => {
				probed.clear();
				nodes_by_degree[0].0
			},
		};

		probed.insert(final_node, now);
		drop(probed);
		drop(graph);

		let amount_msat = random_range(self.min_amount_msat, self.max_amount_msat);
		let payment_params =
			PaymentParameters::from_node_id(final_node, DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA as u32);
		let route_params =
			RouteParameters::from_payment_params_and_value(payment_params, amount_msat);

		let payer = self.channel_manager.get_our_node_id();
		let usable_channels = self.channel_manager.list_usable_channels();
		let first_hops: Vec<&_> = usable_channels.iter().collect();
		let inflight_htlcs = self.channel_manager.compute_inflight_htlcs();

		let route = self
			.router
			.find_route(&payer, &route_params, Some(&first_hops), inflight_htlcs)
			.ok()?;

		let path = route.paths.into_iter().next()?;

		// Liquidity-limit check (mirrors send_preflight_probes): skip the path when the
		// first-hop outbound liquidity is less than path_value * liquidity_limit_multiplier.
		if let Some(first_hop_hop) = path.hops.first() {
			if let Some(ch) = usable_channels
				.iter()
				.find(|h| h.get_outbound_payment_scid() == Some(first_hop_hop.short_channel_id))
			{
				let path_value = path.final_value_msat() + path.fee_msat();
				if ch.next_outbound_htlc_limit_msat
					< path_value.saturating_mul(self.liquidity_limit_multiplier)
				{
					return None;
				}
			}
		}

		Some(path)
	}
}

/// Explores the graph by walking a random number of hops outward from one of our own
/// channels, constructing the [`Path`] explicitly.
///
/// On each tick:
///   1. Picks one of our confirmed, usable channels to start from.
///   2. Performs a deterministic walk of a randomly chosen depth (up to
///      [`MAX_PATH_LENGTH_ESTIMATE`]) through the gossip graph, skipping disabled
///      channels and dead-ends.
///   3. Returns the constructed `Path` so the prober calls `send_probe` directly.
///
/// The probe amount is chosen uniformly at random from `[min_amount_msat, max_amount_msat]`.
///
/// Because path selection ignores the scorer, this probes channels the router
/// would never try on its own, teaching the scorer about previously unknown paths.
pub struct RandomStrategy {
	network_graph: Arc<Graph>,
	channel_manager: Arc<ChannelManager>,
	/// Upper bound on the number of hops in a randomly constructed path.
	pub max_hops: usize,
	/// Lower bound for the randomly chosen probe amount.
	pub min_amount_msat: u64,
	/// Upper bound for the randomly chosen probe amount.
	pub max_amount_msat: u64,
}

impl RandomStrategy {
	/// Creates a new random-walk probing strategy.
	pub(crate) fn new(
		network_graph: Arc<Graph>, channel_manager: Arc<ChannelManager>, max_hops: usize,
		min_amount_msat: u64, max_amount_msat: u64,
	) -> Self {
		assert!(
			min_amount_msat <= max_amount_msat,
			"min_amount_msat must not exceed max_amount_msat"
		);
		Self {
			network_graph,
			channel_manager,
			max_hops: max_hops.clamp(1, MAX_PATH_LENGTH_ESTIMATE as usize),
			min_amount_msat,
			max_amount_msat,
		}
	}

	/// Tries to build a path of `target_hops` hops. Returns `None` if the local node has no
	/// usable channels, or the walk terminates before reaching `target_hops`.
	fn try_build_path(&self, target_hops: usize, amount_msat: u64) -> Option<Path> {
		let initial_channels = self
			.channel_manager
			.list_channels()
			.into_iter()
			.filter(|c| c.is_usable && c.short_channel_id.is_some())
			.collect::<Vec<_>>();

		if initial_channels.is_empty() {
			return None;
		}

		let graph = self.network_graph.read_only();
		let first_hop =
			&initial_channels[random_range(0, initial_channels.len() as u64 - 1) as usize];
		let first_hop_scid = first_hop.short_channel_id?;
		let next_peer_pubkey = first_hop.counterparty.node_id;
		let next_peer_node_id = NodeId::from_pubkey(&next_peer_pubkey);

		// Track the tightest HTLC limit across all hops to cap the probe amount.
		// The first hop limit comes from our live channel state; subsequent hops use htlc_maximum_msat from the gossip channel update.
		let mut route_least_htlc_upper_bound = first_hop.next_outbound_htlc_limit_msat;
		let mut route_greatest_htlc_lower_bound = first_hop.next_outbound_htlc_minimum_msat;

		// Walk the graph: each entry is (node_id, arrived_via_scid, pubkey); first entry is set:
		let mut route: Vec<(NodeId, u64, PublicKey)> =
			vec![(next_peer_node_id, first_hop_scid, next_peer_pubkey)];

		let mut prev_scid = first_hop_scid;
		let mut current_node_id = next_peer_node_id;

		for _ in 1..target_hops {
			let node_info = match graph.node(&current_node_id) {
				Some(n) => n,
				None => break,
			};

			// Outward channels: skip the one we arrived on to avoid backtracking.
			let candidates: Vec<u64> =
				node_info.channels.iter().copied().filter(|&scid| scid != prev_scid).collect();

			if candidates.is_empty() {
				break;
			}

			let next_scid = candidates[random_range(0, candidates.len() as u64 - 1) as usize];
			let next_channel = match graph.channel(next_scid) {
				Some(c) => c,
				None => break,
			};

			// as_directed_from validates that current_node_id is a channel endpoint and that
			// both direction updates are present; effective_capacity covers both htlc_maximum_msat
			// and funding capacity.
			let Some((directed, next_node_id)) = next_channel.as_directed_from(&current_node_id)
			else {
				break;
			};
			// Retrieve the direction-specific update via the public ChannelInfo fields.
			// as_directed_from already checked both directions are Some, but we break
			// defensively rather than unwrap.
			let update = match if directed.source() == &next_channel.node_one {
				next_channel.one_to_two.as_ref()
			} else {
				next_channel.two_to_one.as_ref()
			} {
				Some(u) => u,
				None => break,
			};

			if !update.enabled {
				break;
			}

			route_least_htlc_upper_bound =
				route_least_htlc_upper_bound.min(update.htlc_maximum_msat);

			route_greatest_htlc_lower_bound =
				route_greatest_htlc_lower_bound.max(update.htlc_minimum_msat);

			let next_pubkey = match PublicKey::try_from(*next_node_id) {
				Ok(pk) => pk,
				Err(_) => break,
			};

			route.push((*next_node_id, next_scid, next_pubkey));
			prev_scid = next_scid;
			current_node_id = *next_node_id;
		}

		if route_greatest_htlc_lower_bound > route_least_htlc_upper_bound {
			return None;
		}
		let amount_msat =
			amount_msat.max(route_greatest_htlc_lower_bound).min(route_least_htlc_upper_bound);
		if amount_msat < self.min_amount_msat {
			return None;
		}

		// Assemble hops backwards so each hop's proportional fee is computed on the amount it actually forwards
		let mut hops = Vec::with_capacity(route.len());
		let mut forwarded = amount_msat;
		let last = route.len() - 1;

		// Resolve (node_features, channel_features, maybe_announced_channel) for a hop.
		// The first hop is our local channel and may be unannounced, so its ChannelFeatures
		// are not in the gossip graph — match on SCID to detect it and fall back to local-state
		// defaults. All other (walked) hops were picked from the graph and must resolve there.
		let hop_features =
			|node_id: &NodeId, via_scid: u64| -> Option<(NodeFeatures, ChannelFeatures, bool)> {
				let node_features = graph
					.node(node_id)
					.and_then(|n| n.announcement_info.as_ref().map(|a| a.features().clone()))
					.unwrap_or_else(NodeFeatures::empty);
				let (channel_features, maybe_announced_channel) = if via_scid == first_hop_scid {
					(ChannelFeatures::empty(), false)
				} else {
					(graph.channel(via_scid)?.features.clone(), true)
				};
				Some((node_features, channel_features, maybe_announced_channel))
			};

		// Final hop: fee_msat carries the delivery amount; cltv delta is zero.
		{
			let (node_id, via_scid, pubkey) = route[last];
			let (node_features, channel_features, maybe_announced_channel) =
				hop_features(&node_id, via_scid)?;
			hops.push(RouteHop {
				pubkey,
				node_features,
				short_channel_id: via_scid,
				channel_features,
				fee_msat: amount_msat,
				cltv_expiry_delta: 0,
				maybe_announced_channel,
			});
		}

		// Non-final hops, from second-to-last back to first.
		for i in (0..last).rev() {
			let (node_id, via_scid, pubkey) = route[i];
			let (node_features, channel_features, maybe_announced_channel) =
				hop_features(&node_id, via_scid)?;

			let (_, next_scid, _) = route[i + 1];
			let next_channel = graph.channel(next_scid)?;
			let (directed, _) = next_channel.as_directed_from(&node_id)?;
			let update = match if directed.source() == &next_channel.node_one {
				next_channel.one_to_two.as_ref()
			} else {
				next_channel.two_to_one.as_ref()
			} {
				Some(u) => u,
				None => return None,
			};
			let fee = update.fees.base_msat as u64
				+ (forwarded * update.fees.proportional_millionths as u64 / 1_000_000);
			forwarded += fee;

			hops.push(RouteHop {
				pubkey,
				node_features,
				short_channel_id: via_scid,
				channel_features,
				fee_msat: fee,
				cltv_expiry_delta: update.cltv_expiry_delta as u32,
				maybe_announced_channel,
			});
		}

		hops.reverse();

		// The first-hop HTLC carries amount_msat + all intermediate fees.
		// Verify the total fits within our live outbound limit before returning.
		let total_outgoing: u64 = hops.iter().map(|h| h.fee_msat).sum();
		if total_outgoing > first_hop.next_outbound_htlc_limit_msat {
			return None;
		}

		Some(Path { hops, blinded_tail: None })
	}
}

impl ProbingStrategy for RandomStrategy {
	fn next_probe(&self) -> Option<Path> {
		let target_hops = random_range(1, self.max_hops as u64) as usize;
		let amount_msat = random_range(self.min_amount_msat, self.max_amount_msat);

		self.try_build_path(target_hops, amount_msat)
	}
}

/// Periodically dispatches probes according to a [`ProbingStrategy`].
pub struct Prober {
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) logger: Arc<Logger>,
	/// The strategy that decides what to probe.
	pub strategy: Arc<dyn ProbingStrategy>,
	/// How often to fire a probe attempt.
	pub interval: Duration,
	/// Maximum total millisatoshis that may be locked in in-flight probes at any time.
	pub max_locked_msat: u64,
	pub(crate) locked_msat: Arc<AtomicU64>,
}

fn fmt_path(path: &lightning::routing::router::Path) -> String {
	path.hops
		.iter()
		.map(|h| format!("{}(scid={})", h.pubkey, h.short_channel_id))
		.collect::<Vec<_>>()
		.join(" -> ")
}

impl Prober {
	/// Returns the total millisatoshis currently locked in in-flight probes.
	pub fn locked_msat(&self) -> u64 {
		self.locked_msat.load(Ordering::Relaxed)
	}

	pub(crate) fn handle_probe_successful(&self, path: &lightning::routing::router::Path) {
		let amount: u64 = path.hops.iter().map(|h| h.fee_msat).sum();
		let prev = self
			.locked_msat
			.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(amount)))
			.unwrap_or(0);
		log_debug!(
			self.logger,
			"Probe successful: released {} msat (locked_msat {} -> {}), path: {}",
			amount,
			prev,
			prev.saturating_sub(amount),
			fmt_path(path)
		);
	}

	pub(crate) fn handle_probe_failed(&self, path: &lightning::routing::router::Path) {
		let amount: u64 = path.hops.iter().map(|h| h.fee_msat).sum();
		let prev = self
			.locked_msat
			.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(amount)))
			.unwrap_or(0);
		log_debug!(
			self.logger,
			"Probe failed: released {} msat (locked_msat {} -> {}), path: {}",
			amount,
			prev,
			prev.saturating_sub(amount),
			fmt_path(path)
		);
	}
}

/// Runs the probing loop for the given [`Prober`] until `stop_rx` fires.
pub(crate) async fn run_prober(prober: Arc<Prober>, mut stop_rx: tokio::sync::watch::Receiver<()>) {
	let mut ticker = tokio::time::interval(prober.interval);
	ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

	loop {
		tokio::select! {
			biased;
			_ = stop_rx.changed() => {
				log_debug!(prober.logger, "Stopping background probing.");
				return;
			}
			_ = ticker.tick() => {
				let path = match prober.strategy.next_probe() {
					Some(p) => p,
					None => continue,
				};
				let amount: u64 = path.hops.iter().map(|h| h.fee_msat).sum();
				if prober.locked_msat.load(Ordering::Acquire) + amount > prober.max_locked_msat {
					log_debug!(prober.logger, "Skipping probe: locked-msat budget exceeded.");
					continue;
				}
				match prober.channel_manager.send_probe(path.clone()) {
					Ok(_) => {
						prober.locked_msat.fetch_add(amount, Ordering::Release);
						log_debug!(
							prober.logger,
							"Probe sent: locked {} msat, path: {}",
							amount,
							fmt_path(&path)
						);
					}
					Err(e) => {
						log_debug!(
							prober.logger,
							"Probe send failed: {:?}, path: {}",
							e,
							fmt_path(&path)
						);
					}
				}
			}
		}
	}
}
