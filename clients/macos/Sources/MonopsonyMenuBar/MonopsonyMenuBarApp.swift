// MonopsonyMenuBarApp.swift — app entry point.
//
// Tailscale-style menu-bar app: a single MenuBarExtra (macOS 13+) with a
// dropdown window. All logic lives in AppState; this file is presentation only.
import SwiftUI

@main
struct MonopsonyMenuBarApp: App {
    @StateObject private var appState = AppState()

    var body: some Scene {
        MenuBarExtra {
            MenuContentView()
                .environmentObject(appState)
        } label: {
            // TODO(b4-followup): swap icon based on connection state
            // (e.g. bolt.fill when connected, bolt.slash when disconnected).
            Image(systemName: "arrow.triangle.swap")
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

            // TODO(b4-followup): wire Homebrew env caching to the client CLI
            // (`setup mac` family). Placeholder until the exact subcommand
            // contract is settled.
            Button("Homebrew env caching… (TODO)") {}
                .disabled(true)

            // TODO(b4-followup): wire catalog scan/refresh to the client CLI
            // (`catalog` subcommand family).
            Button("Catalog refresh… (TODO)") {}
                .disabled(true)

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
