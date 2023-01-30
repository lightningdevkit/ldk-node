#!/bin/bash
BINDINGS_DIR="./bindings/kotlin"
TARGET_DIR="./target/bindings/kotlin"
PROJECT_DIR="ldk-node-jvm"
PACKAGE_DIR="org/lightningdevkit/ldknode"
UNIFFI_BINDGEN_BIN="cargo run --features=uniffi/cli --bin uniffi-bindgen"

cargo build --target aarch64-apple-darwin
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language kotlin -o "$TARGET_DIR"

mkdir -p "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin/"$PACKAGE_DIR"
mkdir -p "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/resources/darwin-aarch64/

cp "$TARGET_DIR"/"$PACKAGE_DIR"/ldk_node.kt "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin/"$PACKAGE_DIR"/
cp ./target/aarch64-apple-darwin/debug/libldk_node.dylib "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/resources/darwin-aarch64/libldk_node.dylib
