#!/bin/bash
set -eox pipefail

# Set deployment targets
export IPHONEOS_DEPLOYMENT_TARGET=13.0
export MACOSX_DEPLOYMENT_TARGET=10.15

# Set CMake-specific deployment targets to match
export CMAKE_OSX_DEPLOYMENT_TARGET=$IPHONEOS_DEPLOYMENT_TARGET
export CMAKE_IOS_DEPLOYMENT_TARGET=$IPHONEOS_DEPLOYMENT_TARGET

# Clear any potentially conflicting environment variables
unset SDKROOT
unset BINDGEN_EXTRA_CLANG_ARGS

source ./scripts/uniffi_bindgen_generate_kotlin.sh || exit 1
source ./scripts/uniffi_bindgen_generate_python.sh || exit 1
source ./scripts/uniffi_bindgen_generate_swift.sh || exit 1

