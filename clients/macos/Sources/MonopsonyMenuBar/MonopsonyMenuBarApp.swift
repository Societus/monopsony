// MonopsonyMenuBarApp.swift — app entry point.
//
// Tailscale-style menu-bar app: a single MenuBarExtra (macOS 13+) with a
// dropdown window. All logic lives in AppState/Core; this file is presentation
// only. The menu-bar icon swaps with connection state (slice 6); per-service
// rows (slice 4) and the HIT ratio (slice 5) render from parsed CLI output.
import SwiftUI

@main
struct MonopsonyMenuBarApp: App {
    @StateObject private var appState = AppState()

    init() {
        // CI gate (slice 7): `MonopsonyMenuBar --selftest` runs the
        // pure-logic test suite and exits — no AppKit event loop needed.
        // The CLT toolchain has no XCTest, so this binary mode replaces
        // `swift test` (bench-verified: XCTest absent on CLT-only).
        if CommandLine.arguments.contains("--selftest") {
            let failures = SelfTest.runAll()
            for f in failures {
                print("FAIL \(f)")
            }
            print(failures.isEmpty ? "SELFTEST PASS" : "SELFTEST FAIL (\(failures.count) failures)")
            exit(failures.isEmpty ? 0 : 1)
        }
    }

    var body: some Scene {
        MenuBarExtra {
            MenuContentView()
                .environmentObject(appState)
        } label: {
            // Slice 6: state-aware icon — bolt when connected, slashed bolt
            // when not, warning triangle on errors.
            Image(systemName: appState.iconState.symbolName)
                .foregroundStyle(appState.iconState == .connected ? Color.green : Color.gray)
        }
        .menuBarExtraStyle(.window)
    }
}

struct MenuContentView: View {
    @EnvironmentObject private var appState: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(appState.statusHeadline)
                .font(.headline)
            ForEach(appState.statusDetail, id: \.self) { line in
                Text(line)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .help(line) // full raw line on hover
            }

            // Slice 4: per-service proxy rows from `proxy status`.
            if !appState.proxyRows.isEmpty {
                Divider()
                ForEach(appState.proxyRows, id: \.subsystem) { row in
                    HStack(spacing: 4) {
                        Image(systemName: row.enabled ? "checkmark.circle.fill" : "xmark.circle")
                            .foregroundStyle(row.enabled ? Color.green : Color.secondary)
                            .font(.caption2)
                        Text(row.subsystem)
                            .font(.caption)
                            .frame(width: 90, alignment: .leading)
                        Text(row.detail)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.tail)
                            .help(row.detail)
                    }
                }
            }

            if let error = appState.lastError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
            }

            Divider()

            Button(appState.isAgentRunning ? "Disconnect" : "Connect") {
                if appState.isAgentRunning {
                    appState.stopAgent()
                } else {
                    appState.startAgent()
                }
            }
            .keyboardShortcut("c")

            Divider()

            Text("Configure")
                .font(.caption)
                .foregroundStyle(.secondary)

            Toggle("System proxy forwarding", isOn: Binding(
                get: { appState.systemProxyEnabled },
                set: { appState.setSystemProxy($0) }
            ))

            // Slice 1 status half is live (from `setup mac --status`); the
            // enable/disable action needs sudo via GUI elevation — banked
            // for in-person bench verification (sudo cannot prompt through
            // a menu-bar child process without an osascript wrapper).
            HStack(spacing: 4) {
                Image(systemName: appState.contentCacheOn ? "checkmark.circle.fill" : "xmark.circle")
                    .foregroundStyle(appState.contentCacheOn ? Color.green : Color.secondary)
                    .font(.caption2)
                Text("Content Caching: \(appState.contentCacheDetail)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .help(appState.contentCacheDetail)
            }

            // Slice 2 (reframed): the only live SUS scan verb on current
            // macOS. Whether the scan's traffic routes through Monopsony is
            // the bench experiment, not a code guarantee.
            Button("Trigger Update Scan") {
                appState.lastError = nil
                let output = appState.runClient(["update-scan"])
                if output.isEmpty {
                    appState.lastError = "update-scan failed (see CLI)"
                }
            }
            .keyboardShortcut("u")

            Divider()

            Button("Open Dashboard") {
                appState.openDashboard()
            }
            .keyboardShortcut("d")

            Button("Quit Monopsony") {
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
        .padding(8)
        .frame(width: 300)
        .onAppear {
            appState.menuOpened()
        }
        .onDisappear {
            appState.menuClosed()
        }
    }
}
