// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::ln::functional_test_utils::{
	check_closed_event, connect_block, create_announced_chan_between_nodes, create_chanmon_cfgs,
	create_dummy_block, create_network, create_node_cfgs, create_node_chanmgrs, send_payment,
};
use lightning::util::persist::{
	KVStore, MonitorName, MonitorUpdatingPersister,
	CHANNEL_MONITOR_UPDATE_PERSISTENCE_PRIMARY_NAMESPACE, KVSTORE_NAMESPACE_KEY_MAX_LEN,
};

use lightning::events::ClosureReason;
use lightning::util::test_utils;
use lightning::{check_added_monitors, check_closed_broadcast};

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use std::panic::RefUnwindSafe;
use std::path::PathBuf;

const EXPECTED_UPDATES_PER_PAYMENT: u64 = 5;

pub(crate) fn random_storage_path() -> PathBuf {
	let mut temp_path = std::env::temp_dir();
	let mut rng = thread_rng();
	let rand_dir: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
	temp_path.push(rand_dir);
	temp_path
}

pub(crate) fn do_read_write_remove_list_persist<K: KVStore + RefUnwindSafe>(kv_store: &K) {
	let data = [42u8; 32];

	let primary_namespace = "testspace";
	let secondary_namespace = "testsubspace";
	let key = "testkey";

	// Test the basic KVStore operations.
	kv_store.write(primary_namespace, secondary_namespace, key, &data).unwrap();

	// Test empty primary/secondary namespaces are allowed, but not empty primary namespace and non-empty
	// secondary primary_namespace, and not empty key.
	kv_store.write("", "", key, &data).unwrap();
	let res = std::panic::catch_unwind(|| kv_store.write("", secondary_namespace, key, &data));
	assert!(res.is_err());
	let res = std::panic::catch_unwind(|| {
		kv_store.write(primary_namespace, secondary_namespace, "", &data)
	});
	assert!(res.is_err());

	let listed_keys = kv_store.list(primary_namespace, secondary_namespace).unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], key);

	let read_data = kv_store.read(primary_namespace, secondary_namespace, key).unwrap();
	assert_eq!(data, &*read_data);

	kv_store.remove(primary_namespace, secondary_namespace, key, false).unwrap();

	let listed_keys = kv_store.list(primary_namespace, secondary_namespace).unwrap();
	assert_eq!(listed_keys.len(), 0);

	// Ensure we have no issue operating with primary_namespace/secondary_namespace/key being KVSTORE_NAMESPACE_KEY_MAX_LEN
	let max_chars: String = std::iter::repeat('A').take(KVSTORE_NAMESPACE_KEY_MAX_LEN).collect();
	kv_store.write(&max_chars, &max_chars, &max_chars, &data).unwrap();

	let listed_keys = kv_store.list(&max_chars, &max_chars).unwrap();
	assert_eq!(listed_keys.len(), 1);
	assert_eq!(listed_keys[0], max_chars);

	let read_data = kv_store.read(&max_chars, &max_chars, &max_chars).unwrap();
	assert_eq!(data, &*read_data);

	kv_store.remove(&max_chars, &max_chars, &max_chars, false).unwrap();

	let listed_keys = kv_store.list(&max_chars, &max_chars).unwrap();
	assert_eq!(listed_keys.len(), 0);
}

// Integration-test the given KVStore implementation. Test relaying a few payments and check that
// the persisted data is updated the appropriate number of times.
pub(crate) fn do_test_store<K: KVStore>(store_0: &K, store_1: &K) {
	// This value is used later to limit how many iterations we perform.
	let persister_0_max_pending_updates = 7;
	// Intentionally set this to a smaller value to test a different alignment.
	let persister_1_max_pending_updates = 3;

	let chanmon_cfgs = create_chanmon_cfgs(2);

	let persister_0 = MonitorUpdatingPersister::new(
		store_0,
		&chanmon_cfgs[0].logger,
		persister_0_max_pending_updates,
		&chanmon_cfgs[0].keys_manager,
		&chanmon_cfgs[0].keys_manager,
		&chanmon_cfgs[0].tx_broadcaster,
		&chanmon_cfgs[0].fee_estimator,
	);

	let persister_1 = MonitorUpdatingPersister::new(
		store_1,
		&chanmon_cfgs[1].logger,
		persister_1_max_pending_updates,
		&chanmon_cfgs[1].keys_manager,
		&chanmon_cfgs[1].keys_manager,
		&chanmon_cfgs[1].tx_broadcaster,
		&chanmon_cfgs[1].fee_estimator,
	);

	let mut node_cfgs = create_node_cfgs(2, &chanmon_cfgs);

	let chain_mon_0 = test_utils::TestChainMonitor::new(
		Some(&chanmon_cfgs[0].chain_source),
		&chanmon_cfgs[0].tx_broadcaster,
		&chanmon_cfgs[0].logger,
		&chanmon_cfgs[0].fee_estimator,
		&persister_0,
		&chanmon_cfgs[0].keys_manager,
	);

	let chain_mon_1 = test_utils::TestChainMonitor::new(
		Some(&chanmon_cfgs[1].chain_source),
		&chanmon_cfgs[1].tx_broadcaster,
		&chanmon_cfgs[1].logger,
		&chanmon_cfgs[1].fee_estimator,
		&persister_1,
		&chanmon_cfgs[1].keys_manager,
	);

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
			// check that we stored only one monitor
			assert_eq!(persisted_chan_data_0.len(), 1);
			for (_, mon) in persisted_chan_data_0.iter() {
				assert_eq!(mon.get_latest_update_id(), $expected_update_id);

				let monitor_name = MonitorName::from(mon.get_funding_txo().0);
				assert_eq!(
					store_0
						.list(
							CHANNEL_MONITOR_UPDATE_PERSISTENCE_PRIMARY_NAMESPACE,
							monitor_name.as_str()
						)
						.unwrap()
						.len() as u64,
					mon.get_latest_update_id() % persister_0_max_pending_updates,
					"Wrong number of updates stored in persister 0",
				);
			}
			persisted_chan_data_1 = persister_1.read_all_channel_monitors_with_updates().unwrap();
			assert_eq!(persisted_chan_data_1.len(), 1);
			for (_, mon) in persisted_chan_data_1.iter() {
				assert_eq!(mon.get_latest_update_id(), $expected_update_id);

				let monitor_name = MonitorName::from(mon.get_funding_txo().0);
				assert_eq!(
					store_1
						.list(
							CHANNEL_MONITOR_UPDATE_PERSISTENCE_PRIMARY_NAMESPACE,
							monitor_name.as_str()
						)
						.unwrap()
						.len() as u64,
					mon.get_latest_update_id() % persister_1_max_pending_updates,
					"Wrong number of updates stored in persister 1",
				);
			}
		};
	}

	// Create some initial channel and check that a channel was persisted.
	let _ = create_announced_chan_between_nodes(&nodes, 0, 1);
	check_persisted_data!(0);

	// Send a few payments and make sure the monitors are updated to the latest.
	send_payment(&nodes[0], &vec![&nodes[1]][..], 8_000_000);
	check_persisted_data!(EXPECTED_UPDATES_PER_PAYMENT);
	send_payment(&nodes[1], &vec![&nodes[0]][..], 4_000_000);
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
		send_payment(&nodes[sender], &vec![&nodes[receiver]][..], 21_000);
		check_persisted_data!(i * EXPECTED_UPDATES_PER_PAYMENT);
	}

	// Force close because cooperative close doesn't result in any persisted
	// updates.

	let node_id_1 = nodes[1].node.get_our_node_id();
	let chan_id = nodes[0].node.list_channels()[0].channel_id;
	let err_msg = "Channel force-closed".to_string();
	nodes[0].node.force_close_broadcasting_latest_txn(&chan_id, &node_id_1, err_msg).unwrap();

	let reason = ClosureReason::HolderForceClosed { broadcasted_latest_txn: Some(true) };
	check_closed_event(&nodes[0], 1, reason, false, &[node_id_1], 100000);
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
	check_closed_event(&nodes[1], 1, reason, false, &[node_id_0], 100000);
	check_added_monitors!(nodes[1], 1);

	// Make sure everything is persisted as expected after close.
	check_persisted_data!(persister_0_max_pending_updates * 2 * EXPECTED_UPDATES_PER_PAYMENT + 1);
}
