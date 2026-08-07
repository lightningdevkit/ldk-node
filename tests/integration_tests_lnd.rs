// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(lnd_test)]

mod common;

use common::lnd::TestLndNode;
use common::scenarios::{
	basic_channel_cycle_bolt11_scenario, basic_channel_cycle_bolt12_scenario,
	disconnect_during_payment_scenario, force_close_after_payment_bolt11_scenario,
	force_close_after_payment_bolt12_scenario, keysend_scenario, run_interop_scenario,
	splice_in_bolt11_scenario, splice_in_bolt12_scenario,
};
use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrsd::electrum_client::Client as ElectrumClient;

async fn setup_clients() -> (BitcoindClient, ElectrumClient, TestLndNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();
	let lnd = TestLndNode::from_env().await;
	(bitcoind, electrs, lnd)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_basic_channel_cycle_bolt11() {
	run_interop_scenario(setup_clients(), basic_channel_cycle_bolt11_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "BOLT12 support for LDN is not implemented yet"]
async fn test_basic_channel_cycle_bolt12() {
	run_interop_scenario(setup_clients(), basic_channel_cycle_bolt12_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_keysend() {
	run_interop_scenario(setup_clients(), keysend_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_force_close_after_payment_bolt11() {
	run_interop_scenario(setup_clients(), force_close_after_payment_bolt11_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "BOLT12 support for LDN is not implemented yet"]
async fn test_force_close_after_payment_bolt12() {
	run_interop_scenario(setup_clients(), force_close_after_payment_bolt12_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_disconnect_during_payment() {
	run_interop_scenario(setup_clients(), disconnect_during_payment_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "LND does not implement BOLT splicing"]
async fn test_splice_in_bolt11() {
	run_interop_scenario(setup_clients(), splice_in_bolt11_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "BOLT12 support for LDN is not implemented yet"]
async fn test_splice_in_bolt12() {
	run_interop_scenario(setup_clients(), splice_in_bolt12_scenario).await;
}
