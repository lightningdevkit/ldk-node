#![cfg(vss_test)]

mod common;

use ldk_node::Builder;

#[test]
fn channel_full_cycle_with_vss_store() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	println!("== Node A ==");
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let config_a = common::random_config(true);
	let mut builder_a = Builder::from_config(config_a);
	builder_a.set_esplora_server(esplora_url.clone());
	let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
	let node_a =
		builder_a.build_with_vss_store(vss_base_url.clone(), "node_1_store".to_string()).unwrap();
	node_a.start().unwrap();

	println!("\n== Node B ==");
	let config_b = common::random_config(true);
	let mut builder_b = Builder::from_config(config_b);
	builder_b.set_esplora_server(esplora_url);
	let node_b = builder_b.build_with_vss_store(vss_base_url, "node_2_store".to_string()).unwrap();
	node_b.start().unwrap();

	common::do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true);
}
