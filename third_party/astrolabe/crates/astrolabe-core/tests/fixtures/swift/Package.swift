// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Fixture",
    products: [.library(name: "Kit", targets: ["Kit"])],
    dependencies: [.package(url: "https://example.com/dep.git", from: "1.0.0")],
    targets: [
        .target(name: "Kit", dependencies: ["Util", .product(name: "Dep", package: "dep")]),
        .target(name: "Util"),
        .target(name: "Lib", path: "Custom/Lib"),
        .executableTarget(name: "App", dependencies: ["Kit"]),
        .testTarget(name: "KitTests", dependencies: ["Kit"]),
    ]
)
