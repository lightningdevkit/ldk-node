mod common;
use std::{thread::sleep, time::Duration};

use crate::common::{
	expect_channel_pending_event, expect_channel_ready_event, generate_blocks_and_wait,
	premine_and_distribute_funds, setup_two_nodes, wait_for_tx,
};
use bitcoin::Amount;
use bitcoincore_rpc::{Client as BitcoindClient, RpcApi};
use common::setup_bitcoind_and_electrsd;
use ldk_node::Event;

#[test]
fn send_receive_with_channel_opening_payjoin_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true);
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
	let payjoin_uri = node_a
		.request_payjoin_transaction_with_channel_opening(
			funding_amount_sat,
			None,
			false,
			node_b.node_id(),
			node_b_listening_address,
		)
		.unwrap();
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

#[test]
fn send_receive_regular_payjoin_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let (node_a, node_b) = setup_two_nodes(&electrsd, false, true);
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
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_a.list_channels().len(), 0);
	let payjoin_uri = node_a.request_payjoin_transaction(80_000).unwrap();
	assert!(node_b
		.send_payjoin_transaction(
			payjoin::Uri::try_from(payjoin_uri.to_string()).unwrap().assume_checked()
		)
		.is_ok());
	sleep(Duration::from_secs(3));
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	let node_a_balance = node_a.list_balances();
	let node_b_balance = node_b.list_balances();
	assert_eq!(node_a_balance.total_onchain_balance_sats, 80000);
	assert!(node_b_balance.total_onchain_balance_sats < premine_amount_sat - 80000);
}

mod payjoin_v1 {
	use bitcoin::address::NetworkChecked;
	use bitcoin::base64;
	use bitcoin::Txid;
	use bitcoincore_rpc::Client as BitcoindClient;
	use bitcoincore_rpc::RpcApi;
	use std::collections::HashMap;
	use std::str::FromStr;

	use bitcoincore_rpc::bitcoin::psbt::Psbt;

	pub fn send(
		sender_wallet: &BitcoindClient, payjoin_uri: payjoin::Uri<'static, NetworkChecked>,
	) -> Txid {
		let amount_to_send = payjoin_uri.amount.unwrap();
		let receiver_address = payjoin_uri.address.clone();
		let mut outputs = HashMap::with_capacity(1);
		outputs.insert(receiver_address.to_string(), amount_to_send);
		let options = bitcoincore_rpc::json::WalletCreateFundedPsbtOptions {
			lock_unspent: Some(false),
			fee_rate: Some(bitcoincore_rpc::bitcoin::Amount::from_sat(10000)),
			..Default::default()
		};
		let sender_psbt = sender_wallet
			.wallet_create_funded_psbt(
				&[], // inputs
				&outputs,
				None, // locktime
				Some(options),
				None,
			)
			.unwrap();
		let psbt =
			sender_wallet.wallet_process_psbt(&sender_psbt.psbt, None, None, None).unwrap().psbt;
		let psbt = Psbt::from_str(&psbt).unwrap();
		let (req, ctx) =
			payjoin::send::RequestBuilder::from_psbt_and_uri(psbt.clone(), payjoin_uri)
				.unwrap()
				.build_with_additional_fee(
					bitcoincore_rpc::bitcoin::Amount::from_sat(1),
					None,
					bitcoincore_rpc::bitcoin::FeeRate::MIN,
					true,
				)
				.unwrap()
				.extract_v1()
				.unwrap();
		let url_http = req.url.as_str().replace("https", "http");
		let res = reqwest::blocking::Client::new();
		let res = res
			.post(&url_http)
			.body(req.body.clone())
			.header("content-type", "text/plain")
			.send()
			.unwrap();
		let res = res.text().unwrap();
		let psbt = ctx.process_response(&mut res.as_bytes()).unwrap();
		let psbt = sender_wallet
			.wallet_process_psbt(&base64::encode(psbt.serialize()), None, None, None)
			.unwrap()
			.psbt;
		let tx = sender_wallet.finalize_psbt(&psbt, Some(true)).unwrap().hex.unwrap();
		let txid = sender_wallet.send_raw_transaction(&tx).unwrap();
		txid
	}
}

#[test]
fn receive_payjoin_version_1() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let payjoin_sender_wallet: BitcoindClient = bitcoind.create_wallet("payjoin_sender").unwrap();
	let (node_a, _) = setup_two_nodes(&electrsd, false, true);
	let addr_sender = payjoin_sender_wallet.get_new_address(None, None).unwrap().assume_checked();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_sender],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert_eq!(
		payjoin_sender_wallet.get_balances().unwrap().mine.trusted.to_sat(),
		premine_amount_sat
	);
	assert_eq!(node_a.next_event(), None);
	assert_eq!(node_a.list_channels().len(), 0);
	let pj_uri = node_a.request_payjoin_transaction(80_000).unwrap();
	payjoin_v1::send(
		&payjoin_sender_wallet,
		payjoin::Uri::try_from(pj_uri.to_string()).unwrap().assume_checked(),
	);
	sleep(Duration::from_secs(3));
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	let node_a_balance = node_a.list_balances();
	assert_eq!(node_a_balance.total_onchain_balance_sats, 80000);
}

// test validation of payjoin transaction fails
// test counterparty doesnt return fundingsigned
