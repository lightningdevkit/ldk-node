#!/bin/bash
uniffi-bindgen generate uniffi/ldk_node.udl --language python
uniffi-bindgen generate uniffi/ldk_node.udl --language kotlin

uniffi-bindgen generate uniffi/ldk_node.udl --language swift
#swiftc -module-name ldk_node -emit-library -o libldk_node.dylib -emit-module -emit-module-path ./uniffi -parse-as-library -L ./target/release/ -lldk_node -Xcc -fmodule-map-file=./uniffi/ldk_nodeFFI.modulemap ./uniffi/ldk_node.swift -v
#swiftc -module-name ldk_node -emit-library -o libldk_node.dylib -emit-module -emit-module-path ./uniffi -parse-as-library -L ./target/debug/ -lldk_node -Xcc -fmodule-map-file=./uniffi/ldk_nodeFFI.modulemap ./uniffi/ldk_node.swift -v
#swiftc -module-name ldk_node -emit-library -o libldk_node.dylib -emit-module -emit-module-path ./uniffi -parse-as-library -L ./target/x86_64-apple-darwin/release/ -lldk_node -Xcc -fmodule-map-file=./uniffi/ldk_nodeFFI.modulemap ./uniffi/ldk_node.swift -v
