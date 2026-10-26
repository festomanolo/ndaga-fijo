// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "NdagaFijo",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "NdagaFijo", path: "Sources/NdagaFijo")
    ]
)
