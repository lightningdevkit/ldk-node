# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Rules

- Always ensure tests pass before committing.
- Run `cargo fmt --all` after every code change
- Never add new dependencies unless explicitly requested
- Please always disclose the use of any AI tools in commit messages and PR descriptions
- When adding new `.rs` files, please ensure to always add the licensing header as found in all other files.

## Architecture Overview

LDK-Node is a self-custodial Lightning Network node library built on top of **LDK** (Lightning Development Kit) for Lightning functionality and **BDK** (Bitcoin Development Kit) for on-chain wallet operations. It provides a simple, ready-to-go interface for building Lightning applications with language bindings for Swift, Kotlin, and Python via UniFFI.

### Core Components

| Component | Location | Responsibility |
|-----------|----------|----------------|
| `Node` | `src/lib.rs` | Central abstraction containing all subsystems; entry point for API |
| `Builder` | `src/builder.rs` | Fluent configuration interface for constructing `Node` instances |
| `Wallet` | `src/wallet/` | BDK-based on-chain wallet with SQLite persistence |
| `ChainSource` | `src/chain/` | Chain data abstraction (Esplora, Electrum, Bitcoin Core) |
| `EventHandler` | `src/event.rs` | Translates LDK events to user-facing `Node` events |
| `PaymentStore` | `src/payment/store.rs` | Persistent payment tracking with status and metadata |

### Module Organization

| Module | Purpose |
|--------|---------|
| `payment/` | Payment processing (BOLT11, BOLT12, on-chain, spontaneous, unified) |
| `wallet/` | On-chain wallet abstraction, serialization, persistence |
| `chain/` | Chain data sources, wallet syncing, transaction broadcasting |
| `io/` | Persistence layer (`SQLiteStore`, `VssStore`, KV-store utilities) |
| `liquidity.rs` | Liquidity provider integration (LSPS1/LSPS2) |
| `connection.rs` | Peer connection management and reconnection logic |
| `gossip.rs` | Gossip data source management (RGS, P2P) |
| `graph.rs` | Network graph querying and channel/node information |
| `types.rs` | Type aliases for LDK components (`ChannelManager`, `PeerManager`, etc.) |
| `ffi/` | UniFFI bindings for cross-language support |

### Key Design Patterns

- **Arc-based Shared Ownership**: Extensive use of `Arc` for thread-safe shared components enabling background task spawning
- **Event-Driven Architecture**: Events flow from LDK → `EventHandler` → `EventQueue` → User application
- **Trait-based Abstraction**: `KVStore`/`KVStoreSync` for storage, `ChainSource` for chain backends, `StorableObject` for persistence
- **Builder Pattern**: Fluent configuration with sensible defaults and validation during build phase
- **Background Tasks**: Multiple categories (wallet sync, gossip updates, peer reconnection, fee updates, event processing)

### Lifecycle

**Startup (`node.start()`)**: Acquires lock → starts chain source → updates fee rates → spawns background sync tasks → sets up gossip/listeners/peer reconnection → starts event processor → marks running

**Shutdown (`node.stop()`)**: Acquires lock → signals stop → aborts cancellable tasks → waits for background tasks → disconnects peers → persists final state

### Type Aliases (from `types.rs`)

Key LDK type aliases used throughout the codebase:
- `ChannelManager` - LDK channel management
- `ChainMonitor` - LDK chain monitoring
- `PeerManager` - LDK peer connections
- `OnionMessenger` - LDK onion messaging
- `Router` - LDK pathfinding (`DefaultRouter`)
- `Scorer` - Combined probabilistic + external scoring
- `Graph` - `NetworkGraph`
- `Sweeper` - `OutputSweeper`
