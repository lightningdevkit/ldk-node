#!/bin/bash
# Validate and publish a complete Python wheel release.
#
# Build the four native wheels with python_build_wheel.sh and collect the
# wheels and their .sha256 files in one directory. Publish the unchanged set
# to TestPyPI first. Production publication verifies TestPyPI has files with
# the same hashes before uploading to PyPI.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WHEEL_DIR="$REPO_ROOT/bindings/python/wheelhouse"
INDEX=""
VERSION=""
CHECK_ONLY=false

usage() {
	echo "Usage: $0 --index testpypi|pypi --version VERSION [--wheel-dir DIR] [--check-only]"
}

while [[ $# -gt 0 ]]; do
	case "$1" in
		--index)
			[[ $# -ge 2 ]] || { usage >&2; exit 2; }
			INDEX="$2"
			shift 2
			;;
		--version)
			[[ $# -ge 2 ]] || { usage >&2; exit 2; }
			VERSION="$2"
			shift 2
			;;
		--wheel-dir)
			[[ $# -ge 2 ]] || { usage >&2; exit 2; }
			WHEEL_DIR="$2"
			shift 2
			;;
		--check-only)
			CHECK_ONLY=true
			shift
			;;
		-h|--help)
			usage
			exit 0
			;;
		*)
			echo "Unknown argument: $1" >&2
			usage >&2
			exit 2
			;;
	esac
done

if [[ "$INDEX" != testpypi && "$INDEX" != pypi ]]; then
	echo "--index must be either testpypi or pypi." >&2
	exit 2
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([a-zA-Z0-9.-]+)?$ ]]; then
	echo "--version must be a Python package version such as 0.7.0." >&2
	exit 2
fi

case "$WHEEL_DIR" in
	/*) ;;
	*) WHEEL_DIR="$REPO_ROOT/$WHEEL_DIR" ;;
esac

if [[ ! -d "$WHEEL_DIR" ]]; then
	echo "Wheel directory does not exist: $WHEEL_DIR" >&2
	exit 1
fi

if [[ "$CHECK_ONLY" == false && -z "${UV_PUBLISH_TOKEN:-}" ]]; then
	echo "Set UV_PUBLISH_TOKEN to a token for $INDEX before publishing." >&2
	exit 1
fi

command -v uv >/dev/null 2>&1 || { echo "Required command not found: uv" >&2; exit 1; }

uv run --no-project --python 3.10 python - "$VERSION" "$WHEEL_DIR" "$INDEX" <<'PY'
import email
import hashlib
import json
import re
import sys
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

version, wheel_dir_arg, index = sys.argv[1:]
wheel_dir = Path(wheel_dir_arg)

platforms = (
    "manylinux_2_28_x86_64",
    "manylinux_2_28_aarch64",
    "macosx_10_12_x86_64",
    "macosx_11_0_arm64",
)
expected = {f"ldk_node-{version}-py3-none-{platform}.whl" for platform in platforms}
actual = {path.name for path in wheel_dir.glob("*.whl")}

if actual != expected:
    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    if missing:
        print(f"Missing wheels: {', '.join(missing)}", file=sys.stderr)
    if unexpected:
        print(f"Unexpected wheels: {', '.join(unexpected)}", file=sys.stderr)
    raise SystemExit(1)

source_archives = list(wheel_dir.glob("*.tar.gz")) + list(wheel_dir.glob("*.zip"))
if source_archives:
    print("Source distributions must not be published for version 0.8.", file=sys.stderr)
    raise SystemExit(1)

digests = {}
for filename in sorted(expected):
    wheel_path = wheel_dir / filename
    digest = hashlib.sha256(wheel_path.read_bytes()).hexdigest()
    checksum_path = wheel_dir / f"{filename}.sha256"
    if not checksum_path.is_file():
        print(f"Missing checksum: {checksum_path.name}", file=sys.stderr)
        raise SystemExit(1)

    checksum_fields = checksum_path.read_text(encoding="utf-8").split()
    if len(checksum_fields) != 2 or checksum_fields != [digest, filename]:
        print(f"Checksum mismatch: {checksum_path.name}", file=sys.stderr)
        raise SystemExit(1)

    with zipfile.ZipFile(wheel_path) as wheel:
        names = wheel.namelist()
        metadata_paths = [name for name in names if name.endswith(".dist-info/METADATA")]
        wheel_paths = [name for name in names if name.endswith(".dist-info/WHEEL")]
        if len(metadata_paths) != 1 or len(wheel_paths) != 1:
            print(f"Invalid wheel metadata layout: {filename}", file=sys.stderr)
            raise SystemExit(1)

        metadata = email.message_from_bytes(wheel.read(metadata_paths[0]))
        wheel_metadata = email.message_from_bytes(wheel.read(wheel_paths[0]))
        normalized_name = re.sub(r"[-_.]+", "-", metadata["Name"]).lower()
        expected_tag = filename.removeprefix(f"ldk_node-{version}-").removesuffix(".whl")
        native_library = (
            "ldk_node/libldk_node.so"
            if "manylinux" in filename
            else "ldk_node/libldk_node.dylib"
        )

        if normalized_name != "ldk-node" or metadata["Version"] != version:
            print(f"Name or version mismatch in {filename}", file=sys.stderr)
            raise SystemExit(1)
        if metadata["Requires-Python"] != ">=3.10":
            print(f"Python requirement mismatch in {filename}", file=sys.stderr)
            raise SystemExit(1)
        if wheel_metadata["Root-Is-Purelib"].lower() != "false":
            print(f"Native wheel marked as pure: {filename}", file=sys.stderr)
            raise SystemExit(1)
        if wheel_metadata.get_all("Tag", []) != [expected_tag]:
            print(f"Wheel tag mismatch in {filename}", file=sys.stderr)
            raise SystemExit(1)
        if "ldk_node/ldk_node.py" not in names or native_library not in names:
            print(f"Generated bindings missing from {filename}", file=sys.stderr)
            raise SystemExit(1)

    digests[filename] = digest

if index == "pypi":
    testpypi_url = f"https://test.pypi.org/pypi/ldk-node/{version}/json"
    try:
        with urllib.request.urlopen(testpypi_url, timeout=30) as response:
            testpypi = json.load(response)
    except (OSError, urllib.error.URLError, json.JSONDecodeError) as error:
        print(f"Unable to verify TestPyPI release: {error}", file=sys.stderr)
        raise SystemExit(1)

    uploaded = {
        item["filename"]: item.get("digests", {}).get("sha256")
        for item in testpypi.get("urls", [])
    }
    mismatches = [
        filename
        for filename, digest in digests.items()
        if uploaded.get(filename) != digest
    ]
    if mismatches:
        print(
            "TestPyPI does not contain matching artifacts: " + ", ".join(mismatches),
            file=sys.stderr,
        )
        raise SystemExit(1)

print("Validated release artifacts:")
for filename in sorted(expected):
    print(f"  {filename}  sha256:{digests[filename]}")
PY

if [[ "$CHECK_ONLY" == true ]]; then
	echo "Artifact validation completed without publishing."
	exit 0
fi

shopt -s nullglob
wheels=("$WHEEL_DIR"/*.whl)

case "$INDEX" in
	testpypi)
		PUBLISH_URL="https://test.pypi.org/legacy/"
		CHECK_URL="https://test.pypi.org/simple/"
		;;
	pypi)
		PUBLISH_URL="https://upload.pypi.org/legacy/"
		CHECK_URL="https://pypi.org/simple/"
		;;
esac

echo "Publishing ${#wheels[@]} wheels to $INDEX."
uv publish \
	--publish-url "$PUBLISH_URL" \
	--check-url "$CHECK_URL" \
	--trusted-publishing never \
	"${wheels[@]}"
