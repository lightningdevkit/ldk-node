mod common;

use common::{
	expect_payjoin_tx_sent_successfully_event, generate_blocks_and_wait,
	premine_and_distribute_funds, setup_bitcoind_and_electrsd, setup_two_payjoin_nodes,
	wait_for_tx,
};

use bitcoin::Amount;
use ldk_node::{
	payment::{PaymentDirection, PaymentKind, PaymentStatus},
	Event,
};

use crate::common::expect_payjoin_tx_pending_event;

#[test]
fn send_receive_regular_payjoin_transaction() {
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
	let payjoin_payment = node_a_pj_receiver.payjoin_payment();
	let payjoin_uri = payjoin_payment.receive(Amount::from_sat(80_000)).unwrap();
	let payjoin_uri = payjoin_uri.to_string();
	dbg!(&payjoin_uri);
	let sender_payjoin_payment = node_b_pj_sender.payjoin_payment();
	assert!(sender_payjoin_payment.send(payjoin_uri).is_ok());
	let txid = expect_payjoin_tx_pending_event!(node_b_pj_sender);
	let payments = node_b_pj_sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.amount_msat, Some(80_000));
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert_eq!(payment.direction, PaymentDirection::Outbound);
	assert_eq!(payment.kind, PaymentKind::Payjoin);
	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 3);
	node_b_pj_sender.sync_wallets().unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 4);
	node_b_pj_sender.sync_wallets().unwrap();
	let payments = node_b_pj_sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.status, PaymentStatus::Succeeded);
	expect_payjoin_tx_sent_successfully_event!(node_b_pj_sender);
	let node_b_balance = node_b_pj_sender.list_balances();
	assert!(node_b_balance.total_onchain_balance_sats < premine_amount_sat - 80000);
}

#[test]
fn send_payjoin_with_amount() {
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
	let payjoin_payment = node_a_pj_receiver.payjoin_payment();
	let payjoin_uri = payjoin_payment.receive(Amount::from_sat(100_000_000)).unwrap();
	let payjoin_uri = payjoin_uri.to_string();
	dbg!(&payjoin_uri);
	let sender_payjoin_payment = node_b_pj_sender.payjoin_payment();
	assert!(sender_payjoin_payment.send_with_amount(payjoin_uri, 80_000).is_ok());
	let _txid = expect_payjoin_tx_pending_event!(node_b_pj_sender);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 3);
	node_b_pj_sender.sync_wallets().unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 4);
	node_b_pj_sender.sync_wallets().unwrap();
	let _txid = expect_payjoin_tx_sent_successfully_event!(node_b_pj_sender);
	let node_b_balance = node_b_pj_sender.list_balances();
	assert!(node_b_balance.total_onchain_balance_sats < premine_amount_sat - 80000);
}
