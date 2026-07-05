# Multi-Platform Cache Verification + Windows Backend Design

Created: 2026-07-05
Status: Linux/BSD verification complete. Windows backends implemented but untested.

## Live Cache Verification Results (2026-07-05)

All 5 Linux test clients + 1 BSD client verified through the proxy on 10.10.20.58.

| Client | OS | PM | Backend | Cache Test | Entries | Notes |
|--------|----|----|---------|-----------|---------|-------|
| .58 (server) | Arch | pacman | PacmanBackend | ✅ pacman -Syu | 296 | 71 hits, 419MB |
| .20 | Arch | pacman | PacmanBackend | ✅ (prior session) | — | |
| .62 | Ubuntu 25.04 | apt | AptBackend | ✅ apt install tree | 1 | 50KB .deb cached |
| .242 | Debian 12 | apt | AptBackend | ✅ apt install sl | 3 | 127KB (sl + libgpm2 + libncurses6) |
| .159 | Alpine 3.22 | apk | ApkBackend | ✅ apk add htop | 2 | 617KB (.apk + APKINDEX.tar.gz) |
| .235 | Rocky 9 | dnf | DnfBackend | ✅ dnf install wget | 14 | 20MB (.rpm + repodata) |
| .33 | MidnightBSD | mport | MportBackend | ⏳ Not tested | — | Intermittent VPN (Tailscale bonded) |

### Total cache state after testing
```
geo.mirror.pkgbuild.com | 296 entries |  71 hits | 419 MB
dl.rockylinux.org       |  14 entries |   0 hits |  20 MB
deb.debian.org          |   3 entries |   0 hits | 127 kB
dl-cdn.alpinelinux.org  |   2 entries |   0 hits | 617 kB
archive.ubuntu.com      |   1 entry  |   0 hits |  50 kB
```

## Bugs Found and Fixed This Session

### 1. Server proxy filtering backends by local availability (critical)
- **Root cause**: `get_available_backends()` calls `is_available()` which checks if the PM
  binary exists locally. The Arch server doesn't have `apk`/`apt`/`dnf` installed.
- **Fix**: Split into `get_all_backends()` (server, all PMs) and `get_available_backends()`
  (client, only installed PMs). `matches_package_url()` now uses `get_all_backends()`.
- **Commit**: `5634458`

### 2. DnfBackend URL patterns too narrow
- **Root cause**: Pattern `repo\.rockylinux\.org` only matched `repo.rockylinux.org`,
  but actual package downloads come from `dl.rockylinux.org`.
- **Fix**: Broadened to `rockylinux\.org` and `almalinux\.org` (matches any subdomain).
- **Commit**: `b79b9a4`

### 3. TelemetryConfig Default derives false instead of true
- **Root cause**: `#[derive(Default)]` sets `install_tracking = false` (bool default).
  When `[telemetry]` section is absent from TOML, serde uses Default, bypassing
  `#[serde(default = "default_true")]`.
- **Fix**: Manual `impl Default` that sets `install_tracking = true`.
- **Commit**: `a4fad0a`

## Commits This Session

| Hash | Description |
|------|-------------|
| `d300f0b` | feat: add Alpine (apk) package manager backend |
| `5634458` | fix: server proxy uses get_all_backends() |
| `f130afd` | feat: install telemetry via inventory piggyback |
| `a4fad0a` | fix: TelemetryConfig custom Default |
| `b79b9a4` | fix: broaden dnf URL patterns for dl.rockylinux.org |

## Proxy Testing Notes

All clients require repos switched from `https://` to `http://` for proxy mode:
- **pacman**: `/etc/pacman.d/mirrorlist` (XferCommand with curl -x)
- **apt**: `Acquire::http::Proxy` in apt.conf or `-o Acquire::http::Proxy=`
- **dnf**: Uncomment `baseurl=http://dl.rockylinux.org/...` in `/etc/yum.repos.d/rocky.repo`,
  comment out `mirrorlist=` line
- **apk**: `http_proxy=` env var + `/etc/apk/repositories` switched to http://

HTTPS CONNECT tunneling is NOT implemented on the proxy (port 3128). The proxy
normalizes http:// requests to https:// when fetching upstream.

## Windows Backend Status

Three Windows package manager backends exist in code but have **never been tested live**:

### WingetBackend (`winget.rs`)
- `list_installed`: calls `winget list --accept-source-agreements`
- URL patterns: `github.com/*/releases/download/.*` (GitHub CDN for winget packages)
- Proxy config: `SetEnv` for `HTTP_PROXY`/`HTTPS_PROXY`
- **Untested**: no Windows test client exists

### ChocoBackend (`choco.rs`)
- `list_installed`: calls `choco list --local-only`
- URL patterns: `chocolatey.org/api/v.*/package/.*`
- Proxy config: writes `%USERPROFILE%\.choco\proxy` or env vars
- **Untested**: no Windows test client exists

### ScoopBackend (`scoop.rs`)
- `list_installed`: parses scoop `current` directory for install manifests
- URL patterns: `github.com/*/releases/download/.*` (GitHub raw)
- Proxy config: `SetEnv` for `HTTP_PROXY`/`HTTPS_PROXY`
- **Untested**: no Windows test client exists

## Next Steps: Windows Testing

### What's needed
1. A Windows test VM (Windows 10/11 or Server) on the LAN
2. The monopsony-client compiled for Windows (x86_64-pc-windows-gnu or x86_64-pc-windows-msvc)
3. At least one of winget/choco/scoop installed on the test VM
4. Repos configured to use HTTP proxy: `set HTTP_PROXY=http://10.10.20.58:3128`

### Known unknowns for Windows
- **winget package URLs**: winget packages may use GitHub releases, but the download
  URLs could be `objects.githubusercontent.com` (not `github.com`). The current
  pattern `github.com/*/releases/download/.*` may need broadening.
- **choco API URL**: `chocolatey.org` API may redirect to a CDN. Need to verify
  actual download host.
- **scoop URLs**: scoop downloads from arbitrary URLs defined in manifest JSON.
  The `github.com/*/releases/download/.*` pattern may not cover all scoop packages.
- **Proxy config**: Windows doesn't use `/etc/profile.d/` or environment variables
  the same way. The `SetEnv` ProxyConfigAction may need a Windows-specific
  implementation (registry, netsh, or config file per tool).
- **HTTPS**: Windows package managers may enforce HTTPS with no HTTP fallback.
  The proxy's lack of CONNECT tunneling could be a blocker for Windows clients.

### Cross-compilation for Windows
```bash
# GNU target (simpler, uses MinGW)
cross build --release --target x86_64-pc-windows-gnu -p monopsony-client

# MSVC target (needs Windows toolchain, not available via `cross`)
# Use a Windows machine or CI runner instead
```

## Other Unimplemented Backends (no test clients)

| Backend | Platform | Status | Notes |
|---------|----------|--------|-------|
| PkgBackend | FreeBSD | Implemented | `pkg` (FreeBSD/pkgng) |
| PkgAddBackend | OpenBSD | Implemented | `pkg_add` |
| PkginBackend | NetBSD | Implemented | `pkgin` |
| MportBackend | MidnightBSD | Implemented | Verified binary runs, mport backend works, cache untested |
