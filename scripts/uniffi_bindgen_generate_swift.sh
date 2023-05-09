#!/bin/bash
BINDINGS_DIR="./bindings/swift"
UNIFFI_BINDGEN_BIN="cargo run --features=uniffi/cli --bin uniffi-bindgen"

cargo build --release || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language swift -o "$BINDINGS_DIR" || exit 1

mkdir -p $BINDINGS_DIR

# Install rust target toolchains
rustup install nightly-x86_64-apple-darwin
rustup component add rust-src --toolchain nightly-x86_64-apple-darwin
rustup target add aarch64-apple-ios x86_64-apple-ios
rustup target add aarch64-apple-ios-sim --toolchain nightly
rustup target add aarch64-apple-darwin x86_64-apple-darwin

# Build rust target libs
cargo build --profile release-smaller || exit 1
cargo build --profile release-smaller --target x86_64-apple-darwin || exit 1
cargo build --profile release-smaller --target aarch64-apple-darwin || exit 1
cargo build --profile release-smaller --target x86_64-apple-ios || exit 1
cargo build --profile release-smaller --target aarch64-apple-ios || exit 1
cargo +nightly build --release --target aarch64-apple-ios-sim || exit 1

# Combine ios-sim and apple-darwin (macos) libs for x86_64 and aarch64 (m1)
mkdir -p target/lipo-ios-sim/release-smaller || exit 1
lipo target/aarch64-apple-ios-sim/release/libldk_node.a target/x86_64-apple-ios/release-smaller/libldk_node.a -create -output target/lipo-ios-sim/release-smaller/libldk_node.a || exit 1
mkdir -p target/lipo-macos/release-smaller || exit 1
lipo target/aarch64-apple-darwin/release-smaller/libldk_node.a target/x86_64-apple-darwin/release-smaller/libldk_node.a -create -output target/lipo-macos/release-smaller/libldk_node.a || exit 1

$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language swift -o "$BINDINGS_DIR" || exit 1

swiftc -module-name ldk_node -emit-library -o "$BINDINGS_DIR"/libldk_node.dylib -emit-module -emit-module-path "$BINDINGS_DIR" -parse-as-library -L ./target/release-smaller -lldk_node -Xcc -fmodule-map-file="$BINDINGS_DIR"/ldk_nodeFFI.modulemap "$BINDINGS_DIR"/ldk_node.swift -v || exit 1

# Create xcframework from bindings Swift file and libs
mkdir -p "$BINDINGS_DIR"/Sources/LightningDevKitNode || exit 1
mv "$BINDINGS_DIR"/ldk_node.swift "$BINDINGS_DIR"/Sources/LightningDevKitNode/LightningDevKitNode.swift || exit 1
cp "$BINDINGS_DIR"/ldk_nodeFFI.h "$BINDINGS_DIR"/ldk_nodeFFI.xcframework/ios-arm64/ldk_nodeFFI.framework/Headers || exit 1
cp "$BINDINGS_DIR"/ldk_nodeFFI.h "$BINDINGS_DIR"/ldk_nodeFFI.xcframework/ios-arm64_x86_64-simulator/ldk_nodeFFI.framework/Headers || exit 1
cp "$BINDINGS_DIR"/ldk_nodeFFI.h "$BINDINGS_DIR"/ldk_nodeFFI.xcframework/macos-arm64_x86_64/ldk_nodeFFI.framework/Headers || exit 1
cp target/aarch64-apple-ios/release-smaller/libldk_node.a "$BINDINGS_DIR"/ldk_nodeFFI.xcframework/ios-arm64/ldk_nodeFFI.framework/ldk_nodeFFI || exit 1
cp target/lipo-ios-sim/release-smaller/libldk_node.a "$BINDINGS_DIR"/ldk_nodeFFI.xcframework/ios-arm64_x86_64-simulator/ldk_nodeFFI.framework/ldk_nodeFFI || exit 1
cp target/lipo-macos/release-smaller/libldk_node.a "$BINDINGS_DIR"/ldk_nodeFFI.xcframework/macos-arm64_x86_64/ldk_nodeFFI.framework/ldk_nodeFFI || exit 1
# rm "$BINDINGS_DIR"/ldk_nodeFFI.h || exit 1
# rm "$BINDINGS_DIR"/ldk_nodeFFI.modulemap || exit 1
echo finished successfully!
