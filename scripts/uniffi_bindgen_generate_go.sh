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

# If we're running on macOS, build the macOS library using the host compiler.
# Cross compilation is not supported (needs more complex setup).
if [[ "$OSTYPE" == "darwin"* ]]; then
  build_lib "cargo" "aarch64-apple-darwin" "libldk_node.dylib"
  build_lib "cargo" "x86_64-apple-darwin" "libldk_node.dylib"
  mkdir -p ffi/golang/ldk_node/universal-macos || exit 1
  lipo -create -output "ffi/golang/ldk_node/universal-macos/libldk_node.dylib" "ffi/golang/ldk_node/aarch64-apple-darwin/libldk_node.dylib" "ffi/golang/ldk_node/x86_64-apple-darwin/libldk_node.dylib" || exit 1
fi

build_lib "cross" "x86_64-unknown-linux-gnu" "libldk_node.so"
build_lib "cross" "x86_64-pc-windows-gnu" "ldk_node.dll"
