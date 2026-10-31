# Monopsony macOS Menu-Bar Client (B4 skeleton)

Tailscale-style SwiftUI menu-bar app: a thin shell over the existing Rust
`monopsony-client` daemon. **No new protocol work** — mTLS/gRPC stays in the
Rust daemon; this app only shells out to the CLI and to `launchctl`.

## Layout

```
clients/macos/
├── Package.swift                                  # SPM, zero 3rd-party deps (CLT-only build)
├── README.md                                      # this file
├── Resources/
│   └── com.societus.monopsony-client.plist        # LaunchAgent template (placeholder binary path)
└── Sources/MonopsonyMenuBar/
    ├── MonopsonyMenuBarApp.swift                  # @main, MenuBarExtra + menu items
    ├── AppState.swift                             # ObservableObject: CLI shell-out + launchctl
    ├── Core.swift                                 # pure logic: server.json parse, row/ratio parsers, icon map
    └── SelfTest.swift                             # --selftest suite (CLT has no XCTest)
```

## Build & run (Command Line Tools only — no Xcode needed)

```sh
cd clients/macos
swift build                      # executable in .build/arm64-apple-macosx/debug/
.build/arm64-apple-macosx/debug/MonopsonyMenuBar &
swift build --show-bin-path      # prints the exact bin dir
```

Requirements:

* macOS 13+ (MenuBarExtra window style), built with Xcode CLT (`xcode-select`
  points at CLT is fine — verified no SPM network fetch since the package has
  zero third-party dependencies).
* `monopsony-client` must exist at the path in `AppState.MonopsonyConfig.clientBinary`
  (default `~/.local/bin/monopsony-client`). **Known limitation: the daemon
  binary path must exist** — Connect shows an error otherwise, and the
  LaunchAgent template's `__CLIENT_BINARY__` placeholder is substituted with
  this same path at install time.

## LaunchAgent

Connect copies the bundled template to
`~/Library/LaunchAgents/com.societus.monopsony-client.plist` (substituting the
real binary path for `__CLIENT_BINARY__`), then:

```sh
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.societus.monopsony-client.plist
launchctl bootout    gui/$UID/com.societus.monopsony-client   # Disconnect
```

Modern `bootstrap`/`bootout` syntax only (macOS 10.11+; bench m2mini runs
26.3.1 — no legacy `load`/`unload` fallback). The agent runs
`monopsony-client daemon` with `RunAtLoad` + `KeepAlive`.

Manual install if you prefer:

```sh
sed "s|__CLIENT_BINARY__|$HOME/.local/bin/monopsony-client|" \
  Resources/com.societus.monopsony-client.plist \
  > ~/Library/LaunchAgents/com.societus.monopsony-client.plist
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.societus.monopsony-client.plist
```

## What the menu does today (Wave A — implemented, bench-verified)

* Status line: server + cache HIT ratio from `monopsony-client status --quiet`
  (tracing-suppressed `key: value` output; raw CLI lines shown verbatim when
  nothing parseable comes back).
* Per-service proxy rows (slice 4) — parsed from `proxy status` subsystem
  rows, rendered with ✓/✗ state.
* Cache HIT ratio (slice 5) — counts the server's `X-Cache` decisions at the
  client proxy, persisted by the daemon to
  `~/.local/state/monopsony/hit-counts.json`; "n/a" before any traffic.
* Server host (slice 3) — resolved from `~/.config/monopsony/server.json`
  (`ws_url` → `grpc_addr`, same chain as the daemon); dashboard URL and
  status line both use it. No more constant.
* Menu icon (slice 6) — bolt.fill (green) when connected, bolt.slash when
  disconnected, warning triangle on errors.
* Content Caching status + toggle (slice 1) — status from `setup mac
  --status`, unprivileged. Enable/Disable rides the bench-verified osascript
  admin wrapper (exactly one password dialog per action, absolute binary
  path, explicit `with prompt`, no embedded credentials — see
  `notes/2026-10-31-b4-cc-toggle-prototype.md`; one console approval on the
  bench still pending, fallback = status-only + "run in Terminal" hint).
* Update scan — deliberately absent (bench verdict 2026-10-31,
  `notes/2026-10-31-b4-scan-traffic-verdict.md`: SUS traffic through the
  system proxy is an uncached CONNECT tunnel and `softwareupdate
  --background` is throttle-gated, so the button frequently no-oped).
  The `monopsony-client update-scan` CLI verb stays as the bench/diagnostic
  primitive; `--clear-catalog` is gone on 26.3.1.
* Connect / Disconnect — LaunchAgent bootstrap/bootout (above).
* System proxy toggle — shells out to `monopsony-client proxy system on|off`
  (existing Sprint 26 Tier 1 CLI surface).
* Open Dashboard — `NSWorkspace.open` on `http://<host>:3130` from pairing state.

## Testing (no XCTest — CLT-only toolchain)

`swift test` cannot work: the CLT toolchain ships **no XCTest module**
(bench-verified on macOS 26.3.1). The pure-logic suite runs instead via:

```sh
swift build
.build/debug/MonopsonyMenuBar --selftest   # SELFTEST PASS, exit 0/1
```

CI (Forgejo, macos-arm64 runner) gates on this — see the `swift-selftest`
job in `.github/workflows/ci.yml`.

## Polling strategy

Status refreshes once when the menu opens and then every 5 s **only while the
menu is open** (`menuOpened`/`menuClosed` manage the Timer). Closed menu = no
background work at all.

## Known limitations

* Daemon binary path must exist (see above) — no bundled binary yet.
* Content Caching enable/disable is status-only in the menu; the action needs
  a GUI sudo elevation path (osascript prototype pending — review spec §11).
* Status parsing tracks today's CLI output shape; new fields are picked up
  best-effort, unknown output falls back to raw lines.
* Update-scan traffic routing through Monopsony is a bench experiment, not a
  code guarantee (review spec §11, item 6).
