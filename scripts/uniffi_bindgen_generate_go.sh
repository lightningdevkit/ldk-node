#!/bin/bash

set -e

if ! command -v cross &> /dev/null; then
  echo "cross-rs is required to build bindings. Install it by running:"
  echo "  cargo install cross --git https://github.com/cross-rs/cross"
  exit 1
fi

uniffi-bindgen-go bindings/ldk_node.udl -o ffi/golang -c ./uniffi.toml

build_lib() {
  local TOOL=$1
  local TARGET=$2
  local OUTPUT_FILE=$3

  $TOOL build --release --target $TARGET --features uniffi || exit 1
  mkdir -p "ffi/golang/ldk_node/$TARGET" || exit 1
  cp "target/$TARGET/release/$OUTPUT_FILE" "ffi/golang/ldk_node/$TARGET/" || exit 1
}

is_target_installed() {
  local TARGET=$1
  rustup target list --installed | grep -q $TARGET
}

# If we're running on macOS, build the macOS library using the host compiler.
# Cross compilation is not supported (needs more complex setup).
if [[ "$OSTYPE" == "darwin"* ]]; then
  build_lib "cargo" "aarch64-apple-darwin" "libldk_node.dylib"
fi

build_lib "cross" "x86_64-unknown-linux-gnu" "libldk_node.so"
build_lib "cross" "x86_64-pc-windows-gnu" "ldk_node.dll"
