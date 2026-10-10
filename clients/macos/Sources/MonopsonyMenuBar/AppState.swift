// AppState.swift — ObservableObject bridging the menu bar UI to the Rust daemon.
//
// Thin shell, no new protocol work: everything goes through
//   * `monopsony-client` CLI (status output, system-proxy subcommands)
//   * `launchctl bootstrap/bootout gui/$UID` (modern syntax, macOS 10.11+;
//     m2mini bench is 26.3.1 so no legacy load/unload fallback is needed)
// using the LaunchAgent template in Resources/com.societus.monopsony-client.plist.
import Foundation
import AppKit
import Combine

/// Paths and identifiers for the shell-out surface. Kept in one place so the
/// LaunchAgent plist, README, and code stay in sync.
enum MonopsonyConfig {
    /// LaunchAgent label — must match Label in the plist template.
    static let agentLabel = "com.societus.monopsony-client"
    /// Client binary location on the bench (target/release build). The
    /// LaunchAgent template carries a placeholder; real installs replace it.
    static let clientBinary = NSString(string: "~/.local/bin/monopsony-client").expandingTildeInPath
    /// Dashboard URL (server control plane).
    static var dashboardURL: URL { URL(string: "http://\(serverHost):3130")! }
    /// Server host. TODO(b4-followup): read from the client's saved config
    /// (cert dir / pairing state) instead of this constant.
    static let serverHost = "192.168.1.10"
    static let launchAgentsDir = NSString(string: "~/Library/LaunchAgents").expandingTildeInPath
    static var agentPlistPath: String { launchAgentsDir + "/\(agentLabel).plist" }
}

struct ClientStatus {
    var server: String?
    var connectedClients: Int?
    var hitRatio: Double?
    /// Verbatim CLI output — shown if structured parsing fails.
    var rawLines: [String] = []
    var parseFailed = false
}

@MainActor
final class AppState: ObservableObject {
    @Published var status = ClientStatus()
    @Published var isAgentRunning = false
    @Published var systemProxyEnabled = false
    @Published var lastError: String?

    private var pollTimer: Timer?

    // MARK: - Menu lifecycle (poll only while the menu is open)

    /// Cheap-on-demand: refresh once when the menu opens, then poll on a
    /// 5s timer only for as long as the menu stays open. Closed menu = zero CPU.
    func menuOpened() {
        refresh()
        let timer = Timer(timeInterval: 5.0, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in self?.refresh() }
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    func menuClosed() {
        pollTimer?.invalidate()
        pollTimer = nil
    }

    // MARK: - Status

    func refresh() {
        let output = runClient(["status"])
        status = Self.parseStatus(output)
        isAgentRunning = Self.agentIsRunning()
        systemProxyEnabled = refreshSystemProxy()
    }

    /// Defensive parse of `monopsony-client status` plain-text output.
    /// The CLI today prints "Certificates: ..." / "Package managers detected: N"
    /// lines; richer fields (server, connected clients, HIT ratio) arrive as the
    /// daemon grows them. Anything unparseable keeps the raw lines on screen.
    static func parseStatus(_ output: String) -> ClientStatus {
        var result = ClientStatus()
        let lines = output.split(separator: "\n").map(String.init)
        result.rawLines = lines
        if lines.isEmpty {
            result.parseFailed = true
            return result
        }
        for line in lines {
            // Expected shapes (best effort, case-insensitive label match):
            //   Server: 192.168.1.10
            //   Connected clients: 3
            //   Cache HIT ratio: 87.4%
            let parts = line.split(separator: ":", maxSplits: 1)
            guard parts.count == 2 else { continue }
            let key = parts[0].trimmingCharacters(in: .whitespaces).lowercased()
            let value = parts[1].trimmingCharacters(in: .whitespaces)
            switch key {
            case "server":
                result.server = value
            case "connected clients":
                result.connectedClients = Int(value)
            case "cache hit ratio":
                if value.hasSuffix("%") {
                    result.hitRatio = Double(value.dropLast())
                } else {
                    result.hitRatio = Double(value)
                }
            default:
                break
            }
        }
        // If nothing structured was recognized, surface the raw output instead.
        if result.server == nil && result.connectedClients == nil && result.hitRatio == nil {
            result.parseFailed = true
        }
        return result
    }

    var statusHeadline: String {
        if let err = lastError {
            return "Error: \(err)"
        }
        if isAgentRunning {
            return "Connected — \(status.server ?? "unknown server")"
        }
        return "Disconnected"
    }

    var statusDetail: [String] {
        if status.parseFailed {
            // Show the raw CLI line(s) verbatim on parse failure (spec).
            return status.rawLines.isEmpty ? ["(no status output)"] : status.rawLines
        }
        var lines: [String] = []
        if let clients = status.connectedClients {
            lines.append("Connected clients: \(clients)")
        }
        if let ratio = status.hitRatio {
            lines.append(String(format: "Cache HIT ratio: %.1f%%", ratio))
        }
        return lines
    }

    // MARK: - Connect / Disconnect via LaunchAgent

    func startAgent() {
        lastError = nil
        guard FileManager.default.fileExists(atPath: MonopsonyConfig.clientBinary) else {
            lastError = "Client binary missing: \(MonopsonyConfig.clientBinary)"
            return
        }
        // Install the LaunchAgent plist from our bundled template, then
        // bootstrap it into the per-user gui domain.
        guard installAgentPlist() else { return }
        if Self.agentIsRunning() { // already bootstrapped — restart cleanly
            _ = runLaunchctl(["bootout", "gui/\(getuid())/\(MonopsonyConfig.agentLabel)"])
        }
        let rc = runLaunchctl(["bootstrap", "gui/\(getuid())", MonopsonyConfig.agentPlistPath])
        if rc != 0 {
            lastError = "launchctl bootstrap failed (exit \(rc))"
        }
        refresh()
    }

    func stopAgent() {
        lastError = nil
        let rc = runLaunchctl(["bootout", "gui/\(getuid())/\(MonopsonyConfig.agentLabel)"])
        if rc != 0 {
            lastError = "launchctl bootout failed (exit \(rc))"
        }
        refresh()
    }

    /// True when launchctl lists our label in the caller's gui domain.
    static func agentIsRunning() -> Bool {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        proc.arguments = ["print", "gui/\(getuid())/\(MonopsonyConfig.agentLabel)"]
        let pipe = Pipe()
        proc.standardOutput = Pipe() // discard
        proc.standardError = pipe
        do {
            try proc.run()
            proc.waitUntilExit()
            return proc.terminationStatus == 0
        } catch {
            return false
        }
    }

    /// Copy the bundled plist template into ~/Library/LaunchAgents, filling in
    /// the real binary path. Returns false (and sets lastError) on failure.
    private func installAgentPlist() -> Bool {
        guard let bundled = Bundle.module.url(forResource: "com.societus.monopsony-client", withExtension: "plist") else {
            lastError = "Bundled LaunchAgent template not found"
            return false
        }
        do {
            let fm = FileManager.default
            try fm.createDirectory(atPath: MonopsonyConfig.launchAgentsDir, withIntermediateDirectories: true)
            var plist = try String(contentsOf: bundled, encoding: .utf8)
            plist = plist.replacingOccurrences(of: "__CLIENT_BINARY__", with: MonopsonyConfig.clientBinary)
            try plist.write(toFile: MonopsonyConfig.agentPlistPath, atomically: true, encoding: .utf8)
            return true
        } catch {
            lastError = "plist install failed: \(error.localizedDescription)"
            return false
        }
    }

    // MARK: - Per-PM toggles

    func setSystemProxy(_ on: Bool) {
        lastError = nil
        let verb = on ? "on" : "off"
        let output = runClient(["proxy", "system", verb])
        // TODO(b4-followup): parse the per-service rows the CLI prints and
        // surface partial failures per network service.
        if output.isEmpty {
            lastError = "proxy system \(verb): no output from client"
        }
        systemProxyEnabled = refreshSystemProxy()
    }

    private func refreshSystemProxy() -> Bool {
        // Skeleton-honest probe: ask the CLI for status and look for a
        // positive marker. TODO(b4-followup): structured subcommand output.
        let output = runClient(["proxy", "system", "status"])
        return output.lowercased().contains("forwarded") && !output.lowercased().contains("disabled")
    }

    // MARK: - Dashboard

    func openDashboard() {
        NSWorkspace.shared.open(MonopsonyConfig.dashboardURL)
    }

    // MARK: - Process helpers

    @discardableResult
    private func runLaunchctl(_ args: [String]) -> Int32 {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        proc.arguments = args
        proc.standardOutput = Pipe()
        proc.standardError = Pipe()
        do {
            try proc.run()
            proc.waitUntilExit()
            return proc.terminationStatus
        } catch {
            lastError = "launchctl \(args.first ?? "") failed: \(error.localizedDescription)"
            return -1
        }
    }

    /// Shell out to the monopsony-client binary; returns combined stdout.
    private func runClient(_ args: [String]) -> String {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: MonopsonyConfig.clientBinary)
        proc.arguments = args
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = Pipe()
        do {
            try proc.run()
            // Bound the wait: status calls must never wedge the menu.
            let deadline = Date().addingTimeInterval(10)
            while proc.isRunning && Date() < deadline {
                RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.1))
            }
            if proc.isRunning { // still running past deadline — don't leave a zombie loop
                proc.terminate()
            }
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            return String(data: data, encoding: .utf8) ?? ""
        } catch {
            lastError = "monopsony-client \(args.joined(separator: " ")): \(error.localizedDescription)"
            return ""
        }
    }
}
