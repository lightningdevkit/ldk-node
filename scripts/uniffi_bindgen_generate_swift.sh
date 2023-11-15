#!/bin/bash
BINDINGS_DIR="./bindings/swift"
UNIFFI_BINDGEN_BIN="cargo run --manifest-path bindings/uniffi-bindgen/Cargo.toml"

cargo build --release || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language swift -o "$BINDINGS_DIR" || exit 1

mkdir -p $BINDINGS_DIR

# Install rust target toolchains
rustup install 1.73.0
rustup component add rust-src --toolchain 1.73.0
rustup target add aarch64-apple-ios x86_64-apple-ios --toolchain 1.73.0
rustup target add aarch64-apple-ios-sim --toolchain 1.73.0
rustup target add aarch64-apple-darwin x86_64-apple-darwin --toolchain 1.73.0

# Build rust target libs
cargo build --profile release-smaller --features uniffi || exit 1
cargo build --profile release-smaller --features uniffi --target x86_64-apple-darwin || exit 1
cargo build --profile release-smaller --features uniffi --target aarch64-apple-darwin || exit 1
cargo build --profile release-smaller --features uniffi --target x86_64-apple-ios || exit 1
cargo build --profile release-smaller --features uniffi --target aarch64-apple-ios || exit 1
cargo +1.73.0 build --release --features uniffi --target aarch64-apple-ios-sim || exit 1

# Combine ios-sim and apple-darwin (macos) libs for x86_64 and aarch64 (m1)
mkdir -p target/lipo-ios-sim/release-smaller || exit 1
lipo target/aarch64-apple-ios-sim/release/libldk_node.a target/x86_64-apple-ios/release-smaller/libldk_node.a -create -output target/lipo-ios-sim/release-smaller/libldk_node.a || exit 1
mkdir -p target/lipo-macos/release-smaller || exit 1
lipo target/aarch64-apple-darwin/release-smaller/libldk_node.a target/x86_64-apple-darwin/release-smaller/libldk_node.a -create -output target/lipo-macos/release-smaller/libldk_node.a || exit 1

$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language swift -o "$BINDINGS_DIR" || exit 1

swiftc -module-name LDKNode -emit-library -o "$BINDINGS_DIR"/libldk_node.dylib -emit-module -emit-module-path "$BINDINGS_DIR" -parse-as-library -L ./target/release-smaller -lldk_node -Xcc -fmodule-map-file="$BINDINGS_DIR"/LDKNodeFFI.modulemap "$BINDINGS_DIR"/LDKNode.swift -v || exit 1

# Create xcframework from bindings Swift file and libs
mkdir -p "$BINDINGS_DIR"/Sources/LDKNode || exit 1
mv "$BINDINGS_DIR"/LDKNode.swift "$BINDINGS_DIR"/Sources/LDKNode/LDKNode.swift || exit 1
cp "$BINDINGS_DIR"/LDKNodeFFI.h "$BINDINGS_DIR"/LDKNodeFFI.xcframework/ios-arm64/LDKNodeFFI.framework/Headers || exit 1
cp "$BINDINGS_DIR"/LDKNodeFFI.h "$BINDINGS_DIR"/LDKNodeFFI.xcframework/ios-arm64_x86_64-simulator/LDKNodeFFI.framework/Headers || exit 1
cp "$BINDINGS_DIR"/LDKNodeFFI.h "$BINDINGS_DIR"/LDKNodeFFI.xcframework/macos-arm64_x86_64/LDKNodeFFI.framework/Headers || exit 1
cp target/aarch64-apple-ios/release-smaller/libldk_node.a "$BINDINGS_DIR"/LDKNodeFFI.xcframework/ios-arm64/LDKNodeFFI.framework/LDKNodeFFI || exit 1
cp target/lipo-ios-sim/release-smaller/libldk_node.a "$BINDINGS_DIR"/LDKNodeFFI.xcframework/ios-arm64_x86_64-simulator/LDKNodeFFI.framework/LDKNodeFFI || exit 1
cp target/lipo-macos/release-smaller/libldk_node.a "$BINDINGS_DIR"/LDKNodeFFI.xcframework/macos-arm64_x86_64/LDKNodeFFI.framework/LDKNodeFFI || exit 1
# rm "$BINDINGS_DIR"/LDKNodeFFI.h || exit 1
# rm "$BINDINGS_DIR"/LDKNodeFFI.modulemap || exit 1
echo finished successfully!
