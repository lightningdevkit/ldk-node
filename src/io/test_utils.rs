// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::panic::RefUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use lightning::chain::channelmonitor::{ChannelMonitor, ChannelMonitorUpdate};
use lightning::chain::{chainmonitor, BlockLocator, ChannelMonitorUpdateStatus};
use lightning::events::ClosureReason;
use lightning::io;
use lightning::ln::functional_test_utils::{
	check_added_monitors, check_closed_broadcast, check_closed_event, connect_block,
	create_announced_chan_between_nodes, create_chanmon_cfgs, create_dummy_block, create_network,
	create_node_cfgs, create_node_chanmgrs, send_payment, test_legacy_channel_config,
	TestChanMonCfg,
};
use lightning::util::persist::{
	KVStore, MonitorName, ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
	ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
	KVSTORE_NAMESPACE_KEY_MAX_LEN,
};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning::util::test_channel_signer::TestChannelSigner;
use lightning::util::test_utils;
use rand::distr::Alphanumeric;
use rand::{rng, Rng};

#[path = "in_memory_store.rs"]
mod in_memory_store;

use crate::logger::Logger;
use crate::runtime::Runtime;

pub(crate) struct TestMonitorUpdatePersister<'a, K> {
	store: &'a K,
	runtime: Runtime,
	entropy_source: &'a test_utils::TestKeysInterface,
	signer_provider: &'a test_utils::TestKeysInterface,
}

impl<K: KVStore + Sync> TestMonitorUpdatePersister<'_, K> {
	pub(crate) fn read_all_channel_monitors_with_updates(
		&self,
	) -> Result<Vec<(BlockLocator, ChannelMonitor<TestChannelSigner>)>, io::Error> {
		self.runtime.block_on(async {
			let stored_keys = KVStore::list(
				self.store,
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			)
			.await?;

			let mut res = Vec::with_capacity(stored_keys.len());
			for stored_key in stored_keys {
				let data = KVStore::read(
					self.store,
					CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
					CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
					&stored_key,
				)
				.await?;
				match <Option<(BlockLocator, ChannelMonitor<TestChannelSigner>)>>::read(
					&mut io::Cursor::new(data),
					(self.entropy_source, self.signer_provider),
				) {
					Ok(Some((best_block, channel_monitor))) => {
						res.push((best_block, channel_monitor));
					},
					Ok(None) => {},
					Err(_) => {
						return Err(io::Error::new(
							io::ErrorKind::InvalidData,
							"Failed to read ChannelMonitor",
						));
					},
				}
			}
			Ok(res)
		})
	}

	fn write_monitor(
		&self, monitor_name: MonitorName, monitor: &ChannelMonitor<TestChannelSigner>,
	) -> ChannelMonitorUpdateStatus {
		let write_res = self.runtime.block_on(KVStore::write(
			self.store,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			&monitor_name.to_string(),
			monitor.encode(),
		));
		match write_res {
			Ok(()) => ChannelMonitorUpdateStatus::Completed,
			Err(_) => ChannelMonitorUpdateStatus::UnrecoverableError,
		}
	}
}

impl<K: KVStore + Sync> chainmonitor::Persist<TestChannelSigner>
	for TestMonitorUpdatePersister<'_, K>
{
	fn persist_new_channel(
		&self, monitor_name: MonitorName, monitor: &ChannelMonitor<TestChannelSigner>,
	) -> ChannelMonitorUpdateStatus {
		self.write_monitor(monitor_name, monitor)
	}

	fn update_persisted_channel(
		&self, monitor_name: MonitorName, _monitor_update: Option<&ChannelMonitorUpdate>,
		monitor: &ChannelMonitor<TestChannelSigner>,
	) -> ChannelMonitorUpdateStatus {
		self.write_monitor(monitor_name, monitor)
	}

	fn archive_persisted_channel(&self, monitor_name: MonitorName) {
		let key = monitor_name.to_string();
		self.runtime.block_on(async {
			let monitor = match KVStore::read(
				self.store,
				CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
			)
			.await
			{
				Ok(monitor) => monitor,
				Err(_) => return,
			};

			if KVStore::write(
				self.store,
				ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
				ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
				&key,
				monitor,
			)
			.await
			.is_ok()
			{
				let _ = KVStore::remove(
					self.store,
					CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
					CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
					&key,
					true,
				)
				.await;
			}
		});
	}
}

const EXPECTED_UPDATES_PER_PAYMENT: u64 = 5;

pub(crate) use in_memory_store::InMemoryStore;

pub(crate) fn random_storage_path() -> PathBuf {
	let mut temp_path = std::env::temp_dir();
	let mut rng = rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

async fn catch_future_unwind<F: Future>(future: F) -> std::thread::Result<F::Output> {
	let mut future = std::pin::pin!(future);
	std::future::poll_fn(|cx| {
		match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx))) {
			Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
			Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
			Err(panic) => std::task::Poll::Ready(Err(panic)),
		}
	})
	.await
}

async fn assert_invalid_write_fails<K: KVStore + RefUnwindSafe>(
	kv_store: &K, primary_namespace: &str, secondary_namespace: &str, key: &str, data: Vec<u8>,
) {
	let res = std::panic::catch_unwind(|| {
		KVStore::write(kv_store, primary_namespace, secondary_namespace, key, data)
	});
	if let Ok(fut) = res {
		if let Ok(write_res) = catch_future_unwind(fut).await {
			assert!(write_res.is_err());
		}
	}
}

pub(crate) async fn do_read_write_remove_list_persist<K: KVStore + RefUnwindSafe>(kv_store: &K) {
	let data = vec![42u8; 32];

	let primary_namespace = "testspace";
	let secondary_namespace = "testsubspace";
	let key = "testkey";

	// Test the basic KVStore operations.
	KVStore::write(kv_store, primary_namespace, secondary_namespace, key, data.clone())
		.await
		.unwrap();

	// Test empty primary/secondary namespaces are allowed, but not empty primary namespace and non-empty
	// secondary primary_namespace, and not empty key.
	KVStore::write(kv_store, "", "", key, data.clone()).await.unwrap();
	assert_invalid_write_fails(kv_store, "", secondary_namespace, key, data.clone()).await;
	assert_invalid_write_fails(kv_store, primary_namespace, secondary_namespace, "", data.clone())
		.await;

	let listed_keys =
		KVStore::list(kv_store, primary_namespace, secondary_namespace).await.unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], key);

	let read_data =
		KVStore::read(kv_store, primary_namespace, secondary_namespace, key).await.unwrap();
	assert_eq!(data, &*read_data);

	KVStore::remove(kv_store, primary_namespace, secondary_namespace, key, false).await.unwrap();

	let listed_keys =
		KVStore::list(kv_store, primary_namespace, secondary_namespace).await.unwrap();
	assert_eq!(listed_keys.len(), 0);

	// Ensure we have no issue operating with primary_namespace/secondary_namespace/key being KVSTORE_NAMESPACE_KEY_MAX_LEN
	let max_chars: String = std::iter::repeat('A').take(KVSTORE_NAMESPACE_KEY_MAX_LEN).collect();
	KVStore::write(kv_store, &max_chars, &max_chars, &max_chars, data.clone()).await.unwrap();

	let listed_keys = KVStore::list(kv_store, &max_chars, &max_chars).await.unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], max_chars);

	let read_data = KVStore::read(kv_store, &max_chars, &max_chars, &max_chars).await.unwrap();
	assert_eq!(data, &*read_data);

	KVStore::remove(kv_store, &max_chars, &max_chars, &max_chars, false).await.unwrap();

	let listed_keys = KVStore::list(kv_store, &max_chars, &max_chars).await.unwrap();
	assert_eq!(listed_keys.len(), 0);
}

pub(crate) fn create_persister<'a, K: KVStore + Sync>(
	store: &'a K, chanmon_cfg: &'a TestChanMonCfg, _max_pending_updates: u64,
) -> TestMonitorUpdatePersister<'a, K> {
	let runtime =
		Runtime::new(Arc::new(Logger::new_log_facade())).expect("Failed to setup runtime");
	TestMonitorUpdatePersister {
		store,
		runtime,
		entropy_source: &chanmon_cfg.keys_manager,
		signer_provider: &chanmon_cfg.keys_manager,
	}
}

pub(crate) fn create_chain_monitor<'a, K: KVStore + Sync>(
	chanmon_cfg: &'a TestChanMonCfg, persister: &'a TestMonitorUpdatePersister<'a, K>,
) -> test_utils::TestChainMonitor<'a> {
	test_utils::TestChainMonitor::new(
		Some(&chanmon_cfg.chain_source),
		&chanmon_cfg.tx_broadcaster,
		&chanmon_cfg.logger,
		&chanmon_cfg.fee_estimator,
		persister,
		&chanmon_cfg.keys_manager,
	)
}

// Integration-test the given KVStore implementation. Test relaying a few payments and check that
// the persisted data is updated the appropriate number of times.
pub(crate) fn do_test_store<K: KVStore + Sync>(store_0: &K, store_1: &K) {
	// This value is used later to limit how many iterations we perform.
	let persister_0_max_pending_updates = 7;
	// Intentionally set this to a smaller value to test a different alignment.
	let persister_1_max_pending_updates = 3;

	let chanmon_cfgs = create_chanmon_cfgs(2);

	let persister_0 = create_persister(store_0, &chanmon_cfgs[0], persister_0_max_pending_updates);
	let persister_1 = create_persister(store_1, &chanmon_cfgs[1], persister_1_max_pending_updates);

	let chain_mon_0 = create_chain_monitor(&chanmon_cfgs[0], &persister_0);
	let chain_mon_1 = create_chain_monitor(&chanmon_cfgs[1], &persister_1);

	let mut node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	node_cfgs[0].chain_monitor = chain_mon_0;
	node_cfgs[1].chain_monitor = chain_mon_1;
	let legacy_cfg = test_legacy_channel_config();
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(legacy_cfg.clone()), Some(legacy_cfg)]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	// Check that the persisted channel data is empty before any channels are
	// open.
	let mut persisted_chan_data_0 = persister_0.read_all_channel_monitors_with_updates().unwrap();
	assert_eq!(persisted_chan_data_0.len(), 0);
	let mut persisted_chan_data_1 = persister_1.read_all_channel_monitors_with_updates().unwrap();
	assert_eq!(persisted_chan_data_1.len(), 0);

	// Helper to make sure the channel is on the expected update ID.
	macro_rules! check_persisted_data {
		($expected_update_id:expr) => {
			persisted_chan_data_0 = persister_0.read_all_channel_monitors_with_updates().unwrap();
			assert_eq!(persisted_chan_data_0.len(), 1);
			for (_, mon) in persisted_chan_data_0.iter() {
				assert_eq!(mon.get_latest_update_id(), $expected_update_id);
			}
			persisted_chan_data_1 = persister_1.read_all_channel_monitors_with_updates().unwrap();
			assert_eq!(persisted_chan_data_1.len(), 1);
			for (_, mon) in persisted_chan_data_1.iter() {
				assert_eq!(mon.get_latest_update_id(), $expected_update_id);
			}
		};
	}

	// Create some initial channel and check that a channel was persisted.
	let _ = create_announced_chan_between_nodes(&nodes, 0, 1);
	check_persisted_data!(0);

	// Send a few payments and make sure the monitors are updated to the latest.
	let expected_route = &[&nodes[1]][..];
	send_payment(&nodes[0], expected_route, 8_000_000);
	check_persisted_data!(EXPECTED_UPDATES_PER_PAYMENT);
	let expected_route = &[&nodes[0]][..];
	send_payment(&nodes[1], expected_route, 4_000_000);
	check_persisted_data!(2 * EXPECTED_UPDATES_PER_PAYMENT);

	// Send a few more payments to try all the alignments of max pending updates with
	// updates for a payment sent and received.
	let mut sender = 0;
	for i in 3..=persister_0_max_pending_updates * 2 {
		let receiver;
		if sender == 0 {
			sender = 1;
			receiver = 0;
		} else {
			sender = 0;
			receiver = 1;
		}
		let expected_route = &[&nodes[receiver]][..];
		send_payment(&nodes[sender], expected_route, 21_000);
		check_persisted_data!(i * EXPECTED_UPDATES_PER_PAYMENT);
	}

	// Force close because cooperative close doesn't result in any persisted
	// updates.
	let message = "Channel force-closed".to_owned();
	nodes[0]
		.node
		.force_close_broadcasting_latest_txn(
			&nodes[0].node.list_channels()[0].channel_id,
			&nodes[1].node.get_our_node_id(),
			message.clone(),
		)
		.unwrap();
	check_closed_event(
		&nodes[0],
		1,
		ClosureReason::HolderForceClosed { broadcasted_latest_txn: Some(true), message },
		&[nodes[1].node.get_our_node_id()],
		100000,
	);
	check_closed_broadcast(&nodes[0], 1, true);
	check_added_monitors(&nodes[0], 1);

	let node_txn = nodes[0].tx_broadcaster.txn_broadcast();
	assert_eq!(node_txn.len(), 1);
	let txn = vec![node_txn[0].clone(), node_txn[0].clone()];
	let dummy_block = create_dummy_block(nodes[0].best_block_hash(), 42, txn);
	connect_block(&nodes[1], &dummy_block);

	check_closed_broadcast(&nodes[1], 1, true);
	let reason = ClosureReason::CommitmentTxConfirmed;
	let node_id_0 = nodes[0].node.get_our_node_id();
	check_closed_event(&nodes[1], 1, reason, &[node_id_0], 100000);
	check_added_monitors(&nodes[1], 1);

	// Make sure everything is persisted as expected after close.
	check_persisted_data!(persister_0_max_pending_updates * 2 * EXPECTED_UPDATES_PER_PAYMENT + 1);
}
