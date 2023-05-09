#!/bin/bash
source ./scripts/uniffi_bindgen_generate_kotlin.sh || exit 1
source ./scripts/uniffi_bindgen_generate_python.sh || exit 1
source ./scripts/uniffi_bindgen_generate_swift.sh || exit 1

