#![cfg(vss_test)]

mod common;

use ldk_node::{generate_entropy_mnemonic, Builder};

#[test]
fn channel_full_cycle_with_vss_store() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	println!("== Node A ==");
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let mnemonic_a = generate_entropy_mnemonic();
	let config_a = common::random_config();
	let mut builder_a = Builder::from_entropy_bip39_mnemonic(mnemonic_a, None, Some(config_a));
	builder_a.set_esplora_server(esplora_url.clone());
	let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
	let node_a =
		builder_a.build_with_vss_store(vss_base_url.clone(), "node_1_store".to_string()).unwrap();
	node_a.start().unwrap();

	println!("\n== Node B ==");
	let mnemonic_b = generate_entropy_mnemonic();
	let config_b = common::random_config();
	let mut builder_b = Builder::from_entropy_bip39_mnemonic(mnemonic_b, None, Some(config_b));
	builder_b.set_esplora_server(esplora_url);
	let node_b = builder_b.build_with_vss_store(vss_base_url, "node_2_store".to_string()).unwrap();
	node_b.start().unwrap();

	common::do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false);
}
