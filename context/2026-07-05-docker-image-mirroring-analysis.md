# Docker Image Mirroring — Feasibility Analysis

Created: 2026-07-05
Status: Design analysis. Not implemented.

## The Question

How hard would it be to have Monopsony cache Docker images that clients pull? This
document analyzes the options, effort, and recommended approach.

## How Docker Pulls Images

When `docker pull nginx:latest` runs:

1. **Registry resolution**: Docker daemon contacts `registry-1.docker.io` (default)
2. **Auth dance**: Gets 401, contacts `auth.docker.io` for a Bearer token
3. **Manifest fetch**: `GET /v2/library/nginx/manifests/latest` → JSON listing layers
4. **Blob downloads**: `GET /v2/library/nginx/blobs/sha256:<hash>` for each layer
5. Layers are content-addressed tar.gz files (same hash = same content across all hosts)

Key hosts involved:
- `registry-1.docker.io` — main registry
- `auth.docker.io` — token endpoint
- `production.cloudflare.docker.com` — CDN backing for Docker Hub blobs
- Custom registries: `ghcr.io`, `gcr.io`, `quay.io`, `registry.gitlab.com`, etc.

## Four Approaches

### Option A: Docker's Native Registry Mirror Support (RECOMMENDED)

Docker has **built-in** support for registry mirrors. You configure it client-side:

```json
// /etc/docker/daemon.json on each client
{
  "registry-mirrors": ["http://10.10.20.58:3128"]
}
```

Docker then sends registry API V2 requests to Monopsony's proxy instead of Docker Hub.

**What Monopsony needs to do**: Implement the Docker Registry HTTP API V2 endpoints
on the existing HTTP proxy (port 3128). Only 3 endpoints:

| Endpoint | Purpose | Already cached? |
|----------|---------|-----------------|
| `GET /v2/` | API check (return `{}`) | N/A (instant response) |
| `GET /v2/<name>/manifests/<ref>` | Image manifest (JSON) | Cacheable — small JSON |
| `GET /v2/<name>/blobs/<digest>` | Layer blob (large binary) | **Perfect cache candidate** |

Layer blobs are sha256-addressed and immutable — the same digest serves identical
content to every client. This is the ideal case for a caching proxy.

**Auth handling**: When Docker gets a 401 from the mirror, it contacts the original
`auth.docker.io` directly for a token, then sends the token to the mirror. The mirror
(proxy) doesn't need to handle auth at all — it just validates the Bearer token
or ignores it (open mirror on LAN).

**Effort**: Medium. Need to add 3 route patterns to the proxy router and a
`DockerBackend` that matches `registry-1.docker.io/v2/`, `ghcr.io/v2/`, etc. The
existing caching infrastructure (file storage + DB metadata) works as-is. The blob
responses are just HTTP GET responses with large bodies — same cache path as `.deb`
or `.rpm` files.

**Advantage**: Native Docker support, no client-side proxy config, no HTTPS issues
(Docker talks HTTP to the mirror on the LAN).

### Option B: HTTP Proxy Mode (HTTPS CONNECT Required)

Docker daemon respects `HTTPS_PROXY` / `HTTP_PROXY` env vars. All registry traffic
goes through the proxy as HTTPS CONNECT tunnels.

**Blocker**: Monopsony's proxy does NOT implement CONNECT tunneling. This is the
third time this limitation has surfaced (pacman mirrorlist, Rocky dnf, and now
Docker). Implementing CONNECT would unblock Docker, Rocky mirrorlist, and any
other HTTPS-only client.

**Even with CONNECT**: The proxy can't see inside the TLS tunnel to cache
responses. It would need to do TLS interception (MITM) with a trusted CA on every
client. This is complex, fragile, and a security concern.

**Verdict**: Not viable without MITM. Not recommended.

### Option C: Local Registry as Pull-Through Cache

Run a standard Docker registry container configured as a pull-through cache:

```bash
docker run -d -p 5000:5000 -e REGISTRY_PROXY_REMOTEURL=https://registry-1.docker.io \
  -v /var/lib/registry:/var/lib/registry registry:2
```

Clients configure `daemon.json` with `registry-mirrors: ["http://10.10.20.58:5000"]`.

Monopsony's role: cache the registry container's upstream fetches at the HTTP
proxy level (port 3128), or just let the registry handle its own caching.

**Effort**: Minimal. The registry container does all the work. Monopsony just needs
to proxy its traffic. But this adds a Docker dependency on the server host.

**Verdict**: Good quick-and-dirty solution, but adds a runtime dependency and
doesn't integrate with Monopsony's cache/storage/DB.

### Option D: Content-Addressed Blob Cache (Monopsony Native)

Add URL patterns for Docker registry blob URLs. When a client fetches a blob through
the proxy (via Option A's mirror mode), Monopsony caches it by digest.

```
URL: http://registry-1.docker.io/v2/library/nginx/blobs/sha256:abc123...
Cache path: docker/v2/library/nginx/blobs/sha256:abc123...
```

This is essentially Option A's blob caching, but framed as a natural extension of the
existing PM backend pattern: add a `DockerBackend` with URL patterns that match
registry blob/manifest endpoints.

## Recommended Path: Option A (Registry Mirror API)

### Phase 1: Minimal (manifest + blob passthrough with caching)

1. **Add registry V2 routes** to Monopsony's proxy router:

```rust
// In proxy.rs, add routes before the catch-all
.route("/v2/", axum::routing::get(handle_v2_api_check))
.route("/v2/:name/manifests/:reference", axum::routing::get(handle_manifest))
.route("/v2/:name/blobs/:digest", axum::routing::get(handle_blob))
```

2. **handle_v2_api_check**: Return `{}` with 200. This tells Docker "I am a V2 registry."

3. **handle_manifest**: Proxy the request to the upstream registry
   (`registry-1.docker.io`), cache the JSON response. Pass through the
   `Authorization: Bearer <token>` header from Docker. Cache key = full URL.

4. **handle_blob**: Proxy the request to upstream, cache the response body.
   These are large immutable files (10-500MB each). Cache key = digest.
   Reuse existing `ProxyState::find_backend_for_url()` → `url_to_cache_path()`.

5. **Client config**: Add to ProxyConfigAction for Docker:
   ```json
   {"registry-mirrors": ["http://<monopsony-ip>:3128"]}
   ```
   Written to `/etc/docker/daemon.json` (Linux) or via PowerShell
   `Set-Content` (Windows).

### Phase 2: Multi-registry support

Docker clients can only specify ONE upstream registry per `registry-mirrors` entry,
but Monopsony can proxy to multiple upstream registries based on the image name prefix:

- `library/nginx` → `registry-1.docker.io`
- `ghcr.io/owner/repo` → `ghcr.io` (path includes the registry host)
- `quay.io/owner/repo` → `quay.io`

Monopsony's proxy needs to detect which upstream registry to use based on the
`/v2/<name>` path. For Docker Hub images (no prefix), use `registry-1.docker.io`.
For others, extract the registry host from the image name.

### Phase 3: Manifest caching with TTL

Manifests are mutable (`latest` tag changes). Cache them with a TTL (default: 1 hour)
so clients see updated images eventually. Layer blobs are immutable — cache forever.

Add TTL support to the existing cache infrastructure:
- `cache_entries` table already has `cached_at` timestamp
- Add config: `[cache] manifest_ttl_seconds = 3600`
- On cache hit, check if `now - cached_at > ttl` for manifest URLs

## What Makes This Easy (already in place)

- HTTP proxy already runs on port 3128 with caching infrastructure
- File-based cache with DB metadata (content_length, hit_count, cached_at)
- URL pattern matching via `PackageManagerBackend` trait
- Multi-backend support (add DockerBackend alongside pacman/apt/dnf/apk)
- Rate limiting, bandwidth throttling, concurrent connection limits
- Security: SSRF prevention, body size limits

## What's Missing

| Gap | Effort | Notes |
|-----|--------|-------|
| Registry V2 route handlers | 4-6 hrs | 3 endpoints, reuses existing cache |
| Manifest TTL support | 2 hrs | Add TTL check to cache hit logic |
| Docker client config (daemon.json writer) | 1 hr | ProxyConfigAction for Docker |
| Auth token passthrough | 1 hr | Forward Authorization header to upstream |
| Multi-registry routing | 2 hrs | Parse image name to determine upstream |

**Total estimated effort for Phase 1**: ~8 hours (1-2 focused sessions).

## Why This Is Worth Doing

Docker image pulls are the single largest bandwidth consumer in modern dev/ops
environments. A typical `docker pull` for a full application stack (e.g., a Rails
app with PostgreSQL, Redis, sidekiq) can be 2-5 GB of layer data. Across 10-50
developers or CI workers pulling the same images, that's 20-250 GB of redundant
downloads from Docker Hub.

With Monopsony caching:
- First pull: ~5 GB from Docker Hub
- Subsequent pulls: 0 bytes from Docker Hub (100% cache hit on shared base layers)
- Typical cache hit rate: 80-95% (base layers like `ubuntu:22.04` are shared across
  hundreds of images)

This is the highest-impact feature Monopsony could add for the SMB/office target
market.

## Competition / Prior Art

- **Harbor**: Full-featured registry with caching proxy mode (heavy, requires Docker)
- **Nexus Repository**: Supports Docker proxy repos (Java, heavyweight)
- **Sonatype Nexus**: Same as above
- **Artifactory**: Commercial, supports Docker proxy
- **distribution/distribution3**: The reference registry, has pull-through cache mode

Monopsony's advantage: no extra containers, no JVM, runs as a single binary,
integrates with existing package caching for apt/dnf/pacman/apk.
