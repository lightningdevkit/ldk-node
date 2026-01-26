#!/bin/bash

BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target"
PROJECT_DIR="ldk-node-jvm"
JVM_LIB_DIR="$BINDINGS_DIR/$PROJECT_DIR"

# Install gobley-uniffi-bindgen from fork with patched version
echo "Installing gobley-uniffi-bindgen fork..."
cargo install --git https://github.com/ovitrif/gobley.git --branch fix-v0.2.0 gobley-uniffi-bindgen --force
UNIFFI_BINDGEN_BIN="gobley-uniffi-bindgen"

if [[ "$OSTYPE" == "linux-gnu"* ]]; then
	echo "Building for Linux x86_64..."
	rustup target add x86_64-unknown-linux-gnu || exit 1
	cargo build --release --target x86_64-unknown-linux-gnu --features uniffi || exit 1
	DYNAMIC_LIB_PATH="$TARGET_DIR/x86_64-unknown-linux-gnu/release/libldk_node.so"
	RES_DIR="$JVM_LIB_DIR/lib/src/main/resources/linux-x86-64/"
	mkdir -p $RES_DIR || exit 1
	cp $DYNAMIC_LIB_PATH $RES_DIR || exit 1
else
	echo "Building for macOS x86_64..."
	rustup target add x86_64-apple-darwin || exit 1
	cargo build --release --target x86_64-apple-darwin --features uniffi || exit 1
	DYNAMIC_LIB_PATH="$TARGET_DIR/x86_64-apple-darwin/release/libldk_node.dylib"
	RES_DIR="$JVM_LIB_DIR/lib/src/main/resources/darwin-x86-64/"
	mkdir -p $RES_DIR || exit 1
	cp $DYNAMIC_LIB_PATH $RES_DIR || exit 1

	echo "Building for macOS aarch64..."
	rustup target add aarch64-apple-darwin || exit 1
	cargo build --release --target aarch64-apple-darwin --features uniffi || exit 1
	DYNAMIC_LIB_PATH_ARM="$TARGET_DIR/aarch64-apple-darwin/release/libldk_node.dylib"
	RES_DIR="$JVM_LIB_DIR/lib/src/main/resources/darwin-aarch64/"
	mkdir -p $RES_DIR || exit 1
	cp $DYNAMIC_LIB_PATH_ARM $RES_DIR || exit 1
fi

# Clean old bindings and generate new ones
echo "Cleaning old Kotlin bindings..."
rm -f "$JVM_LIB_DIR/lib/src/main/kotlin/org/lightningdevkit/ldknode/ldk_node.kt"

echo "Generating Kotlin bindings..."
$UNIFFI_BINDGEN_BIN bindings/ldk_node.udl --lib-file $DYNAMIC_LIB_PATH --config uniffi-jvm.toml -o "$JVM_LIB_DIR/lib/src" || exit 1

# Fix incorrect kotlinx.coroutines.IO import (removed in newer kotlinx.coroutines versions)
echo "Fixing Kotlin coroutines imports..."
KOTLIN_BINDINGS_FILE="$JVM_LIB_DIR/lib/src/main/kotlin/org/lightningdevkit/ldknode/ldk_node.jvm.kt"
if [ -f "$KOTLIN_BINDINGS_FILE" ]; then
	sed -i.bak '/import kotlinx\.coroutines\.IO/d' "$KOTLIN_BINDINGS_FILE"
	rm -f "$KOTLIN_BINDINGS_FILE.bak"
fi

# Sync version from Cargo.toml
echo "Syncing version from Cargo.toml..."
CARGO_VERSION=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/' | head -1)
sed -i.bak "s/^libraryVersion=.*/libraryVersion=$CARGO_VERSION/" "$JVM_LIB_DIR/gradle.properties"
rm -f "$JVM_LIB_DIR/gradle.properties.bak"
echo "JVM version synced: $CARGO_VERSION"

# Verify JVM library build (skip tests - they require bitcoind)
echo "Testing JVM library build..."
$JVM_LIB_DIR/gradlew --project-dir "$JVM_LIB_DIR" clean build -x test || exit 1

echo "JVM build process completed successfully!"
