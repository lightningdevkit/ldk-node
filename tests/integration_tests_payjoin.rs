mod common;

use common::{
	expect_payjoin_tx_sent_successfully_event, generate_blocks_and_wait,
	premine_and_distribute_funds, setup_bitcoind_and_electrsd, wait_for_tx,
};

use bitcoin::Amount;
use bitcoincore_rpc::{Client as BitcoindClient, RawTx, RpcApi};
use ldk_node::{
	payment::{PaymentDirection, PaymentKind, PaymentStatus},
	Event,
};
use payjoin::{
	receive::v2::{Enrolled, Enroller},
	OhttpKeys, PjUriBuilder,
};

use crate::common::{
	expect_payjoin_await_confirmation, random_config, setup_node, setup_payjoin_node,
};

struct PayjoinReceiver {
	ohttp_keys: OhttpKeys,
	enrolled: Enrolled,
}

enum ResponseType<'a> {
	ModifyOriginalPsbt(bitcoin::Address),
	BroadcastWithoutResponse(&'a BitcoindClient),
}

impl PayjoinReceiver {
	fn enroll() -> Self {
		let payjoin_directory = payjoin::Url::parse("https://payjo.in").unwrap();
		let payjoin_relay = payjoin::Url::parse("https://pj.bobspacebkk.com").unwrap();
		let ohttp_keys = {
			let payjoin_directory = payjoin_directory.join("/ohttp-keys").unwrap();
			let proxy = reqwest::Proxy::all(payjoin_relay.clone()).unwrap();
			let client = reqwest::blocking::Client::builder().proxy(proxy).build().unwrap();
			let response = client.get(payjoin_directory).send().unwrap();
			let response = response.bytes().unwrap();
			OhttpKeys::decode(response.to_vec().as_slice()).unwrap()
		};
		let mut enroller = Enroller::from_directory_config(
			payjoin_directory.clone(),
			ohttp_keys.clone(),
			payjoin_relay.clone(),
		);
		let (req, ctx) = enroller.extract_req().unwrap();
		let mut headers = reqwest::header::HeaderMap::new();
		headers.insert(
			reqwest::header::CONTENT_TYPE,
			reqwest::header::HeaderValue::from_static("message/ohttp-req"),
		);
		let response = reqwest::blocking::Client::new()
			.post(&req.url.to_string())
			.body(req.body)
			.headers(headers)
			.send()
			.unwrap();
		let response = match response.bytes() {
			Ok(response) => response,
			Err(_) => {
				panic!("Error reading response");
			},
		};
		let enrolled = enroller.process_res(response.to_vec().as_slice(), ctx).unwrap();
		Self { ohttp_keys, enrolled }
	}

	pub(crate) fn receive(
		&self, amount: bitcoin::Amount, receiving_address: bitcoin::Address,
	) -> String {
		let enrolled = self.enrolled.clone();
		let fallback_target = enrolled.fallback_target();
		let ohttp_keys = self.ohttp_keys.clone();
		let pj_part = payjoin::Url::parse(&fallback_target).unwrap();
		let payjoin_uri = PjUriBuilder::new(receiving_address, pj_part, Some(ohttp_keys.clone()))
			.amount(amount)
			.build();
		payjoin_uri.to_string()
	}

	pub(crate) fn process_payjoin_request(self, response_type: Option<ResponseType>) {
		let mut enrolled = self.enrolled;
		let (req, context) = enrolled.extract_req().unwrap();
		let client = reqwest::blocking::Client::new();
		let response = client
			.post(req.url.to_string())
			.body(req.body)
			.headers(PayjoinReceiver::ohttp_headers())
			.send()
			.unwrap();
		let response = response.bytes().unwrap();
		let response = enrolled.process_res(response.to_vec().as_slice(), context).unwrap();
		let unchecked_proposal = response.unwrap();
		match response_type {
			Some(ResponseType::BroadcastWithoutResponse(bitcoind)) => {
				let tx = unchecked_proposal.extract_tx_to_schedule_broadcast();
				let raw_tx = tx.raw_hex();
				bitcoind.send_raw_transaction(raw_tx).unwrap();
				return;
			},
			_ => {},
		}

		let proposal = unchecked_proposal.assume_interactive_receiver();
		let proposal = proposal.check_inputs_not_owned(|_script| Ok(false)).unwrap();
		let proposal = proposal.check_no_mixed_input_scripts().unwrap();
		let proposal = proposal.check_no_inputs_seen_before(|_outpoint| Ok(false)).unwrap();
		let mut provisional_proposal =
			proposal.identify_receiver_outputs(|_script| Ok(true)).unwrap();

		match response_type {
			Some(ResponseType::ModifyOriginalPsbt(substitue_address)) => {
				provisional_proposal.substitute_output_address(substitue_address);
			},
			_ => {},
		}

		// Finalise Payjoin Proposal
		let mut payjoin_proposal =
			provisional_proposal.finalize_proposal(|psbt| Ok(psbt.clone()), None).unwrap();

		let (receiver_request, _) = payjoin_proposal.extract_v2_req().unwrap();
		reqwest::blocking::Client::new()
			.post(&receiver_request.url.to_string())
			.body(receiver_request.body)
			.headers(PayjoinReceiver::ohttp_headers())
			.send()
			.unwrap();
	}

	fn ohttp_headers() -> reqwest::header::HeaderMap {
		let mut headers = reqwest::header::HeaderMap::new();
		headers.insert(
			reqwest::header::CONTENT_TYPE,
			reqwest::header::HeaderValue::from_static("message/ohttp-req"),
		);
		headers
	}
}

// Test sending payjoin transaction with changes to the original PSBT
#[test]
fn send_payjoin_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config_a = random_config(false);
	let config_b = random_config(false);
	let receiver = setup_node(&electrsd, config_a);
	let sender = setup_payjoin_node(&electrsd, config_b);
	let addr_a = sender.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a],
		Amount::from_sat(premine_amount_sat),
	);
	sender.sync_wallets().unwrap();
	assert_eq!(sender.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(sender.list_balances().spendable_onchain_balance_sats, 100_000_00);
	assert_eq!(sender.next_event(), None);

	let payjoin_receiver_handler = PayjoinReceiver::enroll();
	let payjoin_uri = payjoin_receiver_handler
		.receive(Amount::from_sat(80_000), receiver.onchain_payment().new_address().unwrap());

	assert!(sender.payjoin_payment().send(payjoin_uri).is_ok());

	let payments = sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.amount_msat, Some(80_000));
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert_eq!(payment.direction, PaymentDirection::Outbound);
	assert_eq!(payment.kind, PaymentKind::Payjoin);

	let substitue_address = receiver.onchain_payment().new_address().unwrap();
	// Receiver modifies the original PSBT
	payjoin_receiver_handler
		.process_payjoin_request(Some(ResponseType::ModifyOriginalPsbt(substitue_address)));

	let txid = expect_payjoin_await_confirmation!(sender);

	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);
	sender.sync_wallets().unwrap();
	let payments = sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.amount_msat, Some(80_000));
	assert_eq!(payment.status, PaymentStatus::Succeeded);
	assert_eq!(payment.direction, PaymentDirection::Outbound);
	assert_eq!(payment.kind, PaymentKind::Payjoin);
	assert_eq!(payment.txid, Some(txid));

	expect_payjoin_tx_sent_successfully_event!(sender, true);
}

// Test sending payjoin transaction with original PSBT used eventually
#[test]
fn send_payjoin_transaction_original_psbt_used() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config_a = random_config(false);
	let config_b = random_config(false);
	let receiver = setup_node(&electrsd, config_b);
	let sender = setup_payjoin_node(&electrsd, config_a);
	let addr_a = sender.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a],
		Amount::from_sat(premine_amount_sat),
	);
	sender.sync_wallets().unwrap();
	assert_eq!(sender.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(sender.list_balances().spendable_onchain_balance_sats, 100_000_00);
	assert_eq!(sender.next_event(), None);

	let payjoin_receiver_handler = PayjoinReceiver::enroll();
	let payjoin_uri = payjoin_receiver_handler
		.receive(Amount::from_sat(80_000), receiver.onchain_payment().new_address().unwrap());

	assert!(sender.payjoin_payment().send(payjoin_uri).is_ok());

	let payments = sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.amount_msat, Some(80_000));
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert_eq!(payment.direction, PaymentDirection::Outbound);
	assert_eq!(payment.kind, PaymentKind::Payjoin);

	// Receiver does not modify the original PSBT
	payjoin_receiver_handler.process_payjoin_request(None);

	let txid = expect_payjoin_await_confirmation!(sender);

	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);
	sender.sync_wallets().unwrap();

	let _ = expect_payjoin_tx_sent_successfully_event!(sender, false);
}

// Test sending payjoin transaction with receiver broadcasting and not responding to the payjoin
// request
#[test]
fn send_payjoin_transaction_with_receiver_broadcasting() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config_a = random_config(false);
	let config_b = random_config(false);
	let receiver = setup_node(&electrsd, config_b);
	let sender = setup_payjoin_node(&electrsd, config_a);
	let addr_a = sender.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000_00;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a],
		Amount::from_sat(premine_amount_sat),
	);
	sender.sync_wallets().unwrap();
	assert_eq!(sender.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(sender.list_balances().spendable_onchain_balance_sats, 100_000_00);
	assert_eq!(sender.next_event(), None);

	let payjoin_receiver_handler = PayjoinReceiver::enroll();
	let payjoin_uri = payjoin_receiver_handler
		.receive(Amount::from_sat(80_000), receiver.onchain_payment().new_address().unwrap());

	assert!(sender.payjoin_payment().send(payjoin_uri).is_ok());

	let payments = sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.amount_msat, Some(80_000));
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert_eq!(payment.direction, PaymentDirection::Outbound);
	assert_eq!(payment.kind, PaymentKind::Payjoin);

	let txid = payment.txid.unwrap();

	// Receiver broadcasts the transaction without responding to the payjoin request
	payjoin_receiver_handler
		.process_payjoin_request(Some(ResponseType::BroadcastWithoutResponse(&bitcoind.client)));

	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);
	sender.sync_wallets().unwrap();
	let payments = sender.list_payments();
	let payment = payments.first().unwrap();
	assert_eq!(payment.amount_msat, Some(80_000));
	assert_eq!(payment.status, PaymentStatus::Succeeded);
	assert_eq!(payment.direction, PaymentDirection::Outbound);
	assert_eq!(payment.kind, PaymentKind::Payjoin);
	assert_eq!(payment.txid, Some(txid));

	expect_payjoin_tx_sent_successfully_event!(sender, false);
}
