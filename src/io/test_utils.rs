// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::panic::RefUnwindSafe;
use std::path::PathBuf;

use lightning::events::ClosureReason;
use lightning::ln::functional_test_utils::{
	connect_block, create_announced_chan_between_nodes, create_chanmon_cfgs, create_dummy_block,
	create_network, create_node_cfgs, create_node_chanmgrs, send_payment, TestChanMonCfg,
};
use lightning::util::persist::{
	KVStoreSync, MonitorUpdatingPersister, KVSTORE_NAMESPACE_KEY_MAX_LEN,
};

use lightning::util::test_utils;
use lightning::{check_added_monitors, check_closed_broadcast, check_closed_event};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

type TestMonitorUpdatePersister<'a, K> = MonitorUpdatingPersister<
	&'a K,
	&'a test_utils::TestLogger,
	&'a test_utils::TestKeysInterface,
	&'a test_utils::TestKeysInterface,
	&'a test_utils::TestBroadcaster,
	&'a test_utils::TestFeeEstimator,
>;

const EXPECTED_UPDATES_PER_PAYMENT: u64 = 5;

pub(crate) fn random_storage_path() -> PathBuf {
	let mut temp_path = std::env::temp_dir();
	let mut rng = thread_rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

pub(crate) fn do_read_write_remove_list_persist<K: KVStoreSync + RefUnwindSafe>(kv_store: &K) {
	let data = vec![42u8; 32];

	let primary_namespace = "testspace";
	let secondary_namespace = "testsubspace";
	let key = "testkey";

	// Test the basic KVStore operations.
	kv_store.write(primary_namespace, secondary_namespace, key, data.clone()).unwrap();

	// Test empty primary/secondary namespaces are allowed, but not empty primary namespace and non-empty
	// secondary primary_namespace, and not empty key.
	kv_store.write("", "", key, data.clone()).unwrap();
	let res =
		std::panic::catch_unwind(|| kv_store.write("", secondary_namespace, key, data.clone()));
	assert!(res.is_err());
	let res = std::panic::catch_unwind(|| {
		kv_store.write(primary_namespace, secondary_namespace, "", data.clone())
	});
	assert!(res.is_err());

	let listed_keys = kv_store.list(primary_namespace, secondary_namespace).unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], key);

	let read_data = kv_store.read(primary_namespace, secondary_namespace, key).unwrap();
	assert_eq!(data, &*read_data);

	kv_store.remove(primary_namespace, secondary_namespace, key).unwrap();

	let listed_keys = kv_store.list(primary_namespace, secondary_namespace).unwrap();
	assert_eq!(listed_keys.len(), 0);

	// Ensure we have no issue operating with primary_namespace/secondary_namespace/key being KVSTORE_NAMESPACE_KEY_MAX_LEN
	let max_chars: String = std::iter::repeat('A').take(KVSTORE_NAMESPACE_KEY_MAX_LEN).collect();
	kv_store.write(&max_chars, &max_chars, &max_chars, data.clone()).unwrap();

	let listed_keys = kv_store.list(&max_chars, &max_chars).unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], max_chars);

	let read_data = kv_store.read(&max_chars, &max_chars, &max_chars).unwrap();
	assert_eq!(data, &*read_data);

	kv_store.remove(&max_chars, &max_chars, &max_chars).unwrap();

	let listed_keys = kv_store.list(&max_chars, &max_chars).unwrap();
	assert_eq!(listed_keys.len(), 0);
}

pub(crate) fn create_persister<'a, K: KVStoreSync + Sync>(
	store: &'a K, chanmon_cfg: &'a TestChanMonCfg, max_pending_updates: u64,
) -> TestMonitorUpdatePersister<'a, K> {
	MonitorUpdatingPersister::new(
		store,
		&chanmon_cfg.logger,
		max_pending_updates,
		&chanmon_cfg.keys_manager,
		&chanmon_cfg.keys_manager,
		&chanmon_cfg.tx_broadcaster,
		&chanmon_cfg.fee_estimator,
	)
}

pub(crate) fn create_chain_monitor<'a, K: KVStoreSync + Sync>(
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
pub(crate) fn do_test_store<K: KVStoreSync + Sync>(store_0: &K, store_1: &K) {
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
	let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	// Check that the persisted channel data is empty before any channels are
	// open.
	let mut persisted_chan_data_0 = persister_0.read_all_channel_monitors_with_updates().unwrap();
	assert_eq!(persisted_chan_data_0.len(), 0);
	let mut persisted_chan_data_1 = persister_1.read_all_channel_monitors_with_updates().unwrap();
	assert_eq!(persisted_chan_data_1.len(), 0);

	// Helper to make sure the channel is on the expected update ID.
	macro_rules! check_persisted_data {
		($expected_update_id: expr) => {
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
	check_closed_event!(
		nodes[0],
		1,
		ClosureReason::HolderForceClosed { broadcasted_latest_txn: Some(true), message },
		[nodes[1].node.get_our_node_id()],
		100000
	);
	check_closed_broadcast!(nodes[0], true);
	check_added_monitors!(nodes[0], 1);

	let node_txn = nodes[0].tx_broadcaster.txn_broadcast();
	assert_eq!(node_txn.len(), 1);
	let txn = vec![node_txn[0].clone(), node_txn[0].clone()];
	let dummy_block = create_dummy_block(nodes[0].best_block_hash(), 42, txn);
	connect_block(&nodes[1], &dummy_block);

	check_closed_broadcast!(nodes[1], true);
	let reason = ClosureReason::CommitmentTxConfirmed;
	let node_id_0 = nodes[0].node.get_our_node_id();
	check_closed_event!(nodes[1], 1, reason, false, [node_id_0], 100000);
	check_added_monitors!(nodes[1], 1);

	// Make sure everything is persisted as expected after close.
	check_persisted_data!(persister_0_max_pending_updates * 2 * EXPECTED_UPDATES_PER_PAYMENT + 1);
}
