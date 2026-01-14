// swift-tools-version:5.5
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let tag = "v0.7.0-rc.7"
let checksum = "cedb543665007c982eceb84bf8db7be6e0c1f32f36822f3f3e8485ce03030cb4"
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
