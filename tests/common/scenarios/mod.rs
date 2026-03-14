// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Shared interop test scenarios, generic over `ExternalNode`.
//!
//! Each scenario function takes an LDK `Node`, an external node implementation,
//! and the regtest infrastructure clients. Test entry-point files
//! (`integration_tests_{cln,lnd,eclair}.rs`) call these functions with their
//! concrete `ExternalNode` implementation.
//!
//! ## Naming convention
//!
//! - **Building blocks** (e.g. `receive_bolt11_payment`, `force_close_by_ldk`):
//!   Composable primitives that assume the caller has already called
//!   `setup_interop_test` and opened a channel. No `test_` prefix.
//!
//! - **Full scenarios** (e.g. `run_inbound_channel_test`,
//!   `cooperative_close_after_fee_change`): End-to-end flows that call
//!   `setup_interop_test` internally. Used directly as test bodies.
//!
//! - **Combo orchestrator** (`combo::run_interop_combo_test`): Composes
//!   building blocks across disconnect → payment → close phases. Driven by
//!   the `interop_combo_tests!` macro.

pub(crate) mod channel;
pub(crate) mod combo;
pub(crate) mod disconnect;
pub(crate) mod payment;
pub(crate) mod splice;

use std::future::Future;
use std::time::Duration;

use bitcoin::Amount;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::Node;

use super::external_node::{ExternalChannel, ExternalNode};
use super::{generate_blocks_and_wait, premine_and_distribute_funds};

#[derive(Debug, Clone)]
pub(crate) enum Phase {
	Payment,
	Idle,
}

#[derive(Debug, Clone)]
pub(crate) enum Side {
	Ldk,
	External,
}

#[derive(Debug, Clone)]
pub(crate) enum CloseType {
	Cooperative,
	Force,
}

#[derive(Debug, Clone)]
pub(crate) enum PayType {
	Bolt11,
	Keysend,
}

/// Find a specific channel on the external node by its channel ID.
pub(crate) async fn find_ext_channel(
	peer: &(impl ExternalNode + ?Sized), ext_channel_id: &str,
) -> ExternalChannel {
	let channels = peer.list_channels().await.unwrap();
	channels
		.into_iter()
		.find(|ch| ch.channel_id == ext_channel_id)
		.unwrap_or_else(|| panic!("Channel {} not found on {}", ext_channel_id, peer.name()))
}

/// Fund both LDK node and external node, connect them.
pub(crate) async fn setup_interop_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	// Fund LDK node
	let ldk_address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(5_000_000);
	premine_and_distribute_funds(bitcoind, electrs, vec![ldk_address], premine_amount).await;

	// Fund external node using the already-loaded wallet
	let ext_funding_addr_str = peer.get_funding_address().await.unwrap();
	let ext_amount = Amount::from_sat(5_000_000);
	let amounts_json = serde_json::json!({&ext_funding_addr_str: ext_amount.to_btc()});
	let empty_account = serde_json::json!("");
	// Use the ldk_node_test wallet that premine_and_distribute_funds already loaded
	bitcoind
		.call::<serde_json::Value>(
			"sendmany",
			&[empty_account, amounts_json, serde_json::json!(0), serde_json::json!("")],
		)
		.expect("failed to fund external node");
	generate_blocks_and_wait(bitcoind, electrs, 1).await;

	// Wait for external node to sync to the current chain tip before proceeding.
	// Without this, the external node may not have indexed the funding tx yet,
	// causing channel opens to time out.
	let chain_height: u64 = bitcoind.get_blockchain_info().unwrap().blocks.try_into().unwrap();
	peer.wait_for_block_sync(chain_height).await.unwrap();

	node.sync_wallets().unwrap();

	// Connect LDK to external node
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();
	node.connect(ext_node_id, ext_addr, true).unwrap();
}

/// Retry an async operation up to `max_attempts` times with a 1-second delay between attempts.
/// Used for operations that may fail due to gossip propagation delay.
pub(crate) async fn retry_until_ok<F, Fut, T, E>(max_attempts: u32, operation: &str, mut f: F) -> T
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<T, E>>,
	E: std::fmt::Display,
{
	for attempt in 1..=max_attempts {
		match f().await {
			Ok(val) => return val,
			Err(e) => {
				if attempt == max_attempts {
					panic!("{} failed after {} attempts: {}", operation, max_attempts, e);
				}
				tokio::time::sleep(Duration::from_secs(1)).await;
			},
		}
	}
	unreachable!()
}
