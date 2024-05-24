mod common;

use common::{
	expect_channel_pending_event, expect_channel_ready_event,
	expect_payjoin_tx_sent_successfully_event, generate_blocks_and_wait,
	premine_and_distribute_funds, setup_bitcoind_and_electrsd, setup_two_payjoin_nodes,
	wait_for_tx,
};

use bitcoin::Amount;
use ldk_node::Event;

#[test]
fn send_receive_payjoin_transaction_with_channel_opening() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a_pj_receiver, node_b_pj_sender) = setup_two_payjoin_nodes(&electrsd, false);
	let addr_b = node_b_pj_sender.onchain_payment().new_address().unwrap();
	let addr_a = node_a_pj_receiver.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b, addr_a],
		Amount::from_sat(premine_amount_sat),
	);
	node_a_pj_receiver.sync_wallets().unwrap();
	node_b_pj_sender.sync_wallets().unwrap();
	assert_eq!(node_b_pj_sender.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_a_pj_receiver.list_balances().spendable_onchain_balance_sats, 100_000_00);
	assert_eq!(node_a_pj_receiver.next_event(), None);
	assert_eq!(node_a_pj_receiver.list_channels().len(), 0);
	let payjoin_payment = node_a_pj_receiver.payjoin_payment();
	let node_b_listening_address =
		node_b_pj_sender.listening_addresses().unwrap().get(0).unwrap().clone();
	let payjoin_uri = payjoin_payment
		.receive_with_channel_opening(
			80_000,
			None,
			false,
			node_b_pj_sender.node_id(),
			node_b_listening_address,
		).unwrap();
	let payjoin_uri = payjoin_uri.to_string();
	let sender_payjoin_payment = node_b_pj_sender.payjoin_payment();
	assert!(sender_payjoin_payment.send(payjoin_uri).is_ok());
	expect_channel_pending_event!(node_a_pj_receiver, node_b_pj_sender.node_id());
	expect_channel_pending_event!(node_b_pj_sender, node_a_pj_receiver.node_id());
	let txid = expect_payjoin_tx_sent_successfully_event!(node_b_pj_sender);
	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a_pj_receiver.sync_wallets().unwrap();
	node_b_pj_sender.sync_wallets().unwrap();
	let node_b_balance = node_b_pj_sender.list_balances();
	assert!(node_b_balance.total_onchain_balance_sats < premine_amount_sat - 80000);

	expect_channel_ready_event!(node_a_pj_receiver, node_b_pj_sender.node_id());
	expect_channel_ready_event!(node_b_pj_sender, node_a_pj_receiver.node_id());
	let channels = node_a_pj_receiver.list_channels();
	let channel = channels.get(0).unwrap();
	assert_eq!(channel.channel_value_sats, 80_000);
	assert!(channel.is_channel_ready);
	assert!(channel.is_usable);

	assert_eq!(node_a_pj_receiver.list_peers().get(0).unwrap().is_connected, true);
	assert_eq!(node_a_pj_receiver.list_peers().get(0).unwrap().is_persisted, true);
	assert_eq!(
		node_a_pj_receiver.list_peers().get(0).unwrap().node_id,
		node_b_pj_sender.node_id()
	);

	let invoice_amount_1_msat = 2500_000;
	let invoice =
		node_b_pj_sender.bolt11_payment().receive(invoice_amount_1_msat, "test", 1000).unwrap();
	assert!(node_a_pj_receiver.bolt11_payment().send(&invoice).is_ok());
}
