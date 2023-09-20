#!/bin/bash
# cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.2.3+v0.23.0
BINDINGS_DIR="./bindings/csharp"

mkdir -p $BINDINGS_DIR

cargo build --release --features uniffi || exit 1
uniffi-bindgen-cs bindings/ldk_node.udl -o "$BINDINGS_DIR" || exit 1
cp ./target/release/libldk_node.{a,so} "$BINDINGS_DIR" || exit 1
