#!/bin/bash

BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target"
PROJECT_DIR="ldk-node-android"
ANDROID_LIB_DIR="$BINDINGS_DIR/$PROJECT_DIR"

# Install gobley-uniffi-bindgen from fork with patched version
echo "Installing gobley-uniffi-bindgen fork..."
cargo install --git https://github.com/ovitrif/gobley.git --branch fix-v0.2.0 gobley-uniffi-bindgen --force
UNIFFI_BINDGEN_BIN="gobley-uniffi-bindgen"

export_variable_if_not_present() {
  local name="$1"
  local value="$2"

  # Check if the variable is already set
  if [ -z "${!name}" ]; then
    export "$name=$value"
    echo "Exported $name=$value"
  else
    echo "$name is already set to ${!name}, not exporting."
  fi
}

case "$OSTYPE" in
    linux-gnu)
      export_variable_if_not_present "ANDROID_NDK_ROOT" "/opt/android-ndk"
      export_variable_if_not_present "LLVM_ARCH_PATH" "linux-x86_64"
      ;;
    darwin*)
      export_variable_if_not_present "ANDROID_NDK_ROOT" "/opt/homebrew/share/android-ndk"
      export_variable_if_not_present "LLVM_ARCH_PATH" "darwin-x86_64"
      ;;
    *)
      echo "Unknown operating system: $OSTYPE"
      ;;
    esac

PATH="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$LLVM_ARCH_PATH/bin:$PATH"

# Install cargo-ndk if not already installed
if ! command -v cargo-ndk &> /dev/null; then
    echo "Installing cargo-ndk..."
    cargo install cargo-ndk
fi

# Add Android targets
echo "Adding Android targets..."
rustup target add x86_64-linux-android aarch64-linux-android armv7-linux-androideabi

# Build for all Android architectures with page size optimizations
echo "Building for Android architectures..."
JNI_LIB_DIR="$ANDROID_LIB_DIR/lib/src/main/jniLibs"
export RUSTFLAGS="-C link-args=-Wl,-z,max-page-size=16384,-z,common-page-size=16384"
export CFLAGS="-D__ANDROID_MIN_SDK_VERSION__=21"
cargo ndk \
    -o "$JNI_LIB_DIR" \
    -t armeabi-v7a \
    -t arm64-v8a \
    -t x86_64 \
    build --profile release-smaller --features uniffi || exit 1

# Generate Kotlin bindings
echo "Generating Kotlin bindings..."
$UNIFFI_BINDGEN_BIN bindings/ldk_node.udl --lib-file $TARGET_DIR/aarch64-linux-android/release-smaller/libldk_node.so --config uniffi-android.toml -o "$ANDROID_LIB_DIR/lib/src" || exit 1

# Sync version from Cargo.toml
echo "Syncing version from Cargo.toml..."
CARGO_VERSION=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/' | head -1)
sed -i.bak "s/^libraryVersion=.*/libraryVersion=$CARGO_VERSION/" "$ANDROID_LIB_DIR/gradle.properties"
rm -f "$ANDROID_LIB_DIR/gradle.properties.bak"
echo "Version synced: $CARGO_VERSION"

# Verify android library publish
echo "Testing android library publish to Maven Local..."
$ANDROID_LIB_DIR/gradlew --project-dir "$ANDROID_LIB_DIR" clean publishToMavenLocal

echo "Android build process completed successfully!"
