use crate::test::utils::*;
use crate::test::utils::{expect_event, random_config};
use crate::{Builder, Error, Event, PaymentDirection, PaymentStatus};

use bitcoin::Amount;

#[test]
fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	println!("== Node A ==");
	let esplora_url = electrsd.esplora_url.as_ref().unwrap();
	let config_a = random_config(esplora_url);
	let node_a = Builder::from_config(config_a).build();
	node_a.start().unwrap();
	let addr_a = node_a.new_funding_address().unwrap();

	println!("\n== Node B ==");
	let config_b = random_config(esplora_url);
	let node_b = Builder::from_config(config_b).build();
	node_b.start().unwrap();
	let addr_b = node_b.new_funding_address().unwrap();

	let premine_amount_sat = 100_000;

	premine_and_distribute_funds(
		&bitcoind,
		&electrsd,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.on_chain_balance().unwrap().get_spendable(), premine_amount_sat);
	assert_eq!(node_b.on_chain_balance().unwrap().get_spendable(), premine_amount_sat);

	println!("\nA -- connect_open_channel -> B");
	let funding_amount_sat = 80_000;
	let push_msat = (funding_amount_sat / 2) * 1000; // balance the channel
	let node_b_addr = format!("{}@{}", node_b.node_id(), node_b.listening_address().unwrap());
	node_a.connect_open_channel(&node_b_addr, funding_amount_sat, Some(push_msat), true).unwrap();

	expect_event!(node_a, ChannelPending);

	let funding_txo = match node_b.next_event() {
		ref e @ Event::ChannelPending { funding_txo, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			funding_txo
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};

	wait_for_tx(&electrsd, funding_txo.txid);

	println!("\n .. generating blocks, syncing wallets .. ");
	generate_blocks_and_wait(&bitcoind, &electrsd, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let onchain_fee_buffer_sat = 1500;
	let node_a_balance = node_a.on_chain_balance().unwrap();
	let node_a_upper_bound_sat = premine_amount_sat - funding_amount_sat;
	let node_a_lower_bound_sat = premine_amount_sat - funding_amount_sat - onchain_fee_buffer_sat;
	assert!(node_a_balance.get_spendable() < node_a_upper_bound_sat);
	assert!(node_a_balance.get_spendable() > node_a_lower_bound_sat);
	assert_eq!(node_b.on_chain_balance().unwrap().get_spendable(), premine_amount_sat);

	expect_event!(node_a, ChannelReady);

	let channel_id = match node_b.next_event() {
		ref e @ Event::ChannelReady { channel_id, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			channel_id
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};

	println!("\nB receive_payment");
	let invoice_amount_1_msat = 1000000;
	let invoice = node_b.receive_payment(invoice_amount_1_msat, &"asdf", 9217).unwrap();

	println!("\nA send_payment");
	let payment_hash = node_a.send_payment(invoice.clone()).unwrap();

	let outbound_payments_a =
		node_a.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound);
	assert_eq!(outbound_payments_a.len(), 1);

	let inbound_payments_a =
		node_a.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound);
	assert_eq!(inbound_payments_a.len(), 0);

	let outbound_payments_b =
		node_b.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound);
	assert_eq!(outbound_payments_b.len(), 0);

	let inbound_payments_b =
		node_b.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound);
	assert_eq!(inbound_payments_b.len(), 1);

	expect_event!(node_a, PaymentSuccessful);
	expect_event!(node_b, PaymentReceived);
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(invoice_amount_1_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(invoice_amount_1_msat));

	// Assert we fail duplicate outbound payments.
	assert_eq!(Err(Error::NonUniquePaymentHash), node_a.send_payment(invoice));

	// Test under-/overpayment
	let invoice_amount_2_msat = 1000_000;
	let invoice = node_b.receive_payment(invoice_amount_2_msat, &"asdf", 9217).unwrap();

	let underpaid_amount = invoice_amount_2_msat - 1;
	assert_eq!(
		Err(Error::InvalidAmount),
		node_a.send_payment_using_amount(invoice, underpaid_amount)
	);

	let invoice = node_b.receive_payment(invoice_amount_2_msat, &"asdf", 9217).unwrap();
	let overpaid_amount_msat = invoice_amount_2_msat + 100;
	let payment_hash = node_a.send_payment_using_amount(invoice, overpaid_amount_msat).unwrap();
	expect_event!(node_a, PaymentSuccessful);
	let received_amount = match node_b.next_event() {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			amount_msat
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};
	assert_eq!(received_amount, overpaid_amount_msat);
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(overpaid_amount_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(overpaid_amount_msat));

	// Test "zero-amount" invoice payment
	let variable_amount_invoice = node_b.receive_variable_amount_payment(&"asdf", 9217).unwrap();
	let determined_amount_msat = 1234_567;
	assert_eq!(Err(Error::InvalidInvoice), node_a.send_payment(variable_amount_invoice.clone()));
	let payment_hash =
		node_a.send_payment_using_amount(variable_amount_invoice, determined_amount_msat).unwrap();

	expect_event!(node_a, PaymentSuccessful);
	let received_amount = match node_b.next_event() {
		ref e @ Event::PaymentReceived { amount_msat, .. } => {
			println!("{} got event {:?}", std::stringify!(node_b), e);
			node_b.event_handled();
			amount_msat
		}
		ref e => {
			panic!("{} got unexpected event!: {:?}", std::stringify!(node_b), e);
		}
	};
	assert_eq!(received_amount, determined_amount_msat);
	assert_eq!(node_a.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_a.payment(&payment_hash).unwrap().direction, PaymentDirection::Outbound);
	assert_eq!(node_a.payment(&payment_hash).unwrap().amount_msat, Some(determined_amount_msat));
	assert_eq!(node_b.payment(&payment_hash).unwrap().status, PaymentStatus::Succeeded);
	assert_eq!(node_b.payment(&payment_hash).unwrap().direction, PaymentDirection::Inbound);
	assert_eq!(node_b.payment(&payment_hash).unwrap().amount_msat, Some(determined_amount_msat));

	node_b.close_channel(&channel_id, &node_a.node_id()).unwrap();
	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	wait_for_outpoint_spend(&electrsd, funding_txo);

	generate_blocks_and_wait(&bitcoind, &electrsd, 1);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let sum_of_all_payments_sat =
		(push_msat + invoice_amount_1_msat + overpaid_amount_msat + determined_amount_msat) / 1000;
	let node_a_upper_bound_sat =
		(premine_amount_sat - funding_amount_sat) + (funding_amount_sat - sum_of_all_payments_sat);
	let node_a_lower_bound_sat = node_a_upper_bound_sat - onchain_fee_buffer_sat;
	assert!(node_a.on_chain_balance().unwrap().get_spendable() > node_a_lower_bound_sat);
	assert!(node_a.on_chain_balance().unwrap().get_spendable() < node_a_upper_bound_sat);
	let expected_final_amount_node_b_sat = premine_amount_sat + sum_of_all_payments_sat;
	assert_eq!(
		node_b.on_chain_balance().unwrap().get_spendable(),
		expected_final_amount_node_b_sat
	);

	node_a.stop().unwrap();
	println!("\nA stopped");
	node_b.stop().unwrap();
	println!("\nB stopped");
}

#[test]
fn channel_open_fails_when_funds_insufficient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	println!("== Node A ==");
	let esplora_url = electrsd.esplora_url.as_ref().unwrap();
	let config_a = random_config(&esplora_url);
	let node_a = Builder::from_config(config_a).build();
	node_a.start().unwrap();
	let addr_a = node_a.new_funding_address().unwrap();

	println!("\n== Node B ==");
	let config_b = random_config(&esplora_url);
	let node_b = Builder::from_config(config_b).build();
	node_b.start().unwrap();
	let addr_b = node_b.new_funding_address().unwrap();

	let premine_amount_sat = 100_000;

	premine_and_distribute_funds(
		&bitcoind,
		&electrsd,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.on_chain_balance().unwrap().get_spendable(), premine_amount_sat);
	assert_eq!(node_b.on_chain_balance().unwrap().get_spendable(), premine_amount_sat);

	println!("\nA -- connect_open_channel -> B");
	let node_b_addr = format!("{}@{}", node_b.node_id(), node_b.listening_address().unwrap());
	assert_eq!(
		Err(Error::InsufficientFunds),
		node_a.connect_open_channel(&node_b_addr, 120000, None, true)
	);
}

#[test]
fn connect_to_public_testnet_esplora() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = electrsd.esplora_url.as_ref().unwrap();
	let mut config = random_config(&esplora_url);
	config.esplora_server_url = "https://blockstream.info/testnet/api".to_string();
	config.network = bitcoin::Network::Testnet;
	let node = Builder::from_config(config).build();
	node.start().unwrap();
	node.sync_wallets().unwrap();
	node.stop().unwrap();
}

#[test]
fn start_stop_reinit() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = electrsd.esplora_url.as_ref().unwrap();
	let config = random_config(&esplora_url);
	let node = Builder::from_config(config.clone()).build();
	let expected_node_id = node.node_id();

	let funding_address = node.new_funding_address().unwrap();
	let expected_amount = Amount::from_sat(100000);

	premine_and_distribute_funds(&bitcoind, &electrsd, vec![funding_address], expected_amount);
	assert_eq!(node.on_chain_balance().unwrap().get_total(), 0);

	node.start().unwrap();
	assert_eq!(node.start(), Err(Error::AlreadyRunning));

	node.sync_wallets().unwrap();
	assert_eq!(node.on_chain_balance().unwrap().get_spendable(), expected_amount.to_sat());

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(Error::NotRunning));

	node.start().unwrap();
	assert_eq!(node.start(), Err(Error::AlreadyRunning));

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(Error::NotRunning));
	drop(node);

	let reinitialized_node = Builder::from_config(config).build();
	assert_eq!(reinitialized_node.node_id(), expected_node_id);

	reinitialized_node.start().unwrap();

	assert_eq!(
		reinitialized_node.on_chain_balance().unwrap().get_spendable(),
		expected_amount.to_sat()
	);

	reinitialized_node.sync_wallets().unwrap();
	assert_eq!(
		reinitialized_node.on_chain_balance().unwrap().get_spendable(),
		expected_amount.to_sat()
	);

	reinitialized_node.stop().unwrap();
}
