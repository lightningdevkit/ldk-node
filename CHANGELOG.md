# 0.5.0 - Apr. 29, 2025
Besides numerous API improvements and bugfixes this fifth minor release notably adds support for sourcing chain and fee rate data from an Electrum backend, requesting channels via the [bLIP-51 / LSPS1](https://github.com/lightning/blips/blob/master/blip-0051.md) protocol, as well as experimental support for operating as a [bLIP-52 / LSPS2](https://github.com/lightning/blips/blob/master/blip-0052.md) service.

## Feature and API updates
- The `PaymentSuccessful` event now exposes a `payment_preimage` field (#392).
- The node now emits `PaymentForwarded` events for forwarded payments (#404).
- The ability to send custom TLVs as part of spontaneous payments has been added (#411).
- The ability to override the used fee rates for on-chain sending has been added (#434).
- The ability to set a description hash when creating a BOLT11 invoice has been added (#438).
- The ability to export pathfinding scores has been added (#458).
- The ability to request inbound channels from an LSP via the bLIP-51 / LSPS1 protocol has been added (#418).
- The `ChannelDetails` returned by `Node::list_channels` now exposes fields for the channel's SCIDs (#444).
- Lightning peer-to-peer gossip data is now being verified when syncing from a Bitcoin Core RPC backend (#428).
- The logging sub-system was reworked to allow logging to backends using the Rust [`log`](https://crates.io/crates/log) facade, as well as via a custom logger trait (#407, #450, #454).
- On-chain transactions are now added to the internal payment store and exposed via `Node::list_payments` (#432).
- Inbound announced channels are now rejected if not all requirements for operating as a forwarding node (set listening addresses and node alias) have been met (#467).
- Initial support for operating as an bLIP-52 / LSPS2 service has been added (#420).
    - **Note**: bLIP-52 / LSPS2 support is considered 'alpha'/'experimental' and should *not* yet be used in production.
- The `Builder::set_entropy_seed_bytes` method now takes an array rather than a `Vec` (#493).
- The builder will now return a `NetworkMismatch` error in case of network switching (#485).
- The `Bolt11Jit` payment variant now exposes a field telling how much fee the LSP withheld (#497).
- The ability to disable syncing Lightning and on-chain wallets in the background has been added. If it is disabled, the user is responsible for running `Node::sync_wallets` manually (#508).
- The ability to configure the node's announcement addresses independently from the listening addresses has been added (#484).
- The ability to choose whether to honor the Anchor reserves when calling `send_all_to_address` has been added (#345).
- The ability to sync the node via an Electrum backend has been added (#486).

## Bug Fixes and Improvements
- When syncing from Bitcoin Core RPC, syncing mempool entries has been made more efficient (#410, #465).
- We now ensure the our configured fallback rates are used when the configured chain source would return huge bogus values during fee estimation (#430).
- We now re-enabled trying to bump Anchor channel transactions for trusted counterparties in the `ContentiousClaimable` case to reduce the risk of losing funds in certain edge cases (#461).
- An issue that would potentially have us panic on retrying the chain listening initialization when syncing from Bitcoin Core RPC has been fixed (#471).
- The `Node::remove_payment` now also removes the respective entry from the in-memory state, not only from the persisted payment store (#514).

## Compatibility Notes
- The filesystem logger was simplified and its default path changed to `ldk_node.log` in the configured storage directory (#394).
- The BDK dependency has been bumped to `bdk_wallet` v1.0 (#426).
- The LDK dependency has been bumped to `lightning` v0.1 (#426).
- The `rusqlite` dependency has been bumped to v0.31 (#403).
- The minimum supported Rust version (MSRV) has been bumped to v1.75 (#429).

In total, this release features 53 files changed, 6147 insertions, 1193 deletions, in 191 commits from 14 authors in alphabetical order:

- alexanderwiederin
- Andrei
- Artur Gontijo
- Ayla Greystone
- Elias Rohrer
- elnosh
- Enigbe Ochekliye
- Evan Feenstra
- G8XSU
- Joost Jager
- maan2003
- moisesPompilio
- Rob N
- Vincenzo Palazzo

# 0.4.3 - Jan. 23, 2025

This patch release fixes the broken Rust build resulting from `cargo` treating the recent v0.1.0 release of `lightning-liquidity` as API-compatible with the previous v0.1.0-alpha.6 release (even though it's not).

In total, this release features 1 files changed, 1 insertions, 1 deletions in 1 commits from 1 author, in alphabetical order:

- Elias Rohrer

# 0.4.2 - Oct 28, 2024

This patch release fixes an issue that prohibited the node from using available confirmed on-chain funds to spend/bump Anchor outputs (#387).

In total, this release features 1 files changed, 40 insertions, 4 deletions in 3 commits from 3 authors, in alphabetical order:

- Fuyin
- Elias Rohrer


# 0.4.1 - Oct 18, 2024

This patch release fixes a wallet syncing issue where full syncs were used instead of incremental syncs, and vice versa (#383).

In total, this release features 3 files changed, 13 insertions, 9 deletions in 6 commits from 3 authors, in alphabetical order:

- Jeffrey Czyz
- Elias Rohrer
- Tommy Volk

# 0.4.0 - Oct 17, 2024

Besides numerous API improvements and bugfixes this fourth minor release notably adds support for sourcing chain and fee rate data from a Bitcoin Core RPC backend, as well as experimental support for the [VSS] remote storage backend.

## Feature and API updates
- Support for multiple chain sources has been added. To this end, Esplora-specific configuration options can now be given via `EsploraSyncConfig` to `Builder::set_chain_source_esplora`. Furthermore, all configuration objects (including the main `Config`) is now exposed via the `config` sub-module (#365).
- Support for sourcing chain and fee estimation data from a Bitcoin Core RPC backed has been added (#370).
- Initial experimental support for an encrypted [VSS] remote storage backend has been added (#369, #376, #378).
    - **Caution**: VSS support is in **alpha** and is considered experimental. Using VSS (or any remote persistence) may cause LDK to panic if persistence failures are unrecoverable, i.e., if they remain unresolved after internal retries are exhausted.
- Support for setting the `NodeAlias` in public node announcements as been added. We now ensure that announced channels can only be opened and accepted when the required configuration options to operate as a public forwarding node are set (listening addresses and node alias). As part of this `Node::connect_open_channel` was split into `open_channel` and `open_announced_channel` API methods. (#330, #366).
- The `Node` can now be started via a new `Node::start_with_runtime` call that allows to reuse an outer `tokio` runtime context, avoiding runtime stacking when run in `async` environments (#319).
- Support for generating and paying unified QR codes has been added (#302).
- Support for `quantity` and `payer_note` fields when sending or receiving BOLT12 payments has been added (#327).
- Support for setting additional parameters when sending BOLT11 payments has been added (#336, #351).

## Bug Fixes
- The `ChannelConfig` object has been refactored, now allowing to query the currently applied `MaxDustHTLCExposure` limit (#350).
- A bug potentially leading to panicking on shutdown when stacking `tokio` runtime contexts has been fixed (#373).
- We now no longer panic when hitting a persistence failure during event handling. Instead, events will be replayed until successful (#374).
,
## Compatibility Notes
- The LDK dependency has been updated to version 0.0.125 (#358, #375).
- The BDK dependency has been updated to version 1.0-beta.4 (#358).
    - Going forward, the BDK state will be persisted in the configured `KVStore` backend.
    - **Note**: The old descriptor state will *not* be automatically migrated on upgrade, potentially leading to address reuse. Privacy-concious users might want to manually advance the descriptor by requesting new addresses until it reaches the previously observed height.
    - After the node as been successfully upgraded users may safely delete `bdk_wallet_*.sqlite` from the storage path.
- The `rust-bitcoin` dependency has been updated to version 0.32.2 (#358).
- The UniFFI dependency has been updated to version 0.27.3 (#379).
- The `bip21` dependency has been updated to version 0.5 (#358).
- The `rust-esplora-client` has been updated to version 0.9 (#358).

In total, this release features 55 files changed, 6134 insertions, 2184 deletions in 166 commits from 6 authors, in alphabetical order:

- G8XSU
- Ian Slane
- jbesraa
- Elias Rohrer
- elnosh
- Enigbe Ochekliye

[VSS]: https://github.com/lightningdevkit/vss-server/blob/main/README.md

# 0.3.0 - June 21, 2024

This third minor release notably adds support for BOLT12 payments, Anchor
channels, and sourcing inbound liquidity via LSPS2 just-in-time channels.

## Feature and API updates
- Support for creating and paying BOLT12 offers and refunds has been added (#265).
- Support for Anchor channels has been added (#141).
- Support for sourcing inbound liquidity via LSPS2 just-in-time (JIT) channels has been added (#223).
- The node's local view of the network graph can now be accessed via interface methods (#293).
- A new `next_event_async` method was added that allows polling the event queue asynchronously (#224).
- A `default_config` method was introduced that allows to retrieve sane default values, also in bindings (#242).
- The `PaymentFailed` and `ChannelClosed` events now include `reason` fields (#260).
- All available balances outside of channel balances are now exposed via a unified `list_balances` interface method (#250).
- The maximum in-flight HTLC value has been bumped to 100% of the channel capacity for private outbound channels (#303) and, if JIT channel support is enabled, for inbound channels (#262).
- The fee paid is now exposed via the `PaymentSuccessful` event (#271).
- A `status` method has been added allowing to retrieve information about the `Node`'s status (#272).
- `Node` no longer takes a `KVStore` type parameter, allowing to use the filesystem storage backend in bindings (#244).
- The payment APIs have been restructured to use per-type (`bolt11`, `onchain`, `bolt12`, ..) payment handlers which can be accessed via corresponding `Node::{type}_payment` methods (#270).
- Fully resolved channel monitors are now eventually moved to an archive location (#307).
- The ability to register and claim from custom payment hashes generated outside of LDK Node has been added (#308).

## Bug Fixes
- Node announcements are now correctly only broadcast if we have any public, sufficiently confirmed channels (#248, #314).
- Falling back to default fee values is now disallowed on mainnet, ensuring we won't startup without a successful fee cache update (#249).
- Persisted peers are now correctly reconnected after startup (#265).
- Concurrent connection attempts to the same peer are no longer overriding each other (#266).
- Several steps have been taken to reduce the risk of blocking node operation on wallet syncing in the face of unresponsive Esplora services (#281).

## Compatibility Notes
- LDK has been updated to version 0.0.123 (#291).

In total, this release features 54 files changed, 7282 insertions, 2410 deletions in 165 commits from 3 authors, in alphabetical order:

- Elias Rohrer
- jbesraa
- Srikanth Iyengar

# 0.2.2 - May 21, 2024

This is a bugfix release that reestablishes compatibility of Swift packages
with Xcode 15.3 and later.

## Bug Fixes

- Swift bindings can now be built using Xcode 15.3 and later again (#294)

In total, this release features 5 files changed, 66 insertions, 2 deletions
deletions in 2 commits from 1 author, in alphabetical order:

- Elias Rohrer

# 0.2.1 - Jan 26, 2024

This is a bugfix release bumping the used LDK and BDK dependencies to the
latest stable versions.

## Bug Fixes
- Swift bindings now can be built on macOS again.

## Compatibility Notes
- LDK has been updated to version 0.0.121 (#214, #229)
- BDK has been updated to version 0.29.0 (#229)

In total, this release features 30 files changed, 1195 insertions, 1238
deletions in 26 commits from 3 authors, in alphabetical order:

- Elias Rohrer
- GoodDaisy
- Gursharan Singh

# 0.2.0 - Dec 13, 2023

## Feature and API updates
- The capability to send pre-flight probes has been added (#147).
- Pre-flight probes will skip outbound channels based on the liquidity available (#156).
- Additional fields are now exposed via `ChannelDetails` (#165).
- The location of the `logs` directory is now customizable (#129).
- Listening on multiple socket addresses is now supported (#187).
- If available, peer information is now persisted for inbound channels (#170).
- Transaction broadcasting and fee estimation have been reworked and made more robust (#205).
- A module persisting, sweeping, and rebroadcasting output spends has been added (#152).

## Bug Fixes
- No errors are logged anymore when we choose to omit spending of `StaticOutput`s (#137).
- An inconsistent state of the log file symlink no longer results in an error during startup (#153).

## Compatibility Notes
- Our currently supported minimum Rust version (MSRV) is 1.63.0.
- The Rust crate edition has been bumped to 2021.
- Building on Windows is now supported (#160).
- LDK has been updated to version 0.0.118 (#105, #151, #175).

In total, this release features 57 files changed, 7369 insertions, 1738 deletions in 132 commits from 9 authors, in alphabetical order:

- Austin Kelsay
- alexanderwiederin
- Elias Rohrer
- Galder Zamarreño
- Gursharan Singh
- jbesraa
- Justin Moeller
- Max Fang
- Orbital

# 0.1.0 - Jun 22, 2023
This is the first non-experimental release of LDK Node.

- Log files are now split based on the start date of the node (#116).
- Support for allowing inbound trusted 0conf channels has been added (#69).
- Non-permanently connected peers are now included in `Node::list_peers` (#95).
- A utility method for generating a BIP39 mnemonic is now exposed in bindings (#113).
- A `ChannelConfig` may now be specified on channel open or updated afterwards (#122).
- Logging has been improved and `Builder` now returns an error rather than panicking if encountering a build failure (#119).
- In Rust, `Builder::build` now returns a `Node` object rather than wrapping it in an `Arc` (#115).
- A number of `Config` defaults have been updated and are now exposed in bindings (#124).
- The API has been updated to be more aligned between Rust and bindings (#114).

## Compatibility Notes
- Our currently supported minimum Rust version (MSRV) is 1.60.0.
- The superfluous `SendingFailed` payment status has been removed, breaking serialization compatibility with alpha releases (#125).
- The serialization formats of `PaymentDetails` and `Event` types have been updated, ensuring users upgrading from an alpha release fail to start rather than continuing operating with bogus data. Alpha users should wipe their persisted payment metadata (`payments/*`) and event queue (`events`) after the update (#130).

In total, this release includes changes in 52 commits from 2 authors:
- Elias Rohrer
- Richard Ulrich

# 0.1-alpha.1 - Jun 6, 2023
- Generation of Swift, Kotlin (JVM and Android), and Python bindings is now supported through UniFFI (#25).
- Lists of connected peers and channels may now be retrieved in bindings (#56).
- Gossip data may now be sourced from the P2P network, or a Rapid Gossip Sync server (#70).
- Network addresses are now stored and resolved via a `NetAddress` type (#85).
- The `next_event` method has been renamed `wait_next_event` and a new non-blocking method for event queue access has been introduces as `next_event` (#91).
- Node announcements are now regularly broadcasted (#93).
- Duplicate payments are now only avoided if we actually sent them out (#96).
- The `Node` may now be used to sign and verify arbitrary messages (#99).
- A `KVStore` interface is introduced that may be used to implement custom persistence backends (#101).
- An `SqliteStore` persistence backend is added and set as the new default (#100).
- Successful fee rate updates are now mandatory on `Node` startup (#102).
- The wallet sync intervals are now configurable (#102).
- Granularity of logging can now be configured (#108).


In total, this release includes changes in 64 commits from 4 authors:
- Steve Myers
- Elias Rohrer
- Jurvis Tan
- televis

**Note:** This release is still considered experimental, should not be run in
production, and no compatibility guarantees are given until the release of 0.1.

# 0.1-alpha - Apr 27, 2023
This is the first alpha release of LDK Node. It features support for sourcing
chain data via an Esplora server, file system persistence, gossip sourcing via
the Lightning peer-to-peer network, and configurable entropy sources for the
integrated LDK and BDK-based wallets.

**Note:** This release is still considered experimental, should not be run in
production, and no compatibility guarantees are given until the release of 0.1.

