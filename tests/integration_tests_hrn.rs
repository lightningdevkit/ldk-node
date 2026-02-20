// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(hrn_tests)]

mod common;

use bitcoin::Amount;
use common::{
	expect_channel_ready_event, expect_payment_successful_event, generate_blocks_and_wait,
	open_channel, premine_and_distribute_funds, setup_bitcoind_and_electrsd, setup_two_nodes,
	TestChainSource,
};
use ldk_node::payment::UnifiedPaymentResult;
use ldk_node::Event;
use lightning::ln::channelmanager::PaymentId;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unified_send_to_hrn() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	)
	.await;

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcast a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	let test_offer = node_b.bolt12_payment().receive(1000000, "test offer", None, None).unwrap();

	// Sleep one more sec to make sure the node announcement propagates.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let hrn_str = "matt@mattcorallo.com";

	let offer_payment_id: PaymentId =
		match node_a.unified_payment().send(&hrn_str, Some(1000000), None, &test_offer).await {
			Ok(UnifiedPaymentResult::Bolt12 { payment_id }) => {
				println!("\nBolt12 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(UnifiedPaymentResult::Bolt11 { payment_id: _ }) => {
				panic!("Expected Bolt12 payment but got Bolt11");
			},
			Ok(UnifiedPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt12 payment but got On-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt12 payment but got error: {:?}", e);
			},
		};

	expect_payment_successful_event!(node_a, Some(offer_payment_id), None);
}
