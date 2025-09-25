// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(vss_test)]

mod common;

use ldk_node::Builder;
use std::collections::HashMap;

#[test]
fn channel_full_cycle_with_vss_store() {
	let (bitcoind, electrsd) = common::setup_bitcoind_and_electrsd();
	println!("== Node A ==");
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
	let config_a = common::random_config(true, "node_a".to_string());
	let mut builder_a = Builder::from_config(config_a.node_config.clone());
	builder_a.set_chain_source_esplora(esplora_url.clone(), None);
	crate::common::set_builder_log_writer(&mut builder_a, &config_a);

	let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
	let node_a = builder_a
		.build_with_vss_store_and_fixed_headers(
			vss_base_url.clone(),
			"node_1_store".to_string(),
			HashMap::new(),
		)
		.unwrap();
	node_a.start().unwrap();

	println!("\n== Node B ==");
	let config_b = common::random_config(true, "node_b".to_string());
	let mut builder_b = Builder::from_config(config_b.node_config.clone());
	builder_b.set_chain_source_esplora(esplora_url.clone(), None);
	crate::common::set_builder_log_writer(&mut builder_b, &config_b);

	let node_b = builder_b
		.build_with_vss_store_and_fixed_headers(
			vss_base_url,
			"node_2_store".to_string(),
			HashMap::new(),
		)
		.unwrap();
	node_b.start().unwrap();

	common::do_channel_full_cycle(
		node_a,
		node_b,
		&bitcoind.client,
		&electrsd.client,
		false,
		true,
		false,
	);
}
