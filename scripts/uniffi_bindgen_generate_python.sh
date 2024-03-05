#!/bin/bash
BINDINGS_DIR="./bindings/python/src/ldk_node"
UNIFFI_BINDGEN_BIN="cargo run --manifest-path bindings/uniffi-bindgen/Cargo.toml"

if [[ "$OSTYPE" == "linux-gnu"* ]]; then
	# Linux
	DYNAMIC_LIB_PATH="./target/release-smaller/libldk_node.so"
elif [[ "$OSTYPE" == "darwin"* ]]; then
	# macOS
	DYNAMIC_LIB_PATH="./target/release-smaller/libldk_node.dylib"
elif [[ "$OSTYPE" == "msys"* ]]; then
	# Windows
	DYNAMIC_LIB_PATH="./target/release-smaller/libldk_node.dll"
elif [[ "$OSTYPE" == "cygwin"* ]]; then
	# Windows
	DYNAMIC_LIB_PATH="./target/release-smaller/libldk_node.dll"
else
	echo "Unsupported platform!: $OSTYPE"
	exit -1
fi

cargo build --profile release-smaller --features uniffi || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language python -o "$BINDINGS_DIR" || exit 1

mkdir -p $BINDINGS_DIR
cp "$DYNAMIC_LIB_PATH" "$BINDINGS_DIR" || exit 1
