// swift-tools-version:5.9
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let tag = "v0.7.0-rc.59"
let checksum = "b6d27faadc0f62ebdf040a64eba468d5b2ce9b4891bc47c0d22f42837665900d"
let url = "https://github.com/synonymdev/ldk-node/releases/download/\(tag)/LDKNodeFFI.xcframework.zip"

let package = Package(
    name: "ldk-node",
    platforms: [
        .iOS(.v15),
        .macOS(.v12),
    ],
    products: [
        // Products define the executables and libraries a package produces, and make them visible to other packages.
        .library(
            name: "LDKNode",
            targets: ["LDKNodeFFI", "LDKNode"]),
    ],
    targets: [
        .target(
            name: "LDKNode",
            dependencies: ["LDKNodeFFI"],
            path: "./bindings/swift/Sources"
        ),
        .binaryTarget(
            name: "LDKNodeFFI",
            url: url,
            checksum: checksum
            )
    ]
)
