# 0.1 - Jun 22, 2023
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

