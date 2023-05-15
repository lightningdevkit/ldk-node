use super::*;
use crate::WALLET_KEYS_SEED_LEN;

use crate::logger::log_error;
use crate::peer_store::PeerStore;
use crate::{Error, EventQueue, PaymentDetails};

use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::keysinterface::{EntropySource, SignerProvider};
use lightning::routing::gossip::NetworkGraph;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringParameters};
use lightning::util::logger::Logger;
use lightning::util::ser::{Readable, ReadableArgs, Writeable};

use bitcoin::hash_types::{BlockHash, Txid};
use bitcoin::hashes::hex::FromHex;
use rand::{thread_rng, RngCore};

use std::fs;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;

use super::KVStore;

pub(crate) fn read_or_generate_seed_file(keys_seed_path: &str) -> [u8; WALLET_KEYS_SEED_LEN] {
	if Path::new(&keys_seed_path).exists() {
		let seed = fs::read(keys_seed_path).expect("Failed to read keys seed file");
		assert_eq!(
			seed.len(),
			WALLET_KEYS_SEED_LEN,
			"Failed to read keys seed file: unexpected length"
		);
		let mut key = [0; WALLET_KEYS_SEED_LEN];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; WALLET_KEYS_SEED_LEN];
		thread_rng().fill_bytes(&mut key);

		let mut f = fs::File::create(keys_seed_path).expect("Failed to create keys seed file");
		f.write_all(&key).expect("Failed to write node keys seed to disk");
		f.sync_all().expect("Failed to sync node keys seed to disk");
		key
	}
}

/// Read previously persisted [`ChannelMonitor`]s from the store.
pub(crate) fn read_channel_monitors<K: Deref, ES: Deref, SP: Deref>(
	kv_store: K, entropy_source: ES, signer_provider: SP,
) -> std::io::Result<Vec<(BlockHash, ChannelMonitor<<SP::Target as SignerProvider>::Signer>)>>
where
	K::Target: KVStore,
	ES::Target: EntropySource + Sized,
	SP::Target: SignerProvider + Sized,
{
	let mut res = Vec::new();

	for stored_key in kv_store.list(CHANNEL_MONITOR_PERSISTENCE_NAMESPACE)? {
		let txid = Txid::from_hex(stored_key.split_at(64).0).map_err(|_| {
			std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid tx ID in stored key")
		})?;

		let index: u16 = stored_key.split_at(65).1.parse().map_err(|_| {
			std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid tx index in stored key")
		})?;

		match <(BlockHash, ChannelMonitor<<SP::Target as SignerProvider>::Signer>)>::read(
			&mut kv_store.read(CHANNEL_MONITOR_PERSISTENCE_NAMESPACE, &stored_key)?,
			(&*entropy_source, &*signer_provider),
		) {
			Ok((block_hash, channel_monitor)) => {
				if channel_monitor.get_funding_txo().0.txid != txid
					|| channel_monitor.get_funding_txo().0.index != index
				{
					return Err(std::io::Error::new(
						std::io::ErrorKind::InvalidData,
						"ChannelMonitor was stored under the wrong key",
					));
				}
				res.push((block_hash, channel_monitor));
			}
			Err(e) => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					format!("Failed to deserialize ChannelMonitor: {}", e),
				))
			}
		}
	}
	Ok(res)
}

/// Read a previously persisted [`NetworkGraph`] from the store.
pub(crate) fn read_network_graph<K: Deref, L: Deref>(
	kv_store: K, logger: L,
) -> Result<NetworkGraph<L>, std::io::Error>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	let mut reader =
		kv_store.read(NETWORK_GRAPH_PERSISTENCE_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_KEY)?;
	let graph = NetworkGraph::read(&mut reader, logger).map_err(|_| {
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NetworkGraph")
	})?;
	Ok(graph)
}

/// Read a previously persisted [`Scorer`] from the store.
pub(crate) fn read_scorer<K: Deref, G: Deref<Target = NetworkGraph<L>>, L: Deref>(
	kv_store: K, network_graph: G, logger: L,
) -> Result<ProbabilisticScorer<G, L>, std::io::Error>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	let params = ProbabilisticScoringParameters::default();
	let mut reader = kv_store.read(SCORER_PERSISTENCE_NAMESPACE, SCORER_PERSISTENCE_KEY)?;
	let args = (params, network_graph, logger);
	let scorer = ProbabilisticScorer::read(&mut reader, args).map_err(|_| {
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize Scorer")
	})?;
	Ok(scorer)
}

/// Read previously persisted events from the store.
pub(crate) fn read_event_queue<K: Deref, L: Deref>(
	kv_store: K, logger: L,
) -> Result<EventQueue<K, L>, std::io::Error>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	let mut reader =
		kv_store.read(EVENT_QUEUE_PERSISTENCE_NAMESPACE, EVENT_QUEUE_PERSISTENCE_KEY)?;
	let event_queue = EventQueue::read(&mut reader, (kv_store, logger)).map_err(|_| {
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize EventQueue")
	})?;
	Ok(event_queue)
}

/// Read previously persisted peer info from the store.
pub(crate) fn read_peer_info<K: Deref, L: Deref>(
	kv_store: K, logger: L,
) -> Result<PeerStore<K, L>, std::io::Error>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	let mut reader = kv_store.read(PEER_INFO_PERSISTENCE_NAMESPACE, PEER_INFO_PERSISTENCE_KEY)?;
	let peer_info = PeerStore::read(&mut reader, (kv_store, logger)).map_err(|_| {
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize PeerStore")
	})?;
	Ok(peer_info)
}

/// Read previously persisted payments information from the store.
pub(crate) fn read_payments<K: Deref>(kv_store: K) -> Result<Vec<PaymentDetails>, std::io::Error>
where
	K::Target: KVStore,
{
	let mut res = Vec::new();

	for stored_key in kv_store.list(PAYMENT_INFO_PERSISTENCE_NAMESPACE)? {
		let payment = PaymentDetails::read(
			&mut kv_store.read(PAYMENT_INFO_PERSISTENCE_NAMESPACE, &stored_key)?,
		)
		.map_err(|_| {
			std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize Payment")
		})?;
		res.push(payment);
	}
	Ok(res)
}

pub(crate) fn read_latest_rgs_sync_timestamp<K: Deref>(kv_store: K) -> Result<u32, std::io::Error>
where
	K::Target: KVStore,
{
	let mut reader =
		kv_store.read(LATEST_RGS_SYNC_TIMESTAMP_NAMESPACE, LATEST_RGS_SYNC_TIMESTAMP_KEY)?;
	u32::read(&mut reader).map_err(|_| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Failed to deserialize latest RGS sync timestamp",
		)
	})
}

pub(crate) fn write_latest_rgs_sync_timestamp<K: Deref, L: Deref>(
	updated_timestamp: u32, kv_store: K, logger: L,
) -> Result<(), Error>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	let data = updated_timestamp.encode();
	kv_store
		.write(LATEST_RGS_SYNC_TIMESTAMP_NAMESPACE, LATEST_RGS_SYNC_TIMESTAMP_KEY, &data)
		.map_err(|e| {
			log_error!(
				logger,
				"Writing data to key {}/{} failed due to: {}",
				LATEST_RGS_SYNC_TIMESTAMP_NAMESPACE,
				LATEST_RGS_SYNC_TIMESTAMP_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
	Ok(())
}

pub(crate) fn read_latest_node_ann_bcast_timestamp<K: Deref>(
	kv_store: K,
) -> Result<u64, std::io::Error>
where
	K::Target: KVStore,
{
	let mut reader = kv_store
		.read(LATEST_NODE_ANN_BCAST_TIMSTAMP_NAMESPACE, LATEST_NODE_ANN_BCAST_TIMSTAMP_KEY)?;
	u64::read(&mut reader).map_err(|_| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Failed to deserialize latest node announcment broadcast timestamp",
		)
	})
}

pub(crate) fn write_latest_node_ann_bcast_timestamp<K: Deref, L: Deref>(
	updated_timestamp: u64, kv_store: K, logger: L,
) -> Result<(), Error>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	let mut writer = kv_store
		.write(LATEST_NODE_ANN_BCAST_TIMSTAMP_NAMESPACE, LATEST_NODE_ANN_BCAST_TIMSTAMP_KEY)
		.map_err(|e| {
			log_error!(
				logger,
				"Getting writer for key {}/{} failed due to: {}",
				LATEST_NODE_ANN_BCAST_TIMSTAMP_NAMESPACE,
				LATEST_NODE_ANN_BCAST_TIMSTAMP_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
	updated_timestamp.write(&mut writer).map_err(|e| {
		log_error!(
			logger,
			"Writing data to key {}/{} failed due to: {}",
			LATEST_NODE_ANN_BCAST_TIMSTAMP_NAMESPACE,
			LATEST_NODE_ANN_BCAST_TIMSTAMP_KEY,
			e
		);
		Error::PersistenceFailed
	})?;
	writer.commit().map_err(|e| {
		log_error!(
			logger,
			"Committing data to key {}/{} failed due to: {}",
			LATEST_NODE_ANN_BCAST_TIMSTAMP_NAMESPACE,
			LATEST_NODE_ANN_BCAST_TIMSTAMP_KEY,
			e
		);
		Error::PersistenceFailed
	})
}
