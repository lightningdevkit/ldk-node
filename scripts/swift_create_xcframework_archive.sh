ditto -c -k --sequesterRsrc --keepParent ./bindings/swift/LDKNodeFFI.xcframework ./bindings/swift/LDKNodeFFI.xcframework.zip || exit 1
CHECKSUM=`swift package compute-checksum ./bindings/swift/LDKNodeFFI.xcframework.zip` || exit 1
echo "New checksum: $CHECKSUM" || exit 1
python3 ./scripts/swift_update_package_checksum.py --checksum "${CHECKSUM}" || exit 1
