mod common;
use std::{thread::sleep, time::Duration};

use crate::common::{
	expect_channel_pending_event, expect_channel_ready_event, generate_blocks_and_wait,
	premine_and_distribute_funds, setup_two_payjoin_nodes, wait_for_tx,
};
use bitcoin::Amount;
use common::setup_bitcoind_and_electrsd;
use ldk_node::Event;

#[test]
fn send_receive_with_channel_opening_payjoin_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_payjoin_nodes(&electrsd, false);
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_a.list_channels().len(), 0);
	assert_eq!(node_b.next_event(), None);
	assert_eq!(node_b.list_channels().len(), 0);
	let funding_amount_sat = 80_000;
	let node_b_listening_address = node_b.listening_addresses().unwrap().get(0).unwrap().clone();
	let payjoin_uri = node_a.request_payjoin_transaction_with_channel_opening(
		funding_amount_sat,
		None,
		false,
		node_b.node_id(),
		node_b_listening_address,
	);
	let payjoin_uri = match payjoin_uri {
		Ok(payjoin_uri) => payjoin_uri,
		Err(e) => {
			dbg!(&e);
			assert!(false);
			panic!("should generate payjoin uri");
		},
	};
	assert!(node_b
		.send_payjoin_transaction(
			payjoin::Uri::try_from(payjoin_uri.to_string()).unwrap().assume_checked()
		)
		.is_ok());
	expect_channel_pending_event!(node_a, node_b.node_id());
	expect_channel_pending_event!(node_b, node_a.node_id());
	let channels = node_a.list_channels();
	let channel = channels.get(0).unwrap();
	wait_for_tx(&electrsd.client, channel.funding_txo.unwrap().txid);
	sleep(Duration::from_secs(1));
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());
	let channels = node_a.list_channels();
	let channel = channels.get(0).unwrap();
	assert_eq!(channel.channel_value_sats, funding_amount_sat);
	assert_eq!(channel.confirmations.unwrap(), 6);
	assert!(channel.is_channel_ready);
	assert!(channel.is_usable);

	assert_eq!(node_a.list_peers().get(0).unwrap().is_connected, true);
	assert_eq!(node_a.list_peers().get(0).unwrap().is_persisted, true);
	assert_eq!(node_a.list_peers().get(0).unwrap().node_id, node_b.node_id());

	let invoice_amount_1_msat = 2500_000;
	let invoice = node_b.bolt11_payment().receive(invoice_amount_1_msat, "test", 1000).unwrap();
	assert!(node_a.bolt11_payment().send(&invoice).is_ok());
}
