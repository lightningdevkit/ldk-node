// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(cln_test)]

mod common;

use common::cln::TestClnNode;
use common::scenarios::channel::{cooperative_close_by_ldk, open_channel_to_external};
use common::scenarios::payment::receive_keysend_payment;
use common::scenarios::setup_interop_test;
use common::scenarios::suite::{run_combos, run_scenarios};
use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;
use ldk_node::Builder;

async fn setup_clients() -> (BitcoindClient, ElectrumClient, TestClnNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();
	let cln = TestClnNode::from_env();
	(bitcoind, electrs, cln)
}

fn setup_ldk_node() -> ldk_node::Node {
	let config = common::random_config(true);
	let mut builder = Builder::from_config(config.node_config);
	builder.set_chain_source_electrum("tcp://127.0.0.1:50001".to_string(), None);
	let node = builder.build(config.node_entropy).unwrap();
	node.start().unwrap();
	node
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	run_scenarios(&node, &cln, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cln_combos() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	run_combos(&node, &cln, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

/// CLN keysend uses a fixed final_cltv_expiry_delta of 18, but LDK requires
/// at least 42 (HTLC_FAIL_BACK_BUFFER + 3). CLN's keysend RPC has no parameter
/// to override this. Upstream CLN issue needed to add a final_cltv parameter.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "CLN keysend final_cltv=18 < LDK minimum 42 — needs CLN upstream fix"]
async fn test_cln_receive_keysend() {
	let (bitcoind, electrs, cln) = setup_clients().await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &cln, &bitcoind, &electrs).await;
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &cln, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;
	receive_keysend_payment(&node, &cln, 5_000_000).await;
	cooperative_close_by_ldk(&node, &cln, &bitcoind, &electrs, &user_ch).await;
	node.stop().unwrap();
}
