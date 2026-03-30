#!/bin/bash
# Publish Python wheels to PyPI (or TestPyPI).
#
# Usage:
#   ./scripts/python_publish_package.sh                    # publish to PyPI
#   ./scripts/python_publish_package.sh --index testpypi   # publish to TestPyPI
#
# Before running, collect wheels from all target platforms into bindings/python/dist/.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$REPO_ROOT/bindings/python/dist"

if [ ! -d "$DIST_DIR" ] || [ -z "$(ls -A "$DIST_DIR"/*.whl 2>/dev/null)" ]; then
	echo "Error: No wheels found in $DIST_DIR"
	echo "Run ./scripts/python_build_wheel.sh on each target platform first."
	exit 1
fi

echo "Wheels to publish:"
ls -1 "$DIST_DIR"/*.whl
echo ""

uv publish "$@" "$DIST_DIR"/*.whl
