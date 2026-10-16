// MonopsonyMenuBarTests.swift — unit tests for the pure-logic layer (Core.swift
// plus AppState's static parsers). No process spawns, no AppKit — CLT-runnable
// via `swift test` on the m2mini bench.
import XCTest
@testable import MonopsonyMenuBar

// @MainActor: the static parsers under test are isolated to the main actor
// (declared on the @MainActor AppState class); XCTest supports annotated
// test classes and runs them on the main thread.
@MainActor
final class MonopsonyMenuBarTests: XCTestCase {
    // MARK: - Slice 3: server identity from server.json

    func testServerIdentityPrefersWSURL() throws {
        let json = """
        {"grpc_addr": "10.10.20.58:50051", "ws_url": "wss://tunnel.example/monopsony",
         "paired_at": "2026-10-01T00:00:00Z"}
        """
        let id = ServerIdentity.parse(serverJSON: Data(json.utf8))
        XCTAssertNotNil(id)
        XCTAssertEqual(id?.address, "wss://tunnel.example/monopsony")
        XCTAssertEqual(id?.host, "tunnel.example")
        XCTAssertTrue(id?.isTunnel ?? false)
    }

    func testServerIdentityFallsBackToGrpcAddr() {
        let json = """
        {"grpc_addr": "10.10.20.58:50051", "paired_at": "2026-10-01T00:00:00Z"}
        """
        let id = ServerIdentity.parse(serverJSON: Data(json.utf8))
        XCTAssertEqual(id?.address, "10.10.20.58:50051")
        XCTAssertEqual(id?.host, "10.10.20.58")
        XCTAssertFalse(id?.isTunnel ?? true)
    }

    func testServerIdentityNilWhenUnpaired() {
        XCTAssertNil(ServerIdentity.parse(serverJSON: Data("{}".utf8)))
        XCTAssertNil(ServerIdentity.parse(serverJSON: Data("not json".utf8)))
        // Empty strings must not win.
        let json = """
        {"grpc_addr": "", "ws_url": ""}
        """
        XCTAssertNil(ServerIdentity.parse(serverJSON: Data(json.utf8)))
    }

    func testHostOfBareHostPort() {
        XCTAssertEqual(ServerIdentity.hostOf("192.168.1.10:3128"), "192.168.1.10")
        XCTAssertEqual(ServerIdentity.hostOf("m2mini.tailnet"), "m2mini.tailnet")
        XCTAssertEqual(ServerIdentity.hostOf("https://a.b.c:8443/x"), "a.b.c")
    }

    func testDashboardURLUsesResolvedHost() {
        let json = """
        {"grpc_addr": "10.10.20.58:50051"}
        """
        let id = ServerIdentity.parse(serverJSON: Data(json.utf8))
        XCTAssertEqual(id?.dashboardURL?.absoluteString, "http://10.10.20.58:3130")
    }

    // MARK: - Slice 4: proxy status rows

    func testProxyStatusParserServerAndRows() {
        let out = """
        Monopsony Proxy Status
        ═══════════════════════════════════════════════
        Server:           10.10.20.58:50051
        Paired at:        2026-10-01

        pacman           ON  ✓  /etc/pacman.conf points at proxy
        apt              OFF ✗  no apt proxy config
        """
        let report = ProxyStatusParser.parse(out)
        XCTAssertEqual(report.server, "10.10.20.58:50051")
        XCTAssertEqual(report.pairedAt, "2026-10-01")
        XCTAssertEqual(report.rows.count, 2)
        XCTAssertEqual(report.rows[0].subsystem, "pacman")
        XCTAssertTrue(report.rows[0].enabled)
        XCTAssertEqual(report.rows[0].detail, "/etc/pacman.conf points at proxy")
        XCTAssertEqual(report.rows[1].subsystem, "apt")
        XCTAssertFalse(report.rows[1].enabled)
    }

    func testProxyStatusParserIgnoresHeaderNoise() {
        let report = ProxyStatusParser.parse("Monopsony Proxy Status\n═════\n\nNo subsystems detected.")
        XCTAssertNil(report.server)
        XCTAssertTrue(report.rows.isEmpty)
    }

    func testProxyStatusRowDetailMayBeEmpty() {
        let row = ProxyStatusParser.parseRow("brew ON  ✓")
        XCTAssertEqual(row?.subsystem, "brew")
        XCTAssertTrue(row?.enabled ?? false)
        XCTAssertEqual(row?.detail, "")
    }

    // MARK: - Slice 5: HIT ratio

    func testHitRatioFromQuietStatus() {
        let d = HitRatioDisplay.from(statusOutput: "Server: 10.0.0.1:50051\nCache HIT ratio: 87.4%\n")
        XCTAssertEqual(d.ratio ?? -1, 87.4, accuracy: 0.001)
        XCTAssertEqual(d.label, "87.4%")
    }

    func testHitRatioMissingMeansNA() {
        let d = HitRatioDisplay.from(statusOutput: "Server: 10.0.0.1:50051\n")
        XCTAssertNil(d.ratio)
        XCTAssertEqual(d.label, "n/a")
    }

    func testHitRatioAcceptsNoPercentSign() {
        let d = HitRatioDisplay.from(statusOutput: "Cache HIT ratio: 50")
        XCTAssertEqual(d.ratio ?? -1, 50.0, accuracy: 0.001)
    }

    // MARK: - Slice 1 status half: Content Caching row

    func testContentCacheStatusOn() {
        let out = "Apple Content Caching (this Mac):\n  cache         ON  ✓  active — shared content only (personal/iCloud caching off)\n"
        let (on, detail) = AppState.parseContentCacheStatus(out)
        XCTAssertTrue(on)
        XCTAssertTrue(detail.contains("shared content only"))
    }

    func testContentCacheStatusOff() {
        let out = "Apple Content Caching (this Mac):\n  cache         OFF ✗  deactivated\n"
        let (on, detail) = AppState.parseContentCacheStatus(out)
        XCTAssertFalse(on)
        XCTAssertEqual(detail, "off")
    }

    func testContentCacheStatusUnparseable() {
        let (on, detail) = AppState.parseContentCacheStatus("garbage")
        XCTAssertFalse(on)
        XCTAssertEqual(detail, "status unavailable")
    }

    // MARK: - Slice 6: icon mapping

    func testIconMapping() {
        XCTAssertEqual(MenuIcon.connected.symbolName, "bolt.fill")
        XCTAssertEqual(MenuIcon.disconnected.symbolName, "bolt.slash")
        XCTAssertEqual(MenuIcon.error.symbolName, "exclamationmark.triangle.fill")
    }

    // MARK: - parseStatus regression (existing contract, quiet shape)

    func testParseStatusQuietShape() {
        let s = AppState.parseStatus("Server: 10.10.20.58:50051\nCache HIT ratio: 12.5%\n")
        XCTAssertEqual(s.server, "10.10.20.58:50051")
        XCTAssertNotNil(s.hitRatio.ratio)
        XCTAssertFalse(s.parseFailed)
    }

    func testParseStatusEmptyIsParseFailed() {
        var s = AppState.parseStatus("")
        XCTAssertTrue(s.parseFailed)
        s = AppState.parseStatus("\n\n")
        XCTAssertTrue(s.parseFailed)
    }

    func testParseStatusRawLinesPreserved() {
        let s = AppState.parseStatus("something unexpected\nanother line\n")
        XCTAssertTrue(s.parseFailed)
        XCTAssertEqual(s.rawLines.count, 2)
    }
}
