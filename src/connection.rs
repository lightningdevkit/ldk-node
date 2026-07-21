// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::hash_map::{self, HashMap};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::msgs::SocketAddress;

use crate::config::TorConfig;
use crate::logger::{log_debug, log_error, log_info, LdkLogger};
use crate::types::{KeysManager, PeerManager};
use crate::Error;

type PendingConnections =
	Mutex<HashMap<PublicKey, Vec<tokio::sync::oneshot::Sender<Result<(), Error>>>>>;

struct PendingConnectionGuard<'a> {
	pending_connections: &'a PendingConnections,
	node_id: PublicKey,
	active: bool,
}

impl<'a> PendingConnectionGuard<'a> {
	fn new(pending_connections: &'a PendingConnections, node_id: PublicKey) -> Self {
		Self { pending_connections, node_id, active: true }
	}

	fn disarm(&mut self) {
		self.active = false;
	}
}

impl Drop for PendingConnectionGuard<'_> {
	fn drop(&mut self) {
		if !self.active {
			return;
		}
		let mut pending_connections = self.pending_connections.lock().expect("lock");
		if let Some(subscribers) = pending_connections.remove(&self.node_id) {
			for subscriber in subscribers {
				let _ = subscriber.send(Err(Error::ConnectionFailed));
			}
		}
	}
}

pub(crate) struct ConnectionManager<L: Deref + Clone + Sync + Send>
where
	L::Target: LdkLogger,
{
	pending_connections: PendingConnections,
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

		Self { pending_connections, peer_manager, tor_proxy_config, keys_manager, logger }
	}

	pub(crate) async fn connect_peer_if_necessary(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
		if self.peer_manager.peer_by_node_id(&node_id).is_some() {
			return Ok(());
		}

		self.do_connect_peer(node_id, addr).await
	}

	pub(crate) fn disconnect_peer(&self, node_id: PublicKey) {
		self.peer_manager.disconnect_by_node_id(node_id);
	}

	pub(crate) async fn do_connect_peer(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
		// If another task is already connecting, subscribe to its result instead of starting a
		// duplicate attempt.
		if let Some(pending_connection_ready_receiver) =
			self.register_or_subscribe_pending_connection(&node_id)
		{
			return pending_connection_ready_receiver.await.map_err(|e| {
				debug_assert!(false, "Failed to receive connection result: {:?}", e);
				log_error!(self.logger, "Failed to receive connection result: {:?}", e);
				Error::ConnectionFailed
			})?;
		}

		let mut pending_connection =
			PendingConnectionGuard::new(&self.pending_connections, node_id);
		let res = self.do_connect_peer_internal(node_id, addr).await;
		self.propagate_result_to_subscribers(&node_id, res);
		pending_connection.disarm();
		res
	}

	async fn do_connect_peer_internal(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
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
				entry.get_mut().retain(|subscriber| !subscriber.is_closed());
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
	fn pending_connection_guard_notifies_subscribers_when_abandoned() {
		let node_id: PublicKey =
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".parse().unwrap();
		let pending_connections = Mutex::new(HashMap::new());
		let (sender, mut receiver) = tokio::sync::oneshot::channel();
		pending_connections.lock().expect("lock").insert(node_id, vec![sender]);

		let connection_guard = PendingConnectionGuard::new(&pending_connections, node_id);
		drop(connection_guard);

		assert!(
			pending_connections.lock().expect("lock").is_empty(),
			"abandoned connection attempt should remove pending state"
		);
		assert_eq!(receiver.try_recv(), Ok(Err(Error::ConnectionFailed)));
	}
}
