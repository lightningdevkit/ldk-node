// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! End-to-end test for the experimental `option_htlcs_claim_tx` (OP_TEMPLATEHASH) channel type.
//!
//! This exercises the full "claim an offered HTLC on the counterparty's confirmed commitment via
//! the preimage path" flow: the offered HTLC output is a P2TR output whose preimage spend path
//! commits, via `OP_TEMPLATEHASH`, to a fixed zero-fee v3 "HTLC claim transaction" that must be
//! confirmed alongside a fee-paying child as a TRUC 1-parent-1-child package (broadcast through
//! Bitcoin Core's `submitpackage`).
//!
//! `OP_TEMPLATEHASH` is an `OP_SUCCESSx` opcode, so stock `bitcoind` rejects the templated claim
//! transaction as non-standard until the soft fork activates. This test therefore requires a
//! Bitcoin Inquisition build that activates `OP_TEMPLATEHASH` on regtest from genesis (an as-yet
//! unreleased build heading for v29.3; it currently self-reports as v29.2.0), which makes the claim
//! package a standard, relayable transaction: the node's own `submitpackage` broadcast is accepted
//! into the mempool and confirmed by an ordinary block, exercising the real relay path end to end.
//! Point the `BITCOIND_EXE` env var at such a binary when running this test, e.g.
//! `BITCOIND_EXE=/path/to/bitcoin-inquisition cargo test --test integration_tests_htlcs_claim_tx`.
//! The test detects whether the `bitcoind` it launched activates `OP_TEMPLATEHASH` on regtest and
//! skips itself (rather than failing) when run against a stock `bitcoind` that cannot relay the
//! templated claim.

mod common;

use common::logging::{MockLogFacadeLogger, TestLogWriter};
use common::{
	expect_channel_pending_event, expect_channel_ready_event, expect_event,
	expect_payment_claimable_event, generate_blocks_and_wait, premine_and_distribute_funds,
	random_config, setup_node, wait_for_outpoint_spend, wait_for_tx, TestChainSource,
};

use ldk_node::payment::PaymentStatus;
use ldk_node::Event;

use lightning_types::payment::{PaymentHash, PaymentPreimage};

use bitcoin::hashes::{sha256::Hash as Sha256, Hash};
use bitcoin::Amount;

use std::sync::Arc;
use std::time::Duration;

/// Node A pays node B over an `option_htlcs_claim_tx` channel and B holds the preimage without
/// claiming. A then force-closes, putting its commitment (which contains the offered HTLC as a
/// templated P2TR output) on-chain. Only *after* the force close does B claim, forcing the
/// preimage to be revealed exclusively on-chain via the templated HTLC claim transaction. A
/// learning the preimage (PaymentSuccessful) therefore proves the whole templated-claim path
/// worked end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn htlcs_claim_tx_offered_htlc_claimed_via_preimage_path() {
	// Run against the Bitcoin Inquisition build named by the `BITCOIND_EXE` env var, which activates
	// `OP_TEMPLATEHASH` on regtest so the templated HTLC claim transaction is standard and can be
	// relayed through the mempool via `submitpackage`.
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();

	// Skip (rather than fail) unless the `bitcoind` we launched actually activates `OP_TEMPLATEHASH`
	// on regtest: stock Bitcoin Core has no `templatehash` deployment, so its `getdeploymentinfo`
	// omits it entirely, whereas an Inquisition build reports it as active. Without it the templated
	// claim is non-standard and the `submitpackage` relay this test asserts on cannot happen.
	let deployment_info: serde_json::Value =
		bitcoind.client.call("getdeploymentinfo", &[]).expect("getdeploymentinfo failed");
	let templatehash_active = deployment_info
		.get("deployments")
		.and_then(|deployments| deployments.get("templatehash"))
		.map(|templatehash| templatehash.get("active").and_then(|a| a.as_bool()).unwrap_or(true))
		.unwrap_or(false);
	if !templatehash_active {
		eprintln!(
			"skipping htlcs_claim_tx_offered_htlc_claimed_via_preimage_path: the bitcoind under test \
			 does not activate OP_TEMPLATEHASH on regtest. Set BITCOIND_EXE to a Bitcoin Inquisition \
			 build that does."
		);
		return;
	}

	// `option_htlcs_claim_tx` builds on zero-fee-commitment Anchor channels, and confirming the
	// resulting zero-fee package requires Bitcoin Core's `submitpackage`, so we must use the
	// Bitcoin Core RPC chain source (Esplora/Electrum are rejected by the 0FC startup validation).
	let chain_source = TestChainSource::BitcoindRpcSync(&bitcoind);

	// Both nodes negotiate the experimental `option_htlcs_claim_tx` channel type.
	let mut config_a = random_config(true);
	config_a.node_config.anchor_channels_config.as_mut().unwrap().negotiate_htlcs_claim_tx = true;
	let node_a = setup_node(&chain_source, config_a);

	// Capture node B's logs so we can assert it actually broadcast the templated HTLC claim
	// transaction (a message that is only emitted on the `option_htlcs_claim_tx` claim path).
	let node_b_logger = Arc::new(MockLogFacadeLogger::new());
	let mut config_b = random_config(true);
	config_b.node_config.anchor_channels_config.as_mut().unwrap().negotiate_htlcs_claim_tx = true;
	config_b.log_writer = TestLogWriter::Custom(Arc::clone(&node_b_logger) as Arc<_>);
	let node_b = setup_node(&chain_source, config_b);

	println!("\nCreated nodes A and B with 'htlcs_claim_tx' experimental feature set.\n");

	// Fund both nodes: A needs funds for the channel plus its anchor/0FC closing reserve, and B
	// needs confirmed on-chain UTXOs to pay for the fee-bumping child of its HTLC claim package.
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 1_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	)
	.await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Open a channel A -> B and confirm it.
	let funding_amount_sat = 400_000;
	node_a
		.open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			funding_amount_sat,
			None,
			None,
		)
		.unwrap();
	let funding_txo = expect_channel_pending_event!(node_a, node_b.node_id());
	expect_channel_pending_event!(node_b, node_a.node_id());
	wait_for_tx(&electrsd.client, funding_txo.txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	println!("\nChannel between A and B is funded and ready to use.\n");

	let channel_a = node_a.list_channels().into_iter().next().expect("channel should exist");
	let user_channel_id_a = channel_a.user_channel_id;

	// B registers a held invoice (manual claim) so the inbound HTLC stays pending until we
	// explicitly claim it, leaving it live in both commitments. We use a sizeable amount so the
	// offered HTLC is well above the dust threshold and shows up as an on-chain output.
	let htlc_amount_msat = 100_000_000; // 100k sat
	let preimage = PaymentPreimage([42u8; 32]);
	let payment_hash = PaymentHash(Sha256::hash(&preimage.0).to_byte_array());
	let description = ldk_node::lightning_invoice::Bolt11InvoiceDescription::Direct(
		ldk_node::lightning_invoice::Description::new("htlcs-claim-tx".to_string()).unwrap(),
	);
	let invoice = node_b
		.bolt11_payment()
		.receive_for_hash(htlc_amount_msat, &description, 3600, payment_hash)
		.unwrap();

	// A pays the invoice; B sees it as claimable but does NOT claim yet.
	let payment_id = node_a.bolt11_payment().send(&invoice, None).unwrap();
	expect_payment_claimable_event!(node_b, payment_id, payment_hash, htlc_amount_msat);
	assert_eq!(node_a.payment(&payment_id).unwrap().status, PaymentStatus::Pending);

	// A force-closes, broadcasting its commitment (containing the templated offered-HTLC output)
	// together with the 0FC anchor child as a TRUC package.
	println!("\nA force-closes the channel.\n");
	node_a.force_close_channel(&user_channel_id_a, node_b.node_id(), None).unwrap();
	expect_event!(node_a, ChannelClosed);
	expect_event!(node_b, ChannelClosed);

	// Wait for A's commitment (which spends the funding output) to be broadcast, then confirm it.
	wait_for_outpoint_spend(&electrsd.client, funding_txo).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Now that A's commitment is confirmed and the channel is closed, B reveals the preimage. As
	// the channel is closed, this can only resolve A's HTLC by B broadcasting the templated,
	// zero-fee HTLC claim transaction together with its fee-paying child (via `submitpackage`).
	println!("\nB claims the held HTLC (forces on-chain templated claim).\n");
	let claimable_amount_msat = node_b.payment(&payment_id).unwrap().amount_msat.unwrap();
	node_b.bolt11_payment().claim_for_hash(payment_hash, claimable_amount_msat, preimage).unwrap();

	// B's monitor emits a `BumpTransactionEvent::HTLCsClaimTxResolution`, whose default handler
	// builds, signs, and broadcasts the templated claim transaction together with its fee-paying
	// child as a TRUC 1-parent-1-child package via `submitpackage`. Because `OP_TEMPLATEHASH` is
	// *active* on this Inquisition regtest, that package is standard and relayable, so it is accepted
	// into the mempool. Drive B forward (block connections nudge the monitor) until we observe the
	// package sitting in the mempool, checking *before* mining each block so we catch it unconfirmed.
	let b_balance_before = node_b.list_balances().spendable_onchain_balance_sats;
	let mut claim_relayed = false;
	for _ in 0..20 {
		node_b.sync_wallets().unwrap();
		let mempool: Vec<String> =
			bitcoind.client.call("getrawmempool", &[]).expect("getrawmempool failed");
		if !mempool.is_empty() {
			claim_relayed = true;
			break;
		}
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		tokio::time::sleep(Duration::from_millis(300)).await;
	}

	// The templated claim package must have been accepted into the mempool by the node's own
	// `submitpackage` broadcast.
	assert!(
		claim_relayed,
		"the templated HTLC claim package was never accepted into the mempool; submitpackage relay failed"
	);

	// Assert B specifically took the templated `option_htlcs_claim_tx` path: the default bump
	// handler logs this line only when broadcasting the OP_TEMPLATEHASH HTLC claim transaction and
	// its fee-paying child, distinguishing it from an ordinary on-chain HTLC-success claim. Grab the
	// claim transaction's txid from that line so we can tie it to the `submitpackage` acceptance.
	let logs = node_b_logger.retrieve_logs();
	let claim_txid = logs
		.iter()
		.find_map(|l| {
			l.split_once("Broadcasting HTLC claim transaction ")
				.and_then(|(_, rest)| rest.split_whitespace().next())
				.map(str::to_string)
		})
		.expect(
			"node B did not broadcast a templated HTLC claim transaction; the option_htlcs_claim_tx path was not used",
		);

	// And assert Bitcoin Core *accepted* that templated claim package via `submitpackage` (the
	// broadcaster logs this only on a successful package submission).
	// (An unrelated `Failed broadcast` for A's already-mined commitment/anchor package is expected
	// and benign.)
	assert!(
		logs.iter().any(|l| l.contains("Successfully broadcast package") && l.contains(&claim_txid)),
		"node B's submitpackage broadcast of the templated claim package (txid {claim_txid}) was not accepted"
	);

	// Mining an ordinary block now confirms the relayed package straight out of the mempool.
	// Once the claim transaction is confirmed, A reads the preimage out of its witness and resolves
	// the outbound payment it had sent. Since the channel was force-closed before B revealed the
	// preimage, A could *only* have learned it from this on-chain claim.
	let mut a_succeeded = false;
	for i in 0..20 {
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
		node_a.sync_wallets().unwrap();
		node_b.sync_wallets().unwrap();
		if node_a.payment(&payment_id).map(|p| p.status) == Some(PaymentStatus::Succeeded) {
			a_succeeded = true;
			println!("\nA's payment resolved as Succeeded after {} block(s).\n", i + 1);
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(
		a_succeeded,
		"node A never learned the preimage on-chain; the templated HTLC claim path did not complete"
	);

	// A should have surfaced a PaymentSuccessful event for the originally-sent payment.
	expect_event!(node_a, PaymentSuccessful);

	// And B should end up with more spendable on-chain funds than before the claim, reflecting the
	// HTLC value swept in via the claim package's child output (minus fees).
	let mut b_gained = false;
	for _ in 0..10 {
		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
		node_b.sync_wallets().unwrap();
		if node_b.list_balances().spendable_onchain_balance_sats > b_balance_before {
			b_gained = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(b_gained, "node B's on-chain balance did not grow after claiming the offered HTLC");

	node_a.stop().unwrap();
	node_b.stop().unwrap();
}
