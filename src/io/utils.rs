// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::Deref;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use bdk_chain::indexer::keychain_txout::ChangeSet as BdkIndexerChangeSet;
use bdk_chain::local_chain::ChangeSet as BdkLocalChainChangeSet;
use bdk_chain::miniscript::{Descriptor, DescriptorPublicKey};
use bdk_chain::tx_graph::ChangeSet as BdkTxGraphChangeSet;
use bdk_chain::ConfirmationBlockTime;
use bdk_wallet::ChangeSet as BdkWalletChangeSet;
use bitcoin::Network;
use lightning::ln::msgs::DecodeError;
use lightning::routing::gossip::NetworkGraph;
use lightning::routing::scoring::{
	ChannelLiquidities, ProbabilisticScorer, ProbabilisticScoringDecayParameters,
};
use lightning::util::persist::{
	KVStore, KVStoreSync, KVSTORE_NAMESPACE_KEY_ALPHABET, KVSTORE_NAMESPACE_KEY_MAX_LEN,
	NETWORK_GRAPH_PERSISTENCE_KEY, NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
	NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_KEY,
	OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
	SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
	SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{Readable, ReadableArgs, Writeable};
use lightning_types::string::PrintableString;
use rand::rngs::OsRng;
use rand::TryRngCore;

use super::*;
use crate::chain::ChainSource;
use crate::config::WALLET_KEYS_SEED_LEN;
use crate::fee_estimator::OnchainFeeEstimator;
use crate::io::{
	CHANNEL_FORWARDING_STATS_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE,
	CHANNEL_PAIR_FORWARDING_STATS_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_PAIR_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::io::{
	NODE_METRICS_KEY, NODE_METRICS_PRIMARY_NAMESPACE, NODE_METRICS_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::payment::{
	ChannelForwardingStats, ChannelPairForwardingStats, ForwardedPaymentDetails,
	PendingPaymentDetails,
};
use crate::peer_store::PeerStore;
use crate::types::{Broadcaster, DynStore, KeysManager, Sweeper};
use crate::wallet::ser::{ChangeSetDeserWrapper, ChangeSetSerWrapper};
use crate::{Error, EventQueue, NodeMetrics, PaymentDetails};

pub const EXTERNAL_PATHFINDING_SCORES_CACHE_KEY: &str = "external_pathfinding_scores_cache";

pub(crate) fn read_or_generate_seed_file(
	keys_seed_path: &str,
) -> std::io::Result<[u8; WALLET_KEYS_SEED_LEN]> {
	if Path::new(&keys_seed_path).exists() {
		let seed = fs::read(keys_seed_path)?;

		if seed.len() != WALLET_KEYS_SEED_LEN {
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
		OsRng.try_fill_bytes(&mut key).map_err(|_| {
			std::io::Error::new(std::io::ErrorKind::Other, "Failed to generate seed bytes")
		})?;

		if let Some(parent_dir) = Path::new(&keys_seed_path).parent() {
			fs::create_dir_all(parent_dir)?;
		}

		#[cfg(unix)]
		let mut f = OpenOptions::new().write(true).create_new(true).mode(0o400).open(keys_seed_path)?;

		#[cfg(not(unix))]
		let mut f = OpenOptions::new().write(true).create_new(true).open(keys_seed_path)?;

		f.write_all(&key)?;

		f.sync_all()?;
		Ok(key)
	}
}

/// Read a previously persisted [`NetworkGraph`] from the store.
pub(crate) async fn read_network_graph<L: Deref + Clone>(
	kv_store: &DynStore, logger: L,
) -> Result<NetworkGraph<L>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let reader = KVStore::read(
		&*kv_store,
		NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_KEY,
	)
	.await?;
	NetworkGraph::read(&mut &*reader, logger.clone()).map_err(|e| {
		log_error!(logger, "Failed to deserialize NetworkGraph: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NetworkGraph")
	})
}

/// Read a previously persisted [`ProbabilisticScorer`] from the store.
pub(crate) async fn read_scorer<G: Deref<Target = NetworkGraph<L>>, L: Deref + Clone>(
	kv_store: &DynStore, network_graph: G, logger: L,
) -> Result<ProbabilisticScorer<G, L>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let params = ProbabilisticScoringDecayParameters::default();
	let reader = KVStore::read(
		&*kv_store,
		SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
		SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
		SCORER_PERSISTENCE_KEY,
	)
	.await?;
	let args = (params, network_graph, logger.clone());
	ProbabilisticScorer::read(&mut &*reader, args).map_err(|e| {
		log_error!(logger, "Failed to deserialize scorer: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize Scorer")
	})
}

/// Read previously persisted external pathfinding scores from the cache.
pub(crate) async fn read_external_pathfinding_scores_from_cache<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<ChannelLiquidities, std::io::Error>
where
	L::Target: LdkLogger,
{
	let reader = KVStore::read(
		&*kv_store,
		SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
		SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
		EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
	)
	.await?;
	ChannelLiquidities::read(&mut &*reader).map_err(|e| {
		log_error!(logger, "Failed to deserialize scorer: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize Scorer")
	})
}

/// Persist external pathfinding scores to the cache.
pub(crate) async fn write_external_pathfinding_scores_to_cache<L: Deref>(
	kv_store: &DynStore, data: &ChannelLiquidities, logger: L,
) -> Result<(), Error>
where
	L::Target: LdkLogger,
{
	KVStore::write(
		&*kv_store,
		SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
		SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
		EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
		data.encode(),
	)
	.await
	.map_err(|e| {
		log_error!(
			logger,
			"Writing data to key {}/{}/{} failed due to: {}",
			NODE_METRICS_PRIMARY_NAMESPACE,
			NODE_METRICS_SECONDARY_NAMESPACE,
			EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
			e
		);
		Error::PersistenceFailed
	})
}

/// Read previously persisted events from the store.
pub(crate) async fn read_event_queue<L: Deref + Clone>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<EventQueue<L>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let reader = KVStore::read(
		&*kv_store,
		EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
		EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
		EVENT_QUEUE_PERSISTENCE_KEY,
	)
	.await?;
	EventQueue::read(&mut &*reader, (kv_store, logger.clone())).map_err(|e| {
		log_error!(logger, "Failed to deserialize event queue: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize EventQueue")
	})
}

/// Read previously persisted peer info from the store.
pub(crate) async fn read_peer_info<L: Deref + Clone>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<PeerStore<L>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let reader = KVStore::read(
		&*kv_store,
		PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		PEER_INFO_PERSISTENCE_KEY,
	)
	.await?;
	PeerStore::read(&mut &*reader, (kv_store, logger.clone())).map_err(|e| {
		log_error!(logger, "Failed to deserialize peer store: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize PeerStore")
	})
}

/// Generic helper to read persisted items from a KV store namespace.
async fn read_objects_from_store<T, L: Deref>(
	kv_store: &DynStore, logger: L, primary_namespace: &str, secondary_namespace: &str,
) -> Result<Vec<T>, std::io::Error>
where
	T: Readable,
	L::Target: LdkLogger,
{
	let mut stored_keys = KVStore::list(&*kv_store, primary_namespace, secondary_namespace).await?;

	let mut res = Vec::with_capacity(stored_keys.len());

	const BATCH_SIZE: usize = 50;

	let mut set = tokio::task::JoinSet::new();

	// Fill JoinSet with tasks if possible
	while set.len() < BATCH_SIZE && !stored_keys.is_empty() {
		if let Some(next_key) = stored_keys.pop() {
			let fut = KVStore::read(&*kv_store, primary_namespace, secondary_namespace, &next_key);
			set.spawn(fut);
			debug_assert!(set.len() <= BATCH_SIZE);
		}
	}

	let type_name = std::any::type_name::<T>();

	while let Some(read_res) = set.join_next().await {
		// Exit early if we get an IO error.
		let reader = read_res
			.map_err(|e| {
				log_error!(logger, "Failed to read {type_name}: {e}");
				set.abort_all();
				e
			})?
			.map_err(|e| {
				log_error!(logger, "Failed to read {type_name}: {e}");
				set.abort_all();
				e
			})?;

		// Refill set for every finished future, if we still have something to do.
		if let Some(next_key) = stored_keys.pop() {
			let fut = KVStore::read(&*kv_store, primary_namespace, secondary_namespace, &next_key);
			set.spawn(fut);
			debug_assert!(set.len() <= BATCH_SIZE);
		}

		// Handle result.
		let item = T::read(&mut &*reader).map_err(|e| {
			log_error!(logger, "Failed to deserialize {type_name}: {e}");
			std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				format!("Failed to deserialize {type_name}"),
			)
		})?;
		res.push(item);
	}

	debug_assert!(set.is_empty());
	debug_assert!(stored_keys.is_empty());

	Ok(res)
}

/// Read previously persisted payments information from the store.
pub(crate) async fn read_payments<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<Vec<PaymentDetails>, std::io::Error>
where
	L::Target: LdkLogger,
{
	read_objects_from_store(
		kv_store,
		logger,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	)
	.await
}

/// Read previously persisted forwarded payments information from the store.
pub(crate) async fn read_forwarded_payments<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<Vec<ForwardedPaymentDetails>, std::io::Error>
where
	L::Target: LdkLogger,
{
	read_objects_from_store(
		kv_store,
		logger,
		FORWARDED_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		FORWARDED_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	)
	.await
}

/// Read previously persisted channel forwarding stats from the store.
pub(crate) async fn read_channel_forwarding_stats<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<Vec<ChannelForwardingStats>, std::io::Error>
where
	L::Target: LdkLogger,
{
	read_objects_from_store(
		kv_store,
		logger,
		CHANNEL_FORWARDING_STATS_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE,
	)
	.await
}

/// Read previously persisted channel pair forwarding stats from the store.
pub(crate) async fn read_channel_pair_forwarding_stats<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<Vec<ChannelPairForwardingStats>, std::io::Error>
where
	L::Target: LdkLogger,
{
	read_objects_from_store(
		kv_store,
		logger,
		CHANNEL_PAIR_FORWARDING_STATS_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_PAIR_FORWARDING_STATS_PERSISTENCE_SECONDARY_NAMESPACE,
	)
	.await
}

/// Read `OutputSweeper` state from the store.
pub(crate) async fn read_output_sweeper(
	broadcaster: Arc<Broadcaster>, fee_estimator: Arc<OnchainFeeEstimator>,
	chain_data_source: Arc<ChainSource>, keys_manager: Arc<KeysManager>, kv_store: Arc<DynStore>,
	logger: Arc<Logger>,
) -> Result<Sweeper, std::io::Error> {
	let reader = KVStore::read(
		&*kv_store,
		OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_KEY,
	)
	.await?;
	let args = (
		broadcaster,
		fee_estimator,
		Some(chain_data_source),
		Arc::clone(&keys_manager),
		keys_manager,
		kv_store,
		logger.clone(),
	);
	let (_, sweeper) = <(_, Sweeper)>::read(&mut &*reader, args).map_err(|e| {
		log_error!(logger, "Failed to deserialize OutputSweeper: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize OutputSweeper")
	})?;
	Ok(sweeper)
}

pub(crate) async fn read_node_metrics<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<NodeMetrics, std::io::Error>
where
	L::Target: LdkLogger,
{
	let reader = KVStore::read(
		&*kv_store,
		NODE_METRICS_PRIMARY_NAMESPACE,
		NODE_METRICS_SECONDARY_NAMESPACE,
		NODE_METRICS_KEY,
	)
	.await?;
	NodeMetrics::read(&mut &*reader).map_err(|e| {
		log_error!(logger, "Failed to deserialize NodeMetrics: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NodeMetrics")
	})
}

pub(crate) fn write_node_metrics<L: Deref>(
	node_metrics: &NodeMetrics, kv_store: &DynStore, logger: L,
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
			kv_store: &DynStore, logger: L,
		) -> Result<Option<$change_set_type>, std::io::Error>
		where
			L::Target: LdkLogger,
		{
			let reader =
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

			let res: Result<ChangeSetDeserWrapper<$change_set_type>, DecodeError> =
				Readable::read(&mut &*reader);
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
			value: &$change_set_type, kv_store: &DynStore, logger: L,
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
	kv_store: &DynStore, logger: &Logger,
) -> Result<Option<BdkWalletChangeSet>, std::io::Error> {
	let mut change_set = BdkWalletChangeSet::default();

	// We require a descriptor and return `None` to signal creation of a new wallet otherwise.
	if let Some(descriptor) = read_bdk_wallet_descriptor(kv_store, logger)? {
		change_set.descriptor = Some(descriptor);
	} else {
		return Ok(None);
	}

	// We require a change_descriptor and return `None` to signal creation of a new wallet otherwise.
	if let Some(change_descriptor) = read_bdk_wallet_change_descriptor(kv_store, logger)? {
		change_set.change_descriptor = Some(change_descriptor);
	} else {
		return Ok(None);
	}

	// We require a network and return `None` to signal creation of a new wallet otherwise.
	if let Some(network) = read_bdk_wallet_network(kv_store, logger)? {
		change_set.network = Some(network);
	} else {
		return Ok(None);
	}

	read_bdk_wallet_local_chain(&*kv_store, logger)?
		.map(|local_chain| change_set.local_chain = local_chain);
	read_bdk_wallet_tx_graph(&*kv_store, logger)?.map(|tx_graph| change_set.tx_graph = tx_graph);
	read_bdk_wallet_indexer(&*kv_store, logger)?.map(|indexer| change_set.indexer = indexer);
	Ok(Some(change_set))
}

/// Read previously persisted pending payments information from the store.
pub(crate) async fn read_pending_payments<L: Deref>(
	kv_store: &DynStore, logger: L,
) -> Result<Vec<PendingPaymentDetails>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let mut res = Vec::new();

	let mut stored_keys = KVStore::list(
		&*kv_store,
		PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	)
	.await?;

	const BATCH_SIZE: usize = 50;

	let mut set = tokio::task::JoinSet::new();

	// Fill JoinSet with tasks if possible
	while set.len() < BATCH_SIZE && !stored_keys.is_empty() {
		if let Some(next_key) = stored_keys.pop() {
			let fut = KVStore::read(
				&*kv_store,
				PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&next_key,
			);
			set.spawn(fut);
			debug_assert!(set.len() <= BATCH_SIZE);
		}
	}

	while let Some(read_res) = set.join_next().await {
		// Exit early if we get an IO error.
		let reader = read_res
			.map_err(|e| {
				log_error!(logger, "Failed to read PendingPaymentDetails: {}", e);
				set.abort_all();
				e
			})?
			.map_err(|e| {
				log_error!(logger, "Failed to read PendingPaymentDetails: {}", e);
				set.abort_all();
				e
			})?;

		// Refill set for every finished future, if we still have something to do.
		if let Some(next_key) = stored_keys.pop() {
			let fut = KVStore::read(
				&*kv_store,
				PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&next_key,
			);
			set.spawn(fut);
			debug_assert!(set.len() <= BATCH_SIZE);
		}

		// Handle result.
		let pending_payment = PendingPaymentDetails::read(&mut &*reader).map_err(|e| {
			log_error!(logger, "Failed to deserialize PendingPaymentDetails: {}", e);
			std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"Failed to deserialize PendingPaymentDetails",
			)
		})?;
		res.push(pending_payment);
	}

	debug_assert!(set.is_empty());
	debug_assert!(stored_keys.is_empty());

	Ok(res)
}

#[cfg(test)]
mod tests {
	use super::read_or_generate_seed_file;
	use super::test_utils::random_storage_path;

	#[test]
	fn generated_seed_is_readable() {
		let mut rand_path = random_storage_path();
		rand_path.push("test_keys_seed");
		let expected_seed_bytes = read_or_generate_seed_file(&rand_path.to_str().unwrap()).unwrap();
		let read_seed_bytes = read_or_generate_seed_file(&rand_path.to_str().unwrap()).unwrap();
		assert_eq!(expected_seed_bytes, read_seed_bytes);
	}
}
