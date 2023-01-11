#!/bin/bash

# make sure you're using the uniffi-bindgen tool version 0.21.0

# 1. Generate the glue file
# 2. Build the native binaries
# 3. Move them both in the jvm lib
uniffi-bindgen generate uniffi/ldk_node.udl --language kotlin
cargo +nightly build --target aarch64-apple-darwin
mkdir -p ./ldk-node-jvm/lib/src/main/kotlin/org/ldk/
mkdir -p ./ldk-node-jvm/lib/src/main/resources/darwin-aarch64/
cp ./uniffi/org/ldk/ldknode/ldk_node.kt ./ldk-node-jvm/lib/src/main/kotlin/org/ldk/
cp ./target/aarch64-apple-darwin/debug/libldk_node.dylib ./ldk-node-jvm/lib/src/main/resources/darwin-aarch64/libldk_node.dylib
