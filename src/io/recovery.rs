// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::io;
use lightning::util::persist::{
	KVStoreSync, ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
	ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_KEY,
	CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
	CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	CHANNEL_MONITOR_UPDATE_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_KEY,
	NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
	OUTPUT_SWEEPER_PERSISTENCE_KEY, OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
	OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE, SCORER_PERSISTENCE_KEY,
	SCORER_PERSISTENCE_PRIMARY_NAMESPACE, SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};

use crate::io::utils::EXTERNAL_PATHFINDING_SCORES_CACHE_KEY;
use crate::io::{
	BDK_WALLET_CHANGE_DESCRIPTOR_KEY, BDK_WALLET_CHANGE_DESCRIPTOR_PRIMARY_NAMESPACE,
	BDK_WALLET_CHANGE_DESCRIPTOR_SECONDARY_NAMESPACE, BDK_WALLET_DESCRIPTOR_KEY,
	BDK_WALLET_DESCRIPTOR_PRIMARY_NAMESPACE, BDK_WALLET_DESCRIPTOR_SECONDARY_NAMESPACE,
	BDK_WALLET_INDEXER_KEY, BDK_WALLET_INDEXER_PRIMARY_NAMESPACE,
	BDK_WALLET_INDEXER_SECONDARY_NAMESPACE, BDK_WALLET_LOCAL_CHAIN_KEY,
	BDK_WALLET_LOCAL_CHAIN_PRIMARY_NAMESPACE, BDK_WALLET_LOCAL_CHAIN_SECONDARY_NAMESPACE,
	BDK_WALLET_NETWORK_KEY, BDK_WALLET_NETWORK_PRIMARY_NAMESPACE,
	BDK_WALLET_NETWORK_SECONDARY_NAMESPACE, BDK_WALLET_TX_GRAPH_KEY,
	BDK_WALLET_TX_GRAPH_PRIMARY_NAMESPACE, BDK_WALLET_TX_GRAPH_SECONDARY_NAMESPACE,
	EVENT_QUEUE_PERSISTENCE_KEY, EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
	EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE, NODE_METRICS_KEY, NODE_METRICS_PRIMARY_NAMESPACE,
	NODE_METRICS_SECONDARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE, PEER_INFO_PERSISTENCE_KEY,
	PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE, STATIC_INVOICE_STORE_PRIMARY_NAMESPACE,
};
use crate::types::DynStore;

type KeyTuple = (&'static str, &'static str, &'static str);
type NamespacePair = (&'static str, &'static str);

/// Durable entries with well-known, fixed keys.
const DURABLE_EXACT_KEYS: &[KeyTuple] = &[
	(
		CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_KEY,
	),
	(
		NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
		NETWORK_GRAPH_PERSISTENCE_KEY,
	),
	(
		OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
		OUTPUT_SWEEPER_PERSISTENCE_KEY,
	),
	(
		EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
		EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
		EVENT_QUEUE_PERSISTENCE_KEY,
	),
	(
		PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		PEER_INFO_PERSISTENCE_KEY,
	),
	(NODE_METRICS_PRIMARY_NAMESPACE, NODE_METRICS_SECONDARY_NAMESPACE, NODE_METRICS_KEY),
	(
		BDK_WALLET_DESCRIPTOR_PRIMARY_NAMESPACE,
		BDK_WALLET_DESCRIPTOR_SECONDARY_NAMESPACE,
		BDK_WALLET_DESCRIPTOR_KEY,
	),
	(
		BDK_WALLET_CHANGE_DESCRIPTOR_PRIMARY_NAMESPACE,
		BDK_WALLET_CHANGE_DESCRIPTOR_SECONDARY_NAMESPACE,
		BDK_WALLET_CHANGE_DESCRIPTOR_KEY,
	),
	(
		BDK_WALLET_NETWORK_PRIMARY_NAMESPACE,
		BDK_WALLET_NETWORK_SECONDARY_NAMESPACE,
		BDK_WALLET_NETWORK_KEY,
	),
	(
		BDK_WALLET_LOCAL_CHAIN_PRIMARY_NAMESPACE,
		BDK_WALLET_LOCAL_CHAIN_SECONDARY_NAMESPACE,
		BDK_WALLET_LOCAL_CHAIN_KEY,
	),
	(
		BDK_WALLET_TX_GRAPH_PRIMARY_NAMESPACE,
		BDK_WALLET_TX_GRAPH_SECONDARY_NAMESPACE,
		BDK_WALLET_TX_GRAPH_KEY,
	),
	(
		BDK_WALLET_INDEXER_PRIMARY_NAMESPACE,
		BDK_WALLET_INDEXER_SECONDARY_NAMESPACE,
		BDK_WALLET_INDEXER_KEY,
	),
	(
		SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
		SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
		SCORER_PERSISTENCE_KEY,
	),
	(
		SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
		SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
		EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
	),
];

/// Durable namespaces where the key is dynamic (e.g. one entry per payment or monitor).
const DURABLE_DYNAMIC_KEY_NAMESPACES: &[NamespacePair] = &[
	(PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE),
	(
		PENDING_PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PENDING_PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	),
	(STATIC_INVOICE_STORE_PRIMARY_NAMESPACE, ""),
	(
		CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	),
	(
		ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
		ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	),
];

/// Durable namespaces where both the secondary namespace and key are dynamic
/// (e.g. monitor updates are keyed by monitor ID in the secondary namespace
/// and update ID in the key).
const DURABLE_WILDCARD_PRIMARY_NAMESPACES: &[&str] =
	&[CHANNEL_MONITOR_UPDATE_PERSISTENCE_PRIMARY_NAMESPACE];

pub(crate) fn is_known_durable_key(
	primary_namespace: &str, secondary_namespace: &str, key: &str,
) -> bool {
	DURABLE_EXACT_KEYS
		.iter()
		.any(|(p, s, k)| primary_namespace == *p && secondary_namespace == *s && key == *k)
		|| DURABLE_DYNAMIC_KEY_NAMESPACES
			.iter()
			.any(|(p, s)| primary_namespace == *p && secondary_namespace == *s)
		|| DURABLE_WILDCARD_PRIMARY_NAMESPACES.iter().any(|p| primary_namespace == *p)
}

pub(crate) fn filter_known_durable_keys(
	keys: Vec<(String, String, String)>,
) -> Vec<(String, String, String)> {
	keys.into_iter().filter(|(p, s, k)| is_known_durable_key(p, s, k)).collect()
}

/// Restores durable state from a backup store into an empty primary store.
///
/// Enumerates all keys in the backup, filters to the known durable scope,
/// and copies each matching entry to the primary store.
pub(crate) fn restore_from_backup(primary: &DynStore, backup: &DynStore) -> Result<(), io::Error> {
	let all_keys = backup.list_all_keys()?;
	let durable_keys = filter_known_durable_keys(all_keys);

	for (primary_namespace, secondary_namespace, key) in &durable_keys {
		let value = KVStoreSync::read(backup, primary_namespace, secondary_namespace, key)?;

		KVStoreSync::write(primary, primary_namespace, secondary_namespace, key, value)?;
	}

	Ok(())
}

/// Returns all durable keys currently present in the given store.
///
/// This is used to check whether the primary store already contains state
/// before performing a restore from backup.
pub(crate) fn list_existing_durable_keys(
	store: &DynStore,
) -> Result<Vec<(String, String, String)>, io::Error> {
	let all_keys = store.list_all_keys()?;
	Ok(filter_known_durable_keys(all_keys))
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use crate::{io::test_utils::InMemoryStore, types::DynStoreWrapper};

	use super::*;

	fn dyn_store() -> Arc<DynStore> {
		Arc::new(DynStoreWrapper(InMemoryStore::new()))
	}

	#[test]
	fn exact_key_matches() {
		assert!(is_known_durable_key(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		));
		assert!(is_known_durable_key(
			SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
		));
		assert!(is_known_durable_key(
			NODE_METRICS_PRIMARY_NAMESPACE,
			NODE_METRICS_SECONDARY_NAMESPACE,
			NODE_METRICS_KEY,
		));
	}

	#[test]
	fn dynamic_key_namespace_matches() {
		assert!(is_known_durable_key(
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			"some_payment_id",
		));
		assert!(is_known_durable_key(
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			"some_monitor_id",
		));
		assert!(is_known_durable_key(STATIC_INVOICE_STORE_PRIMARY_NAMESPACE, "", "invoice_42",));
	}

	#[test]
	fn scorer_exact_keys_match() {
		assert!(is_known_durable_key(
			SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			SCORER_PERSISTENCE_KEY,
		));
		assert!(is_known_durable_key(
			SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			EXTERNAL_PATHFINDING_SCORES_CACHE_KEY,
		));
		assert!(!is_known_durable_key(
			SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
			SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
			"unknown_scorer_key",
		));
	}

	#[test]
	fn wildcard_primary_namespace_matches() {
		assert!(is_known_durable_key(
			CHANNEL_MONITOR_UPDATE_PERSISTENCE_PRIMARY_NAMESPACE,
			"any_secondary",
			"any_key",
		));
	}

	#[test]
	fn unknown_keys_rejected() {
		assert!(!is_known_durable_key("unknown_namespace", "", "unknown_key"));
		assert!(!is_known_durable_key("backup_retry_queue", "", "some_entry"));
		assert!(!is_known_durable_key("", "", "unknown_key"));
	}

	#[test]
	fn filter_retains_only_durable_keys() {
		let keys = vec![
			(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				CHANNEL_MANAGER_PERSISTENCE_KEY.to_string(),
			),
			("backup_retry_queue".to_string(), "".to_string(), "entry_1".to_string()),
			(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				"pay_001".to_string(),
			),
		];

		let filtered = filter_known_durable_keys(keys);
		assert_eq!(filtered.len(), 2);
		assert_eq!(filtered[0].2, CHANNEL_MANAGER_PERSISTENCE_KEY);
		assert_eq!(filtered[1].2, "pay_001");
	}

	#[test]
	fn list_existing_durable_keys_returns_only_durable_scope() {
		let store = dyn_store();

		KVStoreSync::write(
			&*store,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			b"manager".to_vec(),
		)
		.expect("Failed to write to channel manager namespace.");

		KVStoreSync::write(&*store, "backup_retry_queue", "", "entry_1", b"internal".to_vec())
			.expect("Failed to write to backup retry queue.");

		let durable = list_existing_durable_keys(&*store).expect("Failure listing durable keys.");
		assert_eq!(durable.len(), 1);
		assert_eq!(
			durable[0],
			(
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE.to_string(),
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE.to_string(),
				CHANNEL_MANAGER_PERSISTENCE_KEY.to_string(),
			)
		);
	}

	#[test]
	fn restore_from_backup_copies_only_known_durable_keys() {
		let primary = dyn_store();
		let backup = dyn_store();

		KVStoreSync::write(
			&*backup,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			b"manager".to_vec(),
		)
		.expect("Failed to write to channel manager namespace.");

		KVStoreSync::write(
			&*backup,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			"payment_1",
			b"payment".to_vec(),
		)
		.expect("Failed to write to payment info namespace.");

		KVStoreSync::write(&*backup, "unrelated", "", "garbage", b"nope".to_vec())
			.expect("Failed to write to non-durable namespace.");

		restore_from_backup(&*primary, &*backup).expect("Backup restoration failed.");

		assert_eq!(
			KVStoreSync::read(
				&*primary,
				CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
				CHANNEL_MANAGER_PERSISTENCE_KEY,
			)
			.expect("Failed to read from channel manager namespace."),
			b"manager".to_vec()
		);

		assert_eq!(
			KVStoreSync::read(
				&*primary,
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				"payment_1",
			)
			.expect("Failed to read from payment info namespace."),
			b"payment".to_vec()
		);

		assert!(KVStoreSync::read(&*primary, "unrelated", "", "garbage").is_err());
	}
}
