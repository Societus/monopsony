// Core.swift — pure logic for the Monopsony menu bar app.
//
// Everything here is deterministic and unit-testable without spawning
// processes: config parsing (slice 3), per-service row parsing (slice 4),
// HIT-ratio formatting (slice 5), and status parsing hardening. AppState
// calls into these; the views stay dumb.

import Foundation

/// Server connection identity resolved from the client's pairing state
/// (slice 3). Resolution order mirrors the Rust daemon's
/// `resolve_server_addr`: `ws_url` first (tunnel deployments), then
/// `grpc_addr`, from `~/.config/monopsony/server.json` (cert dir).
struct ServerIdentity: Equatable {
    /// Raw address (host:port or URL) exactly as recorded at pairing time.
    let address: String
    /// Host portion for dashboard URL construction / display.
    let host: String
    /// True when the address came from `ws_url` (Cloudflare-tunnel style).
    let isTunnel: Bool

    /// Parse the client's server.json. `data` is the file's raw bytes;
    /// returns nil when missing/corrupt/unpaired (caller decides fallback).
    static func parse(serverJSON data: Data) -> ServerIdentity? {
        guard let obj = try? JSONSerialization.jsonObject(with: data),
              let dict = obj as? [String: Any] else { return nil }
        // ws_url preferred, exactly like resolve_server_addr in main.rs.
        if let ws = dict["ws_url"] as? String, !ws.isEmpty {
            return ServerIdentity(address: ws, host: ServerIdentity.hostOf(ws), isTunnel: true)
        }
        if let grpc = dict["grpc_addr"] as? String, !grpc.isEmpty {
            return ServerIdentity(address: grpc, host: ServerIdentity.hostOf(grpc), isTunnel: false)
        }
        return nil
    }

    /// Host extraction tolerant of scheme and port: "https://a.b/c" → a.b,
    /// "10.0.0.5:3128" → 10.0.0.5, "wss://x/y" → x.
    static func hostOf(_ raw: String) -> String {
        if let url = URL(string: raw), let host = url.host, !host.isEmpty {
            return host
        }
        // host:port without scheme — strip the port.
        if let colon = raw.firstIndex(of: ":") {
            return String(raw[..<colon])
        }
        return raw
    }

    /// Dashboard URL from the resolved host (server dashboard port 3130).
    /// Force-free: hostOf output is always a valid host component unless the
    /// pairing file was hand-corrupted; callers route through
    /// `dashboardURLValid` when they need a checked variant.
    var dashboardURL: URL? {
        URL(string: "http://\(host):3130")
    }
}

/// One per-service proxy row (slice 4). Mirrors the CLI's
/// `proxy status` subsystem rows: `  <subsystem padded> ON  ✓  <detail>`.
struct ProxyServiceRow: Equatable {
    let subsystem: String
    let enabled: Bool
    let detail: String
}

/// Parsed `monopsony-client proxy status` output: server line + rows.
struct ProxyStatusReport: Equatable {
    var server: String?
    var pairedAt: String?
    var rows: [ProxyServiceRow] = []
}

enum ProxyStatusParser {
    /// Defensive parse of `proxy status` text. Unrecognized lines are
    /// dropped (the report degrades; it never fabricates state).
    static func parse(_ output: String) -> ProxyStatusReport {
        var report = ProxyStatusReport()
        for rawLine in output.split(separator: "\n") {
            let line = rawLine.trimmingCharacters(in: .whitespaces)
            if line.isEmpty { continue }
            if line.hasPrefix("Server:") {
                report.server = line.dropFirst("Server:".count)
                    .trimmingCharacters(in: .whitespaces)
                continue
            }
            if line.hasPrefix("Paired at:") {
                report.pairedAt = line.dropFirst("Paired at:".count)
                    .trimmingCharacters(in: .whitespaces)
                continue
            }
            if let row = parseRow(line) {
                report.rows.append(row)
            }
        }
        return report
    }

    /// One subsystem row: `<name> ON  ✓ <detail>` / `<name> OFF ✗ <detail>`.
    /// The name column is %-padded, so split on whitespace runs.
    static func parseRow(_ line: String) -> ProxyServiceRow? {
        // The ✓/✗ mark is the row's spine: without it, it's not a row.
        guard line.contains("✓") || line.contains("✗") else { return nil }
        let enabled = line.contains("ON")
        let parts = line.split(whereSeparator: { $0 == " " }).map(String.init)
        guard let name = parts.first, !name.isEmpty else { return nil }
        // Detail = everything after the mark token; join back.
        if let markIdx = parts.firstIndex(where: { $0 == "✓" || $0 == "✗" }),
           markIdx + 1 < parts.count {
            return ProxyServiceRow(
                subsystem: name, enabled: enabled,
                detail: parts[(markIdx + 1)...].joined(separator: " "))
        }
        return ProxyServiceRow(subsystem: name, enabled: enabled, detail: "")
    }
}

/// HIT-ratio display state (slice 5). Distinguishes "no data" from a real
/// 0% — the menu shows "n/a" only when the daemon never observed traffic.
struct HitRatioDisplay: Equatable {
    let ratio: Double?
    let hits: Int
    let total: Int

    static func from(statusOutput: String) -> HitRatioDisplay {
        for rawLine in statusOutput.split(separator: "\n") {
            let line = rawLine.trimmingCharacters(in: .whitespaces)
            guard line.lowercased().hasPrefix("cache hit ratio:") else { continue }
            let value = line.dropFirst("cache hit ratio:".count)
                .trimmingCharacters(in: .whitespaces)
            var numStr = value
            if numStr.hasSuffix("%") { numStr = String(numStr.dropLast()) }
            if let r = Double(numStr) {
                // hits/total are not emitted by the CLI; the ratio is the
                // contract. Total stays 0 to signal "ratio only".
                return HitRatioDisplay(ratio: r, hits: 0, total: 0)
            }
        }
        return HitRatioDisplay(ratio: nil, hits: 0, total: 0)
    }

    /// "87.4%" or "n/a" when the daemon has no observations yet.
    var label: String {
        guard let r = ratio else { return "n/a" }
        return String(format: "%.1f%%", r)
    }
}

/// Menu icon selection (slice 6). Pure so tests can pin the mapping.
enum MenuIcon {
    case connected
    case disconnected
    case error

    /// SF Symbol name for the menu bar label.
    var symbolName: String {
        switch self {
        case .connected: return "bolt.fill"
        case .disconnected: return "bolt.slash"
        case .error: return "exclamationmark.triangle.fill"
        }
    }
}
