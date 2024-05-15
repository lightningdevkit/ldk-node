# LDK Node

[![Crate](https://img.shields.io/crates/v/ldk-node.svg?logo=rust)](https://crates.io/crates/ldk-node)
[![Documentation](https://img.shields.io/static/v1?logo=read-the-docs&label=docs.rs&message=ldk-node&color=informational)](https://docs.rs/ldk-node)
[![Maven Central Android](https://img.shields.io/maven-central/v/org.lightningdevkit/ldk-node-android)](https://central.sonatype.com/artifact/org.lightningdevkit/ldk-node-android)
[![Maven Central JVM](https://img.shields.io/maven-central/v/org.lightningdevkit/ldk-node-jvm)](https://central.sonatype.com/artifact/org.lightningdevkit/ldk-node-jvm)
[![Security Audit](https://github.com/lightningdevkit/ldk-node/actions/workflows/audit.yml/badge.svg)](https://github.com/lightningdevkit/ldk-node/actions/workflows/audit.yml)

A ready-to-go Lightning node library built using [LDK][ldk] and [BDK][bdk].

LDK Node is a self-custodial Lightning node in library form. Its central goal is to provide a small, simple, and straightforward interface that enables users to easily set up and run a Lightning node with an integrated on-chain wallet. While minimalism is at its core, LDK Node aims to be sufficiently modular and configurable to be useful for a variety of use cases.

## Getting Started
The primary abstraction of the library is the [`Node`][api_docs_node], which can be retrieved by setting up and configuring a [`Builder`][api_docs_builder] to your liking and calling one of the `build` methods. `Node` can then be controlled via commands such as `start`, `stop`, `connect_open_channel`, `send`, etc.

```rust
use ldk_node::Builder;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use std::str::FromStr;

fn main() {
	let mut builder = Builder::new();
	builder.set_network(Network::Testnet);
	builder.set_esplora_server("https://blockstream.info/testnet/api".to_string());
	builder.set_gossip_source_rgs("https://rapidsync.lightningdevkit.org/testnet/snapshot".to_string());

	let node = builder.build().unwrap();

	node.start().unwrap();

	let funding_address = node.onchain_payment().new_address();

	// .. fund address ..

	let node_id = PublicKey::from_str("NODE_ID").unwrap();
	let node_addr = SocketAddress::from_str("IP_ADDR:PORT").unwrap();
	node.connect_open_channel(node_id, node_addr, 10000, None, None, false).unwrap();

	let event = node.wait_next_event();
	println!("EVENT: {:?}", event);
	node.event_handled();

	let invoice = Bolt11Invoice::from_str("INVOICE_STR").unwrap();
	node.bolt11_payment().send(&invoice).unwrap();

	node.stop().unwrap();
}
```

## Modularity

LDK Node currently comes with a decidedly opinionated set of design choices:

- On-chain data is handled by the integrated [BDK][bdk] wallet.
- Chain data may currently be sourced from an [Esplora][esplora] server, while support for Electrum and `bitcoind` RPC will follow soon.
- Wallet and channel state may be persisted to an [SQLite][sqlite] database, to file system, or to a custom back-end to be implemented by the user.
- Gossip data may be sourced via Lightning's peer-to-peer network or the [Rapid Gossip Sync](https://docs.rs/lightning-rapid-gossip-sync/*/lightning_rapid_gossip_sync/) protocol.
- Entropy for the Lightning and on-chain wallets may be sourced from raw bytes or a [BIP39](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki) mnemonic. In addition, LDK Node offers the means to generate and persist the entropy bytes to disk.

## Language Support
LDK Node itself is written in [Rust][rust] and may therefore be natively added as a library dependency to any `std` Rust program. However, beyond its Rust API it also offers language bindings for [Swift][swift], [Kotlin][kotlin], and [Python][python] based on the [UniFFI](https://github.com/mozilla/uniffi-rs/). Moreover, [Flutter bindings][flutter_bindings] are also available.

## MSRV
The Minimum Supported Rust Version (MSRV) is currently 1.63.0.

[api_docs]: https://docs.rs/ldk-node/*/ldk_node/
[api_docs_node]: https://docs.rs/ldk-node/*/ldk_node/struct.Node.html
[api_docs_builder]: https://docs.rs/ldk-node/*/ldk_node/struct.Builder.html
[rust_crate]: https://crates.io/
[ldk]: https://lightningdevkit.org/
[bdk]: https://bitcoindevkit.org/
[esplora]: https://github.com/Blockstream/esplora
[sqlite]: https://sqlite.org/
[rust]: https://www.rust-lang.org/
[swift]: https://www.swift.org/
[kotlin]: https://kotlinlang.org/
[python]: https://www.python.org/
[flutter_bindings]: https://github.com/LtbLightning/ldk-node-flutter
