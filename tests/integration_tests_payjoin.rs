mod common;

use crate::common::{
	generate_blocks_and_wait, premine_and_distribute_funds, setup_two_payjoin_nodes, wait_for_tx,
};
use bitcoin::Amount;
use common::setup_bitcoind_and_electrsd;

#[test]
fn send_receive_regular_payjoin_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a_pj_receiver, node_b_pj_sender) = setup_two_payjoin_nodes(&electrsd, false);
	let addr_b = node_b_pj_sender.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a_pj_receiver.sync_wallets().unwrap();
	node_b_pj_sender.sync_wallets().unwrap();
	assert_eq!(node_b_pj_sender.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_a_pj_receiver.list_balances().spendable_onchain_balance_sats, 0);
	assert_eq!(node_a_pj_receiver.next_event(), None);
	assert_eq!(node_a_pj_receiver.list_channels().len(), 0);
	let payjoin_uri = node_a_pj_receiver.request_payjoin_transaction(80_000).unwrap();
	let txid = tokio::runtime::Runtime::new().unwrap().handle().block_on(async {
		let txid = node_b_pj_sender
			.send_payjoin_transaction(
				payjoin::Uri::try_from(payjoin_uri.to_string()).unwrap().assume_checked(),
				None,
			)
			.await;
		txid
	});
	dbg!(&txid);
	wait_for_tx(&electrsd.client, txid.unwrap().unwrap());
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a_pj_receiver.sync_wallets().unwrap();
	node_b_pj_sender.sync_wallets().unwrap();
	let node_a_balance = node_a_pj_receiver.list_balances();
	let node_b_balance = node_b_pj_sender.list_balances();
	assert_eq!(node_a_balance.total_onchain_balance_sats, 80000);
	assert!(node_b_balance.total_onchain_balance_sats < premine_amount_sat - 80000);
}
