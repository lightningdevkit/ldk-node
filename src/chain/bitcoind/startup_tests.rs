// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

// The builder's Esplora backend lets us construct a node without network access. All
// synchronization in this test uses the Bitcoin Core startup path and the source below.
#![cfg(feature = "chain-esplora")]

use std::sync::Arc;

use bitcoin::Network;

use crate::builder::NodeBuilder;
use crate::entropy::NodeEntropy;
use crate::io::test_utils::InMemoryStore;
use crate::types::DynStoreRef;
use crate::{Config, Error};

#[test]
fn startup_fails_if_bitcoind_sync_fails() {
	let mut builder =
		NodeBuilder::from_config(Config { network: Network::Testnet, ..Config::default() });
	builder.set_log_facade_logger();
	#[cfg(not(feature = "uniffi"))]
	let entropy = NodeEntropy::from_seed_bytes([42; 64]);
	#[cfg(feature = "uniffi")]
	let entropy = NodeEntropy::from_seed_bytes(vec![42; 64]).unwrap();

	// Persist a wallet first so the Core-backed build does not require fetching a
	// birthday from the unavailable backend. This leaves startup responsible for sync.
	let initialized_node = builder.build_with_store(entropy, InMemoryStore::new()).unwrap();
	let store = DynStoreRef(Arc::clone(&initialized_node.kv_store));
	drop(initialized_node);
	builder.set_chain_source_bitcoind_rpc(
		"127.0.0.1".to_owned(),
		0,
		"test".to_owned(),
		"test".to_owned(),
		None,
	);
	let node = builder.build_with_store(entropy, store).unwrap();

	// Testnet tolerates unavailable fee estimates, so this failure must come from
	// initial chain synchronization. Port zero cannot have a listening RPC server.
	assert_eq!(
		node.start(),
		Err(Error::TxSyncFailed),
		"startup must fail when initial Bitcoin Core synchronization fails",
	);
	assert!(!node.status().is_running);
	assert!(node.list_peers().is_empty());
}
