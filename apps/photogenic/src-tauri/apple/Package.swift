// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "PhotogenicPhotos",
    platforms: [.macOS(.v14), .iOS(.v17)],
    products: [.library(name: "PhotogenicPhotos", type: .static, targets: ["PhotogenicPhotos"])],
    targets: [
        .target(
            name: "PhotogenicPhotos",
            path: "Sources",
            linkerSettings: [.linkedFramework("Photos")]
        ),
    ]
)
