// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

// The builder's Esplora backend lets us construct a node without network access. All
// synchronization in this test uses the Bitcoin Core startup path and the source below.
#![cfg(feature = "chain-esplora")]

use std::collections::HashMap;
use std::sync::Arc;

use bitcoin::block::{Header, Version};
use bitcoin::constants::genesis_block;
use bitcoin::hashes::Hash;
use bitcoin::{Block, BlockHash, CompactTarget, Network, Transaction, TxMerkleNode};
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::{BlockLocator, Listen};
use lightning::ln::functional_test_utils::{
	create_chan_between_nodes_with_value_init, create_chanmon_cfgs, create_network,
	create_node_cfgs, create_node_chanmgrs, test_legacy_channel_config,
};
use lightning::sign::{InMemorySigner, KeysManager};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning_block_sync::{
	BlockData, BlockHeaderData, BlockSource, BlockSourceError, BlockSourceErrorKind,
	BlockSourceResult,
};

use super::synchronize_chain_listeners;
use crate::builder::NodeBuilder;
use crate::entropy::NodeEntropy;
use crate::io::test_utils::InMemoryStore;
use crate::types::DynStoreRef;
use crate::{Config, Error};

struct ForkedBlockSource {
	headers: HashMap<BlockHash, BlockHeaderData>,
	best_block: BlockHash,
	fail_block_fetch: bool,
}

impl ForkedBlockSource {
	fn new(genesis: &Block) -> Self {
		let best_block = genesis.block_hash();
		let header =
			BlockHeaderData { header: genesis.header, height: 0, chainwork: genesis.header.work() };
		Self { headers: HashMap::from([(best_block, header)]), best_block, fail_block_fetch: false }
	}

	fn add_block(&mut self, block: &Block) {
		let parent = self.headers.get(&block.header.prev_blockhash).unwrap();
		let header = BlockHeaderData {
			header: block.header,
			height: parent.height + 1,
			chainwork: parent.chainwork + block.header.work(),
		};
		self.headers.insert(block.block_hash(), header);
	}
}

impl BlockSource for ForkedBlockSource {
	async fn get_header<'a>(
		&'a self, block_hash: &'a BlockHash, _height_hint: Option<u32>,
	) -> BlockSourceResult<BlockHeaderData> {
		self.headers
			.get(block_hash)
			.cloned()
			.ok_or_else(|| BlockSourceError::persistent("unknown test block"))
	}

	async fn get_block<'a>(&'a self, block_hash: &'a BlockHash) -> BlockSourceResult<BlockData> {
		if self.fail_block_fetch {
			return Err(BlockSourceError::transient("injected block fetch failure"));
		}
		// Only the empty replacement block is connected during synchronization. The
		// original funding and commitment transactions have already been persisted.
		assert_eq!(*block_hash, self.best_block);
		Ok(BlockData::HeaderOnly(self.get_header(block_hash, None).await?.header))
	}

	async fn get_best_block(&self) -> BlockSourceResult<(BlockHash, Option<u32>)> {
		Ok((self.best_block, Some(self.headers[&self.best_block].height)))
	}
}

fn next_block(parent: &Block, txdata: Vec<Transaction>, time_offset: u32) -> Block {
	let mut block = Block {
		header: Header {
			version: Version::NO_SOFT_FORK_SIGNALLING,
			prev_blockhash: parent.block_hash(),
			merkle_root: TxMerkleNode::all_zeros(),
			time: parent.header.time + time_offset,
			bits: CompactTarget::from_consensus(0x207fffff),
			nonce: 0,
		},
		txdata,
	};
	if let Some(merkle_root) = block.compute_merkle_root() {
		block.header.merkle_root = merkle_root;
	}
	while block.header.validate_pow(block.header.target()).is_err() {
		block.header.nonce += 1;
	}
	block
}

fn read_monitor(bytes: &[u8], keys: &KeysManager) -> ChannelMonitor<InMemorySigner> {
	<(BlockLocator, ChannelMonitor<InMemorySigner>)>::read(&mut &bytes[..], (keys, keys)).unwrap().1
}

#[test]
fn startup_reconciles_divergent_monitor_tips() {
	do_test_startup_reconciles_divergent_monitor_tips(false);
}

#[test]
fn startup_retries_after_disconnecting_monitor_blocks() {
	do_test_startup_reconciles_divergent_monitor_tips(true);
}

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

fn do_test_startup_reconciles_divergent_monitor_tips(fail_first_sync: bool) {
	let network = Network::Testnet;
	let genesis = genesis_block(network);
	// LDK's node-0 fixture uses this seed and remote-key derivation mode. Deserializing
	// its monitors with the underlying signer gives us the real Node monitor type.
	let keys = KeysManager::new(&[0; 32], genesis.header.time as u64, 0, true);
	let (monitors, funding_transactions) = {
		let monitor_configs = create_chanmon_cfgs(3);
		let node_configs = create_node_cfgs(3, &monitor_configs);
		let channel_configs = vec![Some(test_legacy_channel_config()); 3];
		let channel_managers = create_node_chanmgrs(3, &node_configs, &channel_configs);
		let nodes = create_network(3, &node_configs, &channel_managers);
		let funding_transactions = vec![
			create_chan_between_nodes_with_value_init(&nodes[0], &nodes[1], 100_000, 10_001),
			create_chan_between_nodes_with_value_init(&nodes[0], &nodes[2], 100_000, 10_001),
		];
		let chain_monitor = &nodes[0].chain_monitor.chain_monitor;
		let monitors = chain_monitor
			.list_monitors()
			.into_iter()
			.map(|channel_id| {
				read_monitor(&chain_monitor.get_monitor(channel_id).unwrap().encode(), &keys)
			})
			.collect::<Vec<_>>();
		assert_eq!(monitors.len(), 2);
		(monitors, funding_transactions)
	};

	let monitor_configs = create_chanmon_cfgs(1);
	let monitor_config = &monitor_configs[0];
	let funding_block = next_block(&genesis, funding_transactions, 1);
	for monitor in &monitors {
		monitor.block_connected(
			&funding_block.header,
			&funding_block.txdata.iter().enumerate().collect::<Vec<_>>(),
			1,
			&monitor_config.tx_broadcaster,
			&monitor_config.fee_estimator,
			&monitor_config.logger,
		);
	}
	let commitment =
		monitors[0].unsafe_get_latest_holder_commitment_txn(&monitor_config.logger).remove(0);
	let commitment_txid = commitment.compute_txid();
	let orphan = next_block(&funding_block, vec![commitment], 1);
	monitors[0].block_connected(
		&orphan.header,
		&orphan.txdata.iter().enumerate().collect::<Vec<_>>(),
		2,
		&monitor_config.tx_broadcaster,
		&monitor_config.fee_estimator,
		&monitor_config.logger,
	);
	let ahead_channel_id = monitors[0].channel_id();
	let ahead_funding_txid = monitors[0].get_funding_txo().txid;
	assert!(monitors[0].get_relevant_txids().contains(&(
		commitment_txid,
		2,
		Some(orphan.block_hash()),
	)));
	assert_eq!(monitors[0].current_best_block().height, 2);
	assert_eq!(monitors[1].current_best_block().height, 1);

	let mut builder = NodeBuilder::from_config(Config { network, ..Config::default() });
	builder.set_log_facade_logger();
	#[cfg(not(feature = "uniffi"))]
	let entropy = NodeEntropy::from_seed_bytes([42; 64]);
	#[cfg(feature = "uniffi")]
	let entropy = NodeEntropy::from_seed_bytes(vec![42; 64]).unwrap();
	let node = builder.build_with_store(entropy, InMemoryStore::new()).unwrap();
	// Round-trip each persisted monitor at its own tip, just as after a restart.
	for monitor in monitors {
		let restored_monitor = read_monitor(&monitor.encode(), &keys);
		node.chain_monitor
			.load_existing_monitor(restored_monitor.channel_id(), restored_monitor)
			.unwrap();
	}

	let replacement = next_block(&funding_block, Vec::new(), 2);
	let mut source = ForkedBlockSource::new(&genesis);
	source.add_block(&funding_block);
	source.add_block(&orphan);
	source.add_block(&replacement);
	source.best_block = replacement.block_hash();
	let synchronize = |source: &ForkedBlockSource| {
		node.runtime.block_on(synchronize_chain_listeners(
			source,
			network,
			&node.chain_monitor,
			&node.tx_broadcaster,
			&node.fee_estimator,
			&node.logger,
			Vec::new(),
		))
	};
	if fail_first_sync {
		// All stale blocks are disconnected before replacement blocks are fetched. A
		// fetch failure must therefore leave a partially synchronized monitor whose
		// new locator is used on the next attempt.
		source.fail_block_fetch = true;
		let error = synchronize(&source).err().expect("block fetch must fail");
		assert_eq!(error.kind(), BlockSourceErrorKind::Transient);
		let monitor = node.chain_monitor.get_monitor(ahead_channel_id).unwrap();
		let best_block = monitor.current_best_block();
		assert_eq!(
			best_block.block_hash,
			funding_block.block_hash(),
			"failed synchronization must leave the monitor rewound to the common block",
		);
		assert_eq!(best_block.height, 1);
		assert!(
			!monitor.get_relevant_txids().iter().any(|(txid, _, _)| *txid == commitment_txid),
			"disconnects must remove stale confirmations before a block fetch can fail",
		);
		source.fail_block_fetch = false;
	}
	synchronize(&source).unwrap();

	let monitor = node.chain_monitor.get_monitor(ahead_channel_id).unwrap();
	let relevant_txids = monitor.get_relevant_txids();
	assert!(
		!relevant_txids.iter().any(|(txid, _, _)| *txid == commitment_txid),
		"startup must disconnect the orphaned commitment confirmation",
	);
	assert!(
		relevant_txids.contains(&(ahead_funding_txid, 1, Some(funding_block.block_hash()))),
		"startup must preserve the funding confirmation on the common chain",
	);
	drop(monitor);
	for channel_id in node.chain_monitor.list_monitors() {
		let best_block = node.chain_monitor.get_monitor(channel_id).unwrap().current_best_block();
		assert_eq!(best_block.block_hash, replacement.block_hash());
		assert_eq!(best_block.height, 2);
	}

	// Once startup has reconciled the monitors, ordinary aggregate notifications must
	// accept the same commitment confirming on the replacement chain.
	let reconfirmation = next_block(&replacement, orphan.txdata.clone(), 1);
	node.chain_monitor.filtered_block_connected(
		&reconfirmation.header,
		&reconfirmation.txdata.iter().enumerate().collect::<Vec<_>>(),
		3,
	);
	assert!(
		node.chain_monitor.get_monitor(ahead_channel_id).unwrap().get_relevant_txids().contains(&(
			commitment_txid,
			3,
			Some(reconfirmation.block_hash()),
		)),
		"aggregate synchronization must track the commitment's new confirmation",
	);
}
