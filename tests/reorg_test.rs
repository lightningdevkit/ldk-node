mod common;
use bitcoin::{Amount, ScriptBuf};
use ldk_node::payment::{PaymentDirection, PaymentKind};
use ldk_node::{Event, LightningBalance, PendingSweepBalance};
use proptest::strategy::Strategy;
use proptest::strategy::ValueTree;
use proptest::{prelude::prop, proptest};
use std::collections::{HashMap, HashSet};

use crate::common::{
	bump_fee_and_broadcast, distribute_funds_unconfirmed, expect_event,
	generate_block_and_insert_transactions, generate_blocks_and_wait, invalidate_blocks,
	open_channel, premine_and_distribute_funds, premine_blocks, prepare_rbf,
	setup_bitcoind_and_electrsd, setup_node, wait_for_outpoint_spend, TestChainSource,
};

proptest! {
	#![proptest_config(proptest::test_runner::Config::with_cases(5))]

	#[test]
	fn reorg_test(reorg_depth in 1..=6usize, force_close in prop::bool::ANY) {
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

		let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
		let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
		let chain_source_esplora = TestChainSource::Esplora(&electrsd);

		let anchor_channels = true;
		let nodes = vec![
			setup_node(&chain_source_electrsd, anchor_channels),
			setup_node(&chain_source_bitcoind, anchor_channels),
			setup_node(&chain_source_esplora, anchor_channels),
		];

		let (bitcoind, electrs) = (&bitcoind.client, &electrsd.client);
		macro_rules! reorg {
			($reorg_depth: expr) => {{
				invalidate_blocks(bitcoind, $reorg_depth);
				generate_blocks_and_wait(bitcoind, electrs, $reorg_depth);
			}};
		}

		let amount_sat = 2_100_000;
		let addr_nodes =
			nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect::<Vec<_>>();
		premine_and_distribute_funds(bitcoind, electrs, addr_nodes, Amount::from_sat(amount_sat));

		macro_rules! sync_wallets {
			() => {
				nodes.iter().for_each(|node| node.sync_wallets().unwrap())
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
			let funding_txo = open_channel(node, next_node, funding_amount_sat, true, &electrsd);
			nodes_funding_tx.insert(node.node_id(), funding_txo);
		}

		generate_blocks_and_wait(bitcoind, electrs, 6);
		sync_wallets!();

		reorg!(reorg_depth);
		sync_wallets!();

		macro_rules! collect_channel_ready_events {
			($node:expr, $expected:expr) => {{
				let mut user_channels = HashMap::new();
				for _ in 0..$expected {
					match $node.wait_next_event() {
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

			wait_for_outpoint_spend(electrs, *funding);
		}

		reorg!(reorg_depth);
		sync_wallets!();

		generate_blocks_and_wait(bitcoind, electrs, 1);
		sync_wallets!();

		if force_close {
			nodes.iter().for_each(|node| {
				node.sync_wallets().unwrap();
				// If there is no more balance, there is nothing to process here.
				if node.list_balances().lightning_balances.len() < 1 {
					return;
				}
				match node.list_balances().lightning_balances[0] {
					LightningBalance::ClaimableAwaitingConfirmations {
						confirmation_height,
						..
					} => {
						let cur_height = node.status().current_best_block.height;
						let blocks_to_go = confirmation_height - cur_height;
						generate_blocks_and_wait(bitcoind, electrs, blocks_to_go as usize);
						node.sync_wallets().unwrap();
					},
					_ => panic!("Unexpected balance state for node_hub!"),
				}

				assert!(node.list_balances().lightning_balances.len() < 2);
				assert!(node.list_balances().pending_balances_from_channel_closures.len() > 0);
				match node.list_balances().pending_balances_from_channel_closures[0] {
					PendingSweepBalance::BroadcastAwaitingConfirmation { .. } => {},
					_ => panic!("Unexpected balance state!"),
				}

				generate_blocks_and_wait(&bitcoind, electrs, 1);
				node.sync_wallets().unwrap();
				assert!(node.list_balances().lightning_balances.len() < 2);
				assert!(node.list_balances().pending_balances_from_channel_closures.len() > 0);
				match node.list_balances().pending_balances_from_channel_closures[0] {
					PendingSweepBalance::AwaitingThresholdConfirmations { .. } => {},
					_ => panic!("Unexpected balance state!"),
				}
			});
		}

		generate_blocks_and_wait(bitcoind, electrs, 6);
		sync_wallets!();

		reorg!(reorg_depth);
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
	}

	#[test]
	fn test_reorg_rbf(
		reorg_depth in 2..=5usize,
		quantity_rbf in 2..=6usize,
	) {
		let mut runner = proptest::test_runner::TestRunner::default();
		let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

		let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
		let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
		let chain_source_esplora = TestChainSource::Esplora(&electrsd);

		let anchor_channels = true;
		let nodes = vec![
			setup_node(&chain_source_bitcoind, anchor_channels),
			setup_node(&chain_source_electrsd, anchor_channels),
			setup_node(&chain_source_esplora, anchor_channels),
		];

		let (bitcoind, electrs) = (&bitcoind.client, &electrsd.client);

		let mut amount_sat = 2_100_000;
		let all_addrs =
			nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect::<Vec<_>>();
		let scripts_buf: HashSet<ScriptBuf> =
			all_addrs.iter().map(|addr| addr.script_pubkey()).collect();

		premine_blocks(bitcoind, electrs);
		generate_blocks_and_wait(bitcoind, electrs, reorg_depth);
		let txid = distribute_funds_unconfirmed(bitcoind, electrs, all_addrs, Amount::from_sat(amount_sat));

		let mut is_spendable = false;
		macro_rules! verify_wallet_balances_and_transactions {
			($expected_balance_sat: expr, $expected_size_list_payments: expr) => {
				let spend_balance = if is_spendable { $expected_balance_sat } else { 0 };
				for node in &nodes {
					node.sync_wallets().unwrap();
					let balances = node.list_balances();
					assert_eq!(balances.total_onchain_balance_sats, $expected_balance_sat);
					assert_eq!(balances.spendable_onchain_balance_sats, spend_balance);
				}
			};
		}

		let mut tx_to_amount = HashMap::new();
		let (mut tx, fee_output_index) = prepare_rbf(electrs, txid, &scripts_buf);
		tx_to_amount.insert(tx.clone(), amount_sat);
		generate_block_and_insert_transactions(bitcoind, electrs, &[]);
		verify_wallet_balances_and_transactions!(amount_sat, expected_size_list_payments);
		for _ in 0..quantity_rbf {
			let is_alterable_value = prop::bool::ANY.new_tree(&mut runner).unwrap().current();
			if is_alterable_value {
				let value_sat = (5000..20000u64).new_tree(&mut runner).unwrap().current();
				let is_acrent_value = prop::bool::ANY.new_tree(&mut runner).unwrap().current();
				amount_sat = if is_acrent_value {amount_sat + value_sat} else {amount_sat - value_sat};
				for output in &mut tx.output {
					if scripts_buf.contains(&output.script_pubkey) {
						output.value = Amount::from_sat(amount_sat);
					}
				}
				let fee_sat = Amount::from_sat(scripts_buf.len() as u64 * value_sat);
				if is_acrent_value {
					tx.output[fee_output_index].value -= fee_sat;
				} else {
					tx.output[fee_output_index].value += fee_sat;
				}

			}

			tx = bump_fee_and_broadcast(bitcoind, electrs, tx, fee_output_index, is_spendable);
			tx_to_amount.insert(tx.clone(), amount_sat);

			verify_wallet_balances_and_transactions!(amount_sat, expected_size_list_payments);
		}

		is_spendable = true;
		let index_tx_confirm = (0..tx_to_amount.len() - 1).new_tree(&mut runner).unwrap().current();
		let tx_to_confirm = tx_to_amount.iter().nth(index_tx_confirm).unwrap();
		generate_block_and_insert_transactions(bitcoind, electrs, &[tx_to_confirm.0.clone()]);
		generate_blocks_and_wait(bitcoind, electrs, reorg_depth - 1);
		amount_sat = *tx_to_confirm.1;
		verify_wallet_balances_and_transactions!(amount_sat, expected_size_list_payments);

		invalidate_blocks(bitcoind, reorg_depth);
		generate_block_and_insert_transactions(bitcoind, electrs, &[]);

		let index_tx_confirm = (0..tx_to_amount.len() - 1).new_tree(&mut runner).unwrap().current();
		let tx_to_confirm = tx_to_amount.iter().nth(index_tx_confirm).unwrap();
		generate_block_and_insert_transactions(bitcoind, electrs, &[tx_to_confirm.0.clone()]);
		amount_sat = *tx_to_confirm.1;
		generate_blocks_and_wait(bitcoind, electrs, 5);
		verify_wallet_balances_and_transactions!(amount_sat, expected_size_list_payments);
	}
}
