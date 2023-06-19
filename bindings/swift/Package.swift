// swift-tools-version:5.5
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let package = Package(
    name: "ldk-node",
    platforms: [
        .macOS(.v12),
        .iOS(.v15)
    ],
    products: [
        // Products define the executables and libraries a package produces, and make them visible to other packages.
        .library(
            name: "LDKNode",
            targets: ["LDKNodeFFI", "LDKNode"]),
    ],
    dependencies: [
        // Dependencies declare other packages that this package depends on.
        // .package(url: /* package url */, from: "1.0.0"),
    ],
    targets: [
        // Targets are the basic building blocks of a package. A target can define a module or a test suite.
        // Targets can depend on other targets in this package, and on products in packages this package depends on.
//        .binaryTarget(
//            name: "LDKNodeFFI",
//            url: "https://github.com/lightningdevkit/ldk-node/releases/download/0.3.0/LDKNodeFFI.xcframework.zip",
//            checksum: "<TBD>"),
        .binaryTarget(name: "LDKNodeFFI", path: "./LDKNodeFFI.xcframework"),
        .target(
            name: "LDKNode",
            dependencies: ["LDKNodeFFI"]),
//        .testTarget(
//            name: "LightningDevKitNodeTests",
//            dependencies: ["LightningDevKitNode"]),
    ]
)
