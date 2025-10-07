use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::msgs::OnionMessage;

pub(crate) struct OnionMessageMailbox {
	map: Mutex<HashMap<PublicKey, VecDeque<OnionMessage>>>,
}

impl OnionMessageMailbox {
	const MAX_MESSAGES_PER_PEER: usize = 30;
	const MAX_PEERS: usize = 300;

	pub fn new() -> Self {
		Self { map: Mutex::new(HashMap::with_capacity(Self::MAX_PEERS)) }
	}

	pub(crate) fn onion_message_intercepted(&self, peer_node_id: PublicKey, message: OnionMessage) {
		let mut map = self.map.lock().unwrap();

		let queue = map.entry(peer_node_id).or_insert_with(VecDeque::new);
		if queue.len() >= Self::MAX_MESSAGES_PER_PEER {
			queue.pop_front();
		}
		queue.push_back(message);

		// Enforce a peers limit. If exceeded, evict the peer with the longest queue.
		if map.len() > Self::MAX_PEERS {
			let peer_to_remove =
				map.iter().max_by_key(|(_, queue)| queue.len()).map(|(peer, _)| *peer).unwrap();

			map.remove(&peer_to_remove);
		}
	}

	pub(crate) fn onion_message_peer_connected(
		&self, peer_node_id: PublicKey,
	) -> Vec<OnionMessage> {
		let mut map = self.map.lock().unwrap();

		if let Some(queue) = map.remove(&peer_node_id) {
			queue.into()
		} else {
			Vec::new()
		}
	}

	#[cfg(test)]
	pub(crate) fn is_empty(&self) -> bool {
		let map = self.map.lock().unwrap();
		map.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::key::Secp256k1;
	use bitcoin::secp256k1::{PublicKey, SecretKey};
	use lightning::onion_message;

	use crate::payment::asynchronous::om_mailbox::OnionMessageMailbox;

	#[test]
	fn onion_message_mailbox() {
		let mailbox = OnionMessageMailbox::new();

		let secp = Secp256k1::new();
		let sk_bytes = [12; 32];
		let sk = SecretKey::from_slice(&sk_bytes).unwrap();
		let peer_node_id = PublicKey::from_secret_key(&secp, &sk);

		let blinding_sk = SecretKey::from_slice(&[13; 32]).unwrap();
		let blinding_point = PublicKey::from_secret_key(&secp, &blinding_sk);

		let message_sk = SecretKey::from_slice(&[13; 32]).unwrap();
		let message_point = PublicKey::from_secret_key(&secp, &message_sk);

		let message = lightning::ln::msgs::OnionMessage {
			blinding_point,
			onion_routing_packet: onion_message::packet::Packet {
				version: 0,
				public_key: message_point,
				hop_data: vec![1, 2, 3],
				hmac: [0; 32],
			},
		};
		mailbox.onion_message_intercepted(peer_node_id, message.clone());

		let messages = mailbox.onion_message_peer_connected(peer_node_id);
		assert_eq!(messages.len(), 1);
		assert_eq!(messages[0], message);

		assert!(mailbox.is_empty());

		let messages = mailbox.onion_message_peer_connected(peer_node_id);
		assert_eq!(messages.len(), 0);
	}
}
