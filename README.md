# LDK Node
A ready-to-go Lightning node library built using [LDK](https://lightningdevkit.org/) and [BDK](https://bitcoindevkit.org/).

LDK Node is a non-custodial Lightning node in library form. Its central goal is to provide a small, simple, and straightforward interface that enables users to easily setup and run a Lightning node with an integrated on-chain wallet. While minimalism is at its core, LDK Node aims to be sufficiently modular and configurable to be useful for a variety of use cases. 

## Getting Started

The main interface of the library is the `Node`, which can be retrieved by setting up and configuring a `Builder` to the user's liking and calling `build()`. `Node` can then be controlled via commands such as `start`,`stop`,`connect_open_channel`, `send_payment`, etc.:

```rust
use ldk_node::Builder;
use ldk_node::lightning_invoice::Invoice;
use std::str::FromStr;

fn main() {
	let node = Builder::new()
		.set_network("testnet")
		.set_esplora_server_url("https://blockstream.info/testnet/api".to_string())
		.build();

	node.start().unwrap();

	let _funding_address = node.new_funding_address();

	// .. fund address ..
	
	node.sync_wallets().unwrap();

	node.connect_open_channel("NODE_ID@PEER_ADDR:PORT", 10000, false).unwrap();

	let invoice = Invoice::from_str("INVOICE_STR").unwrap();
	node.send_payment(invoice).unwrap();

	node.stop().unwrap();
}
```

## Modularity

LDK Node currently comes with a decidedly opionated set of design choices:

- On-chain data is handled by the integrated BDK wallet.
- Chain data is accessed via Esplora (support for Electrum and `bitcoind` RPC will follow)
- Wallet and channel state is persisted to file system (support for SQLite will follow)
- Gossip data is sourced via Lightnings peer-to-peer network (support for [Rapid Gossip Sync](https://docs.rs/lightning-rapid-gossip-sync/*/lightning_rapid_gossip_sync/) will follow)
- Entropy for the Lightning and on-chain wallets may be generated and persisted for or provided by the user (support for [BIP39](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki) mnemonics will follow)


## Language Support

LDK Node is written in [Rust](https://www.rust-lang.org/) and may therefore be natively included in any `std` Rust program. Beyond its Rust API it also offers language bindings for Swift, Kotlin, and Python based on [UniFFI](https://github.com/mozilla/uniffi-rs/).
