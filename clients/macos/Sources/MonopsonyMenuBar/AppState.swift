// AppState.swift — ObservableObject bridging the menu bar UI to the Rust daemon.
//
// Thin shell, no new protocol work: everything goes through
//   * `monopsony-client` CLI (status --quiet, proxy status, setup mac --status,
//     update-scan; system-proxy subcommands)
//   * `launchctl bootstrap/bootout gui/$UID` (modern syntax, macOS 10.11+;
//     m2mini bench is 26.3.1 so no legacy load/unload fallback is needed)
// using the LaunchAgent template in Resources/com.societus.monopsony-client.plist.
//
// Pure parsing/formatting logic lives in Core.swift (unit-tested); this file
// owns process spawning, timers, and published state only.
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
    /// Client cert/state dir — matches the Rust client's default_cert_dir.
    /// server.json (written at pairing) is the source of truth for the
    /// server host (slice 3).
    static let certDir = NSString(string: "~/.config/monopsony").expandingTildeInPath
    static var serverJSONPath: String { certDir + "/server.json" }
    static let launchAgentsDir = NSString(string: "~/Library/LaunchAgents").expandingTildeInPath
    static var agentPlistPath: String { launchAgentsDir + "/\(agentLabel).plist" }
}

struct ClientStatus {
    var server: String?
    var connectedClients: Int?
    /// Structured HIT-ratio state (slice 5) — nil ratio = daemon has no data.
    var hitRatio: HitRatioDisplay = HitRatioDisplay(ratio: nil, hits: 0, total: 0)
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
    /// Server identity resolved from pairing state (slice 3).
    @Published var serverIdentity: ServerIdentity?
    /// Per-service proxy rows parsed from `proxy status` (slice 4).
    @Published var proxyRows: [ProxyServiceRow] = []
    /// Apple Content Caching state from `setup mac --status` (slice 1 status).
    @Published var contentCacheOn = false
    @Published var contentCacheDetail = ""

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
        // Slice 3: server identity from the pairing file (no process spawn).
        if let data = FileManager.default.contents(atPath: MonopsonyConfig.serverJSONPath) {
            serverIdentity = ServerIdentity.parse(serverJSON: data)
        } else {
            serverIdentity = nil
        }

        // Slice 5 contract: `status --quiet` prints only key: value lines
        // (tracing suppressed CLI-side). Parse defensively regardless.
        let output = runClient(["status", "--quiet"])
        status = Self.parseStatus(output)

        // Slice 4: per-service rows from `proxy status`.
        let proxyOutput = runClient(["proxy", "status"])
        let report = ProxyStatusParser.parse(proxyOutput)
        proxyRows = report.rows

        // Slice 1 (status half): Content Caching on/off, unprivileged probe.
        let ccOutput = runClient(["setup", "mac", "--status"])
        (contentCacheOn, contentCacheDetail) = Self.parseContentCacheStatus(ccOutput)

        isAgentRunning = Self.agentIsRunning()
        systemProxyEnabled = refreshSystemProxy()
    }

    /// Defensive parse of `monopsony-client status --quiet` output.
    /// Expected lines: `Server: ...`, `Cache HIT ratio: NN.N%`.
    /// Anything unparseable keeps the raw lines on screen.
    /// nonisolated: pure function, no shared state — callable from the
    /// self-test runner and any actor context.
    nonisolated static func parseStatus(_ output: String) -> ClientStatus {
        var result = ClientStatus()
        let lines = output.split(separator: "\n").map(String.init)
        result.rawLines = lines
        if lines.isEmpty {
            result.parseFailed = true
            return result
        }
        for line in lines {
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
                result.hitRatio = HitRatioDisplay.from(statusOutput: line)
            default:
                break
            }
        }
        if result.server == nil && result.connectedClients == nil
            && result.hitRatio.ratio == nil {
            result.parseFailed = true
        }
        return result
    }

    /// Parse `setup mac --status` output. Row shape (main.rs):
    /// `  cache         ON  ✓  active — shared content only ...`.
    /// nonisolated: pure function (see parseStatus).
    nonisolated static func parseContentCacheStatus(_ output: String) -> (Bool, String) {
        for rawLine in output.split(separator: "\n") {
            let line = rawLine.trimmingCharacters(in: .whitespaces)
            if line.hasPrefix("cache") {
                if line.contains("ON") {
                    let afterMark = line.drop(while: { $0 != "✓" && $0 != "✗" })
                    let detail = afterMark.dropFirst()
                        .trimmingCharacters(in: .whitespaces)
                    return (true, detail.isEmpty ? "on" : detail)
                }
                if line.contains("OFF") {
                    return (false, "off")
                }
            }
        }
        return (false, "status unavailable")
    }

    var statusHeadline: String {
        if let err = lastError {
            return "Error: \(err)"
        }
        if isAgentRunning {
            return "Connected — \(status.server ?? serverIdentity?.host ?? "unknown server")"
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
        lines.append("Cache HIT ratio: \(status.hitRatio.label)")
        return lines
    }

    /// Slice 6: icon state for the menu bar label.
    var iconState: MenuIcon {
        if lastError != nil { return .error }
        return isAgentRunning ? .connected : .disconnected
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
        if output.isEmpty {
            lastError = "proxy system \(verb): no output from client"
        }
        systemProxyEnabled = refreshSystemProxy()
    }

    private func refreshSystemProxy() -> Bool {
        // Skeleton-honest probe: ask the CLI for status and look for a
        // positive marker. Structured per-service rows come from `proxy status`
        // (slice 4); this probes the system-proxy lane specifically.
        let output = runClient(["proxy", "system", "status"])
        return output.lowercased().contains("forwarded") && !output.lowercased().contains("disabled")
    }

    // MARK: - Dashboard

    /// Open the server dashboard (slice 3: host from pairing state, not a
    /// constant). Refuses silently-with-error when unpaired or the URL is
    /// not constructable — no force-unwrap crash path.
    func openDashboard() {
        lastError = nil
        guard let identity = serverIdentity else {
            lastError = "Not paired — no server.json at \(MonopsonyConfig.serverJSONPath)"
            return
        }
        guard let url = identity.dashboardURL else {
            lastError = "Cannot build dashboard URL from \(identity.host)"
            return
        }
        NSWorkspace.shared.open(url)
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
    /// Internal: views call this directly for one-shot actions (update-scan).
    func runClient(_ args: [String]) -> String {
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
