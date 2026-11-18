// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(feature = "storage-vss")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;

use ldk_node::entropy::NodeEntropy;
use ldk_node::Builder;

// This needs to remain the only test in this file: the `CryptoProvider` default is process-global,
// so any other test building a `Node` in the same process would install it for us.
#[test]
fn crypto_provider_is_installed_before_vss_store_setup() {
	assert!(rustls::crypto::CryptoProvider::get_default().is_none());

	// Serve a single response with an unsupported protocol version, which makes the VSS store
	// setup fail without retrying.
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let vss_url = format!("http://{}", listener.local_addr().unwrap());
	let server = std::thread::spawn(move || {
		let (mut stream, _) = listener.accept().unwrap();
		let mut request = Vec::new();
		while !request.windows(4).any(|w| w == b"\r\n\r\n") {
			let mut buffer = [0; 1024];
			let bytes_read = stream.read(&mut buffer).unwrap();
			assert!(bytes_read > 0, "HTTP request ended before its headers");
			request.extend_from_slice(&buffer[..bytes_read]);
		}
		let response =
			"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nvss-protocol-version: 999\r\nconnection: close\r\n\r\n";
		stream.write_all(response.as_bytes()).unwrap();
	});

	#[cfg(not(feature = "uniffi"))]
	let node_entropy = NodeEntropy::from_seed_bytes([42u8; 64]);
	#[cfg(feature = "uniffi")]
	let node_entropy = std::sync::Arc::new(NodeEntropy::from_seed_bytes(vec![42u8; 64]).unwrap());
	let res = Builder::new().build_with_vss_store_and_fixed_headers(
		node_entropy,
		vss_url,
		"store_id".to_owned(),
		HashMap::new(),
	);
	assert!(res.is_err());
	server.join().unwrap();

	assert!(
		rustls::crypto::CryptoProvider::get_default().is_some(),
		"The CryptoProvider must be installed before the VSS store makes its first request"
	);
}
