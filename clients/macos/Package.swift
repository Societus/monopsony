// swift-tools-version:5.9
// Monopsony macOS menu-bar client — thin shell over the Rust `monopsony-client` daemon.
// Zero third-party dependencies: AppKit/SwiftUI only, so Command Line Tools can
// build this package fully offline (no Xcode required).
//
// Testing: the CLT-only toolchain has no XCTest module (bench-verified on
// macOS 26.3.1), so there is no SPM testTarget. The pure-logic suite runs via
// `MonopsonyMenuBar --selftest` (SelfTest.swift) — CI gates on that instead
// of `swift test`.
import PackageDescription

let package = Package(
    name: "monopsony-macos",
    platforms: [
        .macOS(.v13)
    ],
    targets: [
        .executableTarget(
            name: "MonopsonyMenuBar",
            // Target root is the package dir so the spec-mandated layout
            // (sources under Sources/, LaunchAgent template under Resources/)
            // stays intact and Bundle.module can serve the plist template.
            // Note: exclude must precede sources/resources in this initializer.
            path: ".",
            exclude: ["README.md", "Tests"],
            sources: ["Sources/MonopsonyMenuBar"],
            resources: [
                .copy("Resources/com.societus.monopsony-client.plist")
            ]
        )
    ]
)
