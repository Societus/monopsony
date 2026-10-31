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

## 10. Wave A results (2026-10-12 — implemented, bench-verified)

Wave A = all slices not requiring sudo/console (per user direction, sudo items banked to §11).

| Slice | Status | Evidence |
|-------|--------|----------|
| 3 config-driven host | DONE | `ServerIdentity` parses `~/.config/monopsony/server.json` (ws_url → grpc_addr, same chain as daemon); 4 unit checks |
| 4 per-service rows | DONE | `ProxyStatusParser` over `proxy status`; rows render in menu; 3 unit checks |
| 5 HIT ratio | DONE | Rust: `hit_counter.rs` counts `X-Cache` at client-proxy forwarding point, persists `~/.local/state/monopsony/hit-counts.json` (30s flush); CLI: `status --quiet` prints `Server:` + `Cache HIT ratio: NN.N%` with tracing suppressed; Swift: `HitRatioDisplay`, "n/a" when no data; 7 unit checks total |
| 6 icon swap | DONE | bolt.fill (connected, green) / bolt.slash / warning-triangle (error) |
| 2 scan trigger (reframed) | DONE (CLI) | `--clear-catalog` does not exist on 26.3.1 (bench-verified via `softwareupdate --help`); `monopsony-client update-scan` wraps the live `--background` verb; menu button wired |
| 1 Homebrew/CC toggle | STATUS-ONLY in UI | `setup mac --status` row parses + renders unprivileged; the enable/disable ACTION needs GUI sudo elevation — banked (§11) |
| 7 Swift CI lane | DONE (repo half) | `swift-selftest` job in ci.yml (macos-arm64, continue-on-error): swift build + `--selftest` + cargo check client |

**Key bench fact (answers spec §6.7):** the CLT-only toolchain has **no XCTest module** — `swift test` cannot work without full Xcode. Testing is an in-binary `--selftest` mode (18 checks, `SELFTEST PASS`, exit 0) run by CI.

**Verification state:** cargo test 355+ green incl. 4 new hit_counter tests; clippy `-D warnings` clean; workspace check clean; on m2mini (macOS 26.3.1): `swift build` green + `--selftest` pass.

---

## 11. Wave B — banked for in-person bench run (sudo/console required)

Wave B files are committed; everything below needs martin at the m2mini console.

1. **Deploy client binary (bench-build — dev host is Linux, cannot produce a macOS binary):**
   ```sh
   # from the dev host (repo root): sync source (gate-blocked for agent — user-run)
   rsync -a --exclude target/ --exclude .git/ --exclude dashboard/ --exclude clients/ --exclude docs/ --exclude dist/ \
     ./ m2mini:/tmp/monopsony-build/
   ssh m2mini 'mkdir -p /tmp/monopsony-build/dashboard/build'
   # on the bench: build with the same env the forgejo-runner LaunchAgent uses
   ssh m2mini 'export PATH="/Users/martin-ci/.rustup/toolchains/1.94.1-aarch64-apple-darwin/bin:/Users/martin/.local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
     export PROTOC=/Users/martin-ci/.local/bin/protoc
     cd /tmp/monopsony-build && cargo build --release -p monopsony-client \
     && cp target/release/monopsony-client ~/.local/bin/monopsony-client'
   # then (console or martin shell): restart the daemon agent if running
   ```
2. **`status --quiet` live check:** run `~/.local/bin/monopsony-client status --quiet` as martin; expect `Server: <grpc_addr-or-ws_url>` from the real pairing file and `Cache HIT ratio:` after traffic.
3. **HIT-ratio meaning experiment (slice 5 gate):** run a brew/apt fetch cold through the daemon → ratio drops; refetch → climbs. If the ratio never moves, slice 5's data source is wrong in the field — record the verdict here.
4. **Menu app connect (GUI):** launch the built app at the console; verify icon states, per-service rows, dashboard open, scan button (`softwareupdate --background` fires without prompting).
5. **sudo elevation prototype (slice 1 action half):** try `osascript -e 'do shell script ... with administrator privileges'` wrapping `setup mac --enable`. If reliable → wire the menu button; if not → slice 1 stays status-only with a "run in Terminal" hint. Record the verdict here.
6. **Scan-traffic experiment (slice 2 gate):** with system-proxy ON (or Content Caching active), trigger update-scan and check whether SUS traffic is observable through Monopsony (server logs / cache growth / Activity). No observable routing → cut the button as cosmetic, per the keep/cut rule.

### 11.1 Agent-assisted partial Wave B run (2026-10-31 — over ssh; nobody at the console)

Full evidence: `notes/2026-10-31-b4-cc-toggle-prototype.md` (§5) and
`notes/2026-10-31-b4-scan-traffic-verdict.md` (§6); verdict table filled in the
bench-pack doc.

- **§5 CC-toggle prototype:** mechanism VERIFIED. `osascript … with administrator
  privileges` cannot elevate from an ssh session (instant `-60007`, no prompt);
  presented fine from a GUI-domain process (transient LaunchAgent, bootstrapped
  gui/501, dialog waited 7.5 min unattended → torn down cleanly). **One in-person
  approval at the console closes this item.** Found + fixed a tilde bug in the §5
  sample (root HOME = `/var/root`). Headless no-prompt alternative documented
  (sudoers NOPASSWD whitelist on a root-owned wrapper). CC on the bench was already
  active + shared-scoped, so `--enable` is idempotent there.
- **§6 scan-traffic gate:** routing through the system HTTP proxy is PROVEN (with
  `proxy on --system`, CONNECT swscan/gdmf/xp.apple.com captured on the 127.0.0.1:3129
  listener), but the traffic is uncached CONNECT tunnel (B3 architecture) and
  `softwareupdate --background` is throttle-gated (frequent no-op). **Verdict: CUT the
  menu scan button, keep the CLI verb**, keep system-proxy + CC as the real macOS
  surfaces. Side-finding: system proxy ON transits ALL scutil clients incl. LAN
  websockets (Chrome→Proxmox consoles observed) — LAN exceptions wanted if system
  proxy ships broadly.
- **Not run this pass:** §2/§3 (server .58 down + daemon crash-looping on missing
  certs — `Error: No client certificates found in /Users/martin/.config/monopsony`),
  §4 menu app connect (GUI at console). `status --quiet` itself passed over ssh
  (`Server: 10.10.20.58:50051`).
- Bench state restored: system proxy off (`HTTPEnable : 0`), CC unchanged
  (activated, shared-only), probe LaunchAgent + plist removed, capture/listener
  processes killed. Evidence files left in `/tmp/b4-*`.

### 11.2 Completion pass (2026-10-31 ~18:05–18:25 MST — .58 back in service; agent over ssh)

Reattempted the items §11.1 could not run (server was down). Fresh pairing via dashboard
login → `POST /api/v1/pair/init` (code TTL ~8 min) → `monopsony-client pair --code …
--cert-dir ~/.monopsony-b4-certs` (same client_id d1e8834e… — dedup-replace). Daemon run
via the `--cert-dir` global flag; experiments through 127.0.0.1:3129; system proxy
toggled on/off; server-side evidence from :9090 metrics + journal.

- **Item 2 (status --quiet live): PASS.** `Server: 10.10.20.58:50051` plus
  `Cache HIT ratio: 50.0%` after traffic.
- **Item 3 (HIT-ratio meaning, slice-5 gate): PASS — the ratio moves correctly.**
  `hello_2.10-3_arm64.deb` cold through the chain: `x-cache: MISS` (server journal
  `Cached: … 52308 bytes, sha256 f7b20d304db0`), refetch `x-cache: HIT` with server-side
  `Joining in-flight fetch` (single-flight); daemon counters `{"hits":1,"misses":1,"stale":0}`;
  displayed ratio 50.0%. The slice-5 data source is correct in the field.
  - **Bench-state finding (root cause of the frozen ratio):** the Oct 15 elevated run
    created `~/.local/state/monopsony/` (and the `~/.config/monopsony/` cert files) as
    ROOT. The daemon reads the zeroed `hit-counts.json` but persist() silently cannot
    write a root-owned dir (fail-open by design) — the displayed ratio never moves — and
    xnu refuses martin renaming a root-owned dir even inside a martin-owned parent
    (rename requires write access on the renamed dir itself). Proven by re-running the
    daemon under a fresh `HOME`: counters + ratio work immediately. **One-line console
    fix: `sudo chown -R martin:staff ~/.config/monopsony ~/.local/state/monopsony`** —
    also un-breaks the LaunchAgent daemon (it crash-loops only because client.key is
    root-owned mode 600).
- **Item 6 (scan-traffic): CUT verdict unchanged — strengthened.** Server-side CONNECT
  counters are observable and move: one controlled CONNECT to swscan.apple.com:443 through
  the real chain moved `monopsony_proxy_connect_bytes_total` 0 → 5372; in the same window
  Chrome tunnelled ~4 MB through the chain (5372 → 4,016,228). BUT two `softwareupdate -l`
  scans bypassed the just-enabled scutil proxy entirely (0 CONNECTs at the daemon, counter
  flat) — Apple's proxy pickup is session-dependent (§11.1 captured swscan/gdmf/xp
  CONNECTs under the identical setting). Cache value remains zero (B3 CONNECT-only).
  Fresh evidence for the LAN-exceptions follow-up: the server journal logged real
  per-target dial failures while the system proxy was on (`CONNECT upstream dial
  ingest:8006 failed: dial to 100.119.165.62:8006 timed out`, same for conductor:8006 and
  thequietbox.tail0964.ts.net:443) — the .58 server cannot reach tailnet/LAN hosts, so
  scutil clients' LAN browsing breaks while system-proxy forwarding is on.
- **New finding — bench client build too old for the current server.** The Oct 14 bench
  binary (md5 701d71aa54af3c3bebf11e9a450fa907) completes mTLS against the Oct 30 server
  (openssl s_client: `Verify return code: 0 (ok)`, TLSv1.3 CHACHA20-POLY1305) but every
  daemon gRPC reconnect dies in ~11 ms (`transport error` client-side, `gRPC connection
  error peer=10.10.20.120` server-side); the client's heartbeat age is frozen at pairing
  time while LAN CT clients on newer builds report normally → version skew, not
  cert/revocation. Item 1 (deploy a fresh bench client — the rsync leg is user-run per
  gate rules) is required before daemon-mode GUI checks (item 4).
- **New finding — `monopsony-server pair` subcommand 401s against its own admin API**
  (`Admin API returned 401 … "auth_method":"none"`) while manual `POST /api/v1/auth/login`
  (session cookie) → `POST /api/v1/pair/init` mints codes fine. Follow-up card created;
  the manual curl recipe is the workaround today.
- **Still open (GUI-only):** item 4 menu-app connect at the console; item 5's single
  console approval for the CC dialog. Nobody at the bench today either.
- Bench state restored: system proxy off (`HTTPEnable : 0`), b4 daemon killed, 3129 free,
  CC unchanged (activated, shared-only). Left intentionally: `/tmp/b4-daemon*.log`,
  `/tmp/b4-su-list4.log` (evidence), `~/.monopsony-b4-certs` (2 KB martin-owned paired
  certs) and `/tmp/b4-home` (HOME skeleton used for the counter proof).

### 11.3 Close-out (2026-10-31 ~12:35–12:45 MST — agent over ssh; rsync + console chown landed)

The §11.2 unblock conditions arrived: the user ran the source sync (this time
INCLUDING `clients/` so the menu app could rebuild — the §11 runbook line excludes
it; use the run-24 variant if repeating) and the console chown. Both state dirs are
`martin:staff` again; the LaunchAgent daemon has been crash-free since 12:32:45
(before the chown it crash-looped "No client certificates found" on the root-owned
600 key).

- **Cert refresh (canonical dir):** `~/.config/monopsony` still held the Oct 15
  pairing from the previous server life, while the cert the server knows for
  d1e8834e… is the Oct 31 dedup-replace set. Copied the Oct 31 pairing from
  `~/.monopsony-b4-certs` into the canonical dir (same client_id; old set backed up
  to `~/.config/monopsony.oct15.bak`). `server.json`: grpc `10.10.20.58:50051`,
  paired `50052`, `paired_at 2026-10-31`.
- **Item 1 (fresh client deploy): DONE.** Bench build per the §11 runbook env
  (martin-ci 1.94.1 toolchain PATH + `PROTOC`), `cargo build --release -p
  monopsony-client` — 60 s clean (2 pre-existing dead_code warnings). Deployed md5
  `c34d2d6cfb70178106a7ab5060ce316f` (replaces Oct 14
  `701d71aa54af3c3bebf11e9a450fa907`); the LaunchAgent `KeepAlive` relaunched the
  daemon (PID 72872) on the new binary within seconds of the kill.
- **gRPC transport failure: ROOT-CAUSED — version skew, closed.** Same certs, same
  server: the Oct 14 binary died ~11 ms post-TLS on every reconnect; the fresh build
  connects instantly — `gRPC connection established` → `Heartbeat sent` on the 60 s
  cycle, inventory accepted (`full_sync`), shadow catalog accepted (10 findings,
  `catalog_hash 7d70cfb9…`). Server side:
  `monopsony_client_last_heartbeat_age_seconds{client_id="d1e8834e…"}` cycling 0 → 60
  (was frozen/absent), `monopsony_client_failed_updates 0`. Zero new err-log lines
  since the restart (err log mtime 12:32:35 predates the 12:39:17 relaunch). Nothing
  to file as a bug.
- **Item 2 (`status --quiet` on canonical certs): PASS.** `Server:
  10.10.20.58:50051`, `Cert Expiry 2027-01-29` (renewal not-due, 89 days), and after
  two proxied fetches of `hello_2.10-3_arm64.deb` through 127.0.0.1:3129 (both
  `x-cache: HIT` — the §11.2 server-side cache entry still serving) the counters
  persisted as `{"hits":2,"misses":0,"stale":0}` and **`Cache HIT ratio: 100.0%`
  renders**. Persist() on the canonical state dir confirmed live (hit-counts.json
  mtime advancing under `martin:staff`). Field note: the ratio line only appears
  once the persisted counters are non-zero — a 0/0 file suppresses it.
- **Menu app rebuilt for item 4:** `swift build` on the synced tree — Build complete
  (43.64 s), zero third-party deps (offline/CLT-safe). Binary staged at
  `/tmp/monopsony-build/clients/macos/.build/debug/MonopsonyMenuBar`.
- **Still open (console-only):** item 4 menu-app GUI connect; item 5 single CC
  dialog approval. Everything agent-side is closed.
- Bench state: LaunchAgent daemon running healthy on the fresh binary (steady state
  — left running), proxy listener up on 127.0.0.1:3129, system proxy OFF
  (`HTTPEnable : 0`), CC unchanged (activated, shared-only), no reboots. New on
  disk: `~/.config/monopsony.oct15.bak` (pre-close-out cert set).

---

**End of review spec.**
