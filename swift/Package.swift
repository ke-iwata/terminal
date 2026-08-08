// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "keterm",
    platforms: [.macOS(.v14)],
    targets: [
        // Everything that isn't drawing or AppKit lives here: the VT
        // parser, the screen model, the pty, and the file/preview
        // helpers. Kept free of UI so all of it can be tested without a
        // window -- the same split the Rust version used, and the reason
        // its terminal emulation had tests at all.
        .target(name: "KetermCore"),
        .executableTarget(name: "keterm", dependencies: ["KetermCore"]),
        .testTarget(name: "KetermCoreTests", dependencies: ["KetermCore"]),
    ]
)
