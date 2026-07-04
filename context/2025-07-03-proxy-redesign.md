# Proxy Architecture Redesign — July 3, 2026

## Intended Flow (User Spec)

```
Client package manager (pacman/apt/apk)
        ↓
Client local proxy (127.0.0.1:3129)
        ↓
Server HTTP forward proxy (port 3128)
        ↓
  ┌───────┴───────┐
  ↓               ↓
Cache HIT     Cache MISS
↓               ↓
Serve from   Fetch from upstream → cache → serve
cache
(if unreachable/timeout → 503, NO fallback)
                ↓
Prefetch scheduler regularly pulls tracked packages → warm cache
```

**Key rules:**
- Cache miss = server fetches upstream, client waits (no client-side fallback)
- Server unreachable = 503 to client's package manager
- Prefetch keeps cache warm for tracked packages

## Changes Made

### 1. crates/monopsony-client/src/main.rs — Derive HTTP proxy address from gRPC server address

The `--server` flag provides `host:50051` (gRPC port). We now derive the HTTP proxy address as `host:3128`:

```rust
// Lines 400-413 in main.rs
let proxy_server_addr = {
    let server_host = server_addr.split(':')
        .next()
        .unwrap_or("127.0.0.1");
    format!("{}:3128", server_host)
};

let proxy_config = proxy::ProxyConfig {
    server_addr: proxy_server_addr,
    timeout_ms,
};
```

### 2. crates/monopsony-client/src/proxy.rs — Complete rewrite

**Before:** Reverse proxy that concatenated paths (`http://server/<path>`) and fell back to upstream on any error. It also matched URL patterns against package managers, which was unnecessary complexity.

**After:** Minimal forward proxy that:
1. Receives absolute URLs from package manager (e.g., `GET http://mirror.example.com/core/os/x86_64/core.db`)
2. Prepends the server's HTTP proxy address: `http://server:3128/http://mirror.example.com/...`
3. Forwards to server proxy (which is an HTTP forward proxy listening on port 3128)
4. Returns 503 on ANY error (timeout, connection refused, server error) — NO upstream fallback

Key code changes:
- Removed all package-manager-specific URL pattern matching
- Removed upstream fallback logic entirely
- Changed URL construction from `http://<server_addr><path>` to `http://<server_addr>/<absolute_url>`
- Added 503 responses for timeout, connection error, and server error
- Server's `X-Cache: HIT/MISS` header propagates through to the package manager

### 3. Server proxy (crates/monopsony-proxy/src/proxy.rs) — No changes needed

The server's HTTP forward proxy on port 3128 already:
- Accepts absolute URLs in request line (`GET http://host/path`)
- Checks cache, fetches upstream on miss, caches result
- Returns `X-Cache: HIT` or `X-Cache: MISS` headers
- Verifies SHA256 on cached files (tamper detection)

### 4. Prefetch scheduler (crates/monopsony-proxy/src/prefetch.rs) — No changes needed

Runs every 3600s, queries `find_uncached_packages()` from DB inventory, fetches missing packages into cache. Works independently once the proxy chain is functional.

## Files Modified

| File | Change Type |
|------|-------------|
| `crates/monopsony-client/src/main.rs` | Modified: proxy address derivation + config |
| `crates/monopsony-client/src/proxy.rs` | Rewritten: complete architecture change |

## Verification Plan (for fresh session)

1. **Clear server cache**: `rm -rf /var/cache/monopsony/*` on server (10.10.20.58)
2. **Start client proxy**: `monopsony-client --cert-dir /var/lib/monopsony/certs daemon --server 10.10.20.58:50051` on Arch client (10.10.20.20)
3. **Test cache MISS**: `pacman -Sy` on client → verify server cache gets populated, `X-Cache: MISS` in response
4. **Test cache HIT**: `pacman -Sy` again → verify `X-Cache: HIT` header, no upstream fetch
5. **Test 503 on failure**: Stop server proxy (`systemctl stop monopsony-proxy`) → `pacman -Sy` → verify 503 returned, NO fallback to upstream
6. **Verify dashboard**: Check http://10.10.20.58:3130 shows cache entries growing

## Architecture Diagram (Corrected)

```
┌─────────────────────────────────────────────────────────────────┐
│  Client Host (10.10.20.20)                                      │
│  ┌─────────────┐    127.0.0.1:3129     ┌─────────────────────┐  │
│  │ pacman/apt  │ ────────────────────▶ │ monopsony-client    │  │
│  │ -x proxy    │  GET http://mirror/   │ proxy.rs (NEW)      │  │
│  └─────────────┘                       │ forwards to         │  │
│                                        │ http://10.10.20.58  │  │
└────────────────────────────────────────│ :3128/http://mirror/│──┘
                                         └──────────┬──────────┘
                                                    │
                                                    ▼
┌─────────────────────────────────────────────────────────────────┐
│  Server Host (10.10.20.58)                                      │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ monopsony-proxy (port 3128) — HTTP FORWARD PROXY           │ │
│  │ ┌──────────┐  miss  ┌──────────────┐  cache  ┌───────────┐ │ │
│  │ │ Cache?   │ ───▶   │ Fetch upstream │ ───▶   │ Save to   │ │ │
│  │ │ (disk+DB)│        │ (original URL) │        │ disk+DB   │ │ │
│  │ └──────────┘        └──────────────┘         └───────────┘ │ │
│  │    │ hit                                            ▲       │ │
│  │    ▼                                               │       │ │
│  │ ┌──────────┐                                        │       │ │
│  │ │ Serve    │                                        │       │ │
│  │ │ from disk│                                        │       │ │
│  │ └──────────┘                                        │       │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

## Key Behavioral Changes

| Scenario | Old Behavior | New Behavior |
|----------|-------------|--------------|
| Cache MISS | Client timed out (wrong port) → fell back to upstream | Server fetches upstream, client waits, cache filled |
| Cache HIT | Never reached server proxy (wrong port) | Server serves from cache, `X-Cache: HIT` |
| Server proxy down | Client fell back to upstream silently | **503 Service Unavailable** to package manager |
| Client proxy timeout | Fell back to upstream | **503 Service Unavailable** |
| Prefetch | Worked but cache never used | Worked + cache actually used by clients |

## Test Commands

```bash
# On server (10.10.20.58) - clear cache
rm -rf /var/cache/monopsony/*

# On client (10.10.20.20) - start proxy
monopsony-client --cert-dir /var/lib/monopsony/certs daemon --server 10.10.20.58:50051

# Test cache miss
curl -x http://127.0.0.1:3129 -v http://geo.mirror.pkgbuild.com/core/os/x86_64/core.db 2>&1 | grep -i x-cache

# Test cache hit
curl -x http://127.0.0.1:3129 -v http://geo.mirror.pkgbuild.com/core/os/x86_64/core.db 2>&1 | grep -i x-cache

# Test 503 when server down (run on server: systemctl stop monopsony-proxy)
curl -x http://127.0.0.1:3129 -v http://geo.mirror.pkgbuild.com/core/os/x86_64/core.db 2>&1 | grep -i "503\|service unavail"
```