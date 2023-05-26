#!/bin/bash
BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target/bindings/kotlin"
PROJECT_DIR="ldk-node-jvm"
PACKAGE_DIR="org/lightningdevkit/ldknode"
UNIFFI_BINDGEN_BIN="cargo run --features=uniffi/cli --bin uniffi-bindgen"

DYNAMIC_LIB_PATH="target/release/libldk_node.dylib"
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
  DYNAMIC_LIB_PATH="target/release/libldk_node.so"
fi

#rustup target add aarch64-apple-darwin
#cargo build --target aarch64-apple-darwin || exit 1
cargo build --release --features uniffi || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language kotlin -o "$TARGET_DIR" || exit 1

mkdir -p "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin/"$PACKAGE_DIR" || exit 1
mkdir -p "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/resources/darwin-aarch64/ || exit 1

cp "$TARGET_DIR"/"$PACKAGE_DIR"/ldk_node.kt "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin/"$PACKAGE_DIR"/ || exit 1
#cp ./target/aarch64-apple-darwin/debug/libldk_node.dylib "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/resources/darwin-aarch64/libldk_node.dylib || exit 1
cp $DYNAMIC_LIB_PATH "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/resources/libldk_node.dylib || exit 1
