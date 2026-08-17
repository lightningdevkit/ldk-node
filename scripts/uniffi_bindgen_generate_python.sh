#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINDINGS_DIR="$REPO_ROOT/bindings/python/src/ldk_node"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
CARGO_BUILD_ARGS=()

case " ${RUSTFLAGS:-} " in
	*" --cfg tokio_unstable "*|*" --cfg=tokio_unstable "*) ;;
	*) export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--cfg tokio_unstable" ;;
esac

case "$(uname -s)" in
	Linux) DYNAMIC_LIB_PATH="$TARGET_DIR/release-smaller/libldk_node.so" ;;
	Darwin)
		case "${ARCHFLAGS:-}" in
			"") DYNAMIC_LIB_PATH="$TARGET_DIR/release-smaller/libldk_node.dylib" ;;
			"-arch x86_64")
				CARGO_BUILD_ARGS=(--target x86_64-apple-darwin)
				DYNAMIC_LIB_PATH="$TARGET_DIR/x86_64-apple-darwin/release-smaller/libldk_node.dylib"
				;;
			"-arch arm64")
				CARGO_BUILD_ARGS=(--target aarch64-apple-darwin)
				DYNAMIC_LIB_PATH="$TARGET_DIR/aarch64-apple-darwin/release-smaller/libldk_node.dylib"
				;;
			*)
				echo "Unsupported ARCHFLAGS: $ARCHFLAGS" >&2
				exit 1
				;;
		esac
		;;
	*)
		echo "Unsupported operating system: $(uname -s)" >&2
		exit 1
		;;
esac

cd "$REPO_ROOT"
mkdir -p "$BINDINGS_DIR"

cargo build "${CARGO_BUILD_ARGS[@]}" --profile release-smaller --features uniffi
cargo run --manifest-path bindings/uniffi-bindgen/Cargo.toml -- \
	generate bindings/ldk_node.udl \
	--lib-file "$DYNAMIC_LIB_PATH" \
	--language python \
	-o "$BINDINGS_DIR"
cp "$DYNAMIC_LIB_PATH" "$BINDINGS_DIR"
