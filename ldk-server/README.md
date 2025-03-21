# LDK Server

**LDK Server** is a fully-functional Lightning node in daemon form, built on top of
[LDK Node](https://github.com/lightningdevkit/ldk-node), which itself provides a powerful abstraction over the
[Lightning Development Kit (LDK)](https://github.com/lightningdevkit/rust-lightning) and uses a built-in
[Bitcoin Development Kit (BDK)](https://bitcoindevkit.org/) wallet.

The primary goal of LDK Server is to provide an efficient, stable, and API-first solution for deploying and managing
a Lightning Network node. With its streamlined setup, LDK Server enables users to easily set up, configure, and run
a Lightning node while exposing a robust, language-agnostic API via [Protocol Buffers (Protobuf)](https://protobuf.dev/).

### Features

- **Out-of-the-Box Lightning Node**:
    - Deploy a Lightning Network node with minimal configuration, no coding required.

- **API-First Design**:
    - Exposes a well-defined API using Protobuf, allowing seamless integration with HTTP-clients or applications.

- **Powered by LDK**:
    - Built on top of LDK-Node, leveraging the modular, reliable, and high-performance architecture of LDK.

- **Effortless Integration**:
    - Ideal for embedding Lightning functionality into payment processors, self-hosted nodes, custodial wallets, or other Lightning-enabled
      applications.

### Project Status

🚧 **Work in Progress**:
- **APIs Under Development**: Expect breaking changes as the project evolves.
- **Potential Bugs and Inconsistencies**: While progress is being made toward stability, unexpected behavior may occur.
- **Improved Logging and Error Handling Coming Soon**: Current error handling is rudimentary (specially for CLI), and usability improvements are actively being worked on.
- **Pending Testing**: Not tested, hence don't use it for production!

We welcome your feedback and contributions to help shape the future of LDK Server!


### Configuration
Refer `./ldk-server/ldk-server-config.toml` to see available configuration options.

### Building
```
git clone https://github.com/lightningdevkit/ldk-server.git
cargo build
```

### Running
```
cargo run --bin ldk-server ./ldk-server/ldk-server.config
```

Interact with the node using CLI:
```
./target/debug/ldk-server-cli -b localhost:3002 onchain-receive # To generate onchain-receive address.
./target/debug/ldk-server-cli -b localhost:3002 help # To print help/available commands.
```
