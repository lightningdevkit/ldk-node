#!/bin/bash
BINDINGS_DIR="./bindings/python/src/ldk_node"
UNIFFI_BINDGEN_BIN="cargo run --manifest-path bindings/uniffi-bindgen/Cargo.toml"

cargo build --profile release-smaller --features uniffi || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language python -o "$BINDINGS_DIR" || exit 1
cp ./target/release-smaller/libldk_node.dylib "$BINDINGS_DIR"/libldk_node.dylib || exit 1
