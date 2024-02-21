mod common;
use crate::common::{premine_and_distribute_funds, random_config, setup_node};
use bitcoin::Amount;
use bitcoincore_rpc::{Client as BitcoindClient, RpcApi};
use common::setup_bitcoind_and_electrsd;

mod mock_payjoin_sender {
	use bitcoincore_rpc::Client as BitcoindClient;
	use bitcoincore_rpc::RpcApi;
	use payjoin::bitcoin::address::NetworkChecked;
	use std::collections::HashMap;
	use std::str::FromStr;

	use bitcoincore_rpc::bitcoin::psbt::Psbt;
	use payjoin::send::RequestBuilder;

	pub fn try_payjoin(
		sender_wallet: &BitcoindClient, pj_uri: payjoin::Uri<'static, NetworkChecked>,
	) -> (String, String) {
		// Step 1. Extract the parameters from the payjoin URI
		let amount_to_send = pj_uri.amount.unwrap();
		let receiver_address = pj_uri.address.clone();
		// Step 2. Construct URI request parameters, a finalized "Original PSBT" paying `.amount` to `.address`
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
		// Step 4. Construct the request with the PSBT and parameters
		let (req, _ctx) = RequestBuilder::from_psbt_and_uri(psbt.clone(), pj_uri)
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
		// Step 5. Send the request and receive response
		// let payjoin_url = pj_uri.extras.e
		// BITCOIN:BCRT1Q0S724W239Z2XQGSZV6TE96HMYLEDCTX3GDFZEP?amount=0.01&pj=https://localhost:3000
		let url_http = req.url.as_str().replace("https", "http");
		dbg!(&url_http);
		let res = reqwest::blocking::Client::new();
		let res = res
			.post(&url_http)
			.body(req.body.clone())
			.header("content-type", "text/plain")
			.send()
			.unwrap();
		let res = res.text().unwrap();
		(res, String::from_utf8(req.body).unwrap())
		// Step 6. Process the response
		//
		// An `Ok` response should include a Payjoin Proposal PSBT.
		// Check that it's signed, following protocol, not trying to steal or otherwise error.
		//let psbt = ctx.process_response(&mut res.as_bytes()).unwrap();
		//// Step 7. Sign and finalize the Payjoin Proposal PSBT
		////
		//// Most software can handle adding the last signatures to a PSBT without issue.
		//let psbt = sender_wallet
		//	.wallet_process_psbt(&base64::encode(psbt.serialize()), None, None, None)
		//	.unwrap()
		//	.psbt;
		//let tx = sender_wallet.finalize_psbt(&psbt, Some(true)).unwrap().hex.unwrap();
		//// Step 8. Broadcast the Payjoin Transaction
		//let txid = sender_wallet.send_raw_transaction(&tx).unwrap();
		//txid
	}
}

#[test]
fn payjoin() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let payjoin_sender_wallet: BitcoindClient = bitcoind.create_wallet("payjoin_sender").unwrap();
	let config_a = random_config();
	let node_a = setup_node(&electrsd, config_a);
	let addr_a = node_a.new_onchain_address().unwrap();
	let addr_sender = payjoin_sender_wallet.get_new_address(None, None).unwrap().assume_checked();
	let premine_amount_sat = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_sender],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(
		payjoin_sender_wallet.get_balances().unwrap().mine.trusted.to_sat(),
		premine_amount_sat
	);
	assert_eq!(node_a.next_event(), None);
	let funding_amount_sat = 80_000;
	let pj_uri = node_a.payjoin_bip21(funding_amount_sat).unwrap();
	println!("Payjoin URI: {:?}", pj_uri);
	dbg!(&pj_uri);
	let (receiver_response, sender_original_psbt) = mock_payjoin_sender::try_payjoin(
		&payjoin_sender_wallet,
		payjoin::Uri::try_from(pj_uri).unwrap().assume_checked(),
	);
	assert_eq!(receiver_response, sender_original_psbt);
}
