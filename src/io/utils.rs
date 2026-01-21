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
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::secp256k1::Secp256k1;
use bitcoin::Network;
use lightning::io::Cursor;
use lightning::ln::msgs::DecodeError;
use lightning::routing::gossip::NetworkGraph;
use lightning::routing::scoring::ChannelLiquidities;
use lightning::util::persist::{
	KVStore, KVStoreSync, KVSTORE_NAMESPACE_KEY_ALPHABET, KVSTORE_NAMESPACE_KEY_MAX_LEN,
	SCORER_PERSISTENCE_PRIMARY_NAMESPACE, SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{Readable, ReadableArgs, Writeable};
use lightning_types::string::PrintableString;
use rand::rngs::OsRng;
use rand::TryRngCore;
use tokio::task::JoinSet;

use super::*;
use crate::config::WALLET_KEYS_SEED_LEN;
use crate::io::{
	NODE_METRICS_KEY, NODE_METRICS_PRIMARY_NAMESPACE, NODE_METRICS_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, LdkLogger, Logger};
use crate::types::{DynStore, WordCount};
use crate::wallet::ser::{ChangeSetDeserWrapper, ChangeSetSerWrapper};
use crate::{Error, NodeMetrics, PaymentDetails};

pub const EXTERNAL_PATHFINDING_SCORES_CACHE_KEY: &str = "external_pathfinding_scores_cache";
pub const NETWORK_GRAPH_LOCAL_CACHE_FILENAME: &str = "network_graph_cache";

/// Generates a random [BIP 39] mnemonic with the specified word count.
///
/// If no word count is specified, defaults to 24 words (256-bit entropy).
///
/// The result may be used to initialize the [`Node`] entropy, i.e., can be given to
/// [`Builder::set_entropy_bip39_mnemonic`].
///
/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
/// [`Node`]: crate::Node
/// [`Builder::set_entropy_bip39_mnemonic`]: crate::Builder::set_entropy_bip39_mnemonic
pub fn generate_entropy_mnemonic(word_count: Option<WordCount>) -> Mnemonic {
	let word_count = word_count.unwrap_or(WordCount::Words24).word_count();
	Mnemonic::generate(word_count).expect("Failed to generate mnemonic")
}

/// Derives the node secret key from a BIP39 mnemonic.
///
/// This is the same key that would be used by a [`Node`] built with this mnemonic via
/// [`Builder::set_entropy_bip39_mnemonic`].
///
/// The derivation follows LDK's KeysManager behavior:
/// 1. BIP39 seed (64 bytes) → BIP32 master key (32 bytes)
/// 2. Those 32 bytes as new seed → BIP32 master → derive m/0' → node_secret
///
/// [`Node`]: crate::Node
/// [`Builder::set_entropy_bip39_mnemonic`]: crate::Builder::set_entropy_bip39_mnemonic
pub fn derive_node_secret_from_mnemonic(
	mnemonic: String, passphrase: Option<String>,
) -> Result<Vec<u8>, Error> {
	let parsed_mnemonic = Mnemonic::parse(&mnemonic).map_err(|_| Error::InvalidMnemonic)?;
	let seed = parsed_mnemonic.to_seed(passphrase.as_deref().unwrap_or(""));

	let master_xpriv =
		Xpriv::new_master(Network::Bitcoin, &seed).map_err(|_| Error::InvalidMnemonic)?;

	let ldk_seed_bytes: [u8; 32] = master_xpriv.private_key.secret_bytes();

	let keys_manager_master =
		Xpriv::new_master(Network::Bitcoin, &ldk_seed_bytes).map_err(|_| Error::InvalidMnemonic)?;

	let node_secret_xpriv = keys_manager_master
		.derive_priv(&Secp256k1::new(), &[ChildNumber::from_hardened_idx(0).unwrap()])
		.map_err(|_| Error::InvalidMnemonic)?;

	Ok(node_secret_xpriv.private_key.secret_bytes().to_vec())
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
		OsRng.try_fill_bytes(&mut key).map_err(|e| {
			log_error!(logger, "Failed to generate entropy: {}", e);
			std::io::Error::new(std::io::ErrorKind::Other, "Failed to generate seed bytes")
		})?;

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

/// Read a previously persisted [`NetworkGraph`] from a local cache file.
pub(crate) fn read_network_graph_from_local_cache<L: Deref + Clone>(
	storage_dir_path: &str, logger: L,
) -> Result<NetworkGraph<L>, std::io::Error>
where
	L::Target: LdkLogger,
{
	let data = read_network_graph_bytes_from_local_cache(storage_dir_path)?;
	let mut reader = Cursor::new(data);
	NetworkGraph::read(&mut reader, logger.clone()).map_err(|e| {
		log_error!(logger, "Failed to deserialize NetworkGraph from local cache: {}", e);
		std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to deserialize NetworkGraph")
	})
}

/// Read raw bytes from the local network graph cache file.
/// Used by LocalGraphStore to intercept KVStore reads.
pub(crate) fn read_network_graph_bytes_from_local_cache(
	storage_dir_path: &str,
) -> Result<Vec<u8>, std::io::Error> {
	let cache_path = format!("{}/{}", storage_dir_path, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
	fs::read(&cache_path)
}

/// Write raw bytes to the local network graph cache file.
/// Used by LocalGraphStore to intercept KVStore writes.
pub(crate) fn write_network_graph_to_local_cache_bytes(
	storage_dir_path: &str, data: &[u8],
) -> Result<(), std::io::Error> {
	// Ensure the storage directory exists
	fs::create_dir_all(storage_dir_path)?;
	let cache_path = format!("{}/{}", storage_dir_path, NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
	fs::write(&cache_path, data)
}

/// Persist external pathfinding scores to the cache.
pub(crate) async fn write_external_pathfinding_scores_to_cache<L: Deref>(
	kv_store: Arc<DynStore>, data: &ChannelLiquidities, logger: L,
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

/// Read previously persisted payments information from the store (async version).
///
/// Uses parallel async reads to improve performance with remote stores like VSS.
pub(crate) async fn read_payments_async<L: Deref + Clone + Send + Sync + 'static>(
	kv_store: Arc<DynStore>, logger: L,
) -> Result<Vec<PaymentDetails>, std::io::Error>
where
	L::Target: LdkLogger,
{
	// First, list all payment keys (single round trip) - spawn_blocking for sync operation
	let kv_store_list = Arc::clone(&kv_store);
	let keys = tokio::task::spawn_blocking(move || {
		KVStoreSync::list(
			&*kv_store_list,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		)
	})
	.await
	.map_err(|e| {
		std::io::Error::new(std::io::ErrorKind::Other, format!("Task join error: {}", e))
	})??;

	if keys.is_empty() {
		return Ok(Vec::new());
	}

	// Execute all reads in parallel using JoinSet
	let mut join_set: JoinSet<Result<PaymentDetails, std::io::Error>> = JoinSet::new();

	for key in keys {
		let store = Arc::clone(&kv_store);
		let log = logger.clone();
		join_set.spawn(async move {
			let data = KVStore::read(
				&*store,
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
			)
			.await?;
			let mut reader = Cursor::new(data);
			PaymentDetails::read(&mut reader).map_err(|e| {
				log_error!(log, "Failed to deserialize PaymentDetails for key {}: {}", key, e);
				std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Failed to deserialize PaymentDetails",
				)
			})
		});
	}

	let mut payments = Vec::with_capacity(join_set.len());
	while let Some(result) = join_set.join_next().await {
		match result {
			Ok(Ok(payment)) => payments.push(payment),
			Ok(Err(e)) => return Err(e),
			Err(e) => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					format!("Task join error: {}", e),
				))
			},
		}
	}
	Ok(payments)
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
	use lightning::sign::KeysManager as LdkKeysManager;

	#[test]
	fn mnemonic_to_entropy_to_mnemonic() {
		// Test default (24 words)
		let mnemonic = generate_entropy_mnemonic(None);
		let entropy = mnemonic.to_entropy();
		assert_eq!(mnemonic, Mnemonic::from_entropy(&entropy).unwrap());
		assert_eq!(mnemonic.word_count(), 24);

		// Test with different word counts
		let word_counts = [
			WordCount::Words12,
			WordCount::Words15,
			WordCount::Words18,
			WordCount::Words21,
			WordCount::Words24,
		];

		for word_count in word_counts {
			let mnemonic = generate_entropy_mnemonic(Some(word_count));
			let entropy = mnemonic.to_entropy();
			assert_eq!(mnemonic, Mnemonic::from_entropy(&entropy).unwrap());

			// Verify expected word count
			let expected_words = match word_count {
				WordCount::Words12 => 12,
				WordCount::Words15 => 15,
				WordCount::Words18 => 18,
				WordCount::Words21 => 21,
				WordCount::Words24 => 24,
			};
			assert_eq!(mnemonic.word_count(), expected_words);
		}
	}

	#[test]
	fn derive_node_secret_matches_keys_manager() {
		// Standard test mnemonic (BIP39 test vector)
		let mnemonic =
			"abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

		// Derive using our function
		let derived_secret = derive_node_secret_from_mnemonic(mnemonic.to_string(), None).unwrap();

		// Derive using LDK's KeysManager (same flow as Builder)
		let parsed = Mnemonic::parse(mnemonic).unwrap();
		let seed = parsed.to_seed("");
		let xpriv = Xpriv::new_master(Network::Bitcoin, &seed).unwrap();
		let ldk_seed: [u8; 32] = xpriv.private_key.secret_bytes();

		let keys_manager = LdkKeysManager::new(&ldk_seed, 0, 0, false);
		let expected_secret = keys_manager.get_node_secret_key();

		assert_eq!(derived_secret, expected_secret.secret_bytes().to_vec());
	}

	#[test]
	fn derive_node_secret_with_passphrase() {
		let mnemonic =
			"abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
		let passphrase = Some("test_passphrase".to_string());

		let derived_secret =
			derive_node_secret_from_mnemonic(mnemonic.to_string(), passphrase).unwrap();

		let parsed = Mnemonic::parse(mnemonic).unwrap();
		let seed = parsed.to_seed("test_passphrase");
		let xpriv = Xpriv::new_master(Network::Bitcoin, &seed).unwrap();
		let ldk_seed: [u8; 32] = xpriv.private_key.secret_bytes();

		let keys_manager = LdkKeysManager::new(&ldk_seed, 0, 0, false);
		let expected_secret = keys_manager.get_node_secret_key();

		assert_eq!(derived_secret, expected_secret.secret_bytes().to_vec());
	}
}
