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
		let mut map = self.map.lock().expect("lock");

		if !map.contains_key(&peer_node_id) && map.len() >= Self::MAX_PEERS {
			return;
		}

		let queue = map.entry(peer_node_id).or_insert_with(VecDeque::new);
		if queue.len() >= Self::MAX_MESSAGES_PER_PEER {
			queue.pop_front();
		}
		queue.push_back(message);
	}

	pub(crate) fn onion_message_peer_connected(
		&self, peer_node_id: PublicKey,
	) -> Vec<OnionMessage> {
		let mut map = self.map.lock().expect("lock");

		if let Some(queue) = map.remove(&peer_node_id) {
			queue.into()
		} else {
			Vec::new()
		}
	}

	#[cfg(test)]
	pub(crate) fn is_empty(&self) -> bool {
		let map = self.map.lock().expect("lock");
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

		let peer_node_id = peer_node_id(12);
		let message = onion_message(13);
		mailbox.onion_message_intercepted(peer_node_id, message.clone());

		let messages = mailbox.onion_message_peer_connected(peer_node_id);
		assert_eq!(messages.len(), 1);
		assert_eq!(messages[0], message);

		assert!(mailbox.is_empty());

		let messages = mailbox.onion_message_peer_connected(peer_node_id);
		assert_eq!(messages.len(), 0);
	}

	#[test]
	fn onion_message_mailbox_keeps_existing_peer_at_capacity() {
		let mailbox = OnionMessageMailbox::new();
		let victim = peer_node_id(1);

		for seed in 0..OnionMessageMailbox::MAX_MESSAGES_PER_PEER {
			mailbox.onion_message_intercepted(victim, onion_message(seed as u64 + 1));
		}

		for peer in 2..(OnionMessageMailbox::MAX_PEERS as u64 + 2) {
			mailbox.onion_message_intercepted(peer_node_id(peer), onion_message(peer));
		}

		let messages = mailbox.onion_message_peer_connected(victim);
		assert_eq!(messages.len(), OnionMessageMailbox::MAX_MESSAGES_PER_PEER);
	}

	#[test]
	fn onion_message_mailbox_drops_new_peer_when_full() {
		let mailbox = OnionMessageMailbox::new();

		for peer in 1..=OnionMessageMailbox::MAX_PEERS as u64 {
			mailbox.onion_message_intercepted(peer_node_id(peer), onion_message(peer));
		}

		let new_peer = peer_node_id(OnionMessageMailbox::MAX_PEERS as u64 + 1);
		mailbox.onion_message_intercepted(new_peer, onion_message(1));
		assert!(mailbox.onion_message_peer_connected(new_peer).is_empty());

		let existing_peer = peer_node_id(1);
		mailbox.onion_message_intercepted(existing_peer, onion_message(2));
		assert_eq!(mailbox.onion_message_peer_connected(existing_peer).len(), 2);
	}

	fn peer_node_id(seed: u64) -> PublicKey {
		let secp = Secp256k1::new();
		let sk = secret_key(seed);
		PublicKey::from_secret_key(&secp, &sk)
	}

	fn onion_message(seed: u64) -> lightning::ln::msgs::OnionMessage {
		let secp = Secp256k1::new();
		let blinding_point = PublicKey::from_secret_key(&secp, &secret_key(seed));
		let message_point = PublicKey::from_secret_key(&secp, &secret_key(seed + 1));

		lightning::ln::msgs::OnionMessage {
			blinding_point,
			onion_routing_packet: onion_message::packet::Packet {
				version: 0,
				public_key: message_point,
				hop_data: vec![1, 2, 3],
				hmac: [0; 32],
			},
		}
	}

	fn secret_key(seed: u64) -> SecretKey {
		let mut bytes = [0; 32];
		bytes[24..].copy_from_slice(&seed.to_be_bytes());
		SecretKey::from_slice(&bytes).unwrap()
	}
}
