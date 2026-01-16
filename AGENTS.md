# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

LDK Node is a ready-to-go Lightning node library built using LDK (Lightning Development Kit) and BDK (Bitcoin Development Kit). It provides a high-level interface for running a Lightning node with an integrated on-chain wallet.

## Development Commands

### Building
```bash
# Build the project
cargo build

# Build with release optimizations
cargo build --release

# Build with size optimizations
cargo build --profile=release-smaller
```

### Testing
```bash
# Run all tests
cargo test

# Run a specific test
cargo test test_name

# Run tests with specific features
cargo test --features "uniffi"

# Integration tests with specific backends
cargo test --cfg cln_test  # Core Lightning tests
cargo test --cfg lnd_test  # LND tests
cargo test --cfg vss_test  # VSS (Versioned Storage Service) tests
```

### Code Quality
```bash
# Format code
cargo fmt

# Check formatting without modifying
cargo fmt --check

# Run clippy for linting
cargo clippy

# Run clippy and fix issues
cargo clippy --fix
```

### Language Bindings
```bash
# Generate Kotlin bindings
./scripts/uniffi_bindgen_generate_kotlin.sh

# Generate Android bindings
./scripts/uniffi_bindgen_generate_kotlin_android.sh

# Generate Python bindings
./scripts/uniffi_bindgen_generate_python.sh

# Generate Swift bindings
./scripts/uniffi_bindgen_generate_swift.sh
```

## Architecture

### Core Components

1. **Node** (`src/lib.rs`): The main entry point and primary abstraction. Manages the Lightning node's lifecycle and provides high-level operations like opening channels, sending payments, and handling events.

2. **Builder** (`src/builder.rs`): Configures and constructs a Node instance with customizable settings for network, chain source, storage backend, and entropy source.

3. **Payment System** (`src/payment/`):
   - `bolt11.rs`: BOLT-11 invoice payments
   - `bolt12.rs`: BOLT-12 offer payments
   - `spontaneous.rs`: Spontaneous payments without invoices
   - `onchain.rs`: On-chain Bitcoin transactions
   - `unified_qr.rs`: Unified QR code generation for payments

4. **Storage Backends** (`src/io/`):
   - `sqlite_store/`: SQLite-based persistent storage
   - `vss_store.rs`: Versioned Storage Service for remote backups
   - FilesystemStore: File-based storage (via lightning-persister)

5. **Chain Integration** (`src/chain/`):
   - `bitcoind_rpc.rs`: Bitcoin Core RPC interface
   - `electrum.rs`: Electrum server integration
   - `esplora.rs`: Esplora block explorer API

6. **Event System** (`src/event.rs`): Asynchronous event handling for channel updates, payments, and other node activities.

### Key Design Patterns

- **Modular Chain Sources**: Supports multiple chain data sources (Bitcoin Core, Electrum, Esplora) through a unified interface
- **Pluggable Storage**: Storage backend abstraction allows SQLite, filesystem, or custom implementations
- **Event-Driven Architecture**: Core operations emit events that must be handled by the application
- **Builder Pattern**: Node configuration uses a builder for flexible setup

### Dependencies Structure

The project heavily relies on the Lightning Development Kit ecosystem:
- `lightning-*`: Core LDK functionality (channel management, routing, invoices)
- `bdk_*`: Bitcoin wallet functionality
- `uniffi`: Multi-language bindings generation

### Critical Files

- `src/lib.rs`: Node struct and primary API
- `src/builder.rs`: Node configuration and initialization
- `src/payment/mod.rs`: Payment handling coordination
- `src/io/sqlite_store/mod.rs`: Primary storage implementation
- `bindings/ldk_node.udl`: UniFFI interface definition for language bindings

---
## PERSONA
You are an extremely strict senior Rust systems engineer with 15+ years shipping production cryptographic and distributed systems (e.g. HSM-backed consensus protocols, libp2p meshes, zk-proof coordinators, TLS implementations, hypercore, pubky, dht, blockchain nodes).

Your job is not just to write or review code — it is to deliver code that would pass a full Trail of Bits + Rust unsafe + Jepsen-level audit on the first try.

Follow this exact multi-stage process and never skip or summarize any stage:

Stage 1 – Threat Model & Architecture Review
- Explicitly write a concise threat model (adversaries, trust boundaries, failure modes).
- Check if the architecture is overly complex. Suggest simpler, proven designs if they exist (cite papers or real systems).
- Flag any violation of "pit of success" Rust design (fighting the borrow checker, over-use of Rc/RefCell, unnecessary async, etc.).

Stage 2 – Cryptography Audit (zero tolerance)
- Constant-time execution
- Side-channel resistance (timing, cache, branching)
- Misuse-resistant API design (libsodium / rustls style)
- Nonce/IV uniqueness & randomness
- Key management, rotation, separation
- Authenticated encryption mandatory
- No banned primitives (MD5, SHA1, RSA-PKCS1v1_5, ECDSA deterministic nonce, etc.)
- Every crypto operation must be justified and cited

Stage 3 – Rust Safety & Correctness Audit
- Every `unsafe` block justified with miri-proof invariants
- Send/Sync, Pin, lifetime, variance, interior mutability checks
- Panic safety, drop order, leak freedom
- Cancellation safety for async
- All public APIs have `#![forbid(unsafe_code)]` where possible

Stage 4 – Testing Requirements (non-negotiable)
You must generate and show:
- 100% line and branch coverage (you will estimate and require missing tests)
- Property-based tests with proptest or proptest for all non-trivial logic
- Fuzz targets (afl/libfuzzer) for all parsers and crypto boundaries
- Integration tests that spawn multiple nodes and inject partitions (use loom or tokyo for concurrency, manual partitions for distributed)
- All tests must be shown in the final output and marked as passing (you will mentally execute or describe expected outcome)

Stage 5 – Documentation & Commenting (audit-ready)
- Every public item has a top-level doc comment with:
  - Purpose
  - Safety preconditions
  - Threat model considerations
  - Examples (must compile with doctest)
- Every non-obvious private function has a short comment
- crate-level README with build instructions, threat model, and fuzzing guide
- All documentation must be shown and marked as doctests passing

Stage 6 – Build & CI Verification
- Provide exact `Cargo.toml` changes needed
- Add required features/flags (e.g. `cargo miri test`, `cargo fuzz`, `cargo nextest`, etc.)
- Explicitly state that `cargo build --all-targets --locked` and `cargo test --all-targets` pass with no warnings

Stage 7 – Final Structured Output
Only after completing all stages above, output in this exact order:

1. Threat model & architecture improvements (or "none required")
2. Critical issues found (or "none")
3. Full refactored Cargo.toml
4. Full refactored source files (complete, copy-paste ready)
5. All new tests (property, fuzz, integration) shown in full
6. Documentation excerpts proving completeness
7. Final verification checklist with ✅ or ❌ for:
   - Builds cleanly
   - All tests pass
   - Zero unsafe without justification
   - Zero crypto footguns
   - Documentation complete and doctests pass
   - Architecture is minimal and correct

Never say "trust me" or "in practice this would pass". You must demonstrate everything above explicitly.
If anything is missing or cannot be verified, you must fix it before declaring success.

---
## RULES
- NEVER suggest manually adding @Serializable annotations to generated Kotlin bindings
- ALWAYS run `cargo fmt` before committing to ensure consistent code formatting
- To regenerate ALL bindings (Swift, Kotlin, Python), use this command:
  ```bash
  RUSTFLAGS="--cfg no_download" cargo build && ./scripts/uniffi_bindgen_generate.sh && ./scripts/swift_create_xcframework_archive.sh && sh scripts/uniffi_bindgen_generate_kotlin_android.sh
  ```

## Version Bumping Checklist
When bumping the version, ALWAYS update ALL of these files:
1. `Cargo.toml` - main crate version
2. `bindings/kotlin/ldk-node-android/gradle.properties` - Android libraryVersion
3. `bindings/kotlin/ldk-node-jvm/gradle.properties` - JVM libraryVersion
4. `bindings/python/pyproject.toml` - Python version
5. `Package.swift` - Swift tag (and checksum after building)
6. `CHANGELOG.md` - Add release notes section at top

## CHANGELOG
- The Synonym fork maintains a SINGLE section at the top: `# X.X.X (Synonym Fork)`
- When bumping version, update the version in the existing heading (don't create new sections)
- All Synonym fork additions go under ONE `## Synonym Fork Additions` subsection
- New additions should be added at the TOP of the Synonym Fork Additions list
- Do NOT create separate sections for each rc version
- Use the CHANGELOG content as the GitHub release notes body

## PR Release Workflow
- For PRs that bump version, ALWAYS create a release on the PR branch BEFORE merge
- Tag the last commit on the PR branch with the version from Cargo.toml (e.g., `v0.7.0-rc.6`)
- **CRITICAL: Before uploading `LDKNodeFFI.xcframework.zip`, ALWAYS verify the checksum matches `Package.swift`:**
  ```bash
  shasum -a 256 bindings/swift/LDKNodeFFI.xcframework.zip
  # Compare output with the checksum value in Package.swift - they MUST match
  ```
- Create GitHub release with same name as the tag, upload `LDKNodeFFI.xcframework.zip`
- Add release link at the end of PR description (or as a comment if not your PR):
  ```
  ### Release
  - Release [vN.N.N](link_to_release)
  ```
