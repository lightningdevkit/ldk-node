# LDK Node

[![Crate](https://img.shields.io/crates/v/ldk-node.svg?logo=rust)](https://crates.io/crates/ldk-node)
[![Documentation](https://img.shields.io/static/v1?logo=read-the-docs&label=docs.rs&message=ldk-node&color=informational)](https://docs.rs/ldk-node)
[![PyPI](https://img.shields.io/pypi/v/ldk-node.svg?logo=python)](https://pypi.org/project/ldk-node/)
[![Maven Central Android](https://img.shields.io/maven-central/v/org.lightningdevkit/ldk-node-android)](https://central.sonatype.com/artifact/org.lightningdevkit/ldk-node-android)
[![Maven Central JVM](https://img.shields.io/maven-central/v/org.lightningdevkit/ldk-node-jvm)](https://central.sonatype.com/artifact/org.lightningdevkit/ldk-node-jvm)
[![Security Audit](https://github.com/lightningdevkit/ldk-node/actions/workflows/audit.yml/badge.svg)](https://github.com/lightningdevkit/ldk-node/actions/workflows/audit.yml)

A ready-to-go Lightning node library built using [LDK][ldk] and [BDK][bdk].

LDK Node is a self-custodial Lightning node in library form. Its central goal is to provide a small, simple, and straightforward interface that enables users to easily set up and run a Lightning node with an integrated on-chain wallet. While minimalism is at its core, LDK Node aims to be sufficiently modular and configurable to be useful for a variety of use cases.

## Getting Started
The primary abstraction of the library is the [`Node`][api_docs_node], which can be retrieved by setting up and configuring a [`Builder`][api_docs_builder] to your liking and calling one of the `build` methods. `Node` can then be controlled via commands such as `start`, `stop`, `open_channel`, `send`, etc.

```rust
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::bip39::Mnemonic;
use ldk_node::entropy::NodeEntropy;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::Builder;
use std::str::FromStr;

fn main() {
	let mut builder = Builder::new();
	builder.set_network(Network::Testnet);
	builder.set_chain_source_esplora("https://blockstream.info/testnet/api".to_string(), None);
	builder.set_gossip_source_rgs(
		"https://rapidsync.lightningdevkit.org/testnet/v2/snapshot".to_string(),
	);


	let mnemonic = Mnemonic::generate(24).unwrap();
	let node_entropy = NodeEntropy::from_bip39_mnemonic(mnemonic, None);
	let node = builder.build(node_entropy).unwrap();

	node.start().unwrap();

	let funding_address = node.onchain_payment().new_address();

	// .. fund address ..

	let node_id = PublicKey::from_str("NODE_ID").unwrap();
	let node_addr = SocketAddress::from_str("IP_ADDR:PORT").unwrap();
	node.open_channel(node_id, node_addr, 10000, None, None).unwrap();

	let event = node.wait_next_event();
	println!("EVENT: {:?}", event);
	node.event_handled();

	let invoice = Bolt11Invoice::from_str("INVOICE_STR").unwrap();
	node.bolt11_payment().send(&invoice, None).unwrap();

	node.stop().unwrap();
}
```

## Modularity

LDK Node currently comes with a decidedly opinionated set of design choices:

- On-chain data is handled by the integrated [BDK][bdk] wallet.
- Chain data may currently be sourced from the Bitcoin Core RPC interface, or from an [Electrum][electrum] or [Esplora][esplora] server.
- Wallet and channel state may be persisted to an [SQLite][sqlite] or [PostgreSQL][postgresql] database, to the filesystem, to a VSS server, or to a custom back-end to be implemented by the user.
- Gossip data may be sourced via Lightning's peer-to-peer network or the [Rapid Gossip Sync](https://docs.rs/lightning-rapid-gossip-sync/*/lightning_rapid_gossip_sync/) protocol.
- Entropy for the Lightning and on-chain wallets may be sourced from raw bytes or a [BIP39](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki) mnemonic. In addition, LDK Node offers the means to generate and persist the entropy bytes to disk.

### Cargo Features

LDK Node's optional dependencies are grouped by the functionality they provide:

| Feature | Functionality |
| --- | --- |
| `chain-esplora` | Esplora chain source |
| `chain-electrum` | Electrum chain source |
| `chain-bitcoind` | Bitcoin Core RPC and REST chain source |
| `storage-sqlite` | SQLite storage |
| `storage-filesystem` | Filesystem storage |
| `storage-vss` | Versioned Storage Service storage |
| `storage-postgres` | PostgreSQL storage |
| `storage-postgres-vendored-tls` | PostgreSQL storage with vendored OpenSSL |
| `unified-payments` | BIP 21 and human-readable-name payment support |
| `uniffi` | UniFFI language bindings |
| `uniffi-default` | The standard language-binding feature set |

The `default` feature set preserves the native Rust API's previous behavior. It enables all three
chain sources, SQLite, filesystem and VSS storage, and unified payments. PostgreSQL and UniFFI
remain opt-in. Every build must enable at least one chain source feature.

On Linux, `storage-postgres` uses the system OpenSSL installation and requires the OpenSSL
development headers and `pkg-config`. Enable `storage-postgres-vendored-tls` instead to build
OpenSSL from source. Vendored builds require a C compiler, `make`, and Perl.

Disable the default features to select only the functionality and dependencies an application
needs. For example:

```shell
cargo build --no-default-features --features chain-esplora,storage-sqlite
```

`uniffi-default` enables UniFFI, Esplora, Electrum, SQLite, VSS, and unified payments. It excludes
Bitcoin Core, filesystem storage, and PostgreSQL. Binding users can add any of those features:

```shell
cargo build --no-default-features --features uniffi-default,chain-bitcoind
```

Use `uniffi` directly instead of `uniffi-default` to assemble a fully custom binding build. For
example, a Bitcoin Core and PostgreSQL-only binding build uses:

```shell
cargo build --no-default-features --features uniffi,chain-bitcoind,storage-postgres
```

The binding generation scripts use `uniffi-default`. Set `LDK_NODE_EXTRA_FEATURES` to add features
to their builds:

```shell
LDK_NODE_EXTRA_FEATURES=chain-bitcoind,storage-postgres \
	./scripts/uniffi_bindgen_generate_python.sh
```

## Compatibility

LDK Node does not provide a stable public API until v1.0. Persisted node state is backwards compatible: newer releases are guaranteed to load state written by older releases. Downgrades are not supported, so state written by a newer release may not load with an older release.

## Language Support
LDK Node itself is written in [Rust][rust] and may therefore be natively added as a library dependency to any `std` Rust program. However, beyond its Rust API it also offers language bindings for [Swift][swift], [Kotlin][kotlin], and [Python][python] based on the [UniFFI](https://github.com/mozilla/uniffi-rs/).

## MSRV
The Minimum Supported Rust Version (MSRV) is currently 1.85.0.

[api_docs]: https://docs.rs/ldk-node/*/ldk_node/
[api_docs_node]: https://docs.rs/ldk-node/*/ldk_node/struct.Node.html
[api_docs_builder]: https://docs.rs/ldk-node/*/ldk_node/struct.Builder.html
[rust_crate]: https://crates.io/
[ldk]: https://lightningdevkit.org/
[bdk]: https://bitcoindevkit.org/
[electrum]: https://github.com/spesmilo/electrum-protocol
[esplora]: https://github.com/Blockstream/esplora
[sqlite]: https://sqlite.org/
[postgresql]: https://www.postgresql.org/
[rust]: https://www.rust-lang.org/
[swift]: https://www.swift.org/
[kotlin]: https://kotlinlang.org/
[python]: https://www.python.org/
