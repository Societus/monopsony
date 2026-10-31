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

            // Slice 1: status is the unprivileged `setup mac --status` probe;
            // the Enable/Disable action rides the bench-verified osascript
            // admin wrapper (notes/2026-10-31-b4-cc-toggle-prototype.md) —
            // exactly one password dialog per action, presented from this
            // GUI-domain process. One console approval on the bench still
            // pending; fallback (documented) is status-only + Terminal hint.
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
                Spacer()
                if appState.isTogglingContentCache {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    // Label mirrors current state; no keyboard shortcut — an
                    // accidental keystroke must not pop a sudo dialog.
                    Button(appState.contentCacheOn ? "Disable" : "Enable") {
                        appState.setContentCaching(!appState.contentCacheOn)
                    }
                    .font(.caption)
                }
            }

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
