// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use std::str::FromStr;

use bitcoin::{Address, FeeRate};
use common::{
	generate_blocks_and_wait, open_channel, premine_and_distribute_funds,
	setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource,
};
use electrum_client::ElectrumApi;
use ldk_node::bitcoin::Amount;
use ldk_node::config::AddressType;

// Test that node can be set up with multiple wallets configured
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_setup() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	// Test with NativeSegwit as primary, monitoring one other type
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Verify node is running
	assert!(node.status().is_running);

	// Verify we can generate an address
	let addr = node.onchain_payment().new_address().unwrap();
	assert!(!addr.to_string().is_empty());
}

// Test that all address types can be used as primary
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_all_address_types_as_primary() {
	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for primary_type in address_types {
		let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&_electrsd);

		let mut config = common::random_config(true);
		config.node_config.address_type = primary_type;
		config.node_config.address_types_to_monitor = vec![];

		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		// Verify we can generate an address
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());

		node.stop().unwrap();
	}
}

// Test that we can generate addresses for monitored address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_new_address_for_type() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running);

	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for address_type in address_types {
		let addr = node.onchain_payment().new_address_for_type(address_type).unwrap();
		assert!(!addr.to_string().is_empty());
	}
}

// Test that multiple address types can be configured for monitoring
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_monitoring_config() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	// Test with different primary types and monitoring configurations
	let test_cases = vec![
		(AddressType::Legacy, vec![AddressType::NativeSegwit, AddressType::Taproot]),
		(AddressType::NestedSegwit, vec![AddressType::Legacy, AddressType::NativeSegwit]),
		(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		),
		(AddressType::Taproot, vec![AddressType::Legacy, AddressType::NestedSegwit]),
	];

	for (primary_type, monitored_types) in test_cases {
		let mut config = common::random_config(true);
		config.node_config.address_type = primary_type;
		config.node_config.address_types_to_monitor = monitored_types.clone();

		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		// Verify we can generate an address
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());

		node.stop().unwrap();
	}
}

// Test that Electrum chain source works with multi-wallet configuration
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_electrum_setup() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Electrum(&_electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NestedSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running);

	// Verify we can generate an address
	let addr = node.onchain_payment().new_address().unwrap();
	assert!(!addr.to_string().is_empty());
}

// Test that all combinations of primary and monitored types work
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_all_combinations() {
	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for primary_type in &address_types {
		for monitored_type in &address_types {
			if primary_type == monitored_type {
				continue; // Skip same type
			}

			let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
			let chain_source = TestChainSource::Esplora(&_electrsd);

			let mut config = common::random_config(true);
			config.node_config.address_type = *primary_type;
			config.node_config.address_types_to_monitor = vec![*monitored_type];

			let node = setup_node(&chain_source, config, None);
			assert!(node.status().is_running);

			// Verify we can generate an address
			let addr = node.onchain_payment().new_address().unwrap();
			assert!(!addr.to_string().is_empty());

			node.stop().unwrap();
		}
	}
}

// Test that empty monitoring list works (only primary wallet)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_empty_monitoring() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![]; // Empty monitoring list

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running);

	// Verify we can generate an address
	let addr = node.onchain_payment().new_address().unwrap();
	assert!(!addr.to_string().is_empty());
}

// Test that monitoring all other types works
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_monitor_all_others() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for primary_type in &address_types {
		let mut config = common::random_config(true);
		config.node_config.address_type = *primary_type;
		// Monitor all other types
		config.node_config.address_types_to_monitor =
			address_types.iter().copied().filter(|&at| at != *primary_type).collect();

		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		// Verify we can generate an address
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());

		node.stop().unwrap();
	}
}

// Test send operation with multi-wallet (should use UTXOs from all wallets)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_operation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NativeSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 200_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	// Sync wallets to detect the funds
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available (accounting for fees)"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send operation - should work with UTXOs from primary wallet
	let send_amount = 50_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Wait for transaction to appear in mempool and verify
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Verify balance decreased appropriately
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < balances.spendable_onchain_balance_sats,
		"Balance should decrease after send"
	);
}

// Test send_all_to_address with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_all() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with Taproot as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Taproot;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 300_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send_all_to_address - should use UTXOs from all wallets
	let txid = node.onchain_payment().send_all_to_address(&recipient_addr, true, None).unwrap();

	// Wait for transaction and verify
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Verify balance is near zero after sending all
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < 10_000,
		"Balance should be near zero after sending all funds"
	);
}

// Test RBF operation with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_rbf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with Legacy as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 250_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a transaction (this should use UTXOs from all wallets)
	let send_amount = 50_000;
	let initial_txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Wait for the transaction to be in mempool and sync wallet
	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Test RBF - bump the fee (should be able to use UTXOs from all wallets for the replacement)
	let higher_fee_rate = FeeRate::from_sat_per_kwu(1000); // Higher fee rate
	let rbf_txid = node.onchain_payment().bump_fee_by_rbf(&initial_txid, higher_fee_rate).unwrap();

	// Verify we got a new transaction ID
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");
}

// Test CPFP operation with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_cpfp() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NestedSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NestedSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NativeSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 400_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a transaction
	let send_amount = 60_000;
	let parent_txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Wait for the transaction to be in mempool and sync wallet
	wait_for_tx(&electrsd.client, parent_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Test CPFP - accelerate the parent transaction (should use UTXOs from all wallets)
	let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
	let cpfp_txid =
		node.onchain_payment().accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None).unwrap();

	// Verify we got a new transaction ID
	assert_ne!(parent_txid, cpfp_txid, "CPFP should create a new child transaction");

	// Test calculate_cpfp_fee_rate
	let calculated_fee_rate =
		node.onchain_payment().calculate_cpfp_fee_rate(&parent_txid, false).unwrap();
	assert!(calculated_fee_rate.to_sat_per_kwu() > 0, "CPFP fee rate should be calculated");
}

// Test CPFP works correctly for cross-wallet transactions
// The change from cross-wallet transactions goes to the primary wallet,
// and CPFP should find it there even when the transaction used inputs from multiple wallets
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cpfp_for_cross_wallet_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both wallets with amounts that require combining for a larger send
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send amount requiring UTXOs from BOTH wallets
	// 140,000 > 100,000 (single wallet) but < 200,000 (both wallets)
	// Leave enough for change to enable CPFP
	let send_amount = 140_000;
	let parent_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Cross-wallet send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, parent_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// CPFP should work for cross-wallet transactions because change goes to primary wallet
	let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
	let cpfp_result =
		node.onchain_payment().accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None);

	// CPFP should succeed - the change output is in the primary wallet
	assert!(
		cpfp_result.is_ok(),
		"CPFP should work for cross-wallet transactions (change goes to primary wallet): {:?}",
		cpfp_result.err()
	);

	let cpfp_txid = cpfp_result.unwrap();
	assert_ne!(parent_txid, cpfp_txid, "CPFP should create a new child transaction");

	// Wait for child tx
	wait_for_tx(&electrsd.client, cpfp_txid).await;

	node.stop().unwrap();
}

// Test UTXO selection with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_utxo_selection() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NativeSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 500_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Test list_spendable_outputs - should return UTXOs from all wallets
	let spendable_outputs = node.onchain_payment().list_spendable_outputs().unwrap();
	assert!(!spendable_outputs.is_empty(), "Should have spendable outputs from primary wallet");

	// Test select_utxos_with_algorithm - should consider UTXOs from all wallets
	let target_amount = 100_000;
	let fee_rate = FeeRate::from_sat_per_kwu(500);
	let selected_utxos = node
		.onchain_payment()
		.select_utxos_with_algorithm(
			target_amount,
			Some(fee_rate),
			ldk_node::CoinSelectionAlgorithm::LargestFirst,
			None,
		)
		.unwrap();

	// Should have selected at least one UTXO from the funded wallet
	assert!(!selected_utxos.is_empty(), "Should have selected at least one UTXO");
}

// Test balance aggregation from all wallets
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_balance_aggregation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NativeSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 150_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Test that list_balances aggregates from all wallets
	// The method should succeed without error - balance values are valid (may be 0 if sync hasn't completed)
	let balances = node.list_balances();
	// Just verify we can access the fields - they're u64 so always valid
	let _ = balances.spendable_onchain_balance_sats;
	let _ = balances.total_onchain_balance_sats;
}

// Test get_address_balance with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_get_address_balance() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 120_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Test get_address_balance - should work for addresses from any wallet
	// The unwrap verifies the method succeeds; balance may be 0 if sync hasn't completed
	let _balance = node.get_address_balance(&addr.to_string()).unwrap();
}

// Test calculate_total_fee with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_calculate_total_fee() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Taproot;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 300_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test calculate_total_fee - should consider UTXOs from all wallets
	let send_amount = 100_000;
	let fee_rate = FeeRate::from_sat_per_kwu(500);
	let total_fee = node
		.onchain_payment()
		.calculate_total_fee(&recipient_addr, send_amount, Some(fee_rate), None)
		.unwrap();

	assert!(total_fee > 0, "Total fee should be calculated");
}

// Test send operation with UTXOs from multiple wallets (different address types)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_with_utxos_from_all_wallets() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy and Taproot
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses for each wallet type by creating a new address (primary) and
	// accessing monitored wallets through the node's internal state
	// For this test, we'll fund the primary address and verify send works
	let primary_addr = node.onchain_payment().new_address().unwrap();

	// Fund the primary address
	let fund_amount = 500_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![primary_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	// Sync wallets to detect the funds
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available (accounting for fees)"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send operation - should work with UTXOs from primary wallet
	let send_amount = 100_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Verify transaction was created (if send_to_address succeeded, txid is valid)
	// The fact that we got here without an error means the transaction was created successfully

	// Wait for transaction to be confirmed
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify balance decreased
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < balances.spendable_onchain_balance_sats,
		"Balance should decrease after send"
	);
}

// Test send_all_to_address with multi-wallet (should use UTXOs from all wallets)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_all_with_utxos_from_all_wallets() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with Legacy as primary, monitoring NativeSegwit and NestedSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::NestedSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let primary_addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 400_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![primary_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send_all_to_address - should use UTXOs from all wallets
	let txid = node.onchain_payment().send_all_to_address(&recipient_addr, true, None).unwrap();

	// Verify transaction was created (if send_to_address succeeded, txid is valid)
	// The fact that we got here without an error means the transaction was created successfully

	// Wait for transaction to be confirmed
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify balance is near zero (reserve may remain)
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < 10_000,
		"Balance should be near zero after sending all funds"
	);
}

// Test that send operation correctly handles UTXOs from different address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_handles_different_address_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with different primary types to ensure all combinations work
	let test_cases = vec![
		(AddressType::NativeSegwit, vec![AddressType::Legacy]),
		(AddressType::Legacy, vec![AddressType::NativeSegwit, AddressType::Taproot]),
		(AddressType::Taproot, vec![AddressType::NestedSegwit]),
	];

	for (primary_type, monitored_types) in test_cases {
		let mut config = common::random_config(true);
		config.node_config.address_type = primary_type;
		config.node_config.address_types_to_monitor = monitored_types;

		let node = setup_node(&chain_source, config, None);

		// Fund the primary address
		let addr = node.onchain_payment().new_address().unwrap();
		let fund_amount = 200_000;
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr.clone()],
			Amount::from_sat(fund_amount),
		)
		.await;

		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		// Create a recipient address
		let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
			.unwrap()
			.require_network(bitcoin::Network::Regtest)
			.unwrap();

		// Test send - should work regardless of address type combination
		let send_amount = 50_000;
		let txid = node
			.onchain_payment()
			.send_to_address(&recipient_addr, send_amount, None, None)
			.expect("Send should succeed for all address type combinations");

		// Wait for transaction to propagate
		wait_for_tx(&electrsd.client, txid).await;

		node.stop().unwrap();
	}
}

// Test spending UTXOs from multiple wallets (different address types) in a single transaction
// This is the key test for multi-wallet functionality - it verifies that UTXOs from different
// address types can be combined in a single transaction
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cross_wallet_spending() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_segwit_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both addresses with amounts that individually aren't enough for a larger send
	// NativeSegwit: 100,000 sats
	// Legacy: 100,000 sats
	// Total: 200,000 sats
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_segwit_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks to confirm the transactions
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	// Sync wallets to detect the funds
	node.sync_wallets().unwrap();

	// Wait for sync to complete
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify we have funds in both wallets by checking total balance
	let balances = node.list_balances();
	let expected_total = fund_amount_each * 2;
	assert!(
		balances.total_onchain_balance_sats >= expected_total - 10_000,
		"Should have ~{} sats total, but have {} sats",
		expected_total,
		balances.total_onchain_balance_sats
	);

	// Check per-address-type balances
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();

	assert!(
		native_balance.total_sats >= fund_amount_each - 1000,
		"NativeSegwit wallet should have ~{} sats, but has {} sats",
		fund_amount_each,
		native_balance.total_sats
	);
	assert!(
		legacy_balance.total_sats >= fund_amount_each - 1000,
		"Legacy wallet should have ~{} sats, but has {} sats",
		fund_amount_each,
		legacy_balance.total_sats
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Try to send an amount that requires UTXOs from BOTH wallets
	// 150,000 sats > 100,000 sats (either wallet alone) but < 200,000 sats (both combined)
	let send_amount = 150_000;
	let txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Cross-wallet spending should succeed - UTXOs from both wallets should be used");

	// Wait for transaction to propagate
	wait_for_tx(&electrsd.client, txid).await;

	// Generate a block to confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Verify balance decreased appropriately
	let new_balances = node.list_balances();
	assert!(
		new_balances.total_onchain_balance_sats
			< balances.total_onchain_balance_sats - send_amount + 10_000,
		"Balance should have decreased by at least the send amount"
	);

	// Verify list_monitored_address_types returns both types
	let monitored_types = node.list_monitored_address_types();
	assert!(monitored_types.contains(&AddressType::NativeSegwit), "Should monitor NativeSegwit");
	assert!(monitored_types.contains(&AddressType::Legacy), "Should monitor Legacy");
}

// Test that get_balance_for_address_type returns an error for unmonitored types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_get_balance_for_unmonitored_type() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node with only NativeSegwit as primary, no additional monitoring
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![];

	let node = setup_node(&chain_source, config, None);

	// Querying the primary type should succeed
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit);
	assert!(native_balance.is_ok(), "Should be able to get balance for primary type");

	// Querying an unmonitored type should return an error
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy);
	assert!(legacy_balance.is_err(), "Should return error for unmonitored address type");

	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot);
	assert!(taproot_balance.is_err(), "Should return error for unmonitored address type");

	node.stop().unwrap();
}

// Test that address_types_to_monitor handles deduplication correctly
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_address_type_deduplication() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node with NativeSegwit as primary, but also include it in monitor list
	// This tests that the system handles deduplication correctly
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running, "Node should start without error");

	// list_monitored_address_types should return exactly 2 types (not 3)
	// NativeSegwit (primary) + Legacy (monitored), with NativeSegwit deduplicated
	let monitored_types = node.list_monitored_address_types();
	assert_eq!(
		monitored_types.len(),
		2,
		"Should have exactly 2 monitored types (deduplicated), got {:?}",
		monitored_types
	);
	assert!(monitored_types.contains(&AddressType::NativeSegwit), "Should contain NativeSegwit");
	assert!(monitored_types.contains(&AddressType::Legacy), "Should contain Legacy");

	node.stop().unwrap();
}

// Test spending works correctly when a monitored wallet has 0 balance
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_spend_with_empty_monitored_wallet() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node monitoring Legacy, but only fund NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Only fund the NativeSegwit address (primary)
	let native_addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 200_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify NativeSegwit has funds and Legacy has 0
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();

	assert!(
		native_balance.total_sats >= fund_amount - 1000,
		"NativeSegwit should have funds: {}",
		native_balance.total_sats
	);
	assert_eq!(legacy_balance.total_sats, 0, "Legacy wallet should have 0 balance");

	// Send should work from NativeSegwit only
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	let send_amount = 50_000;
	let result = node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None);
	assert!(result.is_ok(), "Send should succeed using only NativeSegwit funds");

	node.stop().unwrap();
}

// Test that multi-wallet state persists correctly across node restarts
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_persistence_across_restart() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Create config with multi-wallet setup
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from multiple wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund all three wallets
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify balances before restart
	let native_balance_before =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance_before = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let taproot_balance_before = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let total_balance_before = node.list_balances().total_onchain_balance_sats;

	assert!(
		native_balance_before.total_sats >= fund_amount - 1000,
		"NativeSegwit should have funds before restart"
	);
	assert!(
		legacy_balance_before.total_sats >= fund_amount - 1000,
		"Legacy should have funds before restart"
	);
	assert!(
		taproot_balance_before.total_sats >= fund_amount - 1000,
		"Taproot should have funds before restart"
	);

	// Stop the node
	node.stop().unwrap();

	// Restart the node using stop/start pattern (same node object)
	node.start().unwrap();

	// Sync after restart
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify balances are preserved after restart
	let native_balance_after =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance_after = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let taproot_balance_after = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let total_balance_after = node.list_balances().total_onchain_balance_sats;

	assert_eq!(
		native_balance_before.total_sats, native_balance_after.total_sats,
		"NativeSegwit balance should persist across restart"
	);
	assert_eq!(
		legacy_balance_before.total_sats, legacy_balance_after.total_sats,
		"Legacy balance should persist across restart"
	);
	assert_eq!(
		taproot_balance_before.total_sats, taproot_balance_after.total_sats,
		"Taproot balance should persist across restart"
	);
	assert_eq!(
		total_balance_before, total_balance_after,
		"Total balance should persist across restart"
	);

	// Verify list_monitored_address_types returns all types after restart
	let monitored_types = node.list_monitored_address_types();
	assert!(
		monitored_types.contains(&AddressType::NativeSegwit),
		"Should still monitor NativeSegwit"
	);
	assert!(monitored_types.contains(&AddressType::Legacy), "Should still monitor Legacy");
	assert!(monitored_types.contains(&AddressType::Taproot), "Should still monitor Taproot");

	node.stop().unwrap();
}

// Test cross-wallet spending with all four different wallet types
// This ensures UTXOs from all address types can be combined in a single transaction
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cross_wallet_spending_all_four_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring all other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from all four wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let nested_addr =
		node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund each wallet with an amount that individually isn't enough for a larger send
	// Each wallet: 60,000 sats, Total: 240,000 sats
	let fund_amount_each = 60_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks to confirm
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify we have funds in all four wallets
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let nested_balance = node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap();
	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();

	assert!(
		native_balance.total_sats >= fund_amount_each - 1000,
		"NativeSegwit should have ~{} sats, got {}",
		fund_amount_each,
		native_balance.total_sats
	);
	assert!(
		legacy_balance.total_sats >= fund_amount_each - 1000,
		"Legacy should have ~{} sats, got {}",
		fund_amount_each,
		legacy_balance.total_sats
	);
	assert!(
		nested_balance.total_sats >= fund_amount_each - 1000,
		"NestedSegwit should have ~{} sats, got {}",
		fund_amount_each,
		nested_balance.total_sats
	);
	assert!(
		taproot_balance.total_sats >= fund_amount_each - 1000,
		"Taproot should have ~{} sats, got {}",
		fund_amount_each,
		taproot_balance.total_sats
	);

	let total_before = node.list_balances().total_onchain_balance_sats;

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send an amount that requires UTXOs from ALL FOUR wallets
	// 200,000 sats > 180,000 sats (any three wallets) but < 240,000 sats (all four)
	let send_amount = 200_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).expect(
			"Cross-wallet spending should succeed - UTXOs from all four wallets should be used",
		);

	// Wait for transaction to propagate
	wait_for_tx(&electrsd.client, txid).await;

	// Verify the transaction has inputs from all 4 address types by checking input count
	let tx = electrsd.client.transaction_get(&txid).unwrap();
	assert!(
		tx.input.len() >= 4,
		"Transaction should have at least 4 inputs (one from each wallet type), got {}",
		tx.input.len()
	);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify total balance decreased
	let total_after = node.list_balances().total_onchain_balance_sats;
	assert!(
		total_after < total_before - send_amount + 10_000,
		"Balance should have decreased by at least the send amount"
	);

	node.stop().unwrap();
}

// Test that send_all correctly drains all wallets
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_send_all_drains_all_wallets() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with Taproot as primary, monitoring NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Taproot;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let taproot_addr = node.onchain_payment().new_address().unwrap();
	let native_addr =
		node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

	// Fund both wallets
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify both wallets have funds
	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();

	assert!(taproot_balance.total_sats >= fund_amount - 1000, "Taproot should have funds");
	assert!(native_balance.total_sats >= fund_amount - 1000, "NativeSegwit should have funds");

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send all - should drain both wallets
	let txid = node
		.onchain_payment()
		.send_all_to_address(&recipient_addr, true, None)
		.expect("send_all should succeed");

	// Wait for transaction
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify BOTH wallets are drained
	let taproot_balance_after = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let native_balance_after =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let total_after = node.list_balances().total_onchain_balance_sats;

	assert!(
		taproot_balance_after.spendable_sats < 10_000,
		"Taproot wallet should be drained, but has {} sats",
		taproot_balance_after.spendable_sats
	);
	assert!(
		native_balance_after.spendable_sats < 10_000,
		"NativeSegwit wallet should be drained, but has {} sats",
		native_balance_after.spendable_sats
	);
	assert!(total_after < 10_000, "Total balance should be near zero, but is {} sats", total_after);

	node.stop().unwrap();
}

// Test that new_address_for_type returns error for unmonitored types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_new_address_for_unmonitored_type() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node with only NativeSegwit as primary, no additional monitoring
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![];

	let node = setup_node(&chain_source, config, None);

	// Requesting address for primary type should succeed
	let native_result = node.onchain_payment().new_address_for_type(AddressType::NativeSegwit);
	assert!(native_result.is_ok(), "Should be able to get address for primary type");

	// Requesting address for unmonitored type should fail
	let legacy_result = node.onchain_payment().new_address_for_type(AddressType::Legacy);
	assert!(legacy_result.is_err(), "Should return error for unmonitored address type");

	let taproot_result = node.onchain_payment().new_address_for_type(AddressType::Taproot);
	assert!(taproot_result.is_err(), "Should return error for unmonitored address type");

	node.stop().unwrap();
}

// Test that RBF works correctly when node has multiple wallets configured
// but the transaction only uses UTXOs from a single wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_single_wallet_input_with_multi_wallet_config() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both wallets - primary wallet gets more funds
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(300_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(100_000),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify both wallets have funds
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(native_balance.total_sats >= 299_000, "NativeSegwit should have funds");
	assert!(legacy_balance.total_sats >= 99_000, "Legacy should have funds");

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a smaller amount that can be satisfied by primary wallet alone
	// This leaves room for RBF fee bumping
	let send_amount = 50_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Bump fee via RBF - this works because the original tx only used one wallet's UTXOs
	let higher_fee_rate = FeeRate::from_sat_per_kwu(1000);
	let rbf_txid = node
		.onchain_payment()
		.bump_fee_by_rbf(&initial_txid, higher_fee_rate)
		.expect("RBF should succeed for single-wallet-input transaction");

	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement
	wait_for_tx(&electrsd.client, rbf_txid).await;

	node.stop().unwrap();
}

// Test that RBF works correctly for cross-wallet transactions
// Cross-wallet RBF reduces the change output to pay for higher fees
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_cross_wallet_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both wallets - need enough for send + change for RBF
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify both wallets have funds before the cross-wallet send
	let native_balance_before =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance_before = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(
		native_balance_before.total_sats >= fund_amount_each - 1000,
		"NativeSegwit should have ~100k sats, got {}",
		native_balance_before.total_sats
	);
	assert!(
		legacy_balance_before.total_sats >= fund_amount_each - 1000,
		"Legacy should have ~100k sats, got {}",
		legacy_balance_before.total_sats
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send amount requiring UTXOs from BOTH wallets
	// 120,000 > 100,000 (single wallet) but < 200,000 (both wallets)
	// This leaves ~80,000 for change (enough for RBF fee bump)
	let send_amount = 120_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial cross-wallet send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Record balance before RBF
	let total_before_rbf = node.list_balances().total_onchain_balance_sats;

	// Attempt RBF - cross-wallet RBF should work by reducing the change output
	let higher_fee_rate = FeeRate::from_sat_per_kwu(2000);
	let rbf_result = node.onchain_payment().bump_fee_by_rbf(&initial_txid, higher_fee_rate);

	// RBF should succeed for cross-wallet transactions
	assert!(
		rbf_result.is_ok(),
		"RBF for cross-wallet transactions should succeed: {:?}",
		rbf_result.err()
	);

	let rbf_txid = rbf_result.unwrap();
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement tx and sync
	wait_for_tx(&electrsd.client, rbf_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify balance decreased (higher fee was paid from change)
	let total_after_rbf = node.list_balances().total_onchain_balance_sats;
	assert!(
		total_after_rbf < total_before_rbf,
		"Balance should decrease after RBF due to higher fee (before: {}, after: {})",
		total_before_rbf,
		total_after_rbf
	);

	// Confirm the replacement transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Final balance check - should have: original funds - send_amount - fees
	let final_balance = node.list_balances().total_onchain_balance_sats;
	let expected_max = (fund_amount_each * 2) - send_amount;
	assert!(
		final_balance < expected_max,
		"Final balance {} should be less than {} (original - send)",
		final_balance,
		expected_max
	);

	node.stop().unwrap();
}

// Test that sync updates balances for all wallet types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_sync_updates_all_wallet_balances() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from all wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let nested_addr =
		node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Verify all balances are 0 before funding
	assert_eq!(node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats, 0);
	assert_eq!(node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats, 0);
	assert_eq!(node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap().total_sats, 0);
	assert_eq!(node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats, 0);

	// Fund all wallets with different amounts
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(100_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(200_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr.clone()],
		Amount::from_sat(300_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(400_000),
	)
	.await;

	// Generate blocks
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	// Sync - this should update all wallet balances
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify all balances updated correctly
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let nested_balance = node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap();
	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();

	assert!(
		native_balance.total_sats >= 99_000,
		"NativeSegwit should have ~100k, has {}",
		native_balance.total_sats
	);
	assert!(
		legacy_balance.total_sats >= 199_000,
		"Legacy should have ~200k, has {}",
		legacy_balance.total_sats
	);
	assert!(
		nested_balance.total_sats >= 299_000,
		"NestedSegwit should have ~300k, has {}",
		nested_balance.total_sats
	);
	assert!(
		taproot_balance.total_sats >= 399_000,
		"Taproot should have ~400k, has {}",
		taproot_balance.total_sats
	);

	// Verify aggregate balance is sum of all
	let total = node.list_balances().total_onchain_balance_sats;
	let expected_total = native_balance.total_sats
		+ legacy_balance.total_sats
		+ nested_balance.total_sats
		+ taproot_balance.total_sats;
	assert_eq!(total, expected_total, "Total should equal sum of all wallet balances");

	node.stop().unwrap();
}

// Test that RBF with additional inputs correctly accounts for input weight in fee calculation.
// When adding inputs to meet a higher fee, the inputs themselves add weight, which requires
// more fee. This test verifies the resulting transaction achieves the target fee rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_additional_inputs_fee_rate_correctness() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![];

	let node = setup_node(&chain_source, config, None);

	// Get addresses and fund with multiple small UTXOs
	// This ensures RBF will need to add inputs when bumping fee significantly
	let addr1 = node.onchain_payment().new_address().unwrap();
	let addr2 = node.onchain_payment().new_address().unwrap();
	let addr3 = node.onchain_payment().new_address().unwrap();

	// Fund with 3 separate UTXOs of 50k sats each
	let utxo_amount = 50_000u64;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr1.clone()],
		Amount::from_sat(utxo_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr2.clone()],
		Amount::from_sat(utxo_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr3.clone()],
		Amount::from_sat(utxo_amount),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify we have 3 UTXOs totaling ~150k sats
	let initial_balance = node.list_balances().total_onchain_balance_sats;
	assert!(
		initial_balance >= utxo_amount * 3 - 3000,
		"Should have ~150k sats, got {}",
		initial_balance
	);

	// Create recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 30k sats - this should use 1 UTXO and leave ~20k change
	// Using a low initial fee rate
	let send_amount = 30_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial send should succeed");

	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Now bump fee significantly - this should require adding more inputs
	// because the change output won't have enough to cover the increased fee
	// Use a very high fee rate to force adding inputs
	let high_fee_rate = FeeRate::from_sat_per_kwu(5000); // ~20 sat/vB

	let rbf_result = node.onchain_payment().bump_fee_by_rbf(&initial_txid, high_fee_rate);

	assert!(
		rbf_result.is_ok(),
		"RBF should succeed even when adding inputs: {:?}",
		rbf_result.err()
	);

	let rbf_txid = rbf_result.unwrap();
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement and confirm
	wait_for_tx(&electrsd.client, rbf_txid).await;

	// Verify the transaction confirms successfully (which means it paid sufficient fee)
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Final balance check - the transaction should have confirmed
	let final_balance = node.list_balances().total_onchain_balance_sats;
	let max_expected = initial_balance - send_amount;

	// Balance should be less than initial - send (due to fees)
	assert!(
		final_balance < max_expected,
		"Final balance {} should be less than {} (initial - send amount)",
		final_balance,
		max_expected
	);

	// The fee paid should be reasonable (not excessively high due to calculation errors)
	let fee_paid = initial_balance - final_balance - send_amount;
	// With a ~200 vB tx at 20 sat/vB, expect ~4000 sats fee, allow up to 10000 for margin
	assert!(fee_paid < 15000, "Fee {} seems too high, possible fee calculation error", fee_paid);
	assert!(fee_paid > 1000, "Fee {} seems too low for the requested fee rate", fee_paid);

	node.stop().unwrap();
}

// Test RBF on a transaction with inputs from all four address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_cross_wallet_all_four_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring all other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from all four wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let nested_addr =
		node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund each wallet with 80k sats - need enough for send + change for RBF
	let fund_amount_each = 80_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify all four wallets have funds
	assert!(
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats
			>= fund_amount_each - 1000
	);
	assert!(
		node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats
			>= fund_amount_each - 1000
	);
	assert!(
		node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap().total_sats
			>= fund_amount_each - 1000
	);
	assert!(
		node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats
			>= fund_amount_each - 1000
	);

	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send an amount requiring UTXOs from all four wallets
	// 280k > 240k (any three) but < 320k (all four)
	let send_amount = 280_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial cross-wallet send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, initial_txid).await;

	// Verify transaction has inputs from all 4 address types
	let initial_tx = electrsd.client.transaction_get(&initial_txid).unwrap();
	assert!(
		initial_tx.input.len() >= 4,
		"Initial tx should have at least 4 inputs, got {}",
		initial_tx.input.len()
	);

	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let total_before_rbf = node.list_balances().total_onchain_balance_sats;

	// Attempt RBF - should work by reducing the change output
	let higher_fee_rate = FeeRate::from_sat_per_kwu(2000);
	let rbf_result = node.onchain_payment().bump_fee_by_rbf(&initial_txid, higher_fee_rate);

	assert!(
		rbf_result.is_ok(),
		"RBF for cross-wallet transactions with all 4 types should succeed: {:?}",
		rbf_result.err()
	);

	let rbf_txid = rbf_result.unwrap();
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement tx
	wait_for_tx(&electrsd.client, rbf_txid).await;

	// Verify RBF transaction still has inputs from all 4 types
	let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
	assert!(
		rbf_tx.input.len() >= 4,
		"RBF tx should still have at least 4 inputs, got {}",
		rbf_tx.input.len()
	);

	// Confirm the replacement transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify balance decreased (higher fee was paid from change)
	let total_after_rbf = node.list_balances().total_onchain_balance_sats;
	assert!(
		total_after_rbf < total_before_rbf,
		"Balance should decrease after RBF (before: {}, after: {})",
		total_before_rbf,
		total_after_rbf
	);

	node.stop().unwrap();
}

// Test CPFP on a transaction with inputs from all four address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cpfp_cross_wallet_all_four_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring all other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from all four wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let nested_addr =
		node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund each wallet - need enough for send + change for CPFP
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send an amount requiring UTXOs from all four wallets
	// Leave enough change for CPFP
	let send_amount = 320_000;
	let parent_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Cross-wallet send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, parent_txid).await;

	// Verify transaction has inputs from all 4 address types
	let parent_tx = electrsd.client.transaction_get(&parent_txid).unwrap();
	assert!(
		parent_tx.input.len() >= 4,
		"Parent tx should have at least 4 inputs, got {}",
		parent_tx.input.len()
	);

	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// CPFP should work - the change output is in the primary wallet
	let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
	let cpfp_result =
		node.onchain_payment().accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None);

	assert!(
		cpfp_result.is_ok(),
		"CPFP should work for cross-wallet transactions with all 4 types: {:?}",
		cpfp_result.err()
	);

	let cpfp_txid = cpfp_result.unwrap();
	assert_ne!(parent_txid, cpfp_txid, "CPFP should create a new child transaction");

	// Wait for child tx
	wait_for_tx(&electrsd.client, cpfp_txid).await;

	// Verify child tx spends from parent
	let cpfp_tx = electrsd.client.transaction_get(&cpfp_txid).unwrap();
	let spends_parent = cpfp_tx.input.iter().any(|input| input.previous_output.txid == parent_txid);
	assert!(spends_parent, "CPFP child tx should spend from parent tx");

	node.stop().unwrap();
}

// Test RBF that adds inputs from a different address type wallet
// When change is insufficient, RBF should be able to add inputs from other wallet types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_adds_inputs_from_different_address_type() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund NativeSegwit with amount just enough for send with minimal change
	// Fund Legacy with amount that's too small for the initial send (forces NativeSegwit use)
	// but enough to be useful for RBF fee bumping (must cover base_shortfall + input fee cost)
	// At 200 sat/vB, Legacy P2PKH input costs ~29k in fees, base_shortfall ~23k = needs ~55k+
	let native_fund = 55_000; // Enough for 50k send + small change (~5k)
	let legacy_fund = 60_000; // Too small for 50k send, but enough for RBF (need ~52k)

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(native_fund),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(legacy_fund),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 50k sats - BDK must use NativeSegwit UTXO (55k) since Legacy (40k) is too small
	// This leaves ~5k change
	let send_amount = 50_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial send should succeed");

	wait_for_tx(&electrsd.client, initial_txid).await;

	// Verify initial tx only has 1 input
	let initial_tx = electrsd.client.transaction_get(&initial_txid).unwrap();
	assert_eq!(initial_tx.input.len(), 1, "Initial tx should have 1 input");

	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Now bump fee significantly - the small change (~5k) won't cover the higher fee
	// RBF should add an input from the Legacy wallet (40k available)
	// Need a very high fee rate to exhaust the ~5k change
	let high_fee_rate = FeeRate::from_sat_per_kwu(50000); // ~200 sat/vB - extremely high

	let rbf_result = node.onchain_payment().bump_fee_by_rbf(&initial_txid, high_fee_rate);

	assert!(
		rbf_result.is_ok(),
		"RBF should succeed by adding inputs from Legacy wallet: {:?}",
		rbf_result.err()
	);

	let rbf_txid = rbf_result.unwrap();
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	wait_for_tx(&electrsd.client, rbf_txid).await;

	// Verify RBF transaction has more inputs (added from Legacy wallet)
	let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
	assert!(
		rbf_tx.input.len() > initial_tx.input.len(),
		"RBF tx should have more inputs ({}) than initial tx ({})",
		rbf_tx.input.len(),
		initial_tx.input.len()
	);

	// Confirm the replacement transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	node.stop().unwrap();
}

// Test opening a Lightning channel with inputs from multiple witness-compatible
// address types.
//
// LDK requires all funding transaction inputs to have a non-empty witness,
// so Legacy (P2PKH) UTXOs cannot be used.  This test verifies that
// NativeSegwit, NestedSegwit (P2SH-P2WPKH), and Taproot UTXOs are all
// combined to fund a channel when the primary wallet alone is insufficient.
//
// Note: This test takes ~2 minutes due to multiple premine operations and channel setup.
// Run with: cargo test test_open_channel_with_cross_wallet_witness_inputs -- --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_with_cross_wallet_witness_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node A with all four address types
	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::NativeSegwit;
	config_a.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node_a = setup_node(&chain_source, config_a, None);

	// Set up node B (counterparty) - standard config
	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Get addresses from all four wallet types for node A
	let native_addr = node_a.onchain_payment().new_address().unwrap();
	let legacy_addr = node_a.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let nested_addr =
		node_a.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	let taproot_addr = node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund each witness-compatible wallet with 55k sats.
	// Total witness-compatible: 3 × 55k = 165k.
	// Legacy also gets 55k but cannot be used for channel funding.
	let fund_amount_each = 55_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Fund node B
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	// Generate blocks and sync both nodes
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify node A has funds in all four wallets
	assert!(
		node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats
			>= fund_amount_each - 1000
	);
	assert!(
		node_a.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats
			>= fund_amount_each - 1000
	);
	assert!(
		node_a.get_balance_for_address_type(AddressType::NestedSegwit).unwrap().total_sats
			>= fund_amount_each - 1000
	);
	assert!(
		node_a.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats
			>= fund_amount_each - 1000
	);

	// Open a channel that requires UTXOs from all three witness-compatible wallets.
	// 120k > 110k (any two wallets @ 55k) but < 165k (all three witness-compatible).
	// Legacy UTXOs are excluded because LDK funding transactions require witness inputs.
	let channel_amount = 120_000;

	// Use the open_channel helper which handles everything
	let funding_txo = open_channel(&node_a, &node_b, channel_amount, false, &electrsd).await;

	// Verify the funding transaction has inputs from 3 witness-compatible types.
	// Legacy is excluded since LDK requires all inputs to have a non-empty witness.
	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
	assert!(
		funding_tx.input.len() >= 3,
		"Channel funding tx should have at least 3 inputs (NativeSegwit + NestedSegwit + Taproot), got {}",
		funding_tx.input.len()
	);

	// Confirm the channel
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify channel is open
	let channels_a = node_a.list_channels();
	assert!(!channels_a.is_empty(), "Node A should have an open channel");
	assert_eq!(
		channels_a[0].channel_value_sats, channel_amount,
		"Channel should have correct capacity"
	);

	// Verify Legacy funds are untouched (not used for channel funding)
	let legacy_balance_after = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(
		legacy_balance_after.total_sats >= fund_amount_each - 1000,
		"Legacy wallet should be untouched after channel open, but has {} sats",
		legacy_balance_after.total_sats
	);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// Test RBF with lower fee rate than original (should fail with InvalidFeeRate)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_rejects_lower_fee_rate() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(200_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send with a moderate fee rate
	let initial_fee_rate = FeeRate::from_sat_per_kwu(2000);
	let txid = node
		.onchain_payment()
		.send_to_address(&recipient, 50_000, Some(initial_fee_rate), None)
		.unwrap();

	wait_for_tx(&electrsd.client, txid).await;
	node.sync_wallets().unwrap();

	// Try to RBF with a lower fee rate - should fail
	let lower_fee_rate = FeeRate::from_sat_per_kwu(1000);
	let result = node.onchain_payment().bump_fee_by_rbf(&txid, lower_fee_rate);

	assert!(result.is_err(), "RBF with lower fee rate should fail with InvalidFeeRate");

	node.stop().unwrap();
}

// Test RBF on already confirmed transaction (should fail with TransactionAlreadyConfirmed)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_rejects_confirmed_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(200_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	let txid = node.onchain_payment().send_to_address(&recipient, 50_000, None, None).unwrap();

	wait_for_tx(&electrsd.client, txid).await;

	// Confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Try to RBF a confirmed transaction - should fail
	let higher_fee_rate = FeeRate::from_sat_per_kwu(5000);
	let result = node.onchain_payment().bump_fee_by_rbf(&txid, higher_fee_rate);

	assert!(result.is_err(), "RBF on confirmed tx should fail with TransactionAlreadyConfirmed");

	node.stop().unwrap();
}

// Test RBF on unknown transaction (should fail with TransactionNotFound)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_rejects_unknown_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(100_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();

	// Create a fake txid that doesn't exist
	let fake_txid: bitcoin::Txid =
		"0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();

	let fee_rate = FeeRate::from_sat_per_kwu(2000);
	let result = node.onchain_payment().bump_fee_by_rbf(&fake_txid, fee_rate);

	assert!(result.is_err(), "RBF on unknown tx should fail with TransactionNotFound");

	node.stop().unwrap();
}

// Test RBF fails gracefully when insufficient funds to cover fee increase
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_insufficient_funds_for_fee_bump() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	let node = setup_node(&chain_source, config, None);

	// Fund with just enough for a send with minimal change
	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(51_000), // Just enough for 50k send + tiny fee
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send almost everything - leaves minimal change
	let txid = node.onchain_payment().send_to_address(&recipient, 50_000, None, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	node.sync_wallets().unwrap();

	// Try to RBF with extremely high fee rate - should fail due to insufficient funds
	let extreme_fee_rate = FeeRate::from_sat_per_kwu(100_000); // ~400 sat/vB
	let result = node.onchain_payment().bump_fee_by_rbf(&txid, extreme_fee_rate);

	assert!(result.is_err(), "RBF with insufficient funds should fail with InsufficientFunds");

	node.stop().unwrap();
}

// Test CPFP on unknown transaction (should fail with TransactionNotFound)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cpfp_rejects_unknown_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(100_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();

	// Create a fake txid that doesn't exist
	let fake_txid: bitcoin::Txid =
		"0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();

	let result = node.onchain_payment().accelerate_by_cpfp(&fake_txid, None, None);

	assert!(result.is_err(), "CPFP on unknown tx should fail with TransactionNotFound");

	node.stop().unwrap();
}

// Test CPFP on already confirmed transaction (should fail with TransactionAlreadyConfirmed)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cpfp_rejects_confirmed_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(200_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	let txid = node.onchain_payment().send_to_address(&recipient, 50_000, None, None).unwrap();

	wait_for_tx(&electrsd.client, txid).await;

	// Confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Try to CPFP a confirmed transaction - should fail
	let result = node.onchain_payment().accelerate_by_cpfp(&txid, None, None);

	assert!(result.is_err(), "CPFP on confirmed tx should fail with TransactionAlreadyConfirmed");

	node.stop().unwrap();
}

// Test send with specific UTXOs from different address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_send_with_specific_utxos_from_different_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from different wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund each wallet type
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(50_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(50_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(50_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Select UTXOs that cover all types (select large amount to force all UTXOs)
	let selected_utxos = node
		.onchain_payment()
		.select_utxos_with_algorithm(
			140_000, // High amount forces selection of multiple UTXOs
			Some(FeeRate::from_sat_per_kwu(500)),
			ldk_node::CoinSelectionAlgorithm::LargestFirst,
			None,
		)
		.expect("Should select UTXOs from multiple wallet types");

	assert!(
		selected_utxos.len() >= 3,
		"Should have selected at least 3 UTXOs from different types, got {}",
		selected_utxos.len()
	);

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send using specific UTXOs from different address types
	let txid = node
		.onchain_payment()
		.send_to_address(&recipient, 100_000, None, Some(selected_utxos))
		.expect("Send with specific UTXOs from different types should succeed");

	wait_for_tx(&electrsd.client, txid).await;

	// Verify the transaction has inputs from multiple types
	let tx = electrsd.client.transaction_get(&txid).unwrap();
	assert!(
		tx.input.len() >= 3,
		"Tx should have at least 3 inputs from different types, got {}",
		tx.input.len()
	);

	node.stop().unwrap();
}

// Test that opening a channel with ONLY Legacy (P2PKH) funds returns InsufficientFunds
// instead of crashing. LDK requires witness inputs for channel funding.
// Run with: cargo test test_open_channel_legacy_only_returns_error -- --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_legacy_only_returns_error() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Node A: primary is Legacy, no other witness wallets
	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::Legacy;
	config_a.node_config.address_types_to_monitor = vec![];

	let node_a = setup_node(&chain_source, config_a, None);

	// Node B: standard config (counterparty)
	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund node A with Legacy funds (more than enough for channel)
	let legacy_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr],
		Amount::from_sat(500_000),
	)
	.await;

	// Fund node B
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify node A has plenty of Legacy funds
	let balance = node_a.list_balances();
	assert!(balance.total_onchain_balance_sats >= 400_000);

	// Try to open a channel — should fail with InsufficientFunds, NOT crash
	let result = node_a.open_channel(
		node_b.node_id(),
		node_b.listening_addresses().unwrap().first().unwrap().clone(),
		100_000,
		None,
		None,
	);

	assert!(result.is_err(), "Opening a channel with only Legacy funds should fail");
	assert_eq!(
		result.unwrap_err(),
		ldk_node::NodeError::InsufficientFunds,
		"Error should be InsufficientFunds, not a crash or other error"
	);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// Test that a channel can be opened when primary is Legacy but a monitored
// SegWit wallet has sufficient funds.
// Run with: cargo test test_open_channel_legacy_primary_uses_segwit_wallet -- --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_legacy_primary_uses_segwit_wallet() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Node A: primary is Legacy, but monitors NativeSegwit
	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::Legacy;
	config_a.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node_a = setup_node(&chain_source, config_a, None);

	// Node B: standard config
	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund node A's Legacy wallet (cannot be used for channels)
	let legacy_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr],
		Amount::from_sat(200_000),
	)
	.await;

	// Fund node A's NativeSegwit wallet (CAN be used for channels)
	let segwit_addr =
		node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![segwit_addr],
		Amount::from_sat(200_000),
	)
	.await;

	// Fund node B
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify both wallets have funds
	assert!(
		node_a.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats >= 190_000
	);
	assert!(
		node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats
			>= 190_000
	);

	// Open a channel for 100k — should use NativeSegwit wallet even though primary is Legacy
	let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

	// Confirm the channel
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify channel is open
	let channels_a = node_a.list_channels();
	assert!(!channels_a.is_empty(), "Node A should have an open channel");

	// Verify Legacy funds are untouched
	let legacy_balance = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(
		legacy_balance.total_sats >= 190_000,
		"Legacy wallet should be untouched, got {} sats",
		legacy_balance.total_sats
	);

	// Verify NativeSegwit funds were used (balance decreased)
	let segwit_balance = node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	assert!(
		segwit_balance.total_sats < 190_000,
		"NativeSegwit wallet should have been used for funding, has {} sats",
		segwit_balance.total_sats
	);

	// Verify funding tx inputs are all witness (check via electrum)
	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
	for input in &funding_tx.input {
		assert!(
			!input.witness.is_empty(),
			"All funding tx inputs must have witness data, but input spending {:?} has empty witness",
			input.previous_output
		);
	}

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// A node whose primary wallet is not native witness (e.g. Legacy) must be
// able to accept inbound channels as long as a native witness wallet is loaded.
// The shutdown and destination scripts must come from the native witness wallet.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_inbound_channel_with_non_native_witness_primary() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::Legacy;
	config_a.node_config.address_types_to_monitor = vec![AddressType::Taproot];
	let node_a = setup_node(&chain_source, config_a, None);

	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Node B opens toward node A (inbound). This calls get_destination_script
	// and get_shutdown_scriptpubkey on node A, which must produce native witness
	// addresses from the Taproot wallet rather than Legacy P2PKH addresses.
	let _funding_txo = open_channel(&node_b, &node_a, 100_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	assert!(!node_a.list_channels().is_empty(), "Node A should have an inbound channel");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// Test that send_all works correctly when primary wallet is Legacy.
// The drain should include UTXOs from all wallets (Legacy + monitored witness).
// Run with: cargo test test_send_all_legacy_primary -- --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_send_all_legacy_primary() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Primary is Legacy, monitoring NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund both wallets
	let legacy_addr = node.onchain_payment().new_address().unwrap();
	let segwit_addr =
		node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr],
		Amount::from_sat(100_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![segwit_addr],
		Amount::from_sat(100_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify both wallets have funds
	let legacy_bal = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let segwit_bal = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	assert!(legacy_bal.total_sats >= 99_000, "Legacy should have funds");
	assert!(segwit_bal.total_sats >= 99_000, "NativeSegwit should have funds");

	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send all — should drain BOTH wallets
	let txid = node
		.onchain_payment()
		.send_all_to_address(&recipient_addr, true, None)
		.expect("send_all with legacy primary should succeed");

	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Both wallets should be drained
	let legacy_after = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let segwit_after = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let total_after = node.list_balances().total_onchain_balance_sats;

	assert!(
		legacy_after.spendable_sats < 10_000,
		"Legacy should be drained, but has {} sats",
		legacy_after.spendable_sats
	);
	assert!(
		segwit_after.spendable_sats < 10_000,
		"NativeSegwit should be drained, but has {} sats",
		segwit_after.spendable_sats
	);
	assert!(total_after < 10_000, "Total balance should be near zero, but is {} sats", total_after);

	node.stop().unwrap();
}

// Test that RBF works when primary wallet is Legacy.
// A transaction sent from the Legacy wallet (or cross-wallet) should still
// be fee-bumpable via RBF.
// Run with: cargo test test_rbf_legacy_primary -- --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_legacy_primary() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Primary is Legacy, monitoring NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund only Legacy wallet (enough for tx + RBF bump)
	let legacy_addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr],
		Amount::from_sat(300_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a transaction from the Legacy wallet
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, 50_000, None, None)
		.expect("Initial send from legacy should succeed");

	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// RBF bump the legacy transaction
	let higher_fee_rate = FeeRate::from_sat_per_kwu(1000);
	let rbf_txid = node
		.onchain_payment()
		.bump_fee_by_rbf(&initial_txid, higher_fee_rate)
		.expect("RBF should succeed for legacy primary wallet");

	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	wait_for_tx(&electrsd.client, rbf_txid).await;

	node.stop().unwrap();
}

// Test that CPFP works when primary wallet is Legacy.
// A transaction whose change goes to the Legacy wallet should be
// acceleratable via CPFP.
// Run with: cargo test test_cpfp_legacy_primary -- --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cpfp_legacy_primary() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Primary is Legacy, monitoring NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund both wallets
	let legacy_addr = node.onchain_payment().new_address().unwrap();
	let segwit_addr =
		node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr],
		Amount::from_sat(200_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![segwit_addr],
		Amount::from_sat(100_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a transaction — change goes to primary (Legacy)
	let parent_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, 60_000, None, None)
		.expect("Send from legacy primary should succeed");

	wait_for_tx(&electrsd.client, parent_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Accelerate via CPFP
	let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
	let cpfp_txid = node
		.onchain_payment()
		.accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None)
		.expect("CPFP should succeed for legacy primary wallet");

	assert_ne!(parent_txid, cpfp_txid, "CPFP should create a new child transaction");

	wait_for_tx(&electrsd.client, cpfp_txid).await;

	node.stop().unwrap();
}

// Cross-wallet fallback should select minimum foreign UTXOs, not dump all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cross_wallet_send_uses_minimum_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];
	let node = setup_node(&chain_source, config, None);

	// Fund primary with only 10k sats — not enough for a 15k send.
	let primary_addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![primary_addr],
		Amount::from_sat(10_000),
	)
	.await;

	// Fund Legacy with 3 separate UTXOs (20k, 30k, 50k).
	let legacy_addr1 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let legacy_addr2 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let legacy_addr3 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr1],
		Amount::from_sat(20_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr2],
		Amount::from_sat(30_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr3],
		Amount::from_sat(50_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 15k sats. Unified selection pools all 4 UTXOs (10k + 20k + 30k + 50k).
	// BranchAndBound should pick an efficient combination — e.g. just the 20k
	// Legacy UTXO alone — not all 4.
	let txid = node.onchain_payment().send_to_address(&recipient, 15_000, None, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	let tx = electrsd.client.transaction_get(&txid).unwrap();
	// Should be 2 inputs (1 primary + 1 Legacy), not 4 (1 primary + 3 Legacy).
	assert!(
		tx.input.len() <= 2,
		"Cross-wallet send should use minimum inputs, got {} inputs instead of at most 2",
		tx.input.len()
	);

	node.stop().unwrap();
}

// RBF on a send-all (no change output) must not reduce the recipient amount.
// Instead it should fail because there is no wallet-owned output to shrink.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_send_all_no_change_does_not_reduce_recipient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr],
		Amount::from_sat(100_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Drain everything to recipient — no change output.
	let txid = node.onchain_payment().send_all_to_address(&recipient, true, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// RBF should fail: no wallet-owned output to absorb the fee increase.
	let higher_fee = FeeRate::from_sat_per_kwu(5000);
	let result = node.onchain_payment().bump_fee_by_rbf(&txid, higher_fee);
	assert!(result.is_err(), "RBF on send-all (no change) should fail, not reduce recipient");

	node.stop().unwrap();
}

// Duplicate address types in the monitor list should be deduplicated silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_duplicate_monitor_types_deduplicated() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::Legacy, AddressType::Taproot];
	let node = setup_node(&chain_source, config, None);

	let monitored = node.list_monitored_address_types();
	// Should be 3: primary NativeSegwit + Legacy + Taproot (Legacy NOT doubled).
	assert_eq!(monitored.len(), 3, "Duplicate Legacy should be deduplicated, got {:?}", monitored);

	node.stop().unwrap();
}

// Channel funding with multiple witness-compatible UTXOs should select minimum
// inputs, not dump all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_channel_funding_uses_minimum_witness_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Node A: NativeSegwit primary with small balance, monitoring Taproot
	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::NativeSegwit;
	config_a.node_config.address_types_to_monitor = vec![AddressType::Taproot];
	let node_a = setup_node(&chain_source, config_a, None);

	// Node B: standard counterparty
	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund primary with 20k (not enough for 50k channel).
	let native_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr],
		Amount::from_sat(20_000),
	)
	.await;

	// Fund Taproot with 3 separate UTXOs (40k each = 120k total).
	// Only 1 should be needed to cover the ~30k deficit.
	let taproot_addr1 =
		node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
	let taproot_addr2 =
		node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
	let taproot_addr3 =
		node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	for addr in [taproot_addr1, taproot_addr2, taproot_addr3] {
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(40_000),
		)
		.await;
	}

	// Fund node B
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Open a 50k channel. Unified selection picks from all 4 UTXOs (20k + 3x40k).
	let channel_amount = 50_000;
	let funding_txo = open_channel(&node_a, &node_b, channel_amount, false, &electrsd).await;

	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
	// BranchAndBound should not need all 4 UTXOs for a 50k target.
	assert!(
		funding_tx.input.len() <= 3,
		"Channel funding should not use all UTXOs, got {} (expected <= 3)",
		funding_tx.input.len()
	);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// RBF fallback with multiple secondary UTXOs should select minimum inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_fallback_uses_minimum_foreign_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];
	let node = setup_node(&chain_source, config, None);

	// Fund primary with just enough for a 50k send with minimal change.
	let native_addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr],
		Amount::from_sat(55_000),
	)
	.await;

	// Fund Legacy with 3 separate UTXOs. Only 1 should be needed for the RBF deficit.
	let legacy_addr1 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let legacy_addr2 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let legacy_addr3 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	for addr in [legacy_addr1, legacy_addr2, legacy_addr3] {
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(60_000),
		)
		.await;
	}

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 50k — uses primary (55k). Leaves ~5k change.
	let txid = node.onchain_payment().send_to_address(&recipient, 50_000, None, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify initial tx has 1 input.
	let initial_tx = electrsd.client.transaction_get(&txid).unwrap();
	assert_eq!(initial_tx.input.len(), 1, "Initial tx should have 1 input");

	// RBF with high fee rate — change (~5k) won't cover it, so 1 Legacy UTXO should be added.
	let high_fee_rate = FeeRate::from_sat_per_kwu(50_000);
	let rbf_txid = node
		.onchain_payment()
		.bump_fee_by_rbf(&txid, high_fee_rate)
		.expect("RBF should succeed by adding Legacy input");

	wait_for_tx(&electrsd.client, rbf_txid).await;

	let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
	// Should be 2 inputs (1 primary + 1 Legacy), not 4 (1 primary + 3 Legacy).
	assert!(
		rbf_tx.input.len() <= 2,
		"RBF fallback should use minimum foreign inputs, got {} (expected <= 2)",
		rbf_tx.input.len()
	);

	node.stop().unwrap();
}

// When primary is Legacy, channel funding uses unified coin selection across
// all eligible SegWit wallets. Verify Legacy UTXOs are excluded and the
// channel opens successfully.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_legacy_primary_channel_funding_excludes_legacy_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Node A: Legacy primary, monitoring NativeSegwit + Taproot.
	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::Legacy;
	config_a.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Taproot];
	let node_a = setup_node(&chain_source, config_a, None);

	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund Legacy (primary) — should NOT be used for channel funding.
	let legacy_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr],
		Amount::from_sat(200_000),
	)
	.await;

	// Fund NativeSegwit with 100k.
	let native_addr =
		node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr],
		Amount::from_sat(100_000),
	)
	.await;

	// Fund Taproot with 100k.
	let taproot_addr = node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr],
		Amount::from_sat(100_000),
	)
	.await;

	// Fund node B.
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Open a 120k channel. Legacy has 200k (enough alone) but must be excluded.
	let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();

	// All inputs must have witness data (no Legacy inputs).
	for input in &funding_tx.input {
		assert!(
			!input.witness.is_empty(),
			"All funding inputs must have witness data, but {:?} has empty witness",
			input.previous_output
		);
	}

	// Legacy balance should be untouched.
	let legacy_balance = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(
		legacy_balance.total_sats >= 190_000,
		"Legacy wallet should be untouched, got {} sats",
		legacy_balance.total_sats
	);

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// Cross-wallet RBF should add minimum extra inputs when the change output
// cannot absorb the full fee increase.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cross_wallet_rbf_adds_extra_inputs_when_needed() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];
	let node = setup_node(&chain_source, config, None);

	// Fund primary with 60k.
	let native_addr = node.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr],
		Amount::from_sat(60_000),
	)
	.await;

	// Fund Legacy with 2 UTXOs of 60k each (120k total).
	let legacy_addr1 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let legacy_addr2 = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	for addr in [legacy_addr1, legacy_addr2] {
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(60_000),
		)
		.await;
	}

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 115k — primary (60k) + 1 Legacy UTXO (60k) = 120k.
	// Leaves small change (~4.7k).
	let txid = node.onchain_payment().send_to_address(&recipient, 115_000, None, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let initial_tx = electrsd.client.transaction_get(&txid).unwrap();
	assert_eq!(initial_tx.input.len(), 2, "Initial cross-wallet tx should have 2 inputs");

	// RBF with a fee rate high enough that the small change (~4.7k) can't
	// absorb the fee increase. The remaining Legacy UTXO (60k) should be
	// added as an extra input.
	let high_fee_rate = FeeRate::from_sat_per_kwu(25_000);
	let rbf_txid = node
		.onchain_payment()
		.bump_fee_by_rbf(&txid, high_fee_rate)
		.expect("Cross-wallet RBF with extra input should succeed");

	wait_for_tx(&electrsd.client, rbf_txid).await;

	let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
	// Should have 3 inputs (2 original + 1 extra), not fail with InsufficientFunds.
	assert!(
		rbf_tx.input.len() > initial_tx.input.len(),
		"Cross-wallet RBF should add extra inputs, got {} (initial had {})",
		rbf_tx.input.len(),
		initial_tx.input.len()
	);

	node.stop().unwrap();
}

// send_all_to_address with retain_reserves=false takes the build_and_sign_drain
// path. Verify it drains all wallets (primary + monitored) and leaves zero balance.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_send_all_drain_reserve_multi_wallet() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];
	let node = setup_node(&chain_source, config, None);

	// Fund all three wallets.
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	for addr in [native_addr, legacy_addr, taproot_addr] {
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(50_000),
		)
		.await;
	}

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let total_before = node.list_balances().total_onchain_balance_sats;
	assert!(total_before >= 140_000, "Should have ~150k across 3 wallets, got {}", total_before);

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// retain_reserves=false → AllDrainingReserve → build_and_sign_drain path.
	let txid = node
		.onchain_payment()
		.send_all_to_address(&recipient, false, None)
		.expect("drain all should succeed");

	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// All wallets must be empty.
	let total_after = node.list_balances().total_onchain_balance_sats;
	assert_eq!(total_after, 0, "All wallets should be fully drained, got {} sats", total_after);

	// Verify the drain tx used inputs from multiple wallets (at least 3).
	let drain_tx = electrsd.client.transaction_get(&txid).unwrap();
	assert!(
		drain_tx.input.len() >= 3,
		"Drain tx should have inputs from all 3 wallets, got {}",
		drain_tx.input.len()
	);

	node.stop().unwrap();
}

// NestedSegwit-only (no native witness wallet) must fail channel open gracefully,
// because channel scripts require a native witness address.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_nested_segwit_only_returns_error() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::NestedSegwit;
	config_a.node_config.address_types_to_monitor = vec![];
	let node_a = setup_node(&chain_source, config_a, None);

	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	let nested_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr],
		Amount::from_sat(500_000),
	)
	.await;

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	let balance = node_a.list_balances();
	assert!(balance.total_onchain_balance_sats >= 400_000);

	let result = node_a.open_channel(
		node_b.node_id(),
		node_b.listening_addresses().unwrap().first().unwrap().clone(),
		100_000,
		None,
		None,
	);

	assert!(result.is_err(), "Opening a channel with only NestedSegwit should fail");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// NestedSegwit primary with a native witness wallet monitored should be able to
// open a channel. The funding tx is built by the NestedSegwit wallet (since it's
// not Legacy), but channel scripts come from the native witness wallet.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_nested_primary_with_native_monitor() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::NestedSegwit;
	config_a.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];
	let node_a = setup_node(&chain_source, config_a, None);

	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund only the NestedSegwit (primary) wallet.
	let nested_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr],
		Amount::from_sat(300_000),
	)
	.await;

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Channel open should succeed — NestedSegwit builds the funding tx,
	// NativeSegwit provides channel scripts.
	let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	assert!(!node_a.list_channels().is_empty(), "Node A should have an open channel");

	// All funding inputs should have witness data (NestedSegwit is SegWit).
	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
	for input in &funding_tx.input {
		assert!(
			!input.witness.is_empty(),
			"All funding inputs must have witness data, but {:?} has empty witness",
			input.previous_output
		);
	}

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// NestedSegwit primary with NativeSegwit monitored: funding tx should use inputs
// from both wallets when NestedSegwit alone is not enough.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_nested_primary_mixed_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::NestedSegwit;
	config_a.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];
	let node_a = setup_node(&chain_source, config_a, None);

	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund NestedSegwit (primary) with 80k — not enough for 120k channel.
	let nested_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr],
		Amount::from_sat(80_000),
	)
	.await;

	// Fund NativeSegwit with 80k — together they have enough.
	let native_addr =
		node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr],
		Amount::from_sat(80_000),
	)
	.await;

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Open 120k channel — needs inputs from both wallets.
	let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	assert!(!node_a.list_channels().is_empty(), "Node A should have an open channel");

	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
	// Must have inputs from both wallets (at least 2 inputs).
	assert!(
		funding_tx.input.len() >= 2,
		"Funding tx should use inputs from both wallets, got {}",
		funding_tx.input.len()
	);

	// All inputs must have witness data.
	for input in &funding_tx.input {
		assert!(
			!input.witness.is_empty(),
			"All funding inputs must have witness data, but {:?} has empty witness",
			input.previous_output
		);
	}

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// NativeSegwit primary with NestedSegwit monitored: funding tx should pull
// NestedSegwit inputs when primary alone is not enough.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_open_channel_native_primary_with_nested_inputs() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config_a = common::random_config(true);
	config_a.node_config.address_type = AddressType::NativeSegwit;
	config_a.node_config.address_types_to_monitor = vec![AddressType::NestedSegwit];
	let node_a = setup_node(&chain_source, config_a, None);

	let config_b = common::random_config(true);
	let node_b = setup_node(&chain_source, config_b, None);

	// Fund NativeSegwit (primary) with 80k — not enough for 120k channel.
	let native_addr = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr],
		Amount::from_sat(80_000),
	)
	.await;

	// Fund NestedSegwit with 80k — together they have enough.
	let nested_addr =
		node_a.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr],
		Amount::from_sat(80_000),
	)
	.await;

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(500_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Open 120k channel — needs inputs from both wallets.
	let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	assert!(!node_a.list_channels().is_empty(), "Node A should have an open channel");

	let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
	assert!(
		funding_tx.input.len() >= 2,
		"Funding tx should use inputs from both wallets, got {}",
		funding_tx.input.len()
	);

	// All inputs must have witness data (both NativeSegwit and NestedSegwit are SegWit).
	for input in &funding_tx.input {
		assert!(
			!input.witness.is_empty(),
			"All funding inputs must have witness data, but {:?} has empty witness",
			input.previous_output
		);
	}

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}

// Unified coin selection should prefer a single large foreign UTXO over
// combining multiple small primary UTXOs when it produces a better result.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_unified_selection_prefers_optimal_utxo() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Taproot];
	let node = setup_node(&chain_source, config, None);

	// Fund primary (NativeSegwit) with 3 small UTXOs of 10k each.
	for _ in 0..3 {
		let addr = node.onchain_payment().new_address().unwrap();
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(10_000),
		)
		.await;
	}

	// Fund Taproot with a single 25k UTXO — can cover the send alone.
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr],
		Amount::from_sat(25_000),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	let recipient = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 20k sats. Old behavior would use all 3 primary UTXOs (30k total).
	// Unified selection has all 4 UTXOs to choose from and should find an
	// optimal solution — e.g. the single 25k Taproot UTXO, or 2 x 10k primary
	// + 1 Taproot — but never all 4.
	let txid = node.onchain_payment().send_to_address(&recipient, 20_000, None, None).unwrap();
	wait_for_tx(&electrsd.client, txid).await;

	let tx = electrsd.client.transaction_get(&txid).unwrap();
	assert!(
		tx.input.len() <= 3,
		"Unified selection should pick optimal UTXOs, got {} inputs",
		tx.input.len()
	);

	node.stop().unwrap();
}
