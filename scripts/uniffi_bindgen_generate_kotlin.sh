#!/bin/bash
BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target/bindings/kotlin"
PROJECT_DIR="ldk-node-jvm"
PACKAGE_DIR="org/lightningdevkit/ldknode"
UNIFFI_BINDGEN_BIN="cargo run --manifest-path bindings/uniffi-bindgen/Cargo.toml"

if [[ "$OSTYPE" == "linux-gnu"* ]]; then
	rustup target add x86_64-unknown-linux-gnu || exit 1
	cargo build --release --target x86_64-unknown-linux-gnu --features uniffi || exit 1
	DYNAMIC_LIB_PATH="target/x86_64-unknown-linux-gnu/release/libldk_node.so"
	RES_DIR="$BINDINGS_DIR/$PROJECT_DIR/lib/src/main/resources/linux-x86-64/"
	mkdir -p $RES_DIR || exit 1
	cp $DYNAMIC_LIB_PATH $RES_DIR || exit 1
else
	rustup target add x86_64-apple-darwin || exit 1
	cargo build --release --target x86_64-apple-darwin --features uniffi || exit 1
	DYNAMIC_LIB_PATH="target/x86_64-apple-darwin/release/libldk_node.dylib"
	RES_DIR="$BINDINGS_DIR/$PROJECT_DIR/lib/src/main/resources/darwin-x86-64/"
	mkdir -p $RES_DIR || exit 1
	cp $DYNAMIC_LIB_PATH $RES_DIR || exit 1

	rustup target add aarch64-apple-darwin || exit 1
	cargo build --release --target aarch64-apple-darwin --features uniffi || exit 1
	DYNAMIC_LIB_PATH="target/aarch64-apple-darwin/release/libldk_node.dylib"
	RES_DIR="$BINDINGS_DIR/$PROJECT_DIR/lib/src/main/resources/darwin-aarch64/"
	mkdir -p $RES_DIR || exit 1
	cp $DYNAMIC_LIB_PATH $RES_DIR || exit 1
fi

mkdir -p "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin/"$PACKAGE_DIR" || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language kotlin -o "$TARGET_DIR" || exit 1

cp "$TARGET_DIR"/"$PACKAGE_DIR"/ldk_node.kt "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin/"$PACKAGE_DIR"/ || exit 1
