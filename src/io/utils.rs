// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::fs;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use bdk_chain::indexer::keychain_txout::ChangeSet as BdkIndexerChangeSet;
use bdk_chain::local_chain::ChangeSet as BdkLocalChainChangeSet;
use bdk_chain::miniscript::{Descriptor, DescriptorPublicKey};
use bdk_chain::tx_graph::ChangeSet as BdkTxGraphChangeSet;
use bdk_chain::ConfirmationBlockTime;
use bdk_wallet::ChangeSet as BdkWalletChangeSet;
use bip39::Mnemonic;
use bitcoin::Network;
use lightning::io::Cursor;
use lightning::ln::msgs::DecodeError;
use lightning::routing::gossip::NetworkGraph;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringDecayParameters};
use lightning::util::persist::{
	KVStoreSync, KVSTORE_NAMESPACE_KEY_ALPHABET, KVSTORE_NAMESPACE_KEY_MAX_LEN,
	NETWORK_GRAPH_PERSISTENCE_KEY, NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
	NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_KEY,
	OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
	SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{Readable, ReadableArgs, Writeable};
use lightning_types::string::PrintableString;
use rand::{thread_rng, RngCore};

use super::*;
use crate::chain::ChainSource;
use crate::config::WALLET_KEYS_SEED_LEN;
use crate::fee_estimator::OnchainFeeEstimator;
use crate::io::{
	NODE_METRICS_KEY, NODE_METRICS_PRIMARY_NAMESPACE, NODE_METRICS_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::peer_store::PeerStore;
use crate::types::{Broadcaster, DynStore, KeysManager, Sweeper};
use crate::wallet::ser::{ChangeSetDeserWrapper, ChangeSetSerWrapper};
use crate::{Error, EventQueue, NodeMetrics, PaymentDetails};

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
	L::Target: LdkLogger,
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

		if let Some(parent_dir) = Path::new(&keys_seed_path).parent() {
			fs::create_dir_all(parent_dir).map_err(|e| {
				log_error!(
					logger,
					"Failed to create parent directory for key seed file: {}.",
					keys_seed_path
				);
				e
			})?;
		}

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
	L::Target: LdkLogger,
{
	let mut reader = Cursor::new(KVStoreSync::read(
		&*kv_store,
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
	L::Target: LdkLogger,
{
	let params = ProbabilisticScoringDecayParameters::default();
	let mut reader = Cursor::new(KVStoreSync::read(
		&*kv_store,
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
	L::Target: LdkLogger,
{
	let mut reader = Cursor::new(KVStoreSync::read(
		&*kv_store,
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
	L::Target: LdkLogger,
{
	let mut reader = Cursor::new(KVStoreSync::read(
		&*kv_store,
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
	L::Target: LdkLogger,
{
	let mut res = Vec::new();

	for stored_key in KVStoreSync::list(
		&*kv_store,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	)? {
		let mut reader = Cursor::new(KVStoreSync::read(
			&*kv_store,
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

/// Read `OutputSweeper` state from the store.
pub(crate) fn read_output_sweeper(
	broadcaster: Arc<Broadcaster>, fee_estimator: Arc<OnchainFeeEstimator>,
	chain_data_source: Arc<ChainSource>, keys_manager: Arc<KeysManager>, kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
) -> Result<Sweeper, std::io::Error> {
	let mut reader = Cursor::new(KVStoreSync::read(
		&*kv_store,
		OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_KEY,
	)?);
	let args = (
		broadcaster,
		fee_estimator,
		Some(chain_data_source),
		Arc::clone(&keys_manager),
		keys_manager,
		kv_store,
		logger.clone(),
	);
	let (_, sweeper) = <(_, Sweeper)>::read(&mut reader, args).map_err(|e| {
		log_error!(logger, "Failed to deserialize OutputSweeper: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize OutputSweeper")
	})?;
	Ok(sweeper)
}

pub(crate) fn read_node_metrics<L: Deref>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<NodeMetrics, std::io::Error>
where
	L::Target: LdkLogger,
{
	let mut reader = Cursor::new(KVStoreSync::read(
		&*kv_store,
		NODE_METRICS_PRIMARY_NAMESPACE,
		NODE_METRICS_SECONDARY_NAMESPACE,
		NODE_METRICS_KEY,
	)?);
	NodeMetrics::read(&mut reader).map_err(|e| {
		log_error!(logger, "Failed to deserialize NodeMetrics: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NodeMetrics")
	})
}

pub(crate) fn write_node_metrics<L: Deref>(
	node_metrics: &NodeMetrics, kv_store: Arc<DynStore>, logger: L,
) -> Result<(), Error>
where
	L::Target: LdkLogger,
{
	let data = node_metrics.encode();
	KVStoreSync::write(
		&*kv_store,
		NODE_METRICS_PRIMARY_NAMESPACE,
		NODE_METRICS_SECONDARY_NAMESPACE,
		NODE_METRICS_KEY,
		data,
	)
	.map_err(|e| {
		log_error!(
			logger,
			"Writing data to key {}/{}/{} failed due to: {}",
			NODE_METRICS_PRIMARY_NAMESPACE,
			NODE_METRICS_SECONDARY_NAMESPACE,
			NODE_METRICS_KEY,
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

macro_rules! impl_read_write_change_set_type {
	(
		$read_name:ident,
		$write_name:ident,
		$change_set_type:ty,
		$primary_namespace:expr,
		$secondary_namespace:expr,
		$key:expr
	) => {
		pub(crate) fn $read_name<L: Deref>(
			kv_store: Arc<DynStore>, logger: L,
		) -> Result<Option<$change_set_type>, std::io::Error>
		where
			L::Target: LdkLogger,
		{
			let bytes =
				match KVStoreSync::read(&*kv_store, $primary_namespace, $secondary_namespace, $key)
				{
					Ok(bytes) => bytes,
					Err(e) => {
						if e.kind() == lightning::io::ErrorKind::NotFound {
							return Ok(None);
						} else {
							log_error!(
								logger,
								"Reading data from key {}/{}/{} failed due to: {}",
								$primary_namespace,
								$secondary_namespace,
								$key,
								e
							);
							return Err(e.into());
						}
					},
				};

			let mut reader = Cursor::new(bytes);
			let res: Result<ChangeSetDeserWrapper<$change_set_type>, DecodeError> =
				Readable::read(&mut reader);
			match res {
				Ok(res) => Ok(Some(res.0)),
				Err(e) => {
					log_error!(logger, "Failed to deserialize BDK wallet field: {}", e);
					Err(std::io::Error::new(
						std::io::ErrorKind::InvalidData,
						"Failed to deserialize BDK wallet field",
					))
				},
			}
		}

		pub(crate) fn $write_name<L: Deref>(
			value: &$change_set_type, kv_store: Arc<DynStore>, logger: L,
		) -> Result<(), std::io::Error>
		where
			L::Target: LdkLogger,
		{
			let data = ChangeSetSerWrapper(value).encode();
			KVStoreSync::write(&*kv_store, $primary_namespace, $secondary_namespace, $key, data)
				.map_err(|e| {
					log_error!(
						logger,
						"Writing data to key {}/{}/{} failed due to: {}",
						$primary_namespace,
						$secondary_namespace,
						$key,
						e
					);
					e.into()
				})
		}
	};
}

impl_read_write_change_set_type!(
	read_bdk_wallet_descriptor,
	write_bdk_wallet_descriptor,
	Descriptor<DescriptorPublicKey>,
	BDK_WALLET_DESCRIPTOR_PRIMARY_NAMESPACE,
	BDK_WALLET_DESCRIPTOR_SECONDARY_NAMESPACE,
	BDK_WALLET_DESCRIPTOR_KEY
);

impl_read_write_change_set_type!(
	read_bdk_wallet_change_descriptor,
	write_bdk_wallet_change_descriptor,
	Descriptor<DescriptorPublicKey>,
	BDK_WALLET_CHANGE_DESCRIPTOR_PRIMARY_NAMESPACE,
	BDK_WALLET_CHANGE_DESCRIPTOR_SECONDARY_NAMESPACE,
	BDK_WALLET_CHANGE_DESCRIPTOR_KEY
);

impl_read_write_change_set_type!(
	read_bdk_wallet_network,
	write_bdk_wallet_network,
	Network,
	BDK_WALLET_NETWORK_PRIMARY_NAMESPACE,
	BDK_WALLET_NETWORK_SECONDARY_NAMESPACE,
	BDK_WALLET_NETWORK_KEY
);

impl_read_write_change_set_type!(
	read_bdk_wallet_local_chain,
	write_bdk_wallet_local_chain,
	BdkLocalChainChangeSet,
	BDK_WALLET_LOCAL_CHAIN_PRIMARY_NAMESPACE,
	BDK_WALLET_LOCAL_CHAIN_SECONDARY_NAMESPACE,
	BDK_WALLET_LOCAL_CHAIN_KEY
);

impl_read_write_change_set_type!(
	read_bdk_wallet_tx_graph,
	write_bdk_wallet_tx_graph,
	BdkTxGraphChangeSet<ConfirmationBlockTime>,
	BDK_WALLET_TX_GRAPH_PRIMARY_NAMESPACE,
	BDK_WALLET_TX_GRAPH_SECONDARY_NAMESPACE,
	BDK_WALLET_TX_GRAPH_KEY
);

impl_read_write_change_set_type!(
	read_bdk_wallet_indexer,
	write_bdk_wallet_indexer,
	BdkIndexerChangeSet,
	BDK_WALLET_INDEXER_PRIMARY_NAMESPACE,
	BDK_WALLET_INDEXER_SECONDARY_NAMESPACE,
	BDK_WALLET_INDEXER_KEY
);

// Reads the full BdkWalletChangeSet or returns default fields
pub(crate) fn read_bdk_wallet_change_set(
	kv_store: Arc<DynStore>, logger: Arc<Logger>,
) -> Result<Option<BdkWalletChangeSet>, std::io::Error> {
	let mut change_set = BdkWalletChangeSet::default();

	// We require a descriptor and return `None` to signal creation of a new wallet otherwise.
	if let Some(descriptor) =
		read_bdk_wallet_descriptor(Arc::clone(&kv_store), Arc::clone(&logger))?
	{
		change_set.descriptor = Some(descriptor);
	} else {
		return Ok(None);
	}

	// We require a change_descriptor and return `None` to signal creation of a new wallet otherwise.
	if let Some(change_descriptor) =
		read_bdk_wallet_change_descriptor(Arc::clone(&kv_store), Arc::clone(&logger))?
	{
		change_set.change_descriptor = Some(change_descriptor);
	} else {
		return Ok(None);
	}

	// We require a network and return `None` to signal creation of a new wallet otherwise.
	if let Some(network) = read_bdk_wallet_network(Arc::clone(&kv_store), Arc::clone(&logger))? {
		change_set.network = Some(network);
	} else {
		return Ok(None);
	}

	read_bdk_wallet_local_chain(Arc::clone(&kv_store), Arc::clone(&logger))?
		.map(|local_chain| change_set.local_chain = local_chain);
	read_bdk_wallet_tx_graph(Arc::clone(&kv_store), Arc::clone(&logger))?
		.map(|tx_graph| change_set.tx_graph = tx_graph);
	read_bdk_wallet_indexer(Arc::clone(&kv_store), Arc::clone(&logger))?
		.map(|indexer| change_set.indexer = indexer);
	Ok(Some(change_set))
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
