# Windows System Tray Status Monitor — Implementation Plan

**Date:** 2026-07-05
**Status:** Planning (not yet implemented)
**Scope:** Client-side only. Zero changes to `monopsony-server`.

## Motivation

Package update helpers on Windows are notorious for spawning terminal/PowerShell windows that clutter the desktop. Monopsony's Windows client should provide a lightweight, persistent status indicator that avoids this problem entirely.

The user wants:
- A taskbar icon showing update status at a glance
- Click to see: what packages are being updated, connectivity to server, client telemetry
- No screen clutter from PowerShell/terminal windows

## Architecture Decision: REST Polling, Not WebSockets

### Why Not WebSockets

A WebSocket broadcast layer on `monopsony-server` was considered and **rejected**:

| Concern | WebSocket (rejected) | REST polling (accepted) |
|---------|----------------------|------------------------|
| New server code | ~200-400 lines (WS handler, event bus, broadcast fan-out) | **None** |
| New server deps | `tokio-tungstenite` or `axum ws` | **None** |
| New attack surface | Persistent connections = DoS vector, memory exhaustion | **None** |
| Auth boundary | New WS handshake auth code to write and audit | **Reuses existing dashboard auth** |
| State management | Subscribe/unsubscribe lifecycle, stale connections | **Stateless HTTP** |
| Minimalism | Violates the principle | **Satisfies the principle** |

### Why REST Polling

The server already exposes every data point the tray needs via `monopsony-admin`'s REST API on port 3130:

| Endpoint | Data | Used For |
|----------|------|----------|
| `GET /api/v1/health` | Service health per-subsystem (db, shadow, mirror, tls) | Connection status indicator |
| `GET /api/v1/stats` | Client count, package count, cache exists | Overview tooltip |
| `GET /api/v1/cache/stats` | Total entries, total size bytes | Cache health |
| `GET /api/v1/shadow/packages` | Shadow package list with update availability | Update notifications |
| `GET /api/v1/mirrors` | Mirror list with sync status | Mirror sync progress |
| `GET /api/v1/clients/:id/inventory` | Per-client installed package list | Client telemetry |

All of these are already protected by the dashboard auth middleware (API key or session cookie). The tray simply becomes another consumer of existing authenticated endpoints.

### Security Posture

- **Zero new server attack surface** — no new ports, no new protocols, no new stateful connections
- **Reuses existing auth** — dashboard API key or session token, already battle-tested
- **Client-initiated only** — the tray makes outbound HTTP requests; nothing pushes to it
- **No new trust boundaries** — the tray runs as the logged-in Windows user with no elevation
- **Polling interval is client-controlled** — default 30s, configurable. Server cannot be made to send faster
- **Minimal data exposure** — the tray only requests the endpoints it needs; it doesn't subscribe to a firehose

## Implementation

### Phase 1: System Tray Foundation (Windows-only)

**New dependency:** `tray-icon` (pure Rust, wraps Win32 `Shell_NotifyIcon`)

The `tray-icon` crate requires an event loop. On Windows this means `winit` as a transitive dependency. Both are gated behind `#[cfg(windows)]` so they add zero cost to Linux/macOS builds.

**Changes:**

1. **`crates/monopsony-client/Cargo.toml`**
   - Add `tray-icon = "0.19"` and `winit = "0.30"` as Windows-only dependencies:
     ```toml
     [target.'cfg(windows)'.dependencies]
     tray-icon = "0.19"
     winit = "0.30"
     ```

2. **`crates/monopsony-client/src/tray.rs`** (new file, ~150 lines)
   - `struct TrayState` — holds `tray_icon::TrayIcon`, current status enum, poll interval
   - `enum ServerStatus` — `Connected`, `Degraded`, `Unreachable`
   - `fn build_menu() -> Menu` — right-click context menu:
     - "Status" → opens dashboard in browser
     - "Cache Stats" → shows tooltip with cache entries/size
     - "Open Dashboard" → `open::that("http://<server>:3130")`
     - "Quit" → graceful shutdown
   - `fn update_tooltip(status: &ServerStatus, stats: &StatsResponse)` — formats tooltip text
   - `async fn poll_loop(server_url: &str, api_key: Option<&str>, interval: Duration)` — periodic fetch of `/api/v1/health` and `/api/v1/stats`, updates icon/tooltip
   - Icon states:
     - Green: all services healthy, clients connected
     - Yellow: degraded (db unreachable but proxy working, etc.)
     - Red: server unreachable
     - Blue dot overlay: updates available (shadow packages with new versions)

3. **`crates/monopsony-client/src/main.rs`**
   - New CLI flag: `--tray` (Windows-only, gated with `#[cfg(windows)]`)
   - New subcommand or daemon mode modifier: when `--tray` is passed, spawn the tray task alongside the daemon
   - The tray task runs the `winit` event loop on the main thread (Windows requirement for message pump), while the daemon proxy runs in a background tokio task

4. **Icon assets** (`crates/monopsony-client/assets/`)
   - Embedded via `include_bytes!` — no runtime file I/O, no path discovery issues
   - 3 variants: `icon_connected.ico`, `icon_degraded.ico`, `icon_disconnected.ico`
   - Simple colored circle on transparent background — matches Monopsony palette (#44E2CD teal for connected, #F9BD22 amber for degraded, red for disconnected)
   - Can be generated programmatically with a small build script or hand-crafted 16x16 .ico files

### Phase 2: Status Popup Window (On Click)

When the user clicks the tray icon, show a lightweight status popup instead of (or before) opening the full dashboard.

**Approach:** Use the tray icon's click event to open the existing SvelteKit dashboard at `http://<server>:3130` in the user's default browser. No new UI code needed — the dashboard already exists and shows everything:

- Server health (per-service breakdown)
- Cache statistics (entries, size, hit rates)
- Client inventory (which packages each client has installed)
- Mirror sync status
- Shadow package update availability

This is the minimalist path. If the user later wants a native popup before browser launch, that's a separate enhancement using `tray-icon`'s parent window feature + a minimal `egui` or `iced` overlay. But for v1, browser redirect is sufficient and zero new code.

### Phase 3: Update Notifications

**Approach:** The tray's poll loop checks `GET /api/v1/shadow/packages` for packages with `update_available: true`. When found:
- Change the tray icon to the "updates available" variant
- Show a Windows balloon notification via `tray-icon`'s notification API
- On notification click → open dashboard at the shadow packages page (`http://<server>:3130/shadow`)

This requires no server changes — the data is already there.

### Phase 4: Client Telemetry Display (Future)

The per-client inventory endpoint (`GET /api/v1/clients/:id/inventory`) already exposes what packages a specific client has installed. The tray could show this on right-click → "My Packages" by:
1. Reading the client's own ID from its pairing certificate (already stored in `%APPDATA%\monopsony\`)
2. Fetching `/api/v1/clients/{id}/inventory`
3. Displaying in a tooltip or opening the dashboard clients page

Again, no server changes needed.

## Files Changed Summary

| File | Change | Size |
|------|--------|------|
| `crates/monopsony-client/Cargo.toml` | Add `tray-icon`, `winit` (Windows-only) | +5 lines |
| `crates/monopsony-client/src/tray.rs` | New: tray icon, poll loop, status enum, menu | ~150 lines |
| `crates/monopsony-client/src/main.rs` | Add `--tray` flag, spawn tray task | ~20 lines |
| `crates/monopsony-client/assets/*.ico` | 3 embedded icon files (16x16) | binary |

**Server changes: none.**

## Build & Test Plan

1. Verify Linux build unaffected (`cargo check -p monopsony-client` — new deps are cfg-gated)
2. Verify Windows cross-compile succeeds (`cargo build --target x86_64-pc-windows-gnu -p monopsony-client`)
3. Copy `.exe` to Windows To Go USB, boot, run `monopsony-client daemon --tray`
4. Verify: tray icon appears in notification area
5. Verify: tooltip shows "Connected — X clients, Y packages" (or "Unreachable" if server not running)
6. Verify: right-click menu works
7. Verify: click opens dashboard in browser
8. Verify: stopping the daemon via right-click → Quit exits cleanly
9. Verify: no terminal window spawned

## Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| `tray-icon` crate abandonded/unmaintained | Pure Win32 wrapper, minimal API surface. Easy to replace with direct `windows-rs` calls if needed. No business logic depends on it. |
| `winit` version conflicts with other deps | `winit` is only compiled on `cfg(windows)`. No Linux/macOS impact. Version pin in Cargo.toml. |
| Windows message pump blocks tokio runtime | Run winit event loop on main thread, spawn tokio runtime for daemon proxy + poll loop as a background task. This is the standard Windows GUI pattern. |
| Admin API auth fails silently | Tray shows "Unreachable" icon. Tooltip explains: "Cannot connect to server" or "Auth failed — check API key". No silent failure. |
| Polling overhead on server | 30s interval = 2 requests/min per client. At 200 clients (max target scale) = 400 req/min = ~7 req/sec. Negligible for axum. |

## Open Questions

1. **Icon design:** Programmatic generation via build script, or hand-crafted .ico files? Recommendation: hand-crafted for crispness at 16x16.
2. **Notification sound:** Should the balloon notification play a sound when updates are available? Default yes, configurable.
3. **Startup behavior:** Should the tray auto-start with Windows (registry `Run` key) or require explicit launch? Recommendation: explicit launch only — no auto-start without user consent.
