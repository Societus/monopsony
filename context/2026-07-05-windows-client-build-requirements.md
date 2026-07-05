# Windows Client Build — Requirements & Implementation Plan

Created: 2026-07-05
Status: Requirements document. Not started. For next session.

## Goal

Produce a working `monopsony-client.exe` for Windows that can:
1. Run as a local HTTP proxy (daemon mode)
2. Connect to the server via gRPC with mTLS
3. Report inventory (installed packages) via winget/choco/scoop backends
4. Route package downloads through the server's cache proxy

Deferred for later: Mirror subcommand, shadow packages, ProxyConfigAction execution.

## Target

- **Triple**: `x86_64-pc-windows-gnu` (via `cross`, simplest — no MSVC toolchain needed)
- **Alternative**: `x86_64-pc-windows-msvc` (if gnu target has dependency issues, requires Windows SDK)
- **Minimum OS**: Windows 10 1809+ (winget requirement)

## Dependency Audit

### Crates that work on Windows without modification
| Crate | Notes |
|-------|-------|
| tokio | Full async runtime, Windows-compatible |
| tonic + prost | gRPC client, pure Rust |
| rustls + rustls-pemfile | TLS. `ring` crypto provider compiles for windows-gnu |
| axum + hyper | Local HTTP proxy listener |
| clap | CLI parsing, no platform deps |
| serde + toml + serde_json | Config parsing |
| tracing + tracing-subscriber | Logging |
| hostname | Cross-platform (`hostname::get()` works on Windows) |
| reqwest | HTTP client for tracker/peer discovery |
| chrono + chrono-tz | Time |
| uuid | ID generation |
| regex | URL pattern matching |
| ed25519-dalek + base64 + rand | Shadow signing (compiles, unused in minimal build) |
| which | `which::which("winget")` works on Windows |
| shellexpand | `~` expansion — **check Windows behavior** (see below) |

### Crates with potential Windows issues
| Crate | Issue | Mitigation |
|-------|-------|------------|
| mdns-sd | Uses raw UDP sockets for mDNS. Should work on Windows but untested. | Make mDNS discovery optional/fallback. Already has manual server config. |
| shellexpand | `~` expansion may not resolve correctly on Windows (`%USERPROFILE%` vs `$HOME`). | Replace with explicit `dirs` crate or `std::env::var("USERPROFILE")`. |
| cron | Pure Rust, no platform deps. | Fine. |
| ring (via rustls) | Compiles for windows-gnu target. Historically had issues with windows-msvc. | Use gnu target. |

## Code Changes Required

### 1. Config path (main.rs:26)
**Current**: `default_value = "/etc/monopsony/client.toml"`
**Fix**: Platform-conditional default.

```rust
fn default_config_path() -> String {
    if cfg!(windows) {
        // C:\ProgramData\monopsony\client.toml
        std::env::var("ProgramData")
            .map(|d| format!("{}\\monopsony\\client.toml", d))
            .unwrap_or_else(|_| "C:\\ProgramData\\monopsony\\client.toml".to_string())
    } else {
        "/etc/monopsony/client.toml".to_string()
    }
}
```

Apply same pattern to:
- **cert_dir** (main.rs:31): `~/.config/monopsony` → `%APPDATA%\monopsony` on Windows
- **config_path fallback** (main.rs:790): same function
- **client_id file** (main.rs:260): stored in cert_dir, already relative
- **recipes path** (main.rs:833): hardcoded `/etc/monopsony/recipes/` — cosmetic, only in help text

### 2. Unix-only code (already guarded with `#[cfg(unix)]`)
These compile fine on Windows (the `#[cfg(unix)]` blocks are skipped):
- grpc_client.rs:44 — file permissions on client.key (chmod 600)
- shadow.rs:355 — file permissions on shadow binaries

**No changes needed.** Windows will skip the permission hardening.

### 3. Mirror subcommand (main.rs:620-628)
Writes Linux-specific files: `/etc/pacman.d/mirrorlist`, `/etc/apt/sources.list.d/`, `/etc/yum.repos.d/`.

**Fix**: Gate the entire `run_mirror` function behind `#[cfg(unix)]` or add a Windows branch that prints instructions instead of writing files. Windows package managers don't use repo files — they're configured via registry, PowerShell, or per-tool config.

```rust
#[cfg(windows)]
async fn run_mirror(...) -> anyhow::Result<()> {
    anyhow::bail!("Mirror subcommand not yet supported on Windows. Set HTTP_PROXY manually.");
}
```

### 4. Shadow module (shadow.rs)
- Uses `tokio::process::Command` to run binaries — works on Windows
- Uses `/usr/local/bin` as target_dir (shadow.rs:767) — needs Windows equivalent
- The shadow module is already behind the `Package` subcommand and not needed for proxy/inventory mode
- **Fix**: Gate shadow commands behind `#[cfg(unix)]` or add Windows target_dir. Simplest: disable `Package` subcommand on Windows for now.

### 5. ProxyConfigAction execution (common/package_manager/mod.rs:162-213)
- `SetEnv` calls `std::env::set_var` — works on Windows for current process
- `WriteFile` writes to Linux paths (`/etc/profile.d/`, `/etc/pacman.d/`)
- `RunCommand` is platform-agnostic
- The Windows backends (winget/choco/scoop) currently return `SetEnv` actions, which is fine
- **No changes needed for minimal build.** The client daemon doesn't call `ProxyConfigAction::execute()` during normal daemon mode — only the `Configure` subcommand does.

### 6. discovery.rs — mDNS
`mdns_sd::ServiceDaemon::new()` creates a raw UDP socket for mDNS discovery. This should work on Windows but is untested.

**Risk level**: Low. If it fails, the daemon falls back to manual `--server` flag. No code change needed for minimal build, but should be tested.

## Build Steps

```bash
# 1. Cross-compile from Linux
cross build --release --target x86_64-pc-windows-gnu -p monopsony-client

# 2. Output is at:
#    target/x86_64-pc-windows-gnu/release/monopsony-client.exe

# 3. Copy to Windows machine along with client.toml template
```

## Expected Issues During Cross-Compile

| Issue | Likely? | Fix |
|-------|---------|-----|
| `ring` fails to compile for windows-gnu | Low | Already used by other crates for this target. If it fails, switch to `aws-lc-rs` provider. |
| `mdns-sd` socket errors | Medium | Won't fail at compile time, may fail at runtime. Add `cfg` guard or error handling. |
| `shellexpand` panic on Windows | Medium | Replace with explicit env var lookup for cert_dir default. |
| `hostname` crate Windows support | Low | Crate explicitly supports Windows. |
| `tokio::process::Command` on Windows | Low | Tokio supports Windows async processes. |
| `which` crate on Windows | Low | Crate supports Windows (uses `PATHEXT`). |

## Testing Plan

### Phase 1: Compile (on Linux dev machine)
1. `cross build --release --target x86_64-pc-windows-gnu -p monopsony-client`
2. Verify `.exe` is produced
3. Check binary size is reasonable (~15-30MB)

### Phase 2: Smoke test (on Windows To Go USB)
1. Copy `monopsony-client.exe` to Windows machine
2. Create `C:\ProgramData\monopsony\client.toml` with server address
3. Run: `monopsony-client.exe daemon --server 10.10.20.58:50051`
4. Verify: daemon starts, connects to server, heartbeat works
5. Verify: inventory reports winget/choco/scoop packages
6. Check server: client appears in `clients` table with `os = "windows"`

### Phase 3: Proxy + cache test
1. Set system proxy: `netsh winhttp set proxy 10.10.20.58:3128`
2. Or set env: `set HTTP_PROXY=http://10.10.20.58:3128`
3. Run: `winget install <package>` / `choco install <package>`
4. Check server: cache_entries for winget/choco URLs
5. If HTTPS CONNECT errors: document which PMs require HTTPS

### Phase 4: Pairing test
1. Start pairing on server
2. Run: `monopsony-client.exe pair --code XXXXXX --server 10.10.20.58:50052`
3. Verify certs exchanged, client reconnects via mTLS

## Files to Change

| File | Change | Effort |
|------|--------|--------|
| `main.rs` | `default_config_path()`, `default_cert_dir()` functions with `cfg!` | 30min |
| `main.rs` | Gate `run_mirror()` behind `#[cfg(unix)]` | 5min |
| `main.rs` | Gate shadow `Package` subcommand or add Windows target_dir | 10min |
| `main.rs` | Replace `shellexpand` for cert_dir with explicit env var | 10min |
| `Cargo.toml` | No changes needed (all deps are cross-platform or already cfg-guarded) | 0 |

**Total estimated effort**: ~1 hour of code changes + cross-compile + test.

## What's NOT Changing

- PM backends (winget.rs, choco.rs, scoop.rs) — already implemented, no changes
- gRPC client — already works cross-platform
- Local proxy — axum/hyper, no platform deps
- Server-side code — no changes needed (server is always Linux)
- Proto definitions — no changes
- Config structs — already serde-based, paths are just strings

## Context Files for Reference
- `context/2026-07-04-alpine-apk-backend.md` — apk backend pattern (model winget/choco/scoop testing after this)
- `context/2026-07-04-install-telemetry-design.md` — telemetry fields (already in proto)
- `context/2026-07-05-multi-platform-verification.md` — all Linux backends verified, this doc covers Windows status
