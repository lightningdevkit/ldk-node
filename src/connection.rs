// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::hash_map::{self, HashMap};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitcoin::secp256k1::PublicKey;
use lightning::ln::msgs::SocketAddress;

use crate::config::{TorConfig, PEER_RECONNECTION_INTERVAL, PEER_RECONNECTION_MAX_INTERVAL};
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::types::{KeysManager, PeerManager};
use crate::Error;

struct PeerReconnectState {
	consecutive_failures: u32,
	next_retry_at: Instant,
	next_backoff: Duration,
}

impl PeerReconnectState {
	fn new(now: Instant) -> Self {
		Self {
			consecutive_failures: 0,
			next_retry_at: now,
			next_backoff: PEER_RECONNECTION_INTERVAL,
		}
	}

	/// Bumps the failure count, schedules `next_retry_at` to `now + current backoff`,
	/// and doubles the backoff for the following failure (capped at
	/// [`PEER_RECONNECTION_MAX_INTERVAL`]). Returns the backoff that was scheduled.
	fn record_failure(&mut self, now: Instant) -> Duration {
		self.consecutive_failures = self.consecutive_failures.saturating_add(1);
		let scheduled_backoff = self.next_backoff;
		self.next_retry_at = now + scheduled_backoff;
		self.next_backoff =
			std::cmp::min(scheduled_backoff.saturating_mul(2), PEER_RECONNECTION_MAX_INTERVAL);
		scheduled_backoff
	}
}

pub(crate) struct ConnectionManager<L: Deref + Clone + Sync + Send>
where
	L::Target: LdkLogger,
{
	pending_connections:
		Mutex<HashMap<PublicKey, Vec<tokio::sync::oneshot::Sender<Result<(), Error>>>>>,
	reconnect_state: Mutex<HashMap<PublicKey, PeerReconnectState>>,
	peer_manager: Arc<PeerManager>,
	tor_proxy_config: Option<TorConfig>,
	keys_manager: Arc<KeysManager>,
	logger: L,
}

impl<L: Deref + Clone + Sync + Send> ConnectionManager<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(
		peer_manager: Arc<PeerManager>, tor_proxy_config: Option<TorConfig>,
		keys_manager: Arc<KeysManager>, logger: L,
	) -> Self {
		let pending_connections = Mutex::new(HashMap::new());
		let reconnect_state = Mutex::new(HashMap::new());

		Self {
			pending_connections,
			reconnect_state,
			peer_manager,
			tor_proxy_config,
			keys_manager,
			logger,
		}
	}

	/// Returns whether the background reconnection task should attempt to reconnect
	/// to `node_id` now, based on per-peer exponential backoff state.
	pub(crate) fn is_reconnect_due(&self, node_id: &PublicKey) -> bool {
		self.reconnect_state
			.lock()
			.expect("lock")
			.get(node_id)
			.map_or(true, |state| Instant::now() >= state.next_retry_at)
	}

	/// Records a failed background reconnection attempt: the per-peer retry interval
	/// is doubled (up to [`PEER_RECONNECTION_MAX_INTERVAL`]) and the next retry is
	/// scheduled `now + <previous interval>`. `now` should be the instant of the
	/// reconnection loop tick that scheduled the attempt, so that the resulting
	/// schedule stays aligned with the loop's wakeups rather than drifting by the
	/// attempt's duration.
	///
	/// Successful connects clear any backoff state via [`Self::do_connect_peer`].
	pub(crate) fn record_reconnect_failure(&self, node_id: &PublicKey, now: Instant) {
		let mut state_lock = self.reconnect_state.lock().expect("lock");
		let state = state_lock.entry(*node_id).or_insert_with(|| PeerReconnectState::new(now));
		let scheduled_backoff = state.record_failure(now);

		log_debug!(
			self.logger,
			"Reconnection to peer {} failed ({} consecutive failures); next retry in {}s",
			node_id,
			state.consecutive_failures,
			scheduled_backoff.as_secs(),
		);
	}

	/// Removes any per-peer backoff state for `node_id`, so a subsequent attempt
	/// is treated as a fresh first try. Called when a peer is removed from the
	/// persisted peer store.
	pub(crate) fn clear_reconnect_state(&self, node_id: &PublicKey) {
		self.reconnect_state.lock().expect("lock").remove(node_id);
	}

	/// Drops backoff state for any peer not in `persisted_peers`. This guards against
	/// entries resurrected by a failed attempt that raced the peer's removal from the
	/// store, which would otherwise linger forever.
	pub(crate) fn prune_reconnect_state(&self, persisted_peers: &[PublicKey]) {
		self.reconnect_state
			.lock()
			.expect("lock")
			.retain(|node_id, _| persisted_peers.contains(node_id));
	}

	/// Returns whether an outbound connection attempt to `node_id` is currently in
	/// flight, i.e., whether a [`Self::do_connect_peer`] call would merely subscribe
	/// to another task's attempt (possibly targeting a different address).
	pub(crate) fn has_pending_connection(&self, node_id: &PublicKey) -> bool {
		self.pending_connections.lock().expect("lock").contains_key(node_id)
	}

	pub(crate) async fn connect_peer_if_necessary(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
		if self.peer_manager.peer_by_node_id(&node_id).is_some() {
			// The peer is demonstrably reachable: reset any backoff so a subsequent
			// drop is retried promptly.
			self.clear_reconnect_state(&node_id);
			return Ok(());
		}

		self.do_connect_peer(node_id, addr).await
	}

	pub(crate) async fn do_connect_peer(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
		let res = self.do_connect_peer_internal(node_id, addr).await;
		if res.is_ok() {
			// Any successful connect (including user-initiated ones) resets backoff so the
			// background reconnection loop retries promptly if the peer drops again.
			self.clear_reconnect_state(&node_id);
		}
		self.propagate_result_to_subscribers(&node_id, res);
		res
	}

	async fn do_connect_peer_internal(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
		// First, we check if there is already an outbound connection in flight, if so, we just
		// await on the corresponding watch channel. The task driving the connection future will
		// send us the result..
		let pending_ready_receiver_opt = self.register_or_subscribe_pending_connection(&node_id);
		if let Some(pending_connection_ready_receiver) = pending_ready_receiver_opt {
			return pending_connection_ready_receiver.await.map_err(|e| {
				debug_assert!(false, "Failed to receive connection result: {:?}", e);
				log_error!(self.logger, "Failed to receive connection result: {:?}", e);
				Error::ConnectionFailed
			})?;
		}

		log_info!(self.logger, "Connecting to peer: {}@{}", node_id, addr);

		match addr {
			SocketAddress::OnionV2(old_onion_addr) => {
				log_error!(
					self.logger,
					"Failed to resolve network address {:?}: Resolution of OnionV2 addresses is currently unsupported.",
					old_onion_addr
				);
				Err(Error::InvalidSocketAddress)
			},
			SocketAddress::OnionV3 { .. } => {
				let proxy_config = self.tor_proxy_config.as_ref().ok_or_else(|| {
					log_error!(
						self.logger,
						"Failed to resolve network address {:?}: Tor usage is not configured.",
						addr
					);
					Error::InvalidSocketAddress
				})?;
				let resolved_addrs: Vec<_> =
					tokio::net::lookup_host(proxy_config.proxy_address.to_string())
						.await
						.map_err(|e| {
							log_error!(
								self.logger,
								"Failed to resolve Tor proxy network address {}: {}",
								proxy_config.proxy_address,
								e
							);
							Error::InvalidSocketAddress
						})?
						.collect();

				if resolved_addrs.is_empty() {
					log_error!(
						self.logger,
						"Failed to resolve Tor proxy network address {}",
						proxy_config.proxy_address
					);
					return Err(Error::InvalidSocketAddress);
				}

				let mut res = Err(Error::ConnectionFailed);
				let mut had_failures = false;
				for proxy_addr in resolved_addrs {
					let connection_future = lightning_net_tokio::tor_connect_outbound(
						Arc::clone(&self.peer_manager),
						node_id,
						addr.clone(),
						proxy_addr,
						Arc::clone(&self.keys_manager),
					);
					res = self.await_connection(connection_future, node_id, addr.clone()).await;
					if res.is_ok() {
						if had_failures {
							log_info!(
								self.logger,
								"Successfully connected to peer {}@{} via resolved proxy address {} after previous attempts failed.",
								node_id, addr, proxy_addr
							);
						}
						break;
					}
					had_failures = true;
					log_debug!(
						self.logger,
						"Failed to connect to peer {}@{} via resolved proxy address {}.",
						node_id,
						addr,
						proxy_addr
					);
				}
				res
			},
			_ => {
				let resolved_addrs: Vec<_> = tokio::net::lookup_host(addr.to_string())
					.await
					.map_err(|e| {
						log_error!(
							self.logger,
							"Failed to resolve network address {}: {}",
							addr,
							e
						);
						Error::InvalidSocketAddress
					})?
					.collect();

				if resolved_addrs.is_empty() {
					log_error!(self.logger, "Failed to resolve network address {}", addr);
					return Err(Error::InvalidSocketAddress);
				}

				let mut res = Err(Error::ConnectionFailed);
				let mut had_failures = false;
				for socket_addr in resolved_addrs {
					let connection_future = lightning_net_tokio::connect_outbound(
						Arc::clone(&self.peer_manager),
						node_id,
						socket_addr,
					);
					res = self.await_connection(connection_future, node_id, addr.clone()).await;
					if res.is_ok() {
						if had_failures {
							log_info!(
								self.logger,
								"Successfully connected to peer {}@{} via resolved address {} after previous attempts failed.",
								node_id, addr, socket_addr
							);
						}
						break;
					}
					had_failures = true;
					log_debug!(
						self.logger,
						"Failed to connect to peer {}@{} via resolved address {}.",
						node_id,
						addr,
						socket_addr
					);
				}
				res
			},
		}
	}

	async fn await_connection<F, CF>(
		&self, connection_future: F, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error>
	where
		F: std::future::Future<Output = Option<CF>>,
		CF: std::future::Future<Output = ()>,
	{
		match connection_future.await {
			Some(connection_closed_future) => {
				let mut connection_closed_future = Box::pin(connection_closed_future);
				loop {
					tokio::select! {
						_ = &mut connection_closed_future => {
							log_info!(self.logger, "Peer connection closed: {}@{}", node_id, addr);
							break Err(Error::ConnectionFailed);
						},
						_ = tokio::time::sleep(Duration::from_millis(10)) => {},
					};

					match self.peer_manager.peer_by_node_id(&node_id) {
						Some(_) => break Ok(()),
						None => continue,
					}
				}
			},
			None => {
				log_error!(self.logger, "Failed to connect to peer: {}@{}", node_id, addr);
				Err(Error::ConnectionFailed)
			},
		}
	}

	fn register_or_subscribe_pending_connection(
		&self, node_id: &PublicKey,
	) -> Option<tokio::sync::oneshot::Receiver<Result<(), Error>>> {
		let mut pending_connections_lock = self.pending_connections.lock().expect("lock");
		match pending_connections_lock.entry(*node_id) {
			hash_map::Entry::Occupied(mut entry) => {
				let (tx, rx) = tokio::sync::oneshot::channel();
				entry.get_mut().push(tx);
				Some(rx)
			},
			hash_map::Entry::Vacant(entry) => {
				entry.insert(Vec::new());
				None
			},
		}
	}

	fn propagate_result_to_subscribers(&self, node_id: &PublicKey, res: Result<(), Error>) {
		// Send the result to any other tasks that might be waiting on it by now.
		let mut pending_connections_lock = self.pending_connections.lock().expect("lock");
		if let Some(connection_ready_senders) = pending_connections_lock.remove(node_id) {
			for sender in connection_ready_senders {
				let _ = sender.send(res).map_err(|e| {
					debug_assert!(
						false,
						"Failed to send connection result to subscribers: {:?}",
						e
					);
					log_error!(
						self.logger,
						"Failed to send connection result to subscribers: {:?}",
						e
					);
				});
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reconnect_state_doubles_until_capped() {
		let start = Instant::now();
		let mut state = PeerReconnectState::new(start);

		let scheduled = state.record_failure(start);
		assert_eq!(scheduled, PEER_RECONNECTION_INTERVAL);
		assert_eq!(state.consecutive_failures, 1);
		assert_eq!(state.next_retry_at, start + PEER_RECONNECTION_INTERVAL);

		let mut expected = PEER_RECONNECTION_INTERVAL;
		for failure_count in 2..32 {
			expected = std::cmp::min(expected.saturating_mul(2), PEER_RECONNECTION_MAX_INTERVAL);
			let scheduled = state.record_failure(start);
			assert_eq!(scheduled, expected);
			assert_eq!(state.consecutive_failures, failure_count);
			assert_eq!(state.next_retry_at, start + expected);
			assert!(state.next_backoff <= PEER_RECONNECTION_MAX_INTERVAL);
		}

		// Once capped, further failures stay at the cap.
		assert_eq!(state.next_backoff, PEER_RECONNECTION_MAX_INTERVAL);
		let scheduled = state.record_failure(start);
		assert_eq!(scheduled, PEER_RECONNECTION_MAX_INTERVAL);
		assert_eq!(state.next_backoff, PEER_RECONNECTION_MAX_INTERVAL);
	}

	#[test]
	fn reconnect_state_schedules_relative_to_failure_time() {
		let t0 = Instant::now();
		let mut state = PeerReconnectState::new(t0);

		let _ = state.record_failure(t0);
		assert_eq!(state.next_retry_at, t0 + PEER_RECONNECTION_INTERVAL);

		let t1 = t0 + Duration::from_secs(5);
		let scheduled = state.record_failure(t1);
		assert_eq!(scheduled, PEER_RECONNECTION_INTERVAL * 2);
		assert_eq!(state.next_retry_at, t1 + PEER_RECONNECTION_INTERVAL * 2);
	}
}
