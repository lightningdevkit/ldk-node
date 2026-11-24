# Downstream rusqlite compatibility

This separate Cargo workspace checks that an application can select a stock
rusqlite release while using LDK Node's default features. The test writes through
LDK Node, reopens the store, and reads the database with the application's rusqlite.
Neither rusqlite nor libsqlite3-sys is patched.

LDK Node itself accesses SQLite through sqlite-thin, which accepts any
libsqlite3-sys release from 0.28 onwards. As a build can only hold a single
libsqlite3-sys, the application's rusqlite determines which one is used.

The root crate's development dependencies on LDK Node 0.6.2 and 0.7.0 require
rusqlite 0.31 unconditionally. They constrain root builds even when individual
tests are filtered out, but do not participate in downstream resolution. Keep
this fixture separate so CI actually tests both ends of the tested range.

From the repository root, run:

```sh
export CARGO_TARGET_DIR=$(mktemp -d /tmp/cargo-target-ldk-node-rusqlite-XXXXXX)
export CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback
cargo +1.85.0 test --manifest-path tests/rusqlite-compat/Cargo.toml
```

The fixture defaults to the maximum tested version. To test another release,
change its direct requirement with
`cargo +1.85.0 add --manifest-path tests/rusqlite-compat/Cargo.toml rusqlite@=0.31.0`
and rerun the test. Restore the maximum requirement afterwards. The resolver
setting selects MSRV-compatible versions of unrelated transitive dependencies.

When changing the tested range, update both endpoints in
`.github/workflows/rust.yml` and test each newly included release locally. The
upper endpoint stays at 0.39.0, the newest release line of which every patch
release compiles on Rust 1.85: 0.40.0 and 0.40.1 use `cfg_select!`, and
releases from 0.41 onwards declare a higher `rust-version`.
