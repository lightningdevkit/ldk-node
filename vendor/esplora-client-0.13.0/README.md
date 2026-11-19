# rust-esplora-client

<p>
    <a href="https://crates.io/crates/esplora-client"><img src="https://img.shields.io/crates/v/esplora-client.svg"/></a>
    <a href="https://docs.rs/esplora-client"><img src="https://img.shields.io/badge/docs.rs-esplora--client-blue"/></a>
    <a href="https://blog.rust-lang.org/2023/12/28/Rust-1.75.0.html"><img src="https://img.shields.io/badge/MSRV-1.75.0%2B-orange.svg"/></a>
    <a href="https://github.com/bitcoindevkit/rust-esplora-client/blob/master/LICENSE.md"><img src="https://img.shields.io/badge/License-MIT%2FApache--2.0-red.svg"/></a>
    <a href="https://github.com/bitcoindevkit/rust-esplora-client/actions/workflows/rust.yml"><img src="https://github.com/bitcoindevkit/rust-esplora-client/actions/workflows/rust.yml/badge.svg"></a>
</p>

Asynchronous and blocking clients and types for interacting with Esplora servers over HTTP.

## Developing

This project uses [`just`](https://github.com/casey/just) for command running, and
[`cargo-rbmt`](https://github.com/rust-bitcoin/rust-bitcoin-maintainer-tools/tree/master/cargo-rbmt)
to manage everything related to `cargo`, such as formatting, linting, testing and CI. To install them, run:

```console
~$ cargo install just

~$ cargo install cargo-rbmt
```

A `justfile` is provided for convenience. Run `just` to see available commands:

```console
~$ just
> rust-esplora-client
> Bitcoin Esplora API client library

Available recipes:
    audit      # Run `cargo audit` [alias: a]
    build      # Build `rust-esplora-client` [alias: b]
    check      # Check code formatting, compilation, and linting [alias: c]
    check-sigs # Checks whether all commits in this branch are signed [alias: cs]
    doc        # Generate documentation [alias: d]
    doc-open   # Generate and open documentation [alias: do]
    fmt        # Format code [alias: f]
    lock       # Regenerate Cargo-recent.lock and Cargo-minimal.lock [alias: l]
    pre-push   # Run pre-push checks [alias: p]
    test       # Run tests [alias: t]
    zizmor     # Run Zizmor Static Analysis [alias: z]
```

## Minimum Supported Rust Version

This library should compile with any combination of features on Rust 1.75.0.

To build with the MSRV toolchain, copy `Cargo-minimal.lock` to `Cargo.lock`.

## License

Licensed under either of

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
