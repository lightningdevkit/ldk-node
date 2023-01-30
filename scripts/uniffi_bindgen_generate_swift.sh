#!/bin/bash
BINDINGS_DIR="./bindings/swift"
UNIFFI_BINDGEN_BIN="cargo run --features=uniffi/cli --bin uniffi-bindgen"

cargo build --release
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language swift -o "$BINDINGS_DIR"

mkdir -p $BINDINGS_DIR

swiftc -module-name ldk_node -emit-library -o "$BINDINGS_DIR"/libldk_node.dylib -emit-module -emit-module-path "$BINDINGS_DIR" -parse-as-library -L ./target/release -lldk_node -Xcc -fmodule-map-file="$BINDINGS_DIR"/ldk_nodeFFI.modulemap "$BINDINGS_DIR"/ldk_node.swift -v
