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
    └── AppState.swift                             # ObservableObject: CLI shell-out + launchctl
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

## What the menu does today (skeleton scope)

* Status line: server, connected clients, cache HIT ratio — parsed
  **defensively** from `monopsony-client status` plain-text output; raw CLI
  lines are shown verbatim when nothing parseable comes back.
* Connect / Disconnect — LaunchAgent bootstrap/bootout (above).
* System proxy toggle — shells out to `monopsony-client proxy system on|off`
  (existing Sprint 26 Tier 1 CLI surface).
* Homebrew env caching / Catalog refresh — **stubs, disabled, marked
  TODO(b4-followup)** until the CLI contract is wired.
* Open Dashboard — `NSWorkspace.open` on `http://<server>:3130`
  (server host is currently a constant in `AppState.MonopsonyConfig`;
  TODO: read from the client's saved config).

## Polling strategy

Status refreshes once when the menu opens and then every 5 s **only while the
menu is open** (`menuOpened`/`menuClosed` manage the Timer). Closed menu = no
background work at all.

## Known limitations

* Daemon binary path must exist (see above) — no bundled binary yet.
* `serverHost` for the dashboard URL is a placeholder constant.
* Status parsing tracks today's CLI output shape; new fields are picked up
  best-effort, unknown output falls back to raw lines.
* HIT-ratio parsing assumes the CLI prints `Cache HIT ratio: NN%` — not yet
  emitted by the Rust client; parse hardening is follow-up work.
