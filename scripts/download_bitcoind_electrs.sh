#!/bin/bash
set -eox pipefail

# Our Esplora-based tests require `electrs` and `bitcoind`
# binaries. Here, we download the binaries, validate them, and export their
# location via `ELECTRS_EXE`/`BITCOIND_EXE` which will be used by the
# `electrsd`/`bitcoind` crates in our tests.

HOST_PLATFORM="$(rustc --version --verbose | grep "host:" | awk '{ print $2 }')"
ELECTRS_DL_ENDPOINT="https://github.com/RCasatta/electrsd/releases/download/electrs_releases"
ELECTRS_VERSION="esplora_a33e97e1a1fc63fa9c20a116bb92579bbf43b254"
BITCOIND_DL_ENDPOINT="https://bitcoincore.org/bin/"
BITCOIND_VERSION="27.2"
if [[ "$HOST_PLATFORM" == *linux* ]]; then
	ELECTRS_DL_FILE_NAME=electrs_linux_"$ELECTRS_VERSION".zip
	ELECTRS_DL_HASH="865e26a96e8df77df01d96f2f569dcf9622fc87a8d99a9b8fe30861a4db9ddf1"
	BITCOIND_DL_FILE_NAME=bitcoin-"$BITCOIND_VERSION"-x86_64-linux-gnu.tar.gz
	BITCOIND_DL_HASH="acc223af46c178064c132b235392476f66d486453ddbd6bca6f1f8411547da78"
elif [[ "$HOST_PLATFORM" == *darwin* ]]; then
	ELECTRS_DL_FILE_NAME=electrs_macos_"$ELECTRS_VERSION".zip
	ELECTRS_DL_HASH="2d5ff149e8a2482d3658e9b386830dfc40c8fbd7c175ca7cbac58240a9505bcd"
	BITCOIND_DL_FILE_NAME=bitcoin-"$BITCOIND_VERSION"-x86_64-apple-darwin.tar.gz
	BITCOIND_DL_HASH="6ebc56ca1397615d5a6df2b5cf6727b768e3dcac320c2d5c2f321dcaabc7efa2"
else
	printf "\n\n"
	echo "Unsupported platform: $HOST_PLATFORM Exiting.."
	exit 1
fi

DL_TMP_DIR=$(mktemp -d)
trap 'rm -rf -- "$DL_TMP_DIR"' EXIT

pushd "$DL_TMP_DIR"
ELECTRS_DL_URL="$ELECTRS_DL_ENDPOINT"/"$ELECTRS_DL_FILE_NAME"
curl -L -o "$ELECTRS_DL_FILE_NAME" "$ELECTRS_DL_URL"
echo "$ELECTRS_DL_HASH  $ELECTRS_DL_FILE_NAME"|shasum -a 256 -c
unzip "$ELECTRS_DL_FILE_NAME"
export ELECTRS_EXE="$DL_TMP_DIR"/electrs
chmod +x "$ELECTRS_EXE"

BITCOIND_DL_URL="$BITCOIND_DL_ENDPOINT"/bitcoin-core-"$BITCOIND_VERSION"/"$BITCOIND_DL_FILE_NAME"
curl -L -o "$BITCOIND_DL_FILE_NAME" "$BITCOIND_DL_URL"
echo "$BITCOIND_DL_HASH  $BITCOIND_DL_FILE_NAME"|shasum -a 256 -c
tar xzf "$BITCOIND_DL_FILE_NAME"
export BITCOIND_EXE="$DL_TMP_DIR"/bitcoin-"$BITCOIND_VERSION"/bin/bitcoind
chmod +x "$BITCOIND_EXE"
popd
