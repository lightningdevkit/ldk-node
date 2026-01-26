## Generating the Bindings

## Build All Bindings
Run in the root dir:
```sh
RUSTFLAGS="--cfg no_download" cargo build && ./scripts/uniffi_bindgen_generate.sh && ./scripts/swift_create_xcframework_archive.sh && sh scripts/uniffi_bindgen_generate_kotlin_android.sh
```

---

Detailed instructions for publishing a new version of the bindings.

1. Update `Cargo.toml`
2. Update `libraryVersion` in:
   - `bindings/kotlin/ldk-node-android/gradle.properties`
   - `bindings/kotlin/ldk-node-jvm/gradle.properties`
3. Run the above command to build all bindings
4. Open a PR with the changes
5. Create a new GitHub release with a new tag like `v0.1.0`, uploading the following files:
   - `bindings/swift/LDKNodeFFI.xcframework.zip`
