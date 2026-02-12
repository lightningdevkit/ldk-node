// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------
mod helpers {
	use std::str::FromStr;

	use bitcoin::{Address, Txid};
	use electrsd::corepc_node::Node as BitcoinD;
	use electrsd::ElectrsD;
	use ldk_node::bitcoin::Amount;
	use ldk_node::config::AddressType;
	use ldk_node::Node;

	use crate::common::{generate_blocks_and_wait, premine_and_distribute_funds};

	/// Default amount to fund a peer node for channel tests.
	pub const CHANNEL_PEER_FUNDING_SATS: u64 = 500_000;

	/// Fake txid used in error-case tests (RBF/CPFP reject unknown tx).
	pub fn fake_txid_for_error_tests() -> Txid {
		"0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap()
	}

	/// Standard regtest recipient address used across all tests.
	pub fn test_recipient() -> Address {
		Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
			.unwrap()
			.require_network(bitcoin::Network::Regtest)
			.unwrap()
	}

	/// Fund a single address, mine 6 blocks, sync the node, and sleep.
	pub async fn fund_and_sync(
		bitcoind: &BitcoinD, electrsd: &ElectrsD, node: &Node, addr: Address, amount: u64,
	) {
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(amount),
		)
		.await;
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(2));
	}

	/// Fund multiple addresses (each with its own amount), mine 6 blocks,
	/// sync, and sleep once at the end.
	pub async fn fund_multiple_and_sync(
		bitcoind: &BitcoinD, electrsd: &ElectrsD, node: &Node,
		addrs_and_amounts: Vec<(Address, u64)>,
	) {
		for (addr, amount) in addrs_and_amounts {
			premine_and_distribute_funds(
				&bitcoind.client,
				&electrsd.client,
				vec![addr],
				Amount::from_sat(amount),
			)
			.await;
		}
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(2));
	}

	/// Fund a peer node for channel tests: premine, mine 6 blocks, sync node, sleep.
	pub async fn fund_peer_node_and_sync(
		bitcoind: &BitcoinD, electrsd: &ElectrsD, node: &Node, addr: Address, amount: u64,
	) {
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr],
			Amount::from_sat(amount),
		)
		.await;
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(2));
	}

	/// Mine blocks, sync node(s), and sleep. Used after confirming txs.
	pub async fn confirm_and_sync(
		bitcoind: &BitcoinD, electrsd: &ElectrsD, blocks: usize, nodes: &[&Node],
	) {
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, blocks).await;
		for node in nodes {
			node.sync_wallets().unwrap();
		}
		std::thread::sleep(std::time::Duration::from_secs(1));
	}

	/// Convenience: create a node config with a given primary + monitored types.
	pub fn node_config(
		primary: AddressType, monitored: Vec<AddressType>,
	) -> crate::common::TestConfig {
		let mut config = crate::common::random_config(true);
		config.node_config.address_type = primary;
		config.node_config.address_types_to_monitor = monitored;
		config
	}
}

// ---------------------------------------------------------------------------
// Setup & Configuration
// ---------------------------------------------------------------------------
mod setup {
	use ldk_node::config::AddressType;

	use crate::common::{setup_bitcoind_and_electrsd, setup_node, TestChainSource};
	use crate::helpers::{fund_multiple_and_sync, node_config};

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_multi_wallet_setup() {
		let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&_electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		assert!(node.status().is_running);
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());
		node.stop().unwrap();
	}

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

			let config = node_config(primary_type, vec![]);
			let node = setup_node(&chain_source, config, None);
			assert!(node.status().is_running);

			let addr = node.onchain_payment().new_address().unwrap();
			assert!(!addr.to_string().is_empty());
			node.stop().unwrap();
		}
	}

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
					continue;
				}

				let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
				let chain_source = TestChainSource::Esplora(&_electrsd);

				let config = node_config(*primary_type, vec![*monitored_type]);
				let node = setup_node(&chain_source, config, None);
				assert!(node.status().is_running);

				let addr = node.onchain_payment().new_address().unwrap();
				assert!(!addr.to_string().is_empty());
				node.stop().unwrap();

				// Brief delay to allow OS to release ports (TIME_WAIT) before next bind
				std::thread::sleep(std::time::Duration::from_millis(100));
			}
		}
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_multi_wallet_electrum_setup() {
		let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Electrum(&_electrsd);

		let config = node_config(
			AddressType::NestedSegwit,
			vec![AddressType::NativeSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());
		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_multi_wallet_empty_monitoring() {
		let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&_electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());
		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_new_address_for_type() {
		let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&_electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		for address_type in [
			AddressType::Legacy,
			AddressType::NestedSegwit,
			AddressType::NativeSegwit,
			AddressType::Taproot,
		] {
			let addr = node.onchain_payment().new_address_for_type(address_type).unwrap();
			assert!(!addr.to_string().is_empty());
		}
		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_new_address_for_unmonitored_type() {
		let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		assert!(node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).is_ok());
		assert!(node.onchain_payment().new_address_for_type(AddressType::Legacy).is_err());
		assert!(node.onchain_payment().new_address_for_type(AddressType::Taproot).is_err());

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_duplicate_monitor_types_deduplicated() {
		let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::Legacy, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let monitored = node.list_monitored_address_types();
		assert_eq!(
			monitored.len(),
			3,
			"Duplicate Legacy should be deduplicated, got {:?}",
			monitored
		);

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_multi_wallet_persistence_across_restart() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config =
			node_config(AddressType::NativeSegwit, vec![AddressType::Legacy, AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000), (taproot_addr, 100_000)],
		)
		.await;

		let native_before =
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;
		let legacy_before =
			node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats;
		let taproot_before =
			node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats;
		let total_before = node.list_balances().total_onchain_balance_sats;

		assert!(native_before >= 99_000);
		assert!(legacy_before >= 99_000);
		assert!(taproot_before >= 99_000);

		node.stop().unwrap();
		node.start().unwrap();

		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		assert_eq!(
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats,
			native_before
		);
		assert_eq!(
			node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats,
			legacy_before
		);
		assert_eq!(
			node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats,
			taproot_before
		);
		assert_eq!(node.list_balances().total_onchain_balance_sats, total_before);

		let monitored = node.list_monitored_address_types();
		assert!(monitored.contains(&AddressType::NativeSegwit));
		assert!(monitored.contains(&AddressType::Legacy));
		assert!(monitored.contains(&AddressType::Taproot));

		node.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// Electrum chain source
// ---------------------------------------------------------------------------
mod electrum_chain {
	use ldk_node::config::AddressType;

	use crate::common::{setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource};
	use crate::helpers::{confirm_and_sync, fund_multiple_and_sync, node_config, test_recipient};

	/// Electrum chain source: balance sync and send work with multi-address-types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_electrum_sync_and_send() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Electrum(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000)],
		)
		.await;

		let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
		let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(native_balance.total_sats >= 99_000);
		assert!(legacy_balance.total_sats >= 99_000);

		let txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 50_000, None, None)
			.expect("Send with Electrum should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let total_after = node.list_balances().total_onchain_balance_sats;
		assert!(total_after < 200_000 - 50_000 + 5_000);

		node.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// Balance
// ---------------------------------------------------------------------------
mod balance {
	use ldk_node::config::AddressType;

	use crate::common::{setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource};
	use crate::helpers::{
		confirm_and_sync, fund_and_sync, fund_multiple_and_sync, node_config, test_recipient,
	};

	// --- Balance sync & queries ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_sync_updates_all_wallet_balances() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let nested_addr =
			node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		// Verify all balances are 0 before funding.
		assert_eq!(
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats,
			0
		);
		assert_eq!(node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats, 0);
		assert_eq!(
			node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap().total_sats,
			0
		);
		assert_eq!(node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats, 0);

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(native_addr, 100_000),
				(legacy_addr, 200_000),
				(nested_addr, 300_000),
				(taproot_addr, 400_000),
			],
		)
		.await;

		let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
		let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
		let nested_balance = node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap();
		let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();

		assert!(native_balance.total_sats >= 99_000, "NativeSegwit: {}", native_balance.total_sats);
		assert!(legacy_balance.total_sats >= 199_000, "Legacy: {}", legacy_balance.total_sats);
		assert!(
			nested_balance.total_sats >= 299_000,
			"NestedSegwit: {}",
			nested_balance.total_sats
		);
		assert!(taproot_balance.total_sats >= 399_000, "Taproot: {}", taproot_balance.total_sats);

		let total = node.list_balances().total_onchain_balance_sats;
		let expected_total = native_balance.total_sats
			+ legacy_balance.total_sats
			+ nested_balance.total_sats
			+ taproot_balance.total_sats;
		assert_eq!(total, expected_total);

		node.stop().unwrap();
	}

	// --- Error cases ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_get_balance_for_unmonitored_type() {
		let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		assert!(node.get_balance_for_address_type(AddressType::NativeSegwit).is_ok());
		assert!(node.get_balance_for_address_type(AddressType::Legacy).is_err());
		assert!(node.get_balance_for_address_type(AddressType::Taproot).is_err());

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_spend_with_empty_monitored_wallet() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, native_addr, 200_000).await;

		let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
		let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(native_balance.total_sats >= 199_000);
		assert_eq!(legacy_balance.total_sats, 0);

		let result = node.onchain_payment().send_to_address(&test_recipient(), 50_000, None, None);
		assert!(result.is_ok(), "Send should succeed using only NativeSegwit funds");

		node.stop().unwrap();
	}

	/// Send fails with InsufficientFunds when total across all wallets is insufficient.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_insufficient_funds_returns_error() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config =
			node_config(AddressType::NativeSegwit, vec![AddressType::Legacy, AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		// Total ~150k across all wallets.
		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 50_000), (legacy_addr, 50_000), (taproot_addr, 50_000)],
		)
		.await;

		let total = node.list_balances().total_onchain_balance_sats;
		assert!(total < 200_000);

		let result = node.onchain_payment().send_to_address(&test_recipient(), 200_000, None, None);
		assert!(result.is_err(), "Send should fail when total balance is insufficient");
		let err = result.unwrap_err();
		assert!(
			matches!(
				err,
				ldk_node::NodeError::InsufficientFunds
					| ldk_node::NodeError::OnchainTxCreationFailed
			),
			"Expected InsufficientFunds or OnchainTxCreationFailed, got {:?}",
			err
		);

		node.stop().unwrap();
	}

	// --- Balance consistency ---

	/// Asserts listBalances().spendableOnchainBalanceSats equals sum of listSpendableOutputs() values.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_list_balances_matches_utxo_sum() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (taproot_addr, 200_000)],
		)
		.await;

		let balances = node.list_balances();
		let utxos = node.onchain_payment().list_spendable_outputs().unwrap();
		let utxo_sum: u64 = utxos.iter().map(|u| u.value_sats).sum();

		assert_eq!(
			balances.spendable_onchain_balance_sats,
			utxo_sum,
			"listBalances spendable should equal UTXO sum (spendable={} utxo_sum={})",
			balances.spendable_onchain_balance_sats,
			utxo_sum
		);

		node.stop().unwrap();
	}

	/// Asserts sum of per-type spendable equals listBalances().spendableOnchainBalanceSats.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_per_type_sum_equals_aggregate_spendable() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(native_addr, 80_000),
				(legacy_addr, 70_000),
				(taproot_addr, 90_000),
			],
		)
		.await;

		let aggregate_spendable = node.list_balances().spendable_onchain_balance_sats;
		let per_type_sum: u64 = node
			.list_monitored_address_types()
			.iter()
			.filter_map(|t| node.get_balance_for_address_type(*t).ok())
			.map(|b| b.spendable_sats)
			.sum();

		assert_eq!(
			per_type_sum,
			aggregate_spendable,
			"Sum of per-type spendable should equal aggregate (per_type_sum={} aggregate={})",
			per_type_sum,
			aggregate_spendable
		);

		node.stop().unwrap();
	}

	/// Asserts no phantom balance after spend-all: listSpendableOutputs empty, total==0.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_no_phantom_balance_after_spend_all() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 50_000), (taproot_addr, 50_000)],
		)
		.await;

		let txid = node
			.onchain_payment()
			.send_all_to_address(&test_recipient(), false, None)
			.expect("spend-all should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		// Poll a few times to catch sync timing issues
		for _ in 0..5 {
			let utxos = node.onchain_payment().list_spendable_outputs().unwrap();
			let balances = node.list_balances();

			assert!(
				utxos.is_empty(),
				"listSpendableOutputs should be empty after spend-all"
			);
			assert_eq!(
				balances.total_onchain_balance_sats,
				0,
				"total_onchain_balance_sats should be 0"
			);
			assert_eq!(
				balances.spendable_onchain_balance_sats,
				0,
				"spendable_onchain_balance_sats should be 0"
			);

			std::thread::sleep(std::time::Duration::from_secs(1));
		}

		node.stop().unwrap();
	}

	/// Asserts spendable equals UTXO sum for a single-address-type wallet.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_balance_consistency_single_address_type() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 150_000).await;

		let balances = node.list_balances();
		let utxos = node.onchain_payment().list_spendable_outputs().unwrap();
		let utxo_sum: u64 = utxos.iter().map(|u| u.value_sats).sum();

		assert_eq!(
			balances.spendable_onchain_balance_sats,
			utxo_sum,
			"Single-type: spendable should equal UTXO sum"
		);

		node.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------
mod send {
	use bitcoin::FeeRate;
	use electrum_client::ElectrumApi;
	use ldk_node::config::AddressType;

	use crate::common::{setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource};
	use crate::helpers::{
		confirm_and_sync, fund_and_sync, fund_multiple_and_sync, node_config, test_recipient,
	};

	// --- Basic & cross-wallet sends ---

	/// Basic send works for all primary address types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_basic() {
		let test_cases = vec![
			(AddressType::NativeSegwit, vec![AddressType::Legacy]),
			(AddressType::Legacy, vec![AddressType::NativeSegwit, AddressType::Taproot]),
			(AddressType::Taproot, vec![AddressType::NestedSegwit]),
		];

		for (primary_type, monitored_types) in test_cases {
			let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
			let chain_source = TestChainSource::Esplora(&electrsd);

			let config = node_config(primary_type, monitored_types);
			let node = setup_node(&chain_source, config, None);

			let addr = node.onchain_payment().new_address().unwrap();
			fund_and_sync(&bitcoind, &electrsd, &node, addr, 200_000).await;

			let balances = node.list_balances();
			assert!(balances.spendable_onchain_balance_sats >= 190_000);

			let txid = node
				.onchain_payment()
				.send_to_address(&test_recipient(), 50_000, None, None)
				.expect("Send should succeed for all address type combinations");

			wait_for_tx(&electrsd.client, txid).await;
			confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

			let new_balances = node.list_balances();
			assert!(
				new_balances.spendable_onchain_balance_sats
					< balances.spendable_onchain_balance_sats
			);

			node.stop().unwrap();
		}
	}

	/// Send with custom fee rate across address types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_with_custom_fee_rate() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000)],
		)
		.await;

		let custom_fee_rate = bitcoin::FeeRate::from_sat_per_kwu(800);
		let txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 50_000, Some(custom_fee_rate), None)
			.expect("Send with custom fee rate should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let new_balance = node.list_balances().total_onchain_balance_sats;
		assert!(new_balance < 200_000 - 50_000);

		node.stop().unwrap();
	}

	/// Send requiring UTXOs from two different wallet types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cross_wallet_spending() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000)],
		)
		.await;

		let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
		let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(native_balance.total_sats >= 99_000);
		assert!(legacy_balance.total_sats >= 99_000);

		// 150k > either wallet alone, requires both.
		let txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 150_000, None, None)
			.expect("Cross-wallet spending should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let monitored_types = node.list_monitored_address_types();
		assert!(monitored_types.contains(&AddressType::NativeSegwit));
		assert!(monitored_types.contains(&AddressType::Legacy));

		node.stop().unwrap();
	}

	/// Send requiring UTXOs from all four wallet types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cross_wallet_spending_all_four_types() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let nested_addr =
			node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(native_addr, 60_000),
				(legacy_addr, 60_000),
				(nested_addr, 60_000),
				(taproot_addr, 60_000),
			],
		)
		.await;

		let total_before = node.list_balances().total_onchain_balance_sats;

		// 200k > any three wallets (180k), requires all four.
		let txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 200_000, None, None)
			.expect("Cross-wallet spending with all 4 types should succeed");

		wait_for_tx(&electrsd.client, txid).await;

		let tx = electrsd.client.transaction_get(&txid).unwrap();
		assert!(tx.input.len() >= 4, "Should use at least 4 inputs, got {}", tx.input.len());

		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let total_after = node.list_balances().total_onchain_balance_sats;
		assert!(total_after < total_before - 200_000 + 10_000);

		node.stop().unwrap();
	}

	// --- send_all ---

	/// send_all drains both wallets (retain_reserves=true).
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_all_drains_all_wallets() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::Taproot, vec![AddressType::NativeSegwit]);
		let node = setup_node(&chain_source, config, None);

		let taproot_addr = node.onchain_payment().new_address().unwrap();
		let native_addr =
			node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(taproot_addr, 100_000), (native_addr, 100_000)],
		)
		.await;

		assert!(
			node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats >= 99_000
		);
		assert!(
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats
				>= 99_000
		);

		let txid = node
			.onchain_payment()
			.send_all_to_address(&test_recipient(), true, None)
			.expect("send_all should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		assert!(
			node.get_balance_for_address_type(AddressType::Taproot).unwrap().spendable_sats
				< 10_000
		);
		assert!(
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().spendable_sats
				< 10_000
		);
		assert!(node.list_balances().total_onchain_balance_sats < 10_000);

		node.stop().unwrap();
	}

	/// send_all with retain_reserves=false drains every wallet to zero.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_all_drain_reserve_multi_wallet() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config =
			node_config(AddressType::NativeSegwit, vec![AddressType::Legacy, AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 50_000), (legacy_addr, 50_000), (taproot_addr, 50_000)],
		)
		.await;

		let total_before = node.list_balances().total_onchain_balance_sats;
		assert!(total_before >= 140_000);

		let txid = node
			.onchain_payment()
			.send_all_to_address(&test_recipient(), false, None)
			.expect("drain all should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		assert_eq!(node.list_balances().total_onchain_balance_sats, 0);

		let drain_tx = electrsd.client.transaction_get(&txid).unwrap();
		assert!(
			drain_tx.input.len() >= 3,
			"Should drain all 3 wallets, got {} inputs",
			drain_tx.input.len()
		);

		node.stop().unwrap();
	}

	/// send_all with custom fee rate drains all wallets.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_all_with_custom_fee_rate() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000)],
		)
		.await;

		let custom_fee_rate = bitcoin::FeeRate::from_sat_per_kwu(1000);
		let txid = node
			.onchain_payment()
			.send_all_to_address(&test_recipient(), true, Some(custom_fee_rate))
			.expect("send_all with custom fee rate should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		assert!(node.list_balances().total_onchain_balance_sats < 10_000);

		node.stop().unwrap();
	}

	/// send_all works correctly when primary is Legacy.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_all_legacy_primary() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::Legacy, vec![AddressType::NativeSegwit]);
		let node = setup_node(&chain_source, config, None);

		let legacy_addr = node.onchain_payment().new_address().unwrap();
		let segwit_addr =
			node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(legacy_addr, 100_000), (segwit_addr, 100_000)],
		)
		.await;

		assert!(
			node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats >= 99_000
		);
		assert!(
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats
				>= 99_000
		);

		let txid = node
			.onchain_payment()
			.send_all_to_address(&test_recipient(), true, None)
			.expect("send_all with legacy primary should succeed");

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		assert!(
			node.get_balance_for_address_type(AddressType::Legacy).unwrap().spendable_sats < 10_000
		);
		assert!(
			node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().spendable_sats
				< 10_000
		);
		assert!(node.list_balances().total_onchain_balance_sats < 10_000);

		node.stop().unwrap();
	}

	// --- Manual UTXO selection ---

	/// Send using manually-selected UTXOs from different address types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_send_with_specific_utxos_from_different_types() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config =
			node_config(AddressType::NativeSegwit, vec![AddressType::Legacy, AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 50_000), (legacy_addr, 50_000), (taproot_addr, 50_000)],
		)
		.await;

		let selected_utxos = node
			.onchain_payment()
			.select_utxos_with_algorithm(
				140_000,
				Some(FeeRate::from_sat_per_kwu(500)),
				ldk_node::CoinSelectionAlgorithm::LargestFirst,
				None,
			)
			.expect("Should select UTXOs from multiple wallet types");

		assert!(
			selected_utxos.len() >= 3,
			"Should have at least 3 UTXOs, got {}",
			selected_utxos.len()
		);

		let txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 100_000, None, Some(selected_utxos))
			.expect("Send with specific UTXOs should succeed");

		wait_for_tx(&electrsd.client, txid).await;

		let tx = electrsd.client.transaction_get(&txid).unwrap();
		assert!(tx.input.len() >= 3, "Tx should have at least 3 inputs, got {}", tx.input.len());

		node.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// Channel Funding
// ---------------------------------------------------------------------------
mod channel_funding {
	use electrum_client::ElectrumApi;
	use ldk_node::config::AddressType;
	use ldk_node::Event;

	use crate::common::{
		expect_channel_ready_event, expect_event, open_channel, setup_bitcoind_and_electrsd,
		setup_node, wait_for_outpoint_spend, TestChainSource,
	};
	use crate::helpers::{
		confirm_and_sync, fund_multiple_and_sync, fund_peer_node_and_sync, node_config,
		CHANNEL_PEER_FUNDING_SATS,
	};

	// --- Cross-wallet & witness requirements ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_open_channel_with_cross_wallet_witness_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let native_addr = node_a.onchain_payment().new_address().unwrap();
		let legacy_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let nested_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
		let taproot_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![
				(native_addr, 55_000),
				(legacy_addr, 55_000),
				(nested_addr, 55_000),
				(taproot_addr, 55_000),
			],
		)
		.await;

		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		// 120k > any two witness wallets (110k), needs all three.
		let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		assert!(
			funding_tx.input.len() >= 3,
			"Should have at least 3 witness inputs, got {}",
			funding_tx.input.len()
		);

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		assert!(!node_a.list_channels().is_empty());

		// Legacy funds untouched.
		let legacy_after = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(legacy_after.total_sats >= 54_000);

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	/// Opening a channel with only non-witness outputs (Legacy or NestedSegwit) fails.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_open_channel_non_witness_only_returns_error() {
		for primary_type in [AddressType::Legacy, AddressType::NestedSegwit] {
			let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
			let chain_source = TestChainSource::Esplora(&electrsd);

			let config_a = node_config(primary_type, vec![]);
			let node_a = setup_node(&chain_source, config_a, None);

			let config_b = crate::common::random_config(true);
			let node_b = setup_node(&chain_source, config_b, None);

			let addr_a = node_a.onchain_payment().new_address().unwrap();
			let addr_b = node_b.onchain_payment().new_address().unwrap();

			fund_multiple_and_sync(&bitcoind, &electrsd, &node_a, vec![(addr_a, 500_000)]).await;
			fund_peer_node_and_sync(
				&bitcoind,
				&electrsd,
				&node_b,
				addr_b,
				CHANNEL_PEER_FUNDING_SATS,
			)
			.await;

			let result = node_a.open_channel(
				node_b.node_id(),
				node_b.listening_addresses().unwrap().first().unwrap().clone(),
				100_000,
				None,
				None,
			);

			assert!(result.is_err(), "Opening with only {:?} should fail", primary_type);
			// Legacy returns InsufficientFunds; NestedSegwit may return ChannelCreationFailed
			let err = result.unwrap_err();
			assert!(
				matches!(
					err,
					ldk_node::NodeError::InsufficientFunds
						| ldk_node::NodeError::ChannelCreationFailed
				),
				"Expected InsufficientFunds or ChannelCreationFailed, got {:?}",
				err
			);

			node_a.stop().unwrap();
			node_b.stop().unwrap();
		}
	}

	// --- Legacy primary + Segwit ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_open_channel_legacy_primary_uses_segwit_wallet() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::Legacy, vec![AddressType::NativeSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let legacy_addr = node_a.onchain_payment().new_address().unwrap();
		let segwit_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(legacy_addr, 200_000), (segwit_addr, 200_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		assert!(!node_a.list_channels().is_empty());

		// Legacy untouched.
		let legacy_balance = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(legacy_balance.total_sats >= 190_000);

		// NativeSegwit was used.
		let segwit_balance =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
		assert!(segwit_balance.total_sats < 190_000);

		// All funding inputs have witness data.
		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		for input in &funding_tx.input {
			assert!(
				!input.witness.is_empty(),
				"Input {:?} has empty witness",
				input.previous_output
			);
		}

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_legacy_primary_channel_funding_excludes_legacy_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a =
			node_config(AddressType::Legacy, vec![AddressType::NativeSegwit, AddressType::Taproot]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let legacy_addr = node_a.onchain_payment().new_address().unwrap();
		let native_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
		let taproot_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(legacy_addr, 200_000), (native_addr, 100_000), (taproot_addr, 100_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		// Legacy has 200k (enough alone) but must be excluded.
		let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		for input in &funding_tx.input {
			assert!(!input.witness.is_empty(), "All funding inputs must have witness data");
		}

		// Legacy balance untouched.
		node_a.sync_wallets().unwrap();
		let legacy_balance = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(legacy_balance.total_sats >= 190_000);

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	// --- NestedSegwit primary ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_inbound_channel_with_non_native_witness_primary() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::Legacy, vec![AddressType::Taproot]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let addr_b = node_b.onchain_payment().new_address().unwrap();
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;
		node_a.sync_wallets().unwrap();

		// Node B opens toward node A (inbound). get_shutdown_scriptpubkey must
		// produce a native witness address from Taproot, not Legacy.
		let _funding_txo = open_channel(&node_b, &node_a, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		assert!(!node_a.list_channels().is_empty(), "Node A should have an inbound channel");

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_open_channel_nested_primary_with_native_monitor() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::NestedSegwit, vec![AddressType::NativeSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let nested_addr = node_a.onchain_payment().new_address().unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(&bitcoind, &electrsd, &node_a, vec![(nested_addr, 300_000)]).await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		assert!(!node_a.list_channels().is_empty());

		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		for input in &funding_tx.input {
			assert!(!input.witness.is_empty(), "All inputs must have witness data");
		}

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_open_channel_nested_primary_mixed_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::NestedSegwit, vec![AddressType::NativeSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		// Fund NestedSegwit with 80k — not enough for 120k channel.
		let nested_addr = node_a.onchain_payment().new_address().unwrap();
		let native_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(nested_addr, 80_000), (native_addr, 80_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		assert!(!node_a.list_channels().is_empty());

		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		assert!(funding_tx.input.len() >= 2, "Should use inputs from both wallets");
		for input in &funding_tx.input {
			assert!(!input.witness.is_empty());
		}

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_open_channel_native_primary_with_nested_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::NativeSegwit, vec![AddressType::NestedSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let native_addr = node_a.onchain_payment().new_address().unwrap();
		let nested_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(native_addr, 80_000), (nested_addr, 80_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 120_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		assert!(!node_a.list_channels().is_empty());

		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		assert!(funding_tx.input.len() >= 2, "Should use inputs from both wallets");
		for input in &funding_tx.input {
			assert!(!input.witness.is_empty());
		}

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	// --- Channel close ---

	/// Cooperative channel close: verify funds return to the correct address type.
	/// With Legacy primary + NativeSegwit monitored, channel scripts come from
	/// NativeSegwit. On close, the output should land in NativeSegwit and show
	/// in that wallet's balance.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_channel_close_funds_return_to_correct_address_type() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		// Legacy primary, NativeSegwit monitored — channel scripts from NativeSegwit.
		let config_a = node_config(AddressType::Legacy, vec![AddressType::NativeSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let legacy_addr = node_a.onchain_payment().new_address().unwrap();
		let segwit_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(legacy_addr, 200_000), (segwit_addr, 200_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		// NativeSegwit balance after open (channel holds 100k; we spent it from NativeSegwit).
		let segwit_after_open =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;

		let user_channel_id = expect_channel_ready_event!(node_a, node_b.node_id());
		expect_channel_ready_event!(node_b, node_a.node_id());

		// Cooperative close.
		node_a.close_channel(&user_channel_id, node_b.node_id()).unwrap();
		expect_event!(node_a, ChannelClosed);
		expect_event!(node_b, ChannelClosed);

		wait_for_outpoint_spend(&electrsd.client, funding_txo).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node_a, &node_b]).await;

		// Close output should land in NativeSegwit (shutdown script).
		let segwit_after =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;

		// NativeSegwit should increase after close (channel value returned minus closing fee).
		assert!(
			segwit_after > segwit_after_open,
			"NativeSegwit balance should increase after close (after_open: {}, after_close: {})",
			segwit_after_open,
			segwit_after
		);
		// Legacy should be untouched.
		let legacy_after = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(legacy_after.total_sats >= 199_000, "Legacy balance should be untouched");

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	/// Cooperative channel close: verify funds return to Taproot when primary is Taproot.
	/// With Taproot primary, get_shutdown_scriptpubkey returns a Taproot address.
	/// On close, the output should land in Taproot and show in that wallet's balance.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_channel_close_funds_return_to_taproot() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		// Taproot primary — channel scripts and shutdown from Taproot.
		let config_a = node_config(AddressType::Taproot, vec![AddressType::NativeSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let taproot_addr = node_a.onchain_payment().new_address().unwrap();
		let segwit_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(taproot_addr, 200_000), (segwit_addr, 200_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		// Taproot balance after open (channel holds 100k; we spent it from Taproot).
		let taproot_after_open =
			node_a.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats;

		let user_channel_id = expect_channel_ready_event!(node_a, node_b.node_id());
		expect_channel_ready_event!(node_b, node_a.node_id());

		node_a.close_channel(&user_channel_id, node_b.node_id()).unwrap();
		expect_event!(node_a, ChannelClosed);
		expect_event!(node_b, ChannelClosed);

		wait_for_outpoint_spend(&electrsd.client, funding_txo).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node_a, &node_b]).await;

		// Close output should land in Taproot (shutdown script).
		let taproot_after =
			node_a.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats;

		assert!(
			taproot_after > taproot_after_open,
			"Taproot balance should increase after close (after_open: {}, after_close: {})",
			taproot_after_open,
			taproot_after
		);
		// Close output went to Taproot; NativeSegwit should not have gained it.
		let segwit_after_open =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;
		let segwit_after =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;
		assert!(
			segwit_after <= segwit_after_open + 1000,
			"Close output should not land in NativeSegwit (after_open: {}, after_close: {})",
			segwit_after_open,
			segwit_after
		);

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	/// Cooperative channel close: verify funds return to NativeSegwit when primary is NativeSegwit.
	/// With NativeSegwit primary, get_shutdown_scriptpubkey returns a NativeSegwit address.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_channel_close_funds_return_to_native_segwit() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let segwit_addr = node_a.onchain_payment().new_address().unwrap();
		let legacy_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(segwit_addr, 200_000), (legacy_addr, 200_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		let segwit_after_open =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;

		let user_channel_id = expect_channel_ready_event!(node_a, node_b.node_id());
		expect_channel_ready_event!(node_b, node_a.node_id());

		node_a.close_channel(&user_channel_id, node_b.node_id()).unwrap();
		expect_event!(node_a, ChannelClosed);
		expect_event!(node_b, ChannelClosed);

		wait_for_outpoint_spend(&electrsd.client, funding_txo).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node_a, &node_b]).await;

		let segwit_after =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;

		assert!(
			segwit_after > segwit_after_open,
			"NativeSegwit balance should increase after close (after_open: {}, after_close: {})",
			segwit_after_open,
			segwit_after
		);
		let legacy_after = node_a.get_balance_for_address_type(AddressType::Legacy).unwrap();
		assert!(legacy_after.total_sats >= 199_000, "Legacy balance should be untouched");

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	/// Cooperative channel close: NestedSegwit primary uses monitored NativeSegwit for shutdown.
	/// Verify funds return to NativeSegwit (monitored native witness) on close.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_channel_close_nested_primary_funds_return_to_native_segwit() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		// NestedSegwit primary cannot use its own script for shutdown; uses monitored NativeSegwit.
		let config_a = node_config(AddressType::NestedSegwit, vec![AddressType::NativeSegwit]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let nested_addr = node_a.onchain_payment().new_address().unwrap();
		let segwit_addr =
			node_a.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![(nested_addr, 200_000), (segwit_addr, 200_000)],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 100_000, false, &electrsd).await;

		confirm_and_sync(&bitcoind, &electrsd, 6, &[&node_a, &node_b]).await;

		let segwit_after_open =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;

		let user_channel_id = expect_channel_ready_event!(node_a, node_b.node_id());
		expect_channel_ready_event!(node_b, node_a.node_id());

		node_a.close_channel(&user_channel_id, node_b.node_id()).unwrap();
		expect_event!(node_a, ChannelClosed);
		expect_event!(node_b, ChannelClosed);

		wait_for_outpoint_spend(&electrsd.client, funding_txo).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node_a, &node_b]).await;

		let segwit_after =
			node_a.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats;

		assert!(
			segwit_after > segwit_after_open,
			"NativeSegwit (shutdown script) should increase after close (after_open: {}, after_close: {})",
			segwit_after_open,
			segwit_after
		);

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// RBF
// ---------------------------------------------------------------------------
mod rbf {
	use bitcoin::FeeRate;
	use electrum_client::ElectrumApi;
	use ldk_node::config::AddressType;

	use crate::common::{setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource};
	use crate::helpers::{
		confirm_and_sync, fake_txid_for_error_tests, fund_and_sync, fund_multiple_and_sync,
		node_config, test_recipient,
	};

	// --- Success cases ---

	/// RBF on a transaction using inputs from only the primary wallet.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_single_wallet() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 300_000), (legacy_addr, 100_000)],
		)
		.await;

		// Send a smaller amount that can be satisfied by primary alone.
		let initial_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 50_000, None, None)
			.expect("Initial send should succeed");

		wait_for_tx(&electrsd.client, initial_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		let higher_fee_rate = FeeRate::from_sat_per_kwu(1000);
		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&initial_txid, higher_fee_rate)
			.expect("RBF should succeed");

		assert_ne!(initial_txid, rbf_txid);
		wait_for_tx(&electrsd.client, rbf_txid).await;

		node.stop().unwrap();
	}

	/// RBF on a cross-wallet transaction (change output reduction).
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_cross_wallet_transaction() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000)],
		)
		.await;

		// 120k requires both wallets.
		let initial_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 120_000, None, None)
			.expect("Initial cross-wallet send should succeed");

		wait_for_tx(&electrsd.client, initial_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		let total_before_rbf = node.list_balances().total_onchain_balance_sats;

		let higher_fee_rate = FeeRate::from_sat_per_kwu(2000);
		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&initial_txid, higher_fee_rate)
			.expect("Cross-wallet RBF should succeed");

		assert_ne!(initial_txid, rbf_txid);
		wait_for_tx(&electrsd.client, rbf_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		// Confirm replacement; balance check after confirm avoids sync timing flakiness.
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let total_after_rbf = node.list_balances().total_onchain_balance_sats;
		assert!(
			total_after_rbf < total_before_rbf,
			"Balance should decrease after RBF (higher fee): before={} after={}",
			total_before_rbf,
			total_after_rbf
		);

		node.stop().unwrap();
	}

	/// RBF on a transaction with inputs from all four address types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_cross_wallet_all_four_types() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let nested_addr =
			node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(native_addr, 80_000),
				(legacy_addr, 80_000),
				(nested_addr, 80_000),
				(taproot_addr, 80_000),
			],
		)
		.await;

		// 280k requires all four.
		let initial_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 280_000, None, None)
			.expect("Initial cross-wallet send should succeed");

		wait_for_tx(&electrsd.client, initial_txid).await;

		let initial_tx = electrsd.client.transaction_get(&initial_txid).unwrap();
		assert!(initial_tx.input.len() >= 4);

		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		let total_before_rbf = node.list_balances().total_onchain_balance_sats;

		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&initial_txid, FeeRate::from_sat_per_kwu(2000))
			.expect("RBF with all 4 types should succeed");

		assert_ne!(initial_txid, rbf_txid);
		wait_for_tx(&electrsd.client, rbf_txid).await;

		let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
		assert!(rbf_tx.input.len() >= 4);

		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		assert!(node.list_balances().total_onchain_balance_sats < total_before_rbf);

		node.stop().unwrap();
	}

	/// RBF that needs additional inputs verifies fee accounting.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_additional_inputs_fee_rate_correctness() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr1 = node.onchain_payment().new_address().unwrap();
		let addr2 = node.onchain_payment().new_address().unwrap();
		let addr3 = node.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(addr1, 50_000), (addr2, 50_000), (addr3, 50_000)],
		)
		.await;

		let initial_balance = node.list_balances().total_onchain_balance_sats;
		assert!(initial_balance >= 147_000);

		// Send 30k — uses 1 UTXO, ~20k change.
		let initial_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 30_000, None, None)
			.expect("Initial send should succeed");

		wait_for_tx(&electrsd.client, initial_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		// Very high fee forces adding inputs.
		let high_fee_rate = FeeRate::from_sat_per_kwu(5000);
		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&initial_txid, high_fee_rate)
			.expect("RBF with added inputs should succeed");

		assert_ne!(initial_txid, rbf_txid);
		wait_for_tx(&electrsd.client, rbf_txid).await;

		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let final_balance = node.list_balances().total_onchain_balance_sats;
		let fee_paid = initial_balance - final_balance - 30_000;
		assert!(fee_paid < 15_000, "Fee {} too high", fee_paid);
		assert!(fee_paid > 1_000, "Fee {} too low", fee_paid);

		node.stop().unwrap();
	}

	/// RBF adds inputs from a different address-type wallet when change is insufficient.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_adds_inputs_from_different_address_type() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		// NativeSegwit: just enough for 50k send with small change.
		// Legacy: available for RBF fee bumping.
		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 55_000), (legacy_addr, 60_000)],
		)
		.await;

		let initial_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 50_000, None, None)
			.expect("Initial send should succeed");

		wait_for_tx(&electrsd.client, initial_txid).await;

		let initial_tx = electrsd.client.transaction_get(&initial_txid).unwrap();
		assert_eq!(initial_tx.input.len(), 1);

		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		// Extremely high fee rate to exhaust the small change.
		let high_fee_rate = FeeRate::from_sat_per_kwu(50_000);
		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&initial_txid, high_fee_rate)
			.expect("RBF should add Legacy input");

		assert_ne!(initial_txid, rbf_txid);
		wait_for_tx(&electrsd.client, rbf_txid).await;

		let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
		assert!(rbf_tx.input.len() > initial_tx.input.len());

		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		node.stop().unwrap();
	}

	/// Cross-wallet RBF adds extra inputs when change cannot absorb fee increase.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cross_wallet_rbf_adds_extra_inputs_when_needed() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr1 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let legacy_addr2 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 60_000), (legacy_addr1, 60_000), (legacy_addr2, 60_000)],
		)
		.await;

		// 115k requires primary (60k) + 1 Legacy (60k) = 120k.
		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 115_000, None, None).unwrap();
		wait_for_tx(&electrsd.client, txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		let initial_tx = electrsd.client.transaction_get(&txid).unwrap();
		assert_eq!(initial_tx.input.len(), 2);

		// High fee forces adding the remaining Legacy UTXO.
		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&txid, FeeRate::from_sat_per_kwu(25_000))
			.expect("Cross-wallet RBF with extra input should succeed");

		wait_for_tx(&electrsd.client, rbf_txid).await;

		let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
		assert!(rbf_tx.input.len() > initial_tx.input.len());

		node.stop().unwrap();
	}

	/// RBF with a Legacy primary wallet.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_legacy_primary() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::Legacy, vec![AddressType::NativeSegwit]);
		let node = setup_node(&chain_source, config, None);

		let legacy_addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, legacy_addr, 300_000).await;

		let initial_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 50_000, None, None)
			.expect("Send from Legacy should succeed");

		wait_for_tx(&electrsd.client, initial_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&initial_txid, FeeRate::from_sat_per_kwu(1000))
			.expect("RBF should succeed for Legacy primary");

		assert_ne!(initial_txid, rbf_txid);
		wait_for_tx(&electrsd.client, rbf_txid).await;

		node.stop().unwrap();
	}

	// --- Error cases ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_rejects_lower_fee_rate() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 200_000).await;

		let initial_fee_rate = FeeRate::from_sat_per_kwu(2000);
		let txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 50_000, Some(initial_fee_rate), None)
			.unwrap();

		wait_for_tx(&electrsd.client, txid).await;
		node.sync_wallets().unwrap();

		let result = node.onchain_payment().bump_fee_by_rbf(&txid, FeeRate::from_sat_per_kwu(1000));
		assert!(result.is_err());

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_rejects_confirmed_transaction() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 200_000).await;

		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 50_000, None, None).unwrap();

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let result = node.onchain_payment().bump_fee_by_rbf(&txid, FeeRate::from_sat_per_kwu(5000));
		assert!(result.is_err());

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_rejects_unknown_transaction() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 100_000).await;

		let result = node
			.onchain_payment()
			.bump_fee_by_rbf(&fake_txid_for_error_tests(), FeeRate::from_sat_per_kwu(2000));
		assert!(result.is_err());

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_insufficient_funds_for_fee_bump() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 51_000).await;

		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 50_000, None, None).unwrap();
		wait_for_tx(&electrsd.client, txid).await;
		node.sync_wallets().unwrap();

		let result =
			node.onchain_payment().bump_fee_by_rbf(&txid, FeeRate::from_sat_per_kwu(100_000));
		assert!(result.is_err());

		node.stop().unwrap();
	}

	/// RBF on send_all (no change) must not reduce the recipient amount.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_send_all_no_change_does_not_reduce_recipient() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 100_000).await;

		let txid =
			node.onchain_payment().send_all_to_address(&test_recipient(), true, None).unwrap();
		wait_for_tx(&electrsd.client, txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		let result = node.onchain_payment().bump_fee_by_rbf(&txid, FeeRate::from_sat_per_kwu(5000));
		assert!(result.is_err(), "RBF on send-all (no change) should fail");

		node.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// CPFP
// ---------------------------------------------------------------------------
mod cpfp {
	use bitcoin::FeeRate;
	use electrum_client::ElectrumApi;
	use ldk_node::config::AddressType;

	use crate::common::{setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource};
	use crate::helpers::{
		confirm_and_sync, fake_txid_for_error_tests, fund_and_sync, fund_multiple_and_sync,
		node_config, test_recipient,
	};

	// --- Success cases ---

	/// Single-wallet CPFP with fee rate calculation.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cpfp_single_wallet() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NestedSegwit,
			vec![AddressType::Legacy, AddressType::NativeSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 400_000).await;

		let parent_txid =
			node.onchain_payment().send_to_address(&test_recipient(), 60_000, None, None).unwrap();

		wait_for_tx(&electrsd.client, parent_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
		let cpfp_txid = node
			.onchain_payment()
			.accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None)
			.unwrap();

		assert_ne!(parent_txid, cpfp_txid);

		let calculated_fee_rate =
			node.onchain_payment().calculate_cpfp_fee_rate(&parent_txid, false).unwrap();
		assert!(calculated_fee_rate.to_sat_per_kwu() > 0);
	}

	/// CPFP on a cross-wallet transaction (change goes to primary).
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cpfp_for_cross_wallet_transaction() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 100_000), (legacy_addr, 100_000)],
		)
		.await;

		// 140k requires both wallets.
		let parent_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 140_000, None, None)
			.expect("Cross-wallet send should succeed");

		wait_for_tx(&electrsd.client, parent_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		let cpfp_txid = node
			.onchain_payment()
			.accelerate_by_cpfp(&parent_txid, Some(FeeRate::from_sat_per_kwu(1500)), None)
			.expect("CPFP should work for cross-wallet transactions");

		assert_ne!(parent_txid, cpfp_txid);
		wait_for_tx(&electrsd.client, cpfp_txid).await;

		node.stop().unwrap();
	}

	/// CPFP on a transaction with inputs from all four address types.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cpfp_cross_wallet_all_four_types() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let nested_addr =
			node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(native_addr, 100_000),
				(legacy_addr, 100_000),
				(nested_addr, 100_000),
				(taproot_addr, 100_000),
			],
		)
		.await;

		// 320k requires all four.
		let parent_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 320_000, None, None)
			.expect("Cross-wallet send should succeed");

		wait_for_tx(&electrsd.client, parent_txid).await;

		let parent_tx = electrsd.client.transaction_get(&parent_txid).unwrap();
		assert!(parent_tx.input.len() >= 4);

		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		let cpfp_txid = node
			.onchain_payment()
			.accelerate_by_cpfp(&parent_txid, Some(FeeRate::from_sat_per_kwu(1500)), None)
			.expect("CPFP with all 4 types should succeed");

		assert_ne!(parent_txid, cpfp_txid);
		wait_for_tx(&electrsd.client, cpfp_txid).await;

		let cpfp_tx = electrsd.client.transaction_get(&cpfp_txid).unwrap();
		assert!(cpfp_tx.input.iter().any(|i| i.previous_output.txid == parent_txid));

		node.stop().unwrap();
	}

	/// CPFP with a Legacy primary wallet.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cpfp_legacy_primary() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::Legacy, vec![AddressType::NativeSegwit]);
		let node = setup_node(&chain_source, config, None);

		let legacy_addr = node.onchain_payment().new_address().unwrap();
		let segwit_addr =
			node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(legacy_addr, 200_000), (segwit_addr, 100_000)],
		)
		.await;

		let parent_txid = node
			.onchain_payment()
			.send_to_address(&test_recipient(), 60_000, None, None)
			.expect("Send from legacy should succeed");

		wait_for_tx(&electrsd.client, parent_txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		let cpfp_txid = node
			.onchain_payment()
			.accelerate_by_cpfp(&parent_txid, Some(FeeRate::from_sat_per_kwu(1500)), None)
			.expect("CPFP should succeed for Legacy primary");

		assert_ne!(parent_txid, cpfp_txid);
		wait_for_tx(&electrsd.client, cpfp_txid).await;

		node.stop().unwrap();
	}

	// --- Error cases ---

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cpfp_rejects_unknown_transaction() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 100_000).await;

		let result =
			node.onchain_payment().accelerate_by_cpfp(&fake_txid_for_error_tests(), None, None);
		assert!(result.is_err());

		node.stop().unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cpfp_rejects_confirmed_transaction() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 200_000).await;

		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 50_000, None, None).unwrap();

		wait_for_tx(&electrsd.client, txid).await;
		confirm_and_sync(&bitcoind, &electrsd, 1, &[&node]).await;

		let result = node.onchain_payment().accelerate_by_cpfp(&txid, None, None);
		assert!(result.is_err());

		node.stop().unwrap();
	}
}

// ---------------------------------------------------------------------------
// Coin Selection
// ---------------------------------------------------------------------------
mod coin_selection {
	use bitcoin::FeeRate;
	use electrum_client::ElectrumApi;
	use ldk_node::config::AddressType;

	use crate::common::{
		open_channel, setup_bitcoind_and_electrsd, setup_node, wait_for_tx, TestChainSource,
	};
	use crate::helpers::{
		fund_and_sync, fund_multiple_and_sync, fund_peer_node_and_sync, node_config,
		test_recipient, CHANNEL_PEER_FUNDING_SATS,
	};

	// --- API & fee calculation ---

	/// Basic UTXO selection API works with multi-wallet.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_utxo_selection() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit],
		);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 500_000).await;

		let spendable = node.onchain_payment().list_spendable_outputs().unwrap();
		assert!(!spendable.is_empty());

		let selected = node
			.onchain_payment()
			.select_utxos_with_algorithm(
				100_000,
				Some(FeeRate::from_sat_per_kwu(500)),
				ldk_node::CoinSelectionAlgorithm::LargestFirst,
				None,
			)
			.unwrap();

		assert!(!selected.is_empty());
	}

	/// list_spendable_outputs returns UTXOs from multiple address types when funded.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_list_spendable_outputs_includes_multiple_address_types() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config =
			node_config(AddressType::NativeSegwit, vec![AddressType::Legacy, AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(native_addr, 60_000), (legacy_addr, 50_000), (taproot_addr, 55_000)],
		)
		.await;

		let spendable = node.onchain_payment().list_spendable_outputs().unwrap();
		assert!(
			spendable.len() >= 3,
			"Should have at least 3 UTXOs from 3 address types, got {}",
			spendable.len()
		);

		let total_value: u64 = spendable.iter().map(|u| u.value_sats).sum();
		let expected_min = 60_000 + 50_000 + 55_000 - 5_000;
		assert!(
			total_value >= expected_min,
			"Total spendable value should be >= {} (got {})",
			expected_min,
			total_value
		);

		node.stop().unwrap();
	}

	/// Fee calculation considers all wallets.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_calculate_total_fee() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::Taproot, vec![AddressType::NativeSegwit]);
		let node = setup_node(&chain_source, config, None);

		let addr = node.onchain_payment().new_address().unwrap();
		fund_and_sync(&bitcoind, &electrsd, &node, addr, 300_000).await;

		let total_fee = node
			.onchain_payment()
			.calculate_total_fee(
				&test_recipient(),
				100_000,
				Some(FeeRate::from_sat_per_kwu(500)),
				None,
			)
			.unwrap();

		assert!(total_fee > 0);
	}

	// --- Minimum inputs ---

	/// Unified selection pools all UTXOs; should pick an efficient subset,
	/// not dump all foreign UTXOs.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_cross_wallet_send_uses_minimum_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let primary_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr1 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let legacy_addr2 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let legacy_addr3 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(primary_addr, 10_000),
				(legacy_addr1, 20_000),
				(legacy_addr2, 30_000),
				(legacy_addr3, 50_000),
			],
		)
		.await;

		// BranchAndBound should pick an efficient combination — not all 4.
		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 15_000, None, None).unwrap();
		wait_for_tx(&electrsd.client, txid).await;

		let tx = electrsd.client.transaction_get(&txid).unwrap();
		assert!(tx.input.len() <= 2, "Should use minimum inputs, got {}", tx.input.len());

		node.stop().unwrap();
	}

	// --- Channel funding ---

	/// Channel funding picks minimum witness inputs.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_channel_funding_uses_minimum_witness_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config_a = node_config(AddressType::NativeSegwit, vec![AddressType::Taproot]);
		let node_a = setup_node(&chain_source, config_a, None);

		let config_b = crate::common::random_config(true);
		let node_b = setup_node(&chain_source, config_b, None);

		let native_addr = node_a.onchain_payment().new_address().unwrap();
		let taproot_addr1 =
			node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
		let taproot_addr2 =
			node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
		let taproot_addr3 =
			node_a.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();
		let addr_b = node_b.onchain_payment().new_address().unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node_a,
			vec![
				(native_addr, 20_000),
				(taproot_addr1, 40_000),
				(taproot_addr2, 40_000),
				(taproot_addr3, 40_000),
			],
		)
		.await;
		fund_peer_node_and_sync(&bitcoind, &electrsd, &node_b, addr_b, CHANNEL_PEER_FUNDING_SATS)
			.await;

		let funding_txo = open_channel(&node_a, &node_b, 50_000, false, &electrsd).await;

		let funding_tx = electrsd.client.transaction_get(&funding_txo.txid).unwrap();
		assert!(
			funding_tx.input.len() <= 3,
			"Should not use all UTXOs, got {}",
			funding_tx.input.len()
		);

		node_a.stop().unwrap();
		node_b.stop().unwrap();
	}

	// --- RBF fallback ---

	/// RBF fallback selects minimum foreign inputs.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_rbf_fallback_uses_minimum_foreign_inputs() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Legacy]);
		let node = setup_node(&chain_source, config, None);

		let native_addr = node.onchain_payment().new_address().unwrap();
		let legacy_addr1 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let legacy_addr2 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
		let legacy_addr3 =
			node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![
				(native_addr, 55_000),
				(legacy_addr1, 60_000),
				(legacy_addr2, 60_000),
				(legacy_addr3, 60_000),
			],
		)
		.await;

		// Send 50k — uses primary (55k). Leaves ~5k change.
		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 50_000, None, None).unwrap();
		wait_for_tx(&electrsd.client, txid).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));

		let initial_tx = electrsd.client.transaction_get(&txid).unwrap();
		assert_eq!(initial_tx.input.len(), 1);

		// High fee rate forces adding 1 Legacy UTXO.
		let rbf_txid = node
			.onchain_payment()
			.bump_fee_by_rbf(&txid, FeeRate::from_sat_per_kwu(50_000))
			.expect("RBF should succeed by adding Legacy input");

		wait_for_tx(&electrsd.client, rbf_txid).await;

		let rbf_tx = electrsd.client.transaction_get(&rbf_txid).unwrap();
		assert!(
			rbf_tx.input.len() <= 2,
			"Should use minimum foreign inputs, got {}",
			rbf_tx.input.len()
		);

		node.stop().unwrap();
	}

	// --- Optimal UTXO preference ---

	/// Unified selection prefers a single optimal UTXO over many small ones.
	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn test_unified_selection_prefers_optimal_utxo() {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&electrsd);

		let config = node_config(AddressType::NativeSegwit, vec![AddressType::Taproot]);
		let node = setup_node(&chain_source, config, None);

		// 3 small primary UTXOs (10k each) + 1 large Taproot (25k).
		let addr1 = node.onchain_payment().new_address().unwrap();
		let addr2 = node.onchain_payment().new_address().unwrap();
		let addr3 = node.onchain_payment().new_address().unwrap();
		let taproot_addr =
			node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

		fund_multiple_and_sync(
			&bitcoind,
			&electrsd,
			&node,
			vec![(addr1, 10_000), (addr2, 10_000), (addr3, 10_000), (taproot_addr, 25_000)],
		)
		.await;

		// Send 20k. Unified selection should not use all 4 UTXOs.
		let txid =
			node.onchain_payment().send_to_address(&test_recipient(), 20_000, None, None).unwrap();
		wait_for_tx(&electrsd.client, txid).await;

		let tx = electrsd.client.transaction_get(&txid).unwrap();
		assert!(tx.input.len() <= 3, "Should pick optimal UTXOs, got {}", tx.input.len());

		node.stop().unwrap();
	}
}
