// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "LocalRouter",
    platforms: [.macOS(.v15)],
    targets: [
        .executableTarget(name: "LocalRouter", path: "Sources/LocalRouter")
    ]
)
