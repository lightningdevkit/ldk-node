use crate::logger::{log_error, log_info, Logger};
use crate::types::PeerManager;
use crate::Error;

use lightning::ln::msgs::SocketAddress;

use bitcoin::secp256k1::PublicKey;
use tokio::sync::watch;
use tokio::sync::watch::{Receiver, Sender};

use std::collections::hash_map::{self, HashMap};
use std::net::ToSocketAddrs;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct ConnectionManager<L: Deref + Clone + Sync + Send>
where
	L::Target: Logger,
{
	pending_connections:
		Mutex<HashMap<PublicKey, Vec<tokio::sync::oneshot::Sender<Result<(), Error>>>>>,
	peer_manager: Arc<PeerManager>,
	logger: L,
	is_initial_connection_established: Arc<AtomicBool>,
	initial_connection_established_sender: Sender<bool>,
}

impl<L: Deref + Clone + Sync + Send> ConnectionManager<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(peer_manager: Arc<PeerManager>, logger: L) -> Self {
		let pending_connections = Mutex::new(HashMap::new());
		let is_initial_connection_established = Arc::new(AtomicBool::new(false));
		let (initial_connection_established_sender, _) = watch::channel(false);
		Self {
			pending_connections,
			peer_manager,
			logger,
			is_initial_connection_established,
			initial_connection_established_sender,
		}
	}

	pub(crate) async fn connect_peer_if_necessary(
		&self, node_id: PublicKey, addr: SocketAddress,
	) -> Result<(), Error> {
		if self.peer_manager.peer_by_node_id(&node_id).is_some() {
			return Ok(());
		}

		self.is_initial_connection_established.store(true, Ordering::Release);
		match self.initial_connection_established_sender.send(true) {
			Ok(_) => {},
			Err(err) => {
				log_error!(
					self.logger,
					"Failed to send initial connection state through watch channel {:?}",
					err
				);
			},
		}

		self.do_connect_peer(node_id, addr).await
	}

	pub(crate) async fn do_connect_peer(
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

		let socket_addr = addr
			.to_socket_addrs()
			.map_err(|e| {
				log_error!(self.logger, "Failed to resolve network address {}: {}", addr, e);
				self.propagate_result_to_subscribers(&node_id, Err(Error::InvalidSocketAddress));
				Error::InvalidSocketAddress
			})?
			.next()
			.ok_or_else(|| {
				log_error!(self.logger, "Failed to resolve network address {}", addr);
				self.propagate_result_to_subscribers(&node_id, Err(Error::InvalidSocketAddress));
				Error::InvalidSocketAddress
			})?;

		let connection_future = lightning_net_tokio::connect_outbound(
			Arc::clone(&self.peer_manager),
			node_id,
			socket_addr,
		);

		let res = match connection_future.await {
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
		};

		self.propagate_result_to_subscribers(&node_id, res);

		res
	}

	fn register_or_subscribe_pending_connection(
		&self, node_id: &PublicKey,
	) -> Option<tokio::sync::oneshot::Receiver<Result<(), Error>>> {
		let mut pending_connections_lock = self.pending_connections.lock().unwrap();
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
		let mut pending_connections_lock = self.pending_connections.lock().unwrap();
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

	pub fn subscribe_initial_connection(&self) -> Receiver<bool> {
		self.initial_connection_established_sender.subscribe()
	}

	pub fn is_initial_connection_established(&self) -> bool {
		self.is_initial_connection_established.load(Ordering::Acquire)
	}
}
