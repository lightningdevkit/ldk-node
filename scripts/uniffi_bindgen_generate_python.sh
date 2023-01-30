#!/bin/bash
BINDINGS_DIR="./bindings/python"
UNIFFI_BINDGEN_BIN="cargo +nightly run --features=uniffi/cli --bin uniffi-bindgen"

cargo +nightly build --release || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language python -o "$BINDINGS_DIR" || exit 1
cp ./target/release/libldk_node.dylib "$BINDINGS_DIR"/libldk_node.dylib || exit 1
