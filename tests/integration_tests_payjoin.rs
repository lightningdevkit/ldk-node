mod common;

use common::{
	expect_payjoin_tx_sent_successfully_event, generate_blocks_and_wait,
	premine_and_distribute_funds, setup_bitcoind_and_electrsd, setup_two_payjoin_nodes,
	wait_for_tx,
};

use bitcoin::Amount;
use ldk_node::Event;

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

	let payjoin_uri = tokio::runtime::Runtime::new().unwrap().handle().block_on(async {
		let payjoin_uri = payjoin_payment.receive(Amount::from_sat(80_000)).await.unwrap();
		payjoin_uri
	});
	let payjoin_uri = payjoin_uri.to_string();
	let sender_payjoin_payment = node_b_pj_sender.payjoin_payment();
	assert!(sender_payjoin_payment.send(payjoin_uri).is_ok());
	let txid = expect_payjoin_tx_sent_successfully_event!(node_b_pj_sender);
	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_b_pj_sender.sync_wallets().unwrap();
	let node_b_balance = node_b_pj_sender.list_balances();
	assert!(node_b_balance.total_onchain_balance_sats < premine_amount_sat - 80000);
}
