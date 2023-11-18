#!/bin/bash

BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target"
PROJECT_DIR="ldk-node-android"
PACKAGE_DIR="org/lightningdevkit/ldknode"
UNIFFI_BINDGEN_BIN="cargo run --manifest-path bindings/uniffi-bindgen/Cargo.toml"
ANDROID_NDK_ROOT="/opt/homebrew/share/android-ndk"
LLVM_ARCH_PATH="darwin-x86_64"
PATH="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$LLVM_ARCH_PATH/bin:$PATH"

# The `export`s are required to make the variables available to subprocesses.
export CFLAGS="-D__ANDROID_MIN_SDK_VERSION__=21"
export AR=llvm-ar

# TODO: Remove setting OPENSSL_DIR after BDK upgrade. Depends on: https://github.com/lightningdevkit/ldk-node/issues/195
# The OPENSSL_DIR configuration is currently necessary due to default features being enabled in our esplora-client.
if [ -z "$OPENSSL_DIR" ]; then
    case "$(uname -s)" in
        Darwin)
            # macOS
            export OPENSSL_DIR="/opt/homebrew/Cellar/openssl@3/3.1.4"
            ;;
        Linux)
            # Linux
            export OPENSSL_DIR="/usr/local/openssl"
            ;;
        *)
    esac
fi

rustup target add x86_64-linux-android aarch64-linux-android armv7-linux-androideabi
CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="x86_64-linux-android21-clang" CC="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux/bin/x86_64-linux-android21-clang" cargo build --profile release-smaller --features uniffi --target x86_64-linux-android || exit 1
CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="armv7a-linux-androideabi21-clang" CC="armv7a-linux-androideabi21-clang" cargo build --profile release-smaller --features uniffi --target armv7-linux-androideabi || exit 1
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="aarch64-linux-android21-clang" CC="aarch64-linux-android21-clang" cargo build --profile release-smaller --features uniffi --target aarch64-linux-android || exit 1
$UNIFFI_BINDGEN_BIN generate bindings/ldk_node.udl --language kotlin -o "$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/kotlin || exit 1

JNI_LIB_DIR="$BINDINGS_DIR"/"$PROJECT_DIR"/lib/src/main/jniLibs/ 
mkdir -p $JNI_LIB_DIR/x86_64 || exit 1
mkdir -p $JNI_LIB_DIR/armeabi-v7a || exit 1
mkdir -p $JNI_LIB_DIR/arm64-v8a || exit 1
cp $TARGET_DIR/x86_64-linux-android/release-smaller/libldk_node.so $JNI_LIB_DIR/x86_64/ || exit 1
cp $TARGET_DIR/armv7-linux-androideabi/release-smaller/libldk_node.so $JNI_LIB_DIR/armeabi-v7a/ || exit 1
cp $TARGET_DIR/aarch64-linux-android/release-smaller/libldk_node.so $JNI_LIB_DIR/arm64-v8a/ || exit 1
