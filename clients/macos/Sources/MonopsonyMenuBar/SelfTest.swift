// SelfTest.swift — in-module self-test for the pure-logic layer.
//
// The CLT-only toolchain has NO XCTest module (verified on the m2mini bench,
// macOS 26.3.1: `swift test` fails with "no such module 'XCTest'"), so tests
// run as `MonopsonyMenuBar --selftest`: each check returns nil on pass or a
// failure message; the runner prints PASS/FAIL and exits nonzero on failure.
// CI runs this instead of `swift test` (slice 7).

import Foundation

enum SelfTest {
    /// Run all checks. Returns the list of failures (empty = all pass).
    static func runAll() -> [String] {
        var failures: [String] = []
        let checks: [(String, () -> String?)] = [
            // MARK: Slice 3 — server identity from server.json
            ("serverIdentity.prefersWSURL", testServerIdentityPrefersWSURL),
            ("serverIdentity.fallsBackToGrpcAddr", testServerIdentityFallsBackToGrpcAddr),
            ("serverIdentity.nilWhenUnpaired", testServerIdentityNilWhenUnpaired),
            ("serverIdentity.hostOf", testHostOf),
            ("serverIdentity.dashboardURL", testDashboardURL),
            // MARK: Slice 4 — proxy status rows
            ("proxyStatus.serverAndRows", testProxyStatusServerAndRows),
            ("proxyStatus.ignoresHeaderNoise", testProxyStatusIgnoresHeaderNoise),
            ("proxyStatus.emptyDetail", testProxyStatusEmptyDetail),
            // MARK: Slice 5 — HIT ratio
            ("hitRatio.fromQuietStatus", testHitRatioFromQuietStatus),
            ("hitRatio.missingMeansNA", testHitRatioMissingMeansNA),
            ("hitRatio.noPercentSign", testHitRatioNoPercentSign),
            // MARK: Slice 1 status half — Content Caching
            ("contentCache.on", testContentCacheOn),
            ("contentCache.off", testContentCacheOff),
            ("contentCache.unparseable", testContentCacheUnparseable),
            // MARK: Slice 6 — icon mapping
            ("icon.mapping", testIconMapping),
            // MARK: parseStatus regression (existing contract, quiet shape)
            ("parseStatus.quietShape", testParseStatusQuietShape),
            ("parseStatus.emptyIsParseFailed", testParseStatusEmptyIsParseFailed),
            ("parseStatus.rawLinesPreserved", testParseStatusRawLinesPreserved),
        ]
        for (name, check) in checks {
            if let failure = check() {
                failures.append("\(name): \(failure)")
            }
        }
        return failures
    }

    // MARK: - Slice 3

    static func testServerIdentityPrefersWSURL() -> String? {
        let json = """
        {"grpc_addr": "10.10.20.58:50051", "ws_url": "wss://tunnel.example/monopsony",
         "paired_at": "2026-10-01T00:00:00Z"}
        """
        guard let id = ServerIdentity.parse(serverJSON: Data(json.utf8)) else {
            return "parse returned nil"
        }
        guard id.address == "wss://tunnel.example/monopsony" else { return "address \(id.address)" }
        guard id.host == "tunnel.example" else { return "host \(id.host)" }
        guard id.isTunnel else { return "isTunnel false" }
        return nil
    }

    static func testServerIdentityFallsBackToGrpcAddr() -> String? {
        let json = """
        {"grpc_addr": "10.10.20.58:50051", "paired_at": "2026-10-01T00:00:00Z"}
        """
        guard let id = ServerIdentity.parse(serverJSON: Data(json.utf8)) else {
            return "parse returned nil"
        }
        guard id.address == "10.10.20.58:50051" else { return "address \(id.address)" }
        guard id.host == "10.10.20.58" else { return "host \(id.host)" }
        guard !id.isTunnel else { return "isTunnel true" }
        return nil
    }

    static func testServerIdentityNilWhenUnpaired() -> String? {
        if ServerIdentity.parse(serverJSON: Data("{}".utf8)) != nil { return "{} parsed" }
        if ServerIdentity.parse(serverJSON: Data("not json".utf8)) != nil { return "junk parsed" }
        let empty = """
        {"grpc_addr": "", "ws_url": ""}
        """
        if ServerIdentity.parse(serverJSON: Data(empty.utf8)) != nil { return "empty strings won" }
        return nil
    }

    static func testHostOf() -> String? {
        guard ServerIdentity.hostOf("192.168.1.10:3128") == "192.168.1.10" else { return "host:port" }
        guard ServerIdentity.hostOf("m2mini.tailnet") == "m2mini.tailnet" else { return "bare host" }
        guard ServerIdentity.hostOf("https://a.b.c:8443/x") == "a.b.c" else { return "url" }
        return nil
    }

    static func testDashboardURL() -> String? {
        let json = """
        {"grpc_addr": "10.10.20.58:50051"}
        """
        guard let id = ServerIdentity.parse(serverJSON: Data(json.utf8)),
              let url = id.dashboardURL,
              url.absoluteString == "http://10.10.20.58:3130" else {
            return "dashboard URL wrong"
        }
        return nil
    }

    // MARK: - Slice 4

    static func testProxyStatusServerAndRows() -> String? {
        let out = """
        Monopsony Proxy Status
        ═══════════════════════════════════════════════
        Server:           10.10.20.58:50051
        Paired at:        2026-10-01

        pacman           ON  ✓  /etc/pacman.conf points at proxy
        apt              OFF ✗  no apt proxy config
        """
        let report = ProxyStatusParser.parse(out)
        guard report.server == "10.10.20.58:50051" else { return "server \(report.server ?? "nil")" }
        guard report.pairedAt == "2026-10-01" else { return "pairedAt \(report.pairedAt ?? "nil")" }
        guard report.rows.count == 2 else { return "rows \(report.rows.count)" }
        guard report.rows[0].subsystem == "pacman", report.rows[0].enabled,
              report.rows[0].detail == "/etc/pacman.conf points at proxy" else {
            return "row0 \(report.rows[0])"
        }
        guard report.rows[1].subsystem == "apt", !report.rows[1].enabled else {
            return "row1 \(report.rows[1])"
        }
        return nil
    }

    static func testProxyStatusIgnoresHeaderNoise() -> String? {
        let report = ProxyStatusParser.parse("Monopsony Proxy Status\n═════\n\nNo subsystems detected.")
        guard report.server == nil else { return "server leaked from noise" }
        guard report.rows.isEmpty else { return "rows from noise" }
        return nil
    }

    static func testProxyStatusEmptyDetail() -> String? {
        guard let row = ProxyStatusParser.parseRow("brew ON  ✓") else { return "row nil" }
        guard row.subsystem == "brew", row.enabled, row.detail == "" else { return "\(row)" }
        return nil
    }

    // MARK: - Slice 5

    static func testHitRatioFromQuietStatus() -> String? {
        let d = HitRatioDisplay.from(statusOutput: "Server: 10.0.0.1:50051\nCache HIT ratio: 87.4%\n")
        guard let r = d.ratio, abs(r - 87.4) < 0.001 else { return "ratio \(d.ratio ?? -1)" }
        guard d.label == "87.4%" else { return "label \(d.label)" }
        return nil
    }

    static func testHitRatioMissingMeansNA() -> String? {
        let d = HitRatioDisplay.from(statusOutput: "Server: 10.0.0.1:50051\n")
        guard d.ratio == nil else { return "ratio fabricated" }
        guard d.label == "n/a" else { return "label \(d.label)" }
        return nil
    }

    static func testHitRatioNoPercentSign() -> String? {
        let d = HitRatioDisplay.from(statusOutput: "Cache HIT ratio: 50")
        guard let r = d.ratio, abs(r - 50.0) < 0.001 else { return "ratio \(d.ratio ?? -1)" }
        return nil
    }

    // MARK: - Slice 1 status half

    static func testContentCacheOn() -> String? {
        let out = "Apple Content Caching (this Mac):\n  cache         ON  ✓  active — shared content only (personal/iCloud caching off)\n"
        let (on, detail) = AppState.parseContentCacheStatus(out)
        guard on else { return "on=false" }
        guard detail.contains("shared content only") else { return "detail \(detail)" }
        return nil
    }

    static func testContentCacheOff() -> String? {
        let out = "Apple Content Caching (this Mac):\n  cache         OFF ✗  deactivated\n"
        let (on, detail) = AppState.parseContentCacheStatus(out)
        guard !on else { return "on=true" }
        guard detail == "off" else { return "detail \(detail)" }
        return nil
    }

    static func testContentCacheUnparseable() -> String? {
        let (on, detail) = AppState.parseContentCacheStatus("garbage")
        guard !on, detail == "status unavailable" else { return "(\(on), \(detail))" }
        return nil
    }

    // MARK: - Slice 6

    static func testIconMapping() -> String? {
        guard MenuIcon.connected.symbolName == "bolt.fill" else { return "connected" }
        guard MenuIcon.disconnected.symbolName == "bolt.slash" else { return "disconnected" }
        guard MenuIcon.error.symbolName == "exclamationmark.triangle.fill" else { return "error" }
        return nil
    }

    // MARK: - parseStatus regression

    static func testParseStatusQuietShape() -> String? {
        let s = AppState.parseStatus("Server: 10.10.20.58:50051\nCache HIT ratio: 12.5%\n")
        guard s.server == "10.10.20.58:50051" else { return "server \(s.server ?? "nil")" }
        guard s.hitRatio.ratio != nil else { return "ratio nil" }
        guard !s.parseFailed else { return "parseFailed" }
        return nil
    }

    static func testParseStatusEmptyIsParseFailed() -> String? {
        guard AppState.parseStatus("").parseFailed else { return "empty not failed" }
        guard AppState.parseStatus("\n\n").parseFailed else { return "blank not failed" }
        return nil
    }

    static func testParseStatusRawLinesPreserved() -> String? {
        let s = AppState.parseStatus("something unexpected\nanother line\n")
        guard s.parseFailed else { return "unexpected content parsed" }
        guard s.rawLines.count == 2 else { return "rawLines \(s.rawLines.count)" }
        return nil
    }
}
