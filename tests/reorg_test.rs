mod common;
use std::collections::HashMap;
use std::time::Duration;

use bitcoin::{Amount, BlockHash};
use electrsd::corepc_node::Client as BitcoindClient;
use electrsd::ElectrsD;
use ldk_node::payment::{PaymentDirection, PaymentKind};
use ldk_node::{Event, LightningBalance, PendingSweepBalance};
use proptest::prelude::prop;
use proptest::proptest;
use serde_json::{json, Value};

use crate::common::{
	expect_channel_ready_event, expect_event, generate_blocks_and_wait, invalidate_blocks,
	open_channel, premine_and_distribute_funds, random_chain_source, random_config,
	setup_bitcoind_and_electrsd, setup_node, stop_node, stop_nodes_concurrently, wait_for_block,
	wait_for_outpoint_spend, TestChainSource, TestStoreType,
};

#[derive(Clone, Copy)]
enum TransactionChainSource {
	Esplora,
	Electrum,
}

fn transaction_chain_source<'a>(
	source: TransactionChainSource, electrsd: &'a ElectrsD,
) -> TestChainSource<'a> {
	match source {
		TransactionChainSource::Esplora => TestChainSource::Esplora(electrsd),
		TransactionChainSource::Electrum => TestChainSource::Electrum(electrsd),
	}
}

fn best_block(bitcoind: &BitcoindClient) -> (BlockHash, u32) {
	let block_hash = bitcoind
		.call::<String>("getbestblockhash", &[])
		.expect("failed to get best block hash")
		.parse()
		.expect("best block hash should parse");
	let height = bitcoind.get_blockchain_info().expect("failed to get blockchain info").blocks;
	(block_hash, height as u32)
}

fn assert_node_synced_to_tip(node: &ldk_node::Node, bitcoind: &BitcoindClient) {
	let (block_hash, height) = best_block(bitcoind);
	let node_best_block = node.status().current_best_block;
	assert_eq!(node_best_block.block_hash, block_hash);
	assert_eq!(node_best_block.height, height);
}

async fn wait_for_node_to_reach_tip(node: &ldk_node::Node, bitcoind: &BitcoindClient) -> bool {
	let (block_hash, height) = best_block(bitcoind);
	for _ in 0..80 {
		let node_best_block = node.status().current_best_block;
		if node_best_block.block_hash == block_hash && node_best_block.height == height {
			return true;
		}

		tokio::time::sleep(Duration::from_millis(250)).await;
	}
	false
}

async fn assert_node_reaches_tip(
	node: ldk_node::Node, bitcoind: &BitcoindClient,
) -> ldk_node::Node {
	if wait_for_node_to_reach_tip(&node, bitcoind).await {
		node
	} else {
		let expected = best_block(bitcoind);
		let actual = node.status().current_best_block;
		stop_node(node).await;
		panic!(
			"source-switch sync did not reach backend tip: expected {:?}, actual {:?}",
			expected, actual
		);
	}
}

async fn copy_active_chain(
	source: &BitcoindClient, target: &BitcoindClient, target_electrsd: &ElectrsD,
) {
	let source_height =
		source.get_blockchain_info().expect("failed to get blockchain info").blocks as usize;
	for height in 1..=source_height {
		let block_hash = source
			.get_block_hash(height as u64)
			.expect("failed to get block hash")
			.block_hash()
			.expect("block hash should be present");
		let block_hex = source
			.call::<String>("getblock", &[json!(block_hash.to_string()), json!(0)])
			.expect("failed to get raw block");
		let submit_res = target
			.call::<Value>("submitblock", &[json!(block_hex)])
			.expect("failed to submit block");
		assert!(
			submit_res.is_null() || submit_res == json!("inconclusive"),
			"submitblock failed at height {}: {}",
			height,
			submit_res
		);
	}
	wait_for_block(&target_electrsd.client, source_height).await;
	assert_eq!(best_block(source), best_block(target));
}

async fn wait_for_pending_sweep_balance<F>(node: &ldk_node::Node, mut matches_pending_balance: F)
where
	F: FnMut(&PendingSweepBalance) -> bool,
{
	let mut delay = Duration::from_millis(64);
	let mut tries = 0;
	loop {
		node.sync_wallets().unwrap();
		let balances = node.list_balances();
		if balances
			.pending_balances_from_channel_closures
			.iter()
			.any(|balance| matches_pending_balance(balance))
		{
			return;
		}
		assert!(tries < 20, "Unexpected balance state: {:?}", balances);
		tries += 1;
		tokio::time::sleep(delay).await;
		if delay.as_millis() < 512 {
			delay = delay.mul_f32(2.0);
		}
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn chain_source_switch_reorg_esplora() {
	do_chain_source_switch_reorg_test(TransactionChainSource::Esplora).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn chain_source_switch_reorg_electrum() {
	do_chain_source_switch_reorg_test(TransactionChainSource::Electrum).await;
}

async fn do_chain_source_switch_reorg_test(source_kind: TransactionChainSource) {
	let (confirm_bitcoind, confirm_electrsd) = setup_bitcoind_and_electrsd();
	let (listen_bitcoind, listen_electrsd) = setup_bitcoind_and_electrsd();
	let confirm_source = transaction_chain_source(source_kind, &confirm_electrsd);

	let mut node_a_config = random_config(true);
	node_a_config.store_type = TestStoreType::Sqlite;
	let node_b_config = random_config(true);

	let node_a = setup_node(&confirm_source, node_a_config.clone());
	let node_b = setup_node(&confirm_source, node_b_config);

	let amount_sat = 2_100_000;
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&confirm_bitcoind.client,
		&confirm_electrsd.client,
		vec![addr_a],
		Amount::from_sat(amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	open_channel(&node_a, &node_b, 2_000_000, true, &confirm_electrsd).await;
	generate_blocks_and_wait(&confirm_bitcoind.client, &confirm_electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// At this point both backend clusters share the same active chain tip. The
	// channel funding transaction is in this shared prefix, so later divergent
	// suffixes only exercise tip switching and reorg handling.
	copy_active_chain(&confirm_bitcoind.client, &listen_bitcoind.client, &listen_electrsd).await;
	assert_node_synced_to_tip(&node_a, &confirm_bitcoind.client);

	// The Confirm backend now has a five-block private suffix unknown to the
	// Listen backend. Syncing through Esplora/Electrum persists Node A at this
	// Confirm-only tip.
	generate_blocks_and_wait(&confirm_bitcoind.client, &confirm_electrsd.client, 5).await;
	node_a.sync_wallets().unwrap();
	assert_node_synced_to_tip(&node_a, &confirm_bitcoind.client);
	let confirm_private_tip = best_block(&confirm_bitcoind.client);
	stop_node(node_a).await;

	// The Listen backend gets a different five-block suffix from the same shared
	// ancestor. This is a reorg within the six-confirmation safety target, and
	// the Listen backend cannot resolve the Confirm-only tip by hash.
	generate_blocks_and_wait(&listen_bitcoind.client, &listen_electrsd.client, 5).await;
	let listen_private_tip = best_block(&listen_bitcoind.client);
	assert_ne!(confirm_private_tip.0, listen_private_tip.0);
	assert_eq!(confirm_private_tip.1, listen_private_tip.1);

	let listen_source = TestChainSource::BitcoindRpcSync(&listen_bitcoind);
	let node_a = setup_node(&listen_source, node_a_config.clone());
	// Old LDK would fail to reach the Listen tip here: bitcoind cannot resolve
	// Node A's stale Confirm-only tip, so block sync needs the persisted
	// BlockLocator previous hashes to find the shared ancestor and disconnect
	// the private suffix.
	let node_a = assert_node_reaches_tip(node_a, &listen_bitcoind.client).await;
	assert_node_synced_to_tip(&node_a, &listen_bitcoind.client);
	assert_eq!(node_a.list_channels().len(), 1);
	stop_node(node_a).await;

	// Switch back from the Listen-only tip to the Confirm chain. The same
	// persisted node should now process the opposite reorg through the
	// transaction-based Confirm client.
	let node_a = setup_node(&confirm_source, node_a_config.clone());
	node_a.sync_wallets().unwrap();
	assert_node_synced_to_tip(&node_a, &confirm_bitcoind.client);
	assert_eq!(node_a.list_channels().len(), 1);
	stop_node(node_a).await;

	// Finally switch once more to the Listen source, proving the safety property
	// is repeatable instead of only working for the first source change.
	let node_a = setup_node(&listen_source, node_a_config);
	let node_a = assert_node_reaches_tip(node_a, &listen_bitcoind.client).await;
	assert_node_synced_to_tip(&node_a, &listen_bitcoind.client);
	assert_eq!(node_a.list_channels().len(), 1);

	stop_nodes_concurrently(vec![node_a, node_b]).await;
}

proptest! {
	#![proptest_config(proptest::test_runner::Config::with_cases(5))]
	#[test]
	fn reorg_test(reorg_depth in 1..=6usize, force_close in prop::bool::ANY) {
		let rt = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

			let chain_source_a = random_chain_source(&bitcoind, &electrsd);
			let chain_source_b = random_chain_source(&bitcoind, &electrsd);
			let chain_source_c = random_chain_source(&bitcoind, &electrsd);

			macro_rules! config_node {
				($chain_source: expr, $anchor_channels: expr) => {{
					let config_a = random_config($anchor_channels);
					let node = setup_node(&$chain_source, config_a);
					node
				}};
			}
			let anchor_channels = true;
			let nodes = vec![
				config_node!(chain_source_a, anchor_channels),
				config_node!(chain_source_b, anchor_channels),
				config_node!(chain_source_c, anchor_channels),
			];

			let (bitcoind, electrs) = (&bitcoind.client, &electrsd.client);
			macro_rules! reorg {
				($reorg_depth: expr) => {{
					invalidate_blocks(bitcoind, $reorg_depth);
					generate_blocks_and_wait(bitcoind, electrs, $reorg_depth).await;
				}};
			}

			let amount_sat = 2_100_000;
			let addr_nodes =
				nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect::<Vec<_>>();
			premine_and_distribute_funds(bitcoind, electrs, addr_nodes, Amount::from_sat(amount_sat)).await;

			macro_rules! sync_wallets {
				() => {
					for node in &nodes {
						node.sync_wallets().unwrap();
					}
				};
			}
			sync_wallets!();
			nodes.iter().for_each(|node| {
				assert_eq!(node.list_balances().spendable_onchain_balance_sats, amount_sat);
				assert_eq!(node.list_balances().total_onchain_balance_sats, amount_sat);
			});


			let mut nodes_funding_tx = HashMap::new();
			let funding_amount_sat = 2_000_000;
			for (node, next_node) in nodes.iter().zip(nodes.iter().cycle().skip(1)) {
				let funding_txo = open_channel(node, next_node, funding_amount_sat, true, &electrsd).await;
				nodes_funding_tx.insert(node.node_id(), funding_txo);
			}

			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			sync_wallets!();

			reorg!(reorg_depth);
			sync_wallets!();

			macro_rules! collect_channel_ready_events {
				($node:expr, $expected:expr) => {{
					let mut user_channels = HashMap::new();
					for _ in 0..$expected {
						match $node.next_event_async().await {
							Event::ChannelReady { user_channel_id, counterparty_node_id, .. } => {
								$node.event_handled().unwrap();
								user_channels.insert(counterparty_node_id, user_channel_id);
							},
							other => panic!("Unexpected event: {:?}", other),
						}
					}
					user_channels
				}};
			}

			let mut node_channels_id = HashMap::new();
			for (i, node) in nodes.iter().enumerate() {
				assert_eq!(
					node
						.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
							&& matches!(p.kind, PaymentKind::Onchain { .. }))
						.len(),
					1
				);

				let user_channels = collect_channel_ready_events!(node, 2);
				let next_node = nodes.get((i + 1) % nodes.len()).unwrap();
				let prev_node = nodes.get((i + nodes.len() - 1) % nodes.len()).unwrap();

				assert!(user_channels.get(&Some(next_node.node_id())) != None);
				assert!(user_channels.get(&Some(prev_node.node_id())) != None);

				let user_channel_id =
					user_channels.get(&Some(next_node.node_id())).expect("Missing user channel for node");
				node_channels_id.insert(node.node_id(), *user_channel_id);
			}


			for (node, next_node) in nodes.iter().zip(nodes.iter().cycle().skip(1)) {
				let user_channel_id = node_channels_id.get(&node.node_id()).expect("user channel id not exist");
				let funding = nodes_funding_tx.get(&node.node_id()).expect("Funding tx not exist");

				if force_close {
					node.force_close_channel(&user_channel_id, next_node.node_id(), None).unwrap();
				} else {
					node.close_channel(&user_channel_id, next_node.node_id()).unwrap();
				}

				expect_event!(node, ChannelClosed);
				expect_event!(next_node, ChannelClosed);

				wait_for_outpoint_spend(electrs, *funding).await;
			}

			reorg!(reorg_depth);
			sync_wallets!();

			generate_blocks_and_wait(bitcoind, electrs, 1).await;
			sync_wallets!();

				if force_close {
					let mut found_claimable_balance = false;
					for node in &nodes {
						node.sync_wallets().unwrap();
						let balances = node.list_balances();
						let confirmation_height = balances.lightning_balances.iter().find_map(|b| {
							match b {
								LightningBalance::ClaimableAwaitingConfirmations {
									confirmation_height,
									..
								} => Some(*confirmation_height),
								_ => None,
							}
						});
						let Some(confirmation_height) = confirmation_height else {
							continue;
						};
						found_claimable_balance = true;

						let cur_height = node.status().current_best_block.height;
						let blocks_to_go = confirmation_height.saturating_sub(cur_height);
						if blocks_to_go > 0 {
							generate_blocks_and_wait(bitcoind, electrs, blocks_to_go as usize).await;
							node.sync_wallets().unwrap();
						}

						wait_for_pending_sweep_balance(node, |balance| {
							matches!(
								balance,
								PendingSweepBalance::BroadcastAwaitingConfirmation { .. }
									| PendingSweepBalance::AwaitingThresholdConfirmations { .. }
							)
						})
						.await;

						generate_blocks_and_wait(bitcoind, electrs, 1).await;
						node.sync_wallets().unwrap();
						wait_for_pending_sweep_balance(node, |balance| {
							matches!(balance, PendingSweepBalance::AwaitingThresholdConfirmations { .. })
						})
						.await;
					}
					assert!(found_claimable_balance);
				}

			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			sync_wallets!();

			reorg!(reorg_depth);
			sync_wallets!();

			// The final reorg can leave close or sweep transactions below the wallet's
			// trusted spendable depth even after they are confirmed on the replacement chain.
			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			sync_wallets!();

			let fee_sat = 7000;
			// Check balance after close channel
			nodes.iter().for_each(|node| {
				assert!(node.list_balances().spendable_onchain_balance_sats > amount_sat - fee_sat);
				assert!(node.list_balances().spendable_onchain_balance_sats < amount_sat);

				assert_eq!(node.list_balances().total_anchor_channels_reserve_sats, 0);
				assert!(node.list_balances().lightning_balances.is_empty());

				assert_eq!(node.next_event(), None);
			});

			stop_nodes_concurrently(nodes).await;
		})
	}
}
