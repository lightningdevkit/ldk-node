use crate::logger::{log_debug, LdkLogger, Logger};
use crate::types::{ChannelManager, Graph};

use lightning::routing::gossip::NodeId;
use lightning::routing::router::{Path, RouteHop, MAX_PATH_LENGTH_ESTIMATE};
use lightning_types::features::NodeFeatures;

use bitcoin::secp256k1::PublicKey;

use rand::Rng;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A probe to be dispatched by the Prober.
pub enum Probe {
    /// A manually constructed path; dispatched via `send_probe`.
    PrebuiltRoute(Path),
    /// A destination to reach; the router selects the actual path via
    /// `send_spontaneous_preflight_probes`.
    Destination {
        /// The destination node.
        final_node: PublicKey,
        /// The probe amount in millisatoshis.
        amount_msat: u64,
    },
}

/// Strategy can be used for determining the next target and amount for probing.
pub trait ProbingStrategy: Send + Sync + 'static {
    /// Returns the next probe to run, or `None` to skip this tick.
    fn next_probe(&self) -> Option<Probe>;
}

/// Probes toward the most-connected nodes in the graph.
///
/// Sorts all graph nodes by channel count descending, then cycles through the
/// top-`top_n` entries using `Destination` so the router finds the actual path.
/// The probe amount is chosen uniformly at random from `[min_amount_msat, max_amount_msat]`.
pub struct HighDegreeStrategy {
    network_graph: Arc<Graph>,
    /// How many of the highest-degree nodes to cycle through.
    pub top_n: usize,
    /// Lower bound for the randomly chosen probe amount.
    pub min_amount_msat: u64,
    /// Upper bound for the randomly chosen probe amount.
    pub max_amount_msat: u64,
    cursor: AtomicUsize,
}

impl HighDegreeStrategy {
    /// Creates a new high-degree probing strategy.
    pub(crate) fn new(
        network_graph: Arc<Graph>, top_n: usize, min_amount_msat: u64, max_amount_msat: u64,
    ) -> Self {
        assert!(
            min_amount_msat <= max_amount_msat,
            "min_amount_msat must not exceed max_amount_msat"
        );
        Self { network_graph, top_n, min_amount_msat, max_amount_msat, cursor: AtomicUsize::new(0) }
    }
}

impl ProbingStrategy for HighDegreeStrategy {
    fn next_probe(&self) -> Option<Probe> {
        let graph = self.network_graph.read_only();

        // Collect (pubkey, channel_count) for all nodes.
        let mut nodes_by_degree: Vec<(PublicKey, usize)> = graph
            .nodes()
            .unordered_iter()
            .filter_map(|(id, info)| {
                PublicKey::try_from(*id).ok().map(|pk| (pk, info.channels.len()))
            })
            .collect();

        if nodes_by_degree.is_empty() {
            return None;
        }

        nodes_by_degree.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        let top_n = self.top_n.min(nodes_by_degree.len());

        let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
        let (final_node, _degree) = nodes_by_degree[cursor % top_n];

        let amount_msat = rand::rng().random_range(self.min_amount_msat..=self.max_amount_msat);
        Some(Probe::Destination { final_node, amount_msat })
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
///   3. Returns `Probe::PrebuiltRoute(path)` so the prober calls `send_probe` directly.
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
    fn try_build_path(&self, rng: &mut impl Rng, target_hops: usize, amount_msat: u64) -> Option<Path> {
        let initial_channels =
            self.channel_manager.list_channels().into_iter().filter(|c|
                c.is_usable && c.short_channel_id.is_some()).collect::<Vec<_>>();

        if initial_channels.is_empty() {
            return None;
        }

        let graph = self.network_graph.read_only();
        let first_hop = &initial_channels[rng.random_range(0..initial_channels.len())];
        let first_hop_scid = first_hop.short_channel_id.unwrap();
        let next_peer_pubkey = first_hop.counterparty.node_id;
        let next_peer_node_id = NodeId::from_pubkey(&next_peer_pubkey);

        // Track the tightest HTLC limit across all hops to cap the probe amount.
        // The first hop limit comes from our live channel state; subsequent hops use htlc_maximum_msat from the gossip channel update.
        let mut route_least_htlc_upper_bound = first_hop.next_outbound_htlc_limit_msat;

        // Walk the graph: each entry is (node_id, arrived_via_scid, pubkey); first entry is set:
        let mut route: Vec<(NodeId, u64, PublicKey)> = vec![(next_peer_node_id, first_hop_scid, next_peer_pubkey)];

        let mut prev_scid = first_hop_scid;
        let mut current_node_id = next_peer_node_id;

        for _ in 1..target_hops {
            let node_info = match graph.node(&current_node_id) {
                Some(n) => n,
                None => break,
            };

            // Outward channels: skip the one we arrived on to avoid backtracking.
            let candidates: Vec<u64> = node_info.channels.iter().copied().filter(|&scid| scid != prev_scid).collect();

            if candidates.is_empty() {
                break;
            }

            let next_scid = candidates[rng.random_range(0..candidates.len())];
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
            // Safe to unwrap: as_directed_from already checked both directions are Some.
            let update = if directed.source() == &next_channel.node_one {
                next_channel.one_to_two.as_ref().unwrap()
            } else {
                next_channel.two_to_one.as_ref().unwrap()
            };

            if !update.enabled {
                break;
            }

            route_least_htlc_upper_bound = route_least_htlc_upper_bound.min(update.htlc_maximum_msat);

            let next_pubkey = match PublicKey::try_from(*next_node_id) {
                Ok(pk) => pk,
                Err(_) => break,
            };

            route.push((*next_node_id, next_scid, next_pubkey));
            prev_scid = next_scid;
            current_node_id = *next_node_id;
        }

        let amount_msat = amount_msat.min(route_least_htlc_upper_bound); //cap probe amount
        if amount_msat < self.min_amount_msat {
            return None;
        }

        // Assemble hops.
        // For hop i: fee and CLTV are determined by the *next* channel (what route[i]
        // will charge to forward onward).  For the last hop they are amount_msat and zero expiry delta.
        let mut hops = Vec::with_capacity(route.len());
        for i in 0..route.len() {
            let (node_id, via_scid, pubkey) = route[i];

            let channel_info = graph.channel(via_scid)?;

            let node_features = graph
                .node(&node_id)
                .and_then(|n| n.announcement_info.as_ref().map(|a| a.features().clone()))
                .unwrap_or_else(NodeFeatures::empty);

            let (fee_msat, cltv_expiry_delta) =
                if i + 1 < route.len() { // non-final hop
                    let (_, next_scid, _) = route[i + 1];
                    let next_channel = graph.channel(next_scid)?;
                    let (directed, _) = next_channel.as_directed_from(&node_id)?;
                    let update = if directed.source() == &next_channel.node_one {
                        next_channel.one_to_two.as_ref().unwrap()
                    } else {
                        next_channel.two_to_one.as_ref().unwrap()
                    };
                    let fee = update.fees.base_msat as u64 + (amount_msat * update.fees.proportional_millionths as u64 / 1_000_000);
                    (fee, update.cltv_expiry_delta as u32)
                } else {
                    // Final hop: fee_msat carries the delivery amount; cltv delta is zero.
                    (amount_msat, 0)
                };

            hops.push(RouteHop {
                pubkey,
                node_features,
                short_channel_id: via_scid,
                channel_features: channel_info.features.clone(),
                fee_msat,
                cltv_expiry_delta,
                maybe_announced_channel: true,
            });
        }

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
    fn next_probe(&self) -> Option<Probe> {
        let mut rng = rand::rng();
        let target_hops = rng.random_range(1..=self.max_hops);
        let amount_msat = rng.random_range(self.min_amount_msat..=self.max_amount_msat);

        self.try_build_path(&mut rng, target_hops, amount_msat).map(Probe::PrebuiltRoute)
    }
}

/// Periodically dispatches probes according to a [`ProbingStrategy`].
pub struct Prober {
    /// The channel manager used to send probes.
    pub channel_manager: Arc<ChannelManager>,
    /// Logger.
    pub logger: Arc<Logger>,
    /// The strategy that decides what to probe.
    pub strategy: Arc<dyn ProbingStrategy>,
    /// How often to fire a probe attempt.
    pub interval: Duration,
    /// Passed to `send_spontaneous_preflight_probes`. `None` uses LDK default (3×).
    pub liquidity_limit_multiplier: Option<u64>,
    /// Maximum total millisatoshis that may be locked in in-flight probes at any time.
    pub max_locked_msat: u64,
    /// Current millisatoshis locked in in-flight probes. Shared with the event handler,
    /// which decrements it on `ProbeSuccessful` / `ProbeFailed`.
    pub(crate) locked_msat: Arc<AtomicU64>,
}

/// Runs the probing loop for the given [`Prober`] until `stop_rx` fires.
pub(crate) async fn run_prober(
    prober: Arc<Prober>, mut stop_rx: tokio::sync::watch::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(prober.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                log_debug!(prober.logger, "Stopping background probing.");
                return;
            }
            _ = ticker.tick() => {
                match prober.strategy.next_probe() {
                    None => {}
                    Some(Probe::PrebuiltRoute(path)) => {
                        let amount: u64 = path.hops.iter().map(|h| h.fee_msat).sum();
                        if prober.locked_msat.load(Ordering::Acquire) + amount > prober.max_locked_msat {
                            log_debug!(prober.logger, "Skipping probe: locked-msat budget exceeded.");
                        } else {
                            match prober.channel_manager.send_probe(path) {
                                Ok(_) => {
                                    prober.locked_msat.fetch_add(amount, Ordering::Release);
                                }
                                Err(e) => {
                                    log_debug!(prober.logger, "Prebuilt path probe failed: {:?}", e);
                                }
                            }
                        }
                    }
                    Some(Probe::Destination { final_node, amount_msat }) => {
                        if prober.locked_msat.load(Ordering::Acquire) + amount_msat
                            > prober.max_locked_msat
                        {
                            log_debug!(prober.logger, "Skipping probe: locked-msat budget exceeded.");
                        } else {
                            match prober.channel_manager.send_spontaneous_preflight_probes(
                                final_node,
                                amount_msat,
                                18,
                                prober.liquidity_limit_multiplier,
                            ) {
                                Ok(probes) => {
                                    if !probes.is_empty() {
                                        prober.locked_msat.fetch_add(amount_msat, Ordering::Release);
                                    } else {
                                        log_debug!(prober.logger, "No probe paths found for destination {}; skipping budget increment.", final_node);
                                    }
                                }
                                Err(e) => {
                                    log_debug!(prober.logger, "Route-follow probe to {} failed: {:?}", final_node, e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
