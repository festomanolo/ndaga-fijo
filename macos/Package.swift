// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Rufus",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "Rufus", path: "Sources/Rufus")
    ]
)
