use crate::logger::{log_debug, LdkLogger, Logger};
use crate::types::{ChannelManager, Graph};

use lightning::routing::gossip::NodeId;
use lightning::routing::router::{Path, RouteHop, MAX_PATH_LENGTH_ESTIMATE};
use lightning_types::features::NodeFeatures;

use bitcoin::secp256k1::PublicKey;

use rand::Rng;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    cursor: Mutex<usize>,
}

impl HighDegreeStrategy {
    /// Creates a new high-degree probing strategy.
    pub(crate) fn new(
        network_graph: Arc<Graph>, top_n: usize, min_amount_msat: u64, max_amount_msat: u64,
    ) -> Self {
        Self { network_graph, top_n, min_amount_msat, max_amount_msat, cursor: Mutex::new(0) }
    }
}

impl ProbingStrategy for HighDegreeStrategy {
    fn next_probe(&self) -> Option<Probe> {
        let graph = self.network_graph.read_only();

        // Collect (pubkey, channel_count) for all nodes.
        // wtf it does why we need to iterate here and then sort? maybe we can go just once?
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

        // Most-connected first.
        // wtf it does
        nodes_by_degree.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        let top_n = self.top_n.min(nodes_by_degree.len());

        let mut cursor = self.cursor.lock().unwrap();
        let (final_node, _degree) = nodes_by_degree[*cursor % top_n];
        *cursor = cursor.wrapping_add(1);

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
    /// Cursor advances each tick; used as a seed for hop selection.
    cursor: Mutex<usize>,
}

impl RandomStrategy {
    /// Creates a new random-walk probing strategy.
    pub(crate) fn new(
        network_graph: Arc<Graph>, channel_manager: Arc<ChannelManager>, max_hops: usize,
        min_amount_msat: u64, max_amount_msat: u64,
    ) -> Self {
        Self {
            network_graph,
            channel_manager,
            max_hops: max_hops.clamp(1, MAX_PATH_LENGTH_ESTIMATE as usize),
            min_amount_msat,
            max_amount_msat,
            cursor: Mutex::new(0),
        }
    }

    /// Tries to build a path for the given cursor value and hop count. Returns `None` if the
    /// local node has no usable channels, or the walk terminates before reaching `target_hops`.
    fn try_build_path(&self, cursor: usize, target_hops: usize, amount_msat: u64) -> Option<Path> {
        // Collect confirmed, usable channels: (scid, peer_pubkey).
        let our_channels: Vec<(u64, PublicKey)> = self
            .channel_manager
            .list_channels()
            .into_iter()
            .filter_map(|c| {
                if c.is_usable {
                    c.short_channel_id.map(|scid| (scid, c.counterparty.node_id))
                } else {
                    None
                }
            })
            .collect();

        if our_channels.is_empty() {
            return None;
        }

        let graph = self.network_graph.read_only();
        let (our_scid, peer_pubkey) = our_channels[cursor % our_channels.len()];
        let peer_node_id = NodeId::from_pubkey(&peer_pubkey);

        // Walk the graph: each entry is (node_id, arrived_via_scid, pubkey).
        // We start by having "arrived at peer via our_scid".
        let mut visited: Vec<(NodeId, u64, PublicKey)> =
            vec![(peer_node_id, our_scid, peer_pubkey)];

        let mut prev_scid = our_scid;
        let mut current_node_id = peer_node_id;

        //wtf the real amount of hops is -1 of target?
        for hop_idx in 1..target_hops {
            let node_info = match graph.node(&current_node_id) {
                Some(n) => n,
                None => break,
            };

            // Outward channels: skip the one we arrived on to avoid backtracking.
            let candidates: Vec<u64> = node_info.channels.iter().copied().filter(|&scid| scid != prev_scid).collect();

            if candidates.is_empty() {
                break;
            }

            let next_scid = candidates[(cursor + hop_idx) % candidates.len()];
            let next_channel = match graph.channel(next_scid) {
                Some(c) => c,
                None => break,
            };

            // Determine direction and fetch the channel-update.
            let (update, next_node_id) = if next_channel.node_one == current_node_id {
                match next_channel.one_to_two.as_ref() {
                    Some(u) => (u, next_channel.node_two),
                    None => break,
                }
            } else if next_channel.node_two == current_node_id {
                match next_channel.two_to_one.as_ref() {
                    Some(u) => (u, next_channel.node_one),
                    None => break,
                }
            } else {
                break;
            };

            if !update.enabled {
                break;
            }

            let next_pubkey = match PublicKey::try_from(next_node_id) {
                Ok(pk) => pk,
                Err(_) => break,
            };

            visited.push((next_node_id, next_scid, next_pubkey));
            prev_scid = next_scid;
            current_node_id = next_node_id;
        }

        // Require the full requested depth; shorter walks are uninformative.
        if visited.len() < target_hops {
            return None;
        }

        // Assemble hops.
        // For hop i: fee and CLTV are determined by the *next* channel (what visited[i]
        // will charge to forward onward).  For the last hop they are amount_msat and zero expiry delta.
        let mut hops = Vec::with_capacity(visited.len());
        for i in 0..visited.len() {
            let (node_id, via_scid, pubkey) = visited[i];

            let channel_info = graph.channel(via_scid)?;

            let node_features = graph
                .node(&node_id)
                .and_then(|n| n.announcement_info.as_ref().map(|a| a.features().clone()))
                .unwrap_or_else(NodeFeatures::empty);

            let (fee_msat, cltv_expiry_delta) = if i + 1 < visited.len() {
                // Intermediate hop: look up the next channel's update from node_id.
                let (_next_node_id, next_scid, _) = visited[i + 1];
                let next_channel = graph.channel(next_scid)?;
                let update = if next_channel.node_one == node_id {
                    next_channel.one_to_two.as_ref()?
                } else {
                    next_channel.two_to_one.as_ref()?
                };
                let fee = update.fees.base_msat as u64
                    + (amount_msat * update.fees.proportional_millionths as u64 / 1_000_000);
                (fee, update.cltv_expiry_delta as u32)
            } else {
                // Final hop.
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

        Some(Path { hops, blinded_tail: None })
    }
}

impl ProbingStrategy for RandomStrategy {
    fn next_probe(&self) -> Option<Probe> {
        let mut rng = rand::rng();
        let target_hops = rng.random_range(1..=self.max_hops);
        let amount_msat = rng.random_range(self.min_amount_msat..=self.max_amount_msat);

        let mut cursor = self.cursor.lock().unwrap();
        let path = self.try_build_path(*cursor, target_hops, amount_msat);
        *cursor = cursor.wrapping_add(1);
        path.map(Probe::PrebuiltRoute)
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

impl Prober {
    /// Runs the probing loop until `stop_rx` fires.
    pub async fn run(self: Arc<Self>, mut stop_rx: tokio::sync::watch::Receiver<()>) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    log_debug!(self.logger, "Stopping background probing.");
                    return;
                }
                _ = ticker.tick() => {
                    match self.strategy.next_probe() {
                        None => {}
                        Some(Probe::PrebuiltRoute(path)) => {
                            let amount = path.hops.last().map_or(0, |h| h.fee_msat);
                            if self.locked_msat.load(Ordering::Relaxed) + amount > self.max_locked_msat {
                                log_debug!(self.logger, "Skipping probe: locked-msat budget exceeded.");
                            } else {
                                match self.channel_manager.send_probe(path) {
                                    Ok(_) => {
                                        self.locked_msat.fetch_add(amount, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        log_debug!(self.logger, "Prebuilt path probe failed: {:?}", e);
                                    }
                                }
                            }
                        }
                        Some(Probe::Destination { final_node, amount_msat }) => {
                            if self.locked_msat.load(Ordering::Relaxed) + amount_msat > self.max_locked_msat {
                                log_debug!(self.logger, "Skipping probe: locked-msat budget exceeded.");
                            } else {
                                match self.channel_manager.send_spontaneous_preflight_probes(final_node, amount_msat, 18, self.liquidity_limit_multiplier) {
                                    Ok(probes) => {
                                        if !probes.is_empty() {
                                            self.locked_msat.fetch_add(amount_msat, Ordering::Relaxed);
                                        } else {
                                            log_debug!( self.logger, "No probe paths found for destination {}; skipping budget increment.", final_node);
                                        }
                                    }
                                    Err(e) => {
                                        log_debug!( self.logger, "Route-follow probe to {} failed: {:?}", final_node, e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
