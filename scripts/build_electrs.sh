#!/bin/bash
set -eox pipefail

# Our Esplora-based tests require `electrs` binaries. Here, we
# download the code, build the binaries, and export their location
# via `ELECTRS_EXE`/`BITCOIND_EXE` which will be used by the
# `electrsd`/`bitcoind` crates in our tests.

HOST_PLATFORM="$(rustc --version --verbose | grep "host:" | awk '{ print $2 }')"
ELECTRS_GIT_REPO="https://github.com/tankyleo/blockstream-electrs.git"
ELECTRS_TAG="2026-05-26-electrum-submit-package"
ELECTRS_REV="8c06d8010e43f793b1a65f83695ea846e5cd83ed"
if [[ "$HOST_PLATFORM" != *linux* && "$HOST_PLATFORM" != *darwin* ]]; then
	printf "\n\n"
	echo "Unsupported platform: $HOST_PLATFORM Exiting.."
	exit 1
fi

DL_TMP_DIR=$(mktemp -d)
trap 'rm -rf -- "$DL_TMP_DIR"' EXIT

pushd "$DL_TMP_DIR"
git clone --branch "$ELECTRS_TAG" --depth 1 "$ELECTRS_GIT_REPO" blockstream-electrs
cd blockstream-electrs
CURRENT_HEAD=$(git rev-parse HEAD)
if [ "$CURRENT_HEAD" != "$ELECTRS_REV" ]; then
  echo "ERROR: HEAD does not match expected commit"
  echo "expected: $ELECTRS_REV"
  echo "actual:   $CURRENT_HEAD"
  exit 1
fi
RUSTFLAGS="" cargo build
export ELECTRS_EXE="$DL_TMP_DIR"/blockstream-electrs/target/debug/electrs
chmod +x "$ELECTRS_EXE"
popd
