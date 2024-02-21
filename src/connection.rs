use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::types::PeerManager;
use crate::Error;

use lightning::ln::msgs::SocketAddress;

use bitcoin::secp256k1::PublicKey;

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn connect_peer_if_necessary(
	node_id: PublicKey, addr: SocketAddress, peer_manager: Arc<PeerManager>,
	logger: Arc<FilesystemLogger>,
) -> Result<(), Error> {
	if peer_manager.peer_by_node_id(&node_id).is_some() {
		return Ok(());
	}

	do_connect_peer(node_id, addr, peer_manager, logger).await
}

pub(crate) async fn do_connect_peer(
	node_id: PublicKey, addr: SocketAddress, peer_manager: Arc<PeerManager>,
	logger: Arc<FilesystemLogger>,
) -> Result<(), Error> {
	log_info!(logger, "Connecting to peer: {}@{}", node_id, addr);

	let socket_addr = addr
		.to_socket_addrs()
		.map_err(|e| {
			log_error!(logger, "Failed to resolve network address: {}", e);
			Error::InvalidSocketAddress
		})?
		.next()
		.ok_or(Error::ConnectionFailed)?;

	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), node_id, socket_addr)
		.await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				match futures::poll!(&mut connection_closed_future) {
					std::task::Poll::Ready(_) => {
						log_info!(logger, "Peer connection closed: {}@{}", node_id, addr);
						return Err(Error::ConnectionFailed);
					},
					std::task::Poll::Pending => {},
				}
				// Avoid blocking the tokio context by sleeping a bit
				match peer_manager.peer_by_node_id(&node_id) {
					Some(_) => return Ok(()),
					None => tokio::time::sleep(Duration::from_millis(10)).await,
				}
			}
		},
		None => {
			log_error!(logger, "Failed to connect to peer: {}@{}", node_id, addr);
			Err(Error::ConnectionFailed)
		},
	}
}
