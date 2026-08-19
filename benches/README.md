# Benchmarks

This directory holds [Criterion](https://docs.rs/criterion) benchmarks for LDK Node. There are
three benchmark targets:

| Target | What it measures |
|--------|------------------|
| `database` | Low-level `KVStore` operations (single read/write/remove, batched & concurrent writes, paginated listing, payment reload) across the filesystem, SQLite and PostgreSQL backends. |
| `operations` | Higher-level node operations: payment forwarding, channel opens, and node startup time seeded with varying channel/payment history. |
| `payments` | End-to-end payment throughput between two nodes across store backends. |

The `operations` and `payments` benchmarks spin up real `bitcoind`/`electrs` instances and full
nodes, so they take a while to run. The `database` benchmark only exercises the storage layer and
is comparatively quick.

## Prerequisites

- `bitcoind` and `electrs` binaries for the `operations` and `payments` benchmarks. Point the
  `BITCOIND_EXE` and `ELECTRS_EXE` environment variables at them (see
  `scripts/download_bitcoind_electrs.sh` for a convenient way to fetch them).
- The `bench` feature is required for the `database` benchmark.
- The `postgres` feature plus a reachable PostgreSQL server (set `TEST_POSTGRES_URL`) are required
  to include the PostgreSQL backend. Without it, the filesystem and SQLite backends still run.

## Running

Run everything for real measurements:

```sh
RUSTFLAGS="--cfg tokio_unstable" cargo bench --benches --features bench
```

`--cfg tokio_unstable` is optional; it enables the tokio eager driver handoff used by the benchmark
runtimes. Without it the benchmarks still run.

Run a single target:

```sh
cargo bench --bench database --features bench
cargo bench --bench operations
cargo bench --bench payments
```

Filter to specific cases (Criterion takes a substring filter; the `operations` target also uses it
to skip expensive setup for cases that don't match):

```sh
cargo bench --bench operations -- channel_open
cargo bench --bench database --features bench -- sqlite
```

### PostgreSQL backend

```sh
export TEST_POSTGRES_URL="host=localhost user=postgres password=postgres"
cargo bench --benches --features "bench postgres"
```

Benchmark fixtures create their tables in the `ldk_db` database (the default used when the
connection string omits a `dbname`) and drop them on teardown.

## CI

The `benchmarks` workflow smoke-runs the benchmarks with `cargo test --benches --features bench` (a
single iteration each) to keep them compiling and working rather than collecting full measurements.
It does not require PostgreSQL; PostgreSQL coverage is opt-in for local runs by enabling the
`postgres` feature and setting `TEST_POSTGRES_URL`. The seeded `startup` scenarios in `operations`
detect CI via the `CI` environment variable and only run the smallest scenario there; the full set
runs locally.
