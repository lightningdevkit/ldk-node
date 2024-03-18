use super::*;
use crate::config::WALLET_KEYS_SEED_LEN;

use crate::logger::log_error;
use crate::peer_store::PeerStore;
use crate::sweep::SpendableOutputInfo;
use crate::types::DynStore;
use crate::{Error, EventQueue, PaymentDetails};

use lightning::routing::gossip::NetworkGraph;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringDecayParameters};
use lightning::util::logger::Logger;
use lightning::util::persist::{
	KVSTORE_NAMESPACE_KEY_ALPHABET, KVSTORE_NAMESPACE_KEY_MAX_LEN, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
	SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{Readable, ReadableArgs, Writeable};
use lightning::util::string::PrintableString;

use bip39::Mnemonic;
use rand::{thread_rng, RngCore};

use std::fs;
use std::io::{Cursor, Write};
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

/// Generates a random [BIP 39] mnemonic.
///
/// The result may be used to initialize the [`Node`] entropy, i.e., can be given to
/// [`Builder::set_entropy_bip39_mnemonic`].
///
/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
/// [`Node`]: crate::Node
/// [`Builder::set_entropy_bip39_mnemonic`]: crate::Builder::set_entropy_bip39_mnemonic
pub fn generate_entropy_mnemonic() -> Mnemonic {
	// bip39::Mnemonic supports 256 bit entropy max
	let mut entropy = [0; 32];
	thread_rng().fill_bytes(&mut entropy);
	Mnemonic::from_entropy(&entropy).unwrap()
}

pub(crate) fn read_or_generate_seed_file<L: Deref>(
	keys_seed_path: &str, logger: L,
) -> std::io::Result<[u8; WALLET_KEYS_SEED_LEN]>
where
	L::Target: Logger,
{
	if Path::new(&keys_seed_path).exists() {
		let seed = fs::read(keys_seed_path).map_err(|e| {
			log_error!(logger, "Failed to read keys seed file: {}", keys_seed_path);
			e
		})?;

		if seed.len() != WALLET_KEYS_SEED_LEN {
			log_error!(
				logger,
				"Failed to read keys seed file due to invalid length: {}",
				keys_seed_path
			);
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"Failed to read keys seed file due to invalid length",
			));
		}

		let mut key = [0; WALLET_KEYS_SEED_LEN];
		key.copy_from_slice(&seed);
		Ok(key)
	} else {
		let mut key = [0; WALLET_KEYS_SEED_LEN];
		thread_rng().fill_bytes(&mut key);

		let mut f = fs::File::create(keys_seed_path).map_err(|e| {
			log_error!(logger, "Failed to create keys seed file: {}", keys_seed_path);
			e
		})?;

		f.write_all(&key).map_err(|e| {
			log_error!(logger, "Failed to write node keys seed to disk: {}", keys_seed_path);
			e
		})?;

		f.sync_all().map_err(|e| {
			log_error!(logger, "Failed to sync node keys seed to disk: {}", keys_seed_path);
			e
		})?;

		Ok(key)
	}
}

/// Read a previously persisted [`NetworkGraph`] from the store.
pub(crate) fn read_network_graph<L: Deref + Clone>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<NetworkGraph<L>, std::io::Error>
where
	L::Target: Logger,
{
	let mut reader = Cursor::new(kv_store.read(
		NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_KEY,
	)?);
	NetworkGraph::read(&mut reader, logger.clone()).map_err(|e| {
		log_error!(logger, "Failed to deserialize NetworkGraph: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NetworkGraph")
	})
}

/// Read a previously persisted [`ProbabilisticScorer`] from the store.
pub(crate) fn read_scorer<G: Deref<Target = NetworkGraph<L>>, L: Deref + Clone>(
	kv_store: Arc<DynStore>, network_graph: G, logger: L,
) -> Result<ProbabilisticScorer<G, L>, std::io::Error>
where
	L::Target: Logger,
{
	let params = ProbabilisticScoringDecayParameters::default();
	let mut reader = Cursor::new(kv_store.read(
		SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
		SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
		SCORER_PERSISTENCE_KEY,
	)?);
	let args = (params, network_graph, logger.clone());
	ProbabilisticScorer::read(&mut reader, args).map_err(|e| {
		log_error!(logger, "Failed to deserialize scorer: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize Scorer")
	})
}

/// Read previously persisted events from the store.
pub(crate) fn read_event_queue<L: Deref + Clone>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<EventQueue<L>, std::io::Error>
where
	L::Target: Logger,
{
	let mut reader = Cursor::new(kv_store.read(
		EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
		EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
		EVENT_QUEUE_PERSISTENCE_KEY,
	)?);
	EventQueue::read(&mut reader, (kv_store, logger.clone())).map_err(|e| {
		log_error!(logger, "Failed to deserialize event queue: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize EventQueue")
	})
}

/// Read previously persisted peer info from the store.
pub(crate) fn read_peer_info<L: Deref + Clone>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<PeerStore<L>, std::io::Error>
where
	L::Target: Logger,
{
	let mut reader = Cursor::new(kv_store.read(
		PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		PEER_INFO_PERSISTENCE_KEY,
	)?);
	PeerStore::read(&mut reader, (kv_store, logger.clone())).map_err(|e| {
		log_error!(logger, "Failed to deserialize peer store: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize PeerStore")
	})
}

/// Read previously persisted payments information from the store.
pub(crate) fn read_payments<L: Deref>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<Vec<PaymentDetails>, std::io::Error>
where
	L::Target: Logger,
{
	let mut res = Vec::new();

	for stored_key in kv_store.list(
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	)? {
		let mut reader = Cursor::new(kv_store.read(
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&stored_key,
		)?);
		let payment = PaymentDetails::read(&mut reader).map_err(|e| {
			log_error!(logger, "Failed to deserialize PaymentDetails: {}", e);
			std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"Failed to deserialize PaymentDetails",
			)
		})?;
		res.push(payment);
	}
	Ok(res)
}

/// Read previously persisted spendable output information from the store.
pub(crate) fn read_spendable_outputs<L: Deref>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<Vec<SpendableOutputInfo>, std::io::Error>
where
	L::Target: Logger,
{
	let mut res = Vec::new();

	for stored_key in kv_store.list(
		SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	)? {
		let mut reader = Cursor::new(kv_store.read(
			SPENDABLE_OUTPUT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			SPENDABLE_OUTPUT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&stored_key,
		)?);
		let output = SpendableOutputInfo::read(&mut reader).map_err(|e| {
			log_error!(logger, "Failed to deserialize SpendableOutputInfo: {}", e);
			std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"Failed to deserialize SpendableOutputInfo",
			)
		})?;
		res.push(output);
	}
	Ok(res)
}

pub(crate) fn read_latest_rgs_sync_timestamp<L: Deref>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<u32, std::io::Error>
where
	L::Target: Logger,
{
	let mut reader = Cursor::new(kv_store.read(
		LATEST_RGS_SYNC_TIMESTAMP_PRIMARY_NAMESPACE,
		LATEST_RGS_SYNC_TIMESTAMP_SECONDARY_NAMESPACE,
		LATEST_RGS_SYNC_TIMESTAMP_KEY,
	)?);
	u32::read(&mut reader).map_err(|e| {
		log_error!(logger, "Failed to deserialize latest RGS sync timestamp: {}", e);
		std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Failed to deserialize latest RGS sync timestamp",
		)
	})
}

pub(crate) fn write_latest_rgs_sync_timestamp<L: Deref>(
	updated_timestamp: u32, kv_store: Arc<DynStore>, logger: L,
) -> Result<(), Error>
where
	L::Target: Logger,
{
	let data = updated_timestamp.encode();
	kv_store
		.write(
			LATEST_RGS_SYNC_TIMESTAMP_PRIMARY_NAMESPACE,
			LATEST_RGS_SYNC_TIMESTAMP_SECONDARY_NAMESPACE,
			LATEST_RGS_SYNC_TIMESTAMP_KEY,
			&data,
		)
		.map_err(|e| {
			log_error!(
				logger,
				"Writing data to key {}/{}/{} failed due to: {}",
				LATEST_RGS_SYNC_TIMESTAMP_PRIMARY_NAMESPACE,
				LATEST_RGS_SYNC_TIMESTAMP_SECONDARY_NAMESPACE,
				LATEST_RGS_SYNC_TIMESTAMP_KEY,
				e
			);
			Error::PersistenceFailed
		})
}

pub(crate) fn read_latest_node_ann_bcast_timestamp<L: Deref>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<u64, std::io::Error>
where
	L::Target: Logger,
{
	let mut reader = Cursor::new(kv_store.read(
		LATEST_NODE_ANN_BCAST_TIMESTAMP_PRIMARY_NAMESPACE,
		LATEST_NODE_ANN_BCAST_TIMESTAMP_SECONDARY_NAMESPACE,
		LATEST_NODE_ANN_BCAST_TIMESTAMP_KEY,
	)?);
	u64::read(&mut reader).map_err(|e| {
		log_error!(
			logger,
			"Failed to deserialize latest node announcement broadcast timestamp: {}",
			e
		);
		std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"Failed to deserialize latest node announcement broadcast timestamp",
		)
	})
}

pub(crate) fn write_latest_node_ann_bcast_timestamp<L: Deref>(
	updated_timestamp: u64, kv_store: Arc<DynStore>, logger: L,
) -> Result<(), Error>
where
	L::Target: Logger,
{
	let data = updated_timestamp.encode();
	kv_store
		.write(
			LATEST_NODE_ANN_BCAST_TIMESTAMP_PRIMARY_NAMESPACE,
			LATEST_NODE_ANN_BCAST_TIMESTAMP_SECONDARY_NAMESPACE,
			LATEST_NODE_ANN_BCAST_TIMESTAMP_KEY,
			&data,
		)
		.map_err(|e| {
			log_error!(
				logger,
				"Writing data to key {}/{}/{} failed due to: {}",
				LATEST_NODE_ANN_BCAST_TIMESTAMP_PRIMARY_NAMESPACE,
				LATEST_NODE_ANN_BCAST_TIMESTAMP_SECONDARY_NAMESPACE,
				LATEST_NODE_ANN_BCAST_TIMESTAMP_KEY,
				e
			);
			Error::PersistenceFailed
		})
}

pub(crate) fn is_valid_kvstore_str(key: &str) -> bool {
	key.len() <= KVSTORE_NAMESPACE_KEY_MAX_LEN
		&& key.chars().all(|c| KVSTORE_NAMESPACE_KEY_ALPHABET.contains(c))
}

pub(crate) fn check_namespace_key_validity(
	primary_namespace: &str, secondary_namespace: &str, key: Option<&str>, operation: &str,
) -> Result<(), std::io::Error> {
	if let Some(key) = key {
		if key.is_empty() {
			debug_assert!(
				false,
				"Failed to {} {}/{}/{}: key may not be empty.",
				operation,
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace),
				PrintableString(key)
			);
			let msg = format!(
				"Failed to {} {}/{}/{}: key may not be empty.",
				operation,
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace),
				PrintableString(key)
			);
			return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
		}

		if primary_namespace.is_empty() && !secondary_namespace.is_empty() {
			debug_assert!(false,
				"Failed to {} {}/{}/{}: primary namespace may not be empty if a non-empty secondary namespace is given.",
				operation,
				PrintableString(primary_namespace), PrintableString(secondary_namespace), PrintableString(key));
			let msg = format!(
				"Failed to {} {}/{}/{}: primary namespace may not be empty if a non-empty secondary namespace is given.", operation,
				PrintableString(primary_namespace), PrintableString(secondary_namespace), PrintableString(key));
			return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
		}

		if !is_valid_kvstore_str(primary_namespace)
			|| !is_valid_kvstore_str(secondary_namespace)
			|| !is_valid_kvstore_str(key)
		{
			debug_assert!(
				false,
				"Failed to {} {}/{}/{}: primary namespace, secondary namespace, and key must be valid.",
				operation,
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace),
				PrintableString(key)
			);
			let msg = format!(
				"Failed to {} {}/{}/{}: primary namespace, secondary namespace, and key must be valid.",
				operation,
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace),
				PrintableString(key)
			);
			return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
		}
	} else {
		if primary_namespace.is_empty() && !secondary_namespace.is_empty() {
			debug_assert!(false,
				"Failed to {} {}/{}: primary namespace may not be empty if a non-empty secondary namespace is given.",
				operation, PrintableString(primary_namespace), PrintableString(secondary_namespace));
			let msg = format!(
				"Failed to {} {}/{}: primary namespace may not be empty if a non-empty secondary namespace is given.",
				operation, PrintableString(primary_namespace), PrintableString(secondary_namespace));
			return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
		}
		if !is_valid_kvstore_str(primary_namespace) || !is_valid_kvstore_str(secondary_namespace) {
			debug_assert!(
				false,
				"Failed to {} {}/{}: primary namespace and secondary namespace must be valid.",
				operation,
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace)
			);
			let msg = format!(
				"Failed to {} {}/{}: primary namespace and secondary namespace must be valid.",
				operation,
				PrintableString(primary_namespace),
				PrintableString(secondary_namespace)
			);
			return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mnemonic_to_entropy_to_mnemonic() {
		let mnemonic = generate_entropy_mnemonic();

		let entropy = mnemonic.to_entropy();
		assert_eq!(mnemonic, Mnemonic::from_entropy(&entropy).unwrap());
	}
}
