# Phase 4 / B4 — SwiftUI Menu-Bar Client — Review Spec

**Date:** 2026-10-12
**Status:** REVIEW — skeleton exists, full implementation pending
**Purpose:** Red-team the Phase 4 implementation plan and skeleton code quality before cloud-model implementation

---

## 1. What Already Exists (Skeleton — committed 34b921a)

### Files
| File | Lines | Purpose |
|------|-------|---------|
| `Package.swift` | 27 | SPM package, zero deps, macOS 13+, CLT-only build |
| `MonopsonyMenuBarApp.swift` | 100 | `@main` + `MenuBarExtra` window, menu layout |
| `AppState.swift` | 280 | `ObservableObject` bridging to CLI + launchctl |
| `com.societus.monopsony-client.plist` | 43 | LaunchAgent template with `__CLIENT_BINARY__` placeholder |
| `README.md` | 91 | Build instructions, known limitations |

### What's Wired
- **Daemon lifecycle:** `launchctl bootstrap/bootout gui/$UID` — Connect/Disconnect
- **Status polling:** 5s timer only while menu open; closed menu = zero CPU
- **System proxy toggle:** `monopsony-client proxy system on|off` (existing Sprint 26 Tier 1 CLI)
- **Status parsing:** Defensive parse of `monopsony-client status` text output; raw lines shown on parse failure
- **Dashboard open:** `NSWorkspace.open` on `http://<server>:3130` (placeholder server host)
- **Build:** `swift build` proven on m2mini (CLT-only, zero deps, ad-hoc codesign)

### What's Stubbed (TODO-b4-followup markers)
1. Menu icon swap based on connection state (arrow.swap → bolt/bolt.slash)
2. Homebrew env caching toggle (disabled button, no CLI wiring)
3. Catalog refresh toggle (disabled button, no CLI wiring)
4. Server host read from client config (currently `192.168.1.10` constant)
5. Per-service proxy status rows (CLI prints rows but UI doesn't parse them)
6. HIT-ratio display (assumes CLI prints `Cache HIT ratio: NN%` — not yet emitted)

---

## 2. Implementation Scope — What Needs to Be Built

### Slice 1: Homebrew env caching toggle
- Wire to `monopsony-client setup mac` (Phase 3.5 already exists)
- Menu button → `setup mac --enable` / `setup mac --disable`
- Status indicator: ON/OFF based on `setup mac --status` output
- Error handling: display sudo failure messages in menu

### Slice 2: Catalog refresh toggle
- Wire to `monopsony-client catalog` (Phase 2 was NO-GO; catalog refresh = `softwareupdate --clear-catalog` or `softwareupdated --reset`?)
- Clarify: what is "catalog refresh" actually doing on macOS?
- Status indicator: last refresh time

### Slice 3: Config-driven server host
- Read from client's saved config (cert dir / pairing state) instead of constant
- Fallback to user input if config missing

### Slice 4: Per-service proxy status display
- Parse `report_system_proxy_rows` output (existing CLI prints per-service rows)
- Show in menu instead of generic "forwarded" message

### Slice 5: HIT-ratio display
- Depends on CLI emitting structured output (not yet done)
- Current parser handles `Cache HIT ratio: NN%` format

### Slice 6: Icon swap
- `arrow.triangle.swap` → `bolt.fill` when connected
- `bolt.slash` when disconnected
- Status-aware icon color (green/gray)

### Slice 7: Darwin CI lane
- Forgejo runner on m2mini with `macos-arm64` label
- CI job: `swift build` + `swift test` for the menu app
- Non-blocking (`continue-on-error: true`) until flaky tests shake out

---

## 3. Code Quality Review Points

### 3.1 Architecture — Thin Shell Pattern
The skeleton's core design principle is "thin shell over Rust daemon" — **no new protocol work**. Verify this holds:
- All daemon management goes through `monopsony-client` CLI (not direct API calls)
- No Swift-side caching of state that could diverge from daemon
- No custom TLS/mTLS handling (stays in Rust)
- No gRPC or HTTP client in Swift (only CLI shell-out)

### 3.2 Process Management — LaunchAgent
**Current code:** `AppState.startAgent()` installs LaunchAgent plist, bootstraps gui/$UID.
**Review questions:**
- What happens if the LaunchAgent plist install fails? (Current: sets `lastError`, but does the menu show it?)
- What happens if the daemon crashes after bootstrap? (`KeepAlive: true` should restart it, but does the UI reflect this?)
- What about `RunAtLoad` — does the daemon start before the user can see the menu?
- Edge case: user manually stops the daemon via `launchctl bootout` — does the UI detect this?

### 3.3 CLI Shell-Out — `runClient()`
**Current code:** `Process` + `Pipe` with 10s timeout + `RunLoop.main.run()` polling.
**Review questions:**
- Is the 10s timeout appropriate for all CLI commands? (Some may take longer)
- What happens if the CLI hangs past 10s? (Current: `proc.terminate()` — is this graceful?)
- Should we use `Pipe.fileHandleForReading.readDataToEndOfFile()` in a background thread instead of blocking the main thread?
- Error handling: `pipe.readDataToEndOfFile()` can throw — is the catch sufficient?

### 3.4 Status Parsing — Defensive Parse
**Current code:** `ClientStatus.parseStatus()` splits on `: ` and matches keys case-insensitively.
**Review questions:**
- What happens if the CLI output format changes? (Current: falls back to raw lines — good)
- What about multi-value fields? (e.g., `Server: 192.168.1.10, 10.10.20.58`)
- Should we switch to JSON output from the CLI for structured parsing? (Trade-off: more robust but requires CLI changes)

### 3.5 Network Security — Dashboard URL
**Current code:** `MonopsonyConfig.dashboardURL = URL(string: "http://\(serverHost):3130")!`
**Review questions:**
- Dashboard is HTTP (not HTTPS) — is this acceptable for a local server?
- What if the server is on a different port? (No port override in config)
- Should we validate the URL before opening? (Current: force-unwrapped `URL(string:)` — can crash on invalid URL)

### 3.6 User Experience — Menu Lifecycle
**Current code:** `menuOpened()` starts 5s timer; `menuClosed()` invalidates it.
**Review questions:**
- What if the user opens the menu rapidly? (Timer is recreated each time — potential leak?)
- Should we debounce menu opens? (e.g., only refresh if >2s since last refresh)
- What about menu dismissal via escape key? (Does `menuClosed()` fire?)

### 3.7 Error Handling — `lastError`
**Current code:** `@Published var lastError: String?` — shown in red in the menu.
**Review questions:**
- What happens if multiple errors occur simultaneously? (Only the last one is shown)
- Should errors be dismissable? (Current: only cleared by the next successful action)
- Should we log errors to a file for debugging? (No file logging currently)

### 3.8 Configuration — Hardcoded Values
**Current code:** `serverHost = "192.168.1.10"` — placeholder.
**Review questions:**
- What's the source of truth for server config? (Client's saved config? Pairing state?)
- Should we read from `client.toml` or the cert dir?
- What if the user has multiple servers? (No multi-server support currently)

### 3.9 Testing — Swift Unit Tests
**Current code:** No Swift tests in the skeleton.
**Review questions:**
- Should we add unit tests for `parseStatus()`? (Easy to test with fixture strings)
- Should we add UI tests? (Harder to automate, but valuable for menu lifecycle)
- What about integration tests? (Requires a running daemon — complex)

### 3.10 Packaging — Distribution
**Current code:** `swift build` produces an executable in `.build/`.
**Review questions:**
- How do we distribute this to users? (No app bundle, no installer)
- Should we create an `.app` bundle? (Requires Xcode or `xcodebuild`)
- What about notarization? (Deferred until public release, but ad-hoc codesign is sufficient for internal use)

---

## 4. Security Considerations

### 4.1 sudo / Privilege Escalation
- `setup mac` uses `sudo sh -c "..."` — the script is constructed from constants (no user input) — **low risk**
- Verify the script has no injection vectors (current: only `ASSET_CACHE_UTIL` and `DEFAULTS` constants — safe)
- What if the user's sudo timeout expires mid-operation? (Current: single prompt, so no issue)

### 4.2 LaunchAgent Plist — Code Signing
- The LaunchAgent plist is installed to `~/Library/LaunchAgents/`
- macOS requires LaunchAgents to be code-signed or the user must approve them
- Current: no code signing of the plist — does macOS accept it? (Likely yes for user-owned files)

### 4.3 Network Exposure
- Dashboard URL is `http://192.168.1.10:3130` — HTTP, not HTTPS
- If the server is on a public IP, this is a security risk
- Recommendation: validate server is on a private IP range before opening dashboard

### 4.4 Data Leakage
- CLI output is captured via `Pipe` — does it contain sensitive data? (Passwords, tokens, etc.)
- Current: `monopsony-client status` output is safe (no secrets)
- Future: if CLI output changes, ensure no secrets are leaked to the menu

---

## 5. Testing Plan

### 5.1 Unit Tests (Swift)
- `parseStatus()` with fixture strings (success, partial, failure)
- `enabled_and_private()` with various `ContentCacheStatus` inputs
- `installAgentPlist()` with mocked `FileManager`

### 5.2 Integration Tests (Manual)
- Connect/Disconnect: verify daemon starts/stops
- System proxy toggle: verify `networksetup` changes
- Homebrew env caching: verify `setup mac --enable` / `--disable`
- Status refresh: verify menu updates correctly
- Error handling: verify error messages display correctly

### 5.3 Fleet Verification (Bench)
- m2mini: full chain verification (same as Phase 3.5)
- Second device: verify Content Caching serves content
- Regression: verify existing Phase 1 (Homebrew) still works

---

## 6. Open Questions for Cloud Model

1. **Catalog refresh:** What is this actually doing? Phase 2 was NO-GO (CatalogURL dead). Is `softwareupdate --clear-catalog` the right command?
2. **Server host:** What's the source of truth? Client config? Pairing state? Manual input?
3. **Distribution:** How do we distribute the menu app? App bundle? DMG? Homebrew tap?
4. **Notarization:** Deferred until public release, but is ad-hoc codesign sufficient for internal use?
5. **Multi-server:** Should we support multiple servers? (Current: single server only)
6. **Error logging:** Should errors be logged to a file for debugging?
7. **CI safety:** What's the minimum CI setup for Swift builds? (CLT only? Xcode?)

---

## 7. Red-Team Checklist

- [ ] Verify no new protocol work (all daemon management via CLI)
- [ ] Verify no custom TLS/mTLS handling
- [ ] Verify no gRPC or HTTP client in Swift
- [ ] Verify LaunchAgent plist install error handling
- [ ] Verify daemon crash detection (does UI reflect `KeepAlive` restart?)
- [ ] Verify CLI timeout handling (10s timeout — is it appropriate?)
- [ ] Verify dashboard URL validation (force-unwrapped URL — can crash)
- [ ] Verify menu timer lifecycle (no leaks on rapid open/close)
- [ ] Verify error dismissal (only cleared by next success — is this OK?)
- [ ] Verify sudo script injection safety (no user input — safe)
- [ ] Verify LaunchAgent code signing requirements (user-owned files — likely OK)
- [ ] Verify network exposure (HTTP dashboard — is this acceptable?)
- [ ] Verify data leakage (CLI output — no secrets currently)
- [ ] Verify distribution plan (no app bundle — is this sufficient?)

---

## 8. Estimated Effort

| Slice | Effort | Dependencies |
|-------|--------|--------------|
| Slice 1: Homebrew toggle | 1–2 days | Phase 3.5 (done) |
| Slice 2: Catalog refresh | 0.5 days | Clarify scope |
| Slice 3: Config-driven server host | 0.5 days | None |
| Slice 4: Per-service proxy status | 0.5 days | None |
| Slice 5: HIT-ratio display | 0.5 days | CLI output change |
| Slice 6: Icon swap | 0.5 days | None |
| Slice 7: Darwin CI lane | 1 day | Runner setup |
| **Total** | **~4–5 days** | |

---

## 9. Acceptance Criteria

- [ ] Menu bar icon present with working menus
- [ ] Daemon start/stop from the menu works
- [ ] Status panel shows live server state over the tailnet
- [ ] Homebrew env toggle actually flips `setup mac --enable` / `--disable`
- [ ] System proxy toggle works (existing Sprint 26 Tier 1)
- [ ] Dashboard open works (HTTP to server)
- [ ] Error messages display correctly
- [ ] CI lane green on a commit touching darwin paths
- [ ] Zero regressions on existing Phase 1 (Homebrew) functionality

---

**End of review spec.**
