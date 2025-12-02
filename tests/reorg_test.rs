mod common;
use std::collections::HashMap;

use bitcoin::Amount;
use ldk_node::payment::{PaymentDirection, PaymentKind};
use ldk_node::{Event, LightningBalance, PendingSweepBalance};
use proptest::prelude::prop;
use proptest::proptest;

use crate::common::{
	expect_event, generate_blocks_and_wait, invalidate_blocks, open_channel,
	premine_and_distribute_funds, random_config, setup_bitcoind_and_electrsd, setup_node,
	wait_for_outpoint_spend, TestChainSource,
};

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

			let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
			let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
			let chain_source_esplora = TestChainSource::Esplora(&electrsd);

			macro_rules! config_node {
				($chain_source: expr, $anchor_channels: expr) => {{
					let config_a = random_config($anchor_channels);
					let node = setup_node(&$chain_source, config_a);
					node
				}};
			}
			let anchor_channels = true;
			let nodes = vec![
				config_node!(chain_source_electrsd, anchor_channels),
				config_node!(chain_source_bitcoind, anchor_channels),
				config_node!(chain_source_esplora, anchor_channels),
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
				for node in &nodes {
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
							generate_blocks_and_wait(bitcoind, electrs, blocks_to_go as usize).await;
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

					generate_blocks_and_wait(&bitcoind, electrs, 1).await;
					node.sync_wallets().unwrap();
					assert!(node.list_balances().lightning_balances.len() < 2);
					assert!(node.list_balances().pending_balances_from_channel_closures.len() > 0);
					match node.list_balances().pending_balances_from_channel_closures[0] {
						PendingSweepBalance::AwaitingThresholdConfirmations { .. } => {},
						_ => panic!("Unexpected balance state!"),
					}
				}
			}

			generate_blocks_and_wait(bitcoind, electrs, 6).await;
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
		})
	}
}
