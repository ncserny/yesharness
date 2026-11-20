// swift-tools-version: 6.0
// yh-dictate: YesHarness's speech helper. Apple's on-device recognizer (SpeechAnalyzer), or NVIDIA's
// Parakeet Ultra running on the Neural Engine through FluidAudio.
import Foundation
import PackageDescription

let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path

let package = Package(
    name: "yh-dictate",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/FluidInference/FluidAudio.git", exact: "0.18.4"),
    ],
    targets: [
        .executableTarget(
            name: "yh-dictate",
            dependencies: [.product(name: "FluidAudio", package: "FluidAudio")],
            path: "Sources/yh-dictate",
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                // Embed Info.plist (microphone / speech usage strings) into the command-line binary.
                .unsafeFlags(["-Xlinker", "-sectcreate", "-Xlinker", "__TEXT", "-Xlinker", "__info_plist", "-Xlinker", here + "/Info.plist"]),
            ]
        ),
    ]
)
