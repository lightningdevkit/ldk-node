#!/bin/bash
# Build a Python wheel for the current platform.
#
# This script compiles the Rust library, generates Python bindings via UniFFI,
# and builds a platform-specific wheel using uv + hatchling.
#
# Run this on each target platform (Linux, macOS) to collect wheels, then use
# scripts/python_publish_package.sh to publish them.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

# Generate bindings and compile the native library
echo "Generating Python bindings..."
./scripts/uniffi_bindgen_generate_python.sh

# Build the wheel
echo "Building wheel..."
cd bindings/python
uv build --wheel

echo ""
echo "Wheel built successfully:"
ls -1 dist/*.whl
echo ""
echo "Collect wheels from all target platforms into dist/, then run:"
echo "  ./scripts/python_publish_package.sh"
