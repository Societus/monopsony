# Monopsony - Development Context

```

## Project Overview

```
Monopsony is a centralized package caching proxy system - like Windows Update's LAN sharing but for all package managers across all OSes. A central server pre-fetches and caches packages from upstream repositories, and clients redirect their package managers through it via a local HTTP proxy.
**Target scale:** SMB/office (20-200 clients, single server)
**Git state:** Forgejo: base.societus.club/societus/monopsony.git. Local: ~/projects/monopsony. **v1.0.0-rc2. Sprint 27 (client health & CheckMK) CLOSED 2026-10-31 — Part A+B landed 2026-10-16 (9b5337b/e6cda8e/82af381), fleet bench-proven 2026-10-17; sprints 29–33 landed in phases, per-sprint state in docs/sprint-roadmap.md. sqlx 0.9.0 + rusqlite 0.37 landed 2026-10-31 (libsqlite3-sys unified, future-incompat gone). 1348 workspace tests passing, clippy: 15 pre-existing warnings only (2026-10-31). 16 PM backends (15 distro/language + Homebrew).**
## Build & Run
```
bash
cargo check                    # Verify compilation
cargo build                    # Build all crates
cargo build -p monopsony-server # Build server only
cargo build -p monopsony-client # Build client only
cargo test                     # Run tests
```
**Container:** `docker compose up -d` or `podman-compose up -d`. See `docs/container-deploy.md`. Multi-stage Dockerfile produces Alpine/musl image. Entrypoint auto-detects Docker/Podman and rootless mode.

**protoc** must be installed for gRPC codegen (`pacman -S protobuf` or `uv tool install grpcio-tools`). Common warnings: unused fields on `AptBackend.config_path`, `DnfBackend.config_path` -- these are harmless.

**PROTOC env var:** Set `PROTOC=/home/societus/.local/bin/python-grpc-tools-protoc` if using the uv-installed protoc.

## Compilation Status

| Crate | Status | Notes |
|-------|--------|-------|
| `monopsony-common` | Compiles clean | shadow_types (shared), AuthMode enum, config validation |
| `monopsony-db` | Compiles clean | PostgreSQL Database struct, migrations, CRUD for clients/packages/cache_entries/api_keys |
| `monopsony-proxy` | Compiles clean | HTTP caching proxy, cache_storage, object_store_backend, prefetch, purge, security |
| `monopsony-control` | Compiles clean | gRPC server, auth, discovery (mDNS), TLS, metrics, NAT, announce, tracker |
| `monopsony-mirror` | Compiles clean | MirrorManager, manifest signing, public_rate_limit |
| `monopsony-shadow` | Compiles clean | ShadowPackageManager, signing, recipe, conflict, cache, source, git_mirror |
| `monopsony-server` | Compiles clean | Thin orchestrator -- wires the 5 crates above into a single binary |
| `monopsony-client` | Compiles clean | Local proxy, gRPC client, shadow CLI, trust enforcement |
| `monopsony-tracker` | Compiles clean | HTTP API, recipe admin routes (quarantine reinstate/remove), graceful shutdown |

Full workspace: `cargo check` passes, 0 errors, warnings only (dead_code in shadow modules -- not yet wired to gRPC handlers).
Tests: 1348 passed, 0 failed, 3 ignored (workspace, 2026-10-31). Clippy: 15 pre-existing warnings (shadow_executor ×2, trust.rs, grpc_client ×4, control grpc.rs, tls.rs, pairing ×6) — new code stays clean.

## Release Status

**v1.0.0-rc2** — All sprints complete. Release CI ready (dual-arch musl, SBOM, GPG signing). Requires GitHub secrets: `GPG_PRIVATE_KEY`, `GPG_PASSPHRASE`, `GPG_KEY_ID`. Tag with `git tag -s v1.0.0` to trigger release workflow.
## Critical Issues

None currently. All modules compile.
## Workspace Structure
```
monopsony/
├── Cargo.toml                    # Workspace root (workspace.package, workspace.dependencies)
├── Cargo.lock
├── AGENTS.md                     # This file
├── docs/
│   ├── sprint-roadmap.md
│   ├── sprint-rollback-log.md
│   ├── design/
│   │   └── shadow-packages.md      # Shadow package system intent/north-star — read before extending the recipe pipeline
│   └── research/
│       └── public-mirror-network.md  # Phase 1-3 design: public mirrors, tracker, darknet transports
├── Dockerfile                     # Multi-stage container build (Alpine/musl)
├── docker-compose.yml             # Server + PostgreSQL (Docker/Podman)
├── dist/
│   ├── container/
│   │   └── entrypoint.sh          # Container entrypoint (Docker/Podman/rootless detect)
│   ├── install.sh                  # curl|bash installer (client default, --server for server); init-system detection incl. launchd
│   ├── init/                       # Canonical init templates (systemd x2, OpenRC, runit run+log, s6, SysV, launchd plist)
│   │   ├── monopsony-server.service    # systemd unit (server)
│   │   ├── monopsony-server.openrc     # OpenRC init script
│   │   └── monopsony-client.service    # systemd unit (client daemon)
│   ├── monopsony-tmpfiles.conf     # tmpfiles.d entry
│   ├── monopsony-sysusers.conf     # sysusers.d entry
│   ├── server.toml.example         # Fully commented server config
│   └── mirrors.toml.example        # Mirror config examples (Arch, Debian, Alpine)
├── .github/workflows/ci.yml       # CI: check, test, clippy, musl build
└── proto/
│   └── monopsony.proto           # gRPC service definitions (MonopsonyControl service)
└── crates/
    ├── monopsony-common/         # Shared types, traits, proto stubs
    │   ├── Cargo.toml
    │   ├── build.rs              # Proto compilation via tonic-build
    │   └── src/
    │       ├── lib.rs            # Re-exports: config::*, Error/Result, models::*, PackageManagerBackend/Kinds/Ref/ProxyConfigAction/UrlPattern
    │       ├── config.rs         # ServerConfig, ClientConfig + sub-structs (all TOML-based)
    │       ├── models.rs         # ClientId, PackageId, ApiKeyId, Client, Package, ClientPackage, ApiKey, PurgeEntry, OsInfo, ServerStatus, DiscoveryInfo
    │       ├── error.rs          # Error enum (Config, Io, Database, PackageManager, Cache, Network, Tls, Certificate, Auth, Discovery, Grpc, InvalidUrl, PackageNotFound, ClientNotFound, UpstreamFetch, HashMismatch, CommandFailed, Parse, Toml)
    │       ├── metrics.rs        # Metric names + helper fns (cache_request_hit/miss, etc.) — called from client proxy + control client_health
    │       ├── proto.rs          # Re-exports tonic-generated proto types
    │       ├── shadow_types.rs  # Shared types: ShadowSource, InstallMethod, ShadowRecipe, ReleaseAsset, AssetSelector, HealthCheck
    │       └── package_manager/
    │           ├── mod.rs        # PackageManagerBackend trait, PackageManagerKind, PackageRef, UrlPattern, ProxyConfigAction, get_available_backends(), get_backend()
    │           ├── apt.rs        # Full implementation (dpkg -l, apt proxy config, Debian/Ubuntu URL patterns)
    │           ├── dnf.rs        # Full implementation (rpm/dnf list_installed, extended URL patterns)
    │           ├── pacman.rs     # Full implementation (pacman -Q list_installed, extended URL patterns)
    │           ├── winget.rs     # Full implementation (winget list, GitHub CDN URL patterns)
    │           ├── choco.rs      # Full implementation (choco list --local-only, chocolatey.org patterns)
    │           └── scoop.rs      # Full implementation (scoop list, GitHub raw URL patterns)
    ├── monopsony-db/             # Database layer (extracted from monopsony-server)
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       └── db.rs             # PostgreSQL Database struct with PgPool, migrations, CRUD
    ├── monopsony-proxy/          # HTTP caching proxy (extracted from monopsony-server)
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── proxy.rs          # Full HTTP caching proxy with CacheEntry metadata, ProxyState
    │       ├── cache_storage.rs  # File-based cache with JSON metadata
    │       ├── object_store_backend.rs  # Object store backend abstraction
    │       ├── prefetch.rs       # Prefetch task for warming cache
    │       ├── purge.rs          # Cache purge task
    │       └── security.rs       # URL validation, private IP checks, body size limits
    ├── monopsony-control/        # gRPC + coordination (extracted from monopsony-server)
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── grpc.rs           # MonopsonyControl tonic trait impl
    │       ├── auth.rs           # AuthMode: open, token, mTLS
    │       ├── discovery.rs      # DiscoveryService with mDNS broadcast via mdns-sd
    │       ├── tls.rs            # TlsManager with CA load/create, mTLS config
    │       ├── metrics.rs        # Metrics HTTP server (/health, /metrics)
    │       ├── nat.rs            # NAT type detection
    │       ├── announce.rs       # Tracker announcement
    │       └── tracker.rs        # Tracker client
    ├── monopsony-mirror/         # Mirror subsystem (extracted from monopsony-server)
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── mirror.rs         # MirrorManager, MirrorState, Axum router
    │       ├── manifest.rs       # Mirror manifest with Ed25519 signing
    │       ├── public_rate_limit.rs  # Rate limiting for public mirrors
    │       └── url_validation.rs # Duplicated from security.rs (is_private_ip, validate_url_host)
    ├── monopsony-shadow/         # Shadow packages (extracted from monopsony-server)
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── shadow.rs         # ShadowPackageManager: orchestration
    │       ├── shadow_signing.rs # Ed25519 recipe signing: keygen, sign, verify
    │       ├── shadow_recipe.rs  # TOML recipe parsing + validation
    │       ├── shadow_conflict.rs # PM conflict detection + masking
    │       ├── shadow_cache.rs   # Content-addressed blob store + disk-budget GC
    │       ├── shadow_source.rs  # UpdateSource trait + GithubReleaseSource + GitBuildSource + DirectUrlSource
    │       └── shadow_git_mirror.rs # Git mirror maintenance
    ├── monopsony-server/         # Thin orchestrator binary
    │   ├── Cargo.toml
    │   └── src/
    │       └── main.rs           # CLI (clap), wires db+proxy+control+mirror crates into single binary
    ├── monopsony-client/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── main.rs           # CLI (clap) with subcommands: Daemon, Discover, Status, Configure, Pair
    │       ├── proxy.rs          # Local HTTP proxy with timeout + upstream fallback
    │       ├── grpc_client.rs    # Real tonic gRPC client with register/heartbeat/inventory RPCs
    │       ├── discovery.rs      # mDNS browsing with spawn_blocking
    │       ├── shadow.rs         # Client-side shadow package ops
    │       ├── shadow_cli.rs     # Clap subcommands for shadow ops
    │       └── trust.rs          # Client trust enforcement
    └── monopsony-tracker/
        ├── Cargo.toml
        └── src/
            └── main.rs           # HTTP API, recipe admin routes, graceful shutdown
```

## Key Types & Interfaces

### PackageManagerBackend Trait (monopsony-common/src/package_manager/mod.rs:137)
```rust
#[async_trait]
pub trait PackageManagerBackend: Send + Sync {
    fn kind(&self) -> PackageManagerKind;
    fn is_available(&self) -> bool;
    async fn list_installed(&self) -> Result<Vec<PackageRef>>;
    fn upstream_url_patterns(&self) -> Vec<UrlPattern>;
    fn url_to_cache_path(&self, url: &str) -> Option<String>;
    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction;
    fn remove_proxy_config(&self) -> ProxyConfigAction;
}
```

### ProxyConfigAction Enum (monopsony-common/src/package_manager/mod.rs:105)
```rust
pub enum ProxyConfigAction {
    WriteFile { path: PathBuf, content: String },
    RemoveFile { path: PathBuf },
    SetEnv { key: String, value: String },
    UnsetEnv { key: String },
    RunCommand { program: String, args: Vec<String> },
    Multiple(Vec<ProxyConfigAction>),
    None,
}
```

### Server HTTP Proxy (monopsony-proxy/src/proxy.rs)
The server's HTTP proxy is the most complete module. It:
- Uses `axum` with `ConnectInfo<SocketAddr>` for the router
- `ProxyState` holds a `hyper` client, cache_dir, meta_dir, and package manager backends
- Matches incoming URLs against package manager URL patterns
- On cache hit: reads file from disk + JSON metadata, returns with X-Cache: HIT
- On cache miss: fetches upstream, saves file + JSON metadata to disk, returns with X-Cache: MISS
- Non-package URLs pass through without caching
- Cache metadata stored as JSON sidecar files in `meta/` subdirectory
- DB read-through: checks `db.get_cache_entry()` first, then verifies filesystem. DB-less mode falls through to filesystem check.

### gRPC Service Definition (proto/monopsony.proto)
- `MonopsonyControl` service with bidirectional streaming `ControlStream`
- Unary RPCs: `Register`, `ReportInventory`, `Heartbeat`
- Pairing messages: `PairRequest` (code + client info), `PairResponse` (certs + client_id + grpc_addr)
- Messages: `ClientMessage` (oneof: register/heartbeat/inventory/delta), `ServerMessage` (oneof: register/command/sync_request)
- `Command` enum: RESYNC, UPDATE_CONFIG, RESTART
- `InventoryDelta` handler is a stub (logs warning, returns no-op)

## Implementation Plan

| Step | Description | Status | Notes |
|------|-------------|--------|-------|
| 1 | Workspace skeleton + common types | Done | All types, traits, models in place |
| 2 | Server HTTP caching proxy | **Done** | proxy.rs works with file-based cache + DB write-through |
| 3 | PostgreSQL integration | **Done** | db.rs — PgPool, migrations, CRUD. UPSERT inventory (D-02 fixed) |
| 4 | mTLS + Server Discovery | **Done** | discovery.rs done, tls.rs implemented, mTLS wired into gRPC (always required) |
| 5 | Client local proxy | **Done** | proxy.rs with timeout + fallback |
| 6a | gRPC Basic (register + heartbeat) | **Done** | Tonic server trait + client wired, real RPCs |
| 6b | gRPC Inventory Reporting | **Done** | ReportInventory RPC + bidirectional ControlStream |
| 7 | Pairing mechanism | **Done** | 6-char code exchange on plaintext TCP port 50052, protobuf-framed. Server generates client cert, client saves + reconnects via mTLS |
| 8 | Client daemon mode + CLI | **Done** | Daemon requires paired certs, heartbeat loop + inventory reporting work. Exponential backoff reconnect (1s→60s). Configure subcommand executes ProxyConfigActions. |
| 9 | Additional package managers | **Done** | All 15 backends implemented (apt, dnf, pacman, apk, winget, choco, scoop, mport, pkg, pkg_add, pkgin, zypper, xbps, emerge, nix) |
| 10 | Pre-fetch scheduler | **Done** | prefetch.rs: inventory-driven prefetch with priority (client_count DESC), bounded concurrency (buffer_unordered), per-request timeout, SSRF validation, bandwidth throttle, atomic writes, response size limit. |
| 11 | Hardening + Public Mirror Network | **Done** | Phase 1 complete: graceful shutdown (broadcast + 30s drain timeout), Prometheus metrics wired (proxy/gRPC/prefetch + mirror sync/serve), rate limiting middleware (token-bucket per-IP), auth middleware (X-Monopsony-Key for private mirrors), mirror manifest endpoint, SHA-256 hash verification during sync. Phase 2+3 design in `docs/research/public-mirror-network.md`. |
| 12 | Distribution mirror support | **Done** | mirror.rs module, DB schema + CRUD, client `Mirror` subcommand, Mirror HTTP router (:3129), sync loop, gRPC MirrorControl service (5 RPCs: ListMirrors, TriggerMirrorSync, GetMirrorStatus, UpsertMirrorConfig, DeleteMirrorConfig). |
| 13 | Phase 2: Tracker service | **Done** | `monopsony-tracker` crate: SQLite-backed HTTP tracker, POST /announce + GET /discover, haversine geo scoring, stale purge, 5 tests. Default port :3130. |
| 14 | Phase 2: Peer exchange | **Done** | GET /.well-known/monopsony/peers on mirror router. Serves local peer list derived from tracker data. |
| 15 | Phase 2: Mirror announce-to-tracker | **Done** | announce.rs module in server. TrackerAnnouncer background task, CLI args --tracker-url / --tracker-interval, periodic POST /announce of public mirrors. reqwest dep added. |
| 16 | Phase 2: Client discovery v2 | **Done** | Multi-source discovery: tracker → peer exchange → mDNS fallback. New CLI args --tracker-url, --peer-url, --distro on Discover subcommand. DiscoveryResult aggregates all sources. |
| 17 | Phase 2: STUN NAT detection | **Done** | nat.rs module in server. RFC 5389 STUN binding request, XOR-MAPPED-ADDRESS parsing, NatType enum (Public/FullCone/RestrictedCone/PortRestrictedCone/Symmetric/Unknown), 4 tests. |

## Architecture Decisions

| Decision | Choice |
|----------|--------|
| Language | Rust (edition 2021) |
| Interception | Local HTTP proxy on clients |
| Control plane | gRPC with bidirectional streaming (`tonic`) |
| Security | mTLS with built-in private CA (`rustls` + `rcgen`) — always required on gRPC port 50051. Pairing via plaintext TCP port 50052 (6-char code exchange, protobuf-framed) |
| Server discovery | mDNS (`mdns-sd`) with manual config fallback |
| Auth | API keys (validated over mTLS connection) |
| Database | PostgreSQL (via `sqlx` with `runtime-tokio`, `tls-rustls`) |
| HTTP framework | `axum` 0.7 (server proxy), `hyper` 1.x (upstream client) |
| Observability | Prometheus metrics (`metrics` crate) — wired into proxy/gRPC/prefetch/mirror; exporter on :9090/metrics |
| CLI | `clap` 4 with derive macros |
| Caching | File-based with JSON sidecar metadata (will migrate to PostgreSQL for tracking) |

## Key Dependencies (workspace-level)

- **tokio** 1 (full) — async runtime
- **axum** 0.7, **tower** 0.5, **tower-http** 0.6 — HTTP server
- **hyper** 1, **hyper-util** 0.1 — HTTP client for upstream fetch
- **tonic** 0.12, **prost** 0.13 — gRPC (no `tls` feature enabled)
- **sqlx** 0.9 (postgres, chrono, uuid) — Database (upgraded from 0.8.0, 2026-10-31; libsqlite3-sys/rusqlite conflict resolved: sqlx 0.9 + rusqlite 0.37 unify on libsqlite3-sys 0.35, owned solely by rusqlite; postgres-only, sqlite feature never enabled)
- **rustls** 0.23, **rcgen** 0.13 (with x509-parser feature) — TLS/CA
- **mdns-sd** 0.11 — mDNS discovery
- **metrics** 0.24, **metrics-exporter-prometheus** 0.16 — Observability (wired into proxy/gRPC/prefetch/mirror; :9090 exporter)
- **clap** 4 (derive, env) — CLI
- **tracing** 0.1, **tracing-subscriber** 0.3 (env-filter, json) — Logging
- **sha2** 0.10, **hex** 0.4 — Hashing
- **chrono** 0.4, **uuid** 1 — Time/ID types
- **tokio-cron-scheduler** 0.13 — Pre-fetch scheduling (not yet used)

## Known Gaps (what needs doing before alpha)

1. ~~**Client reconnect logic**~~ — **DONE**
2. ~~**Execute ProxyConfigActions**~~ — **DONE**
3. ~~**Read-through from DB in proxy**~~ — **DONE**
4. ~~**Unify config structs**~~ — **DONE**
5. ~~**Distribution mirror support**~~ — **DONE**
6. ~~**Pre-fetch scheduler**~~ — **DONE**
7. ~~**Hardening**~~ — **DONE** Phase 1 complete. Rate limiting, auth, hash verification, graceful shutdown, Prometheus metrics, manifest endpoint.
8. ~~**Metrics integration**~~ — **DONE** metrics crate wired into proxy/gRPC/prefetch/mirror. Prometheus exporter on :9090/metrics.

## Remaining work (post-alpha / Phase 3)
- Phase 3: Tor/I2P/GNUnet transport support
- Configurable hash verification policy (fail-closed vs fail-open)
- GPG signature verification for repo metadata (Release/InRelease files)
- ~~Resolve sqlx 0.8.6+ upgrade blocked by libsqlite3-sys link conflict with rusqlite 0.31~~ — DONE 2026-10-31 (sqlx 0.9.0 + rusqlite 0.37; libsqlite3-sys unified at 0.35.0; sqlx-postgres 0.8.0's never-type-fallback future-incompat lint also fixed. Premise correction: sqlx-postgres never pulled libsqlite3-sys — the resolve-time conflict came from the sqlx facade's optional sqlx-sqlite dep (links = "sqlite3") whose libsqlite3-sys range >=0.30.1,<0.38.0 was disjoint from rusqlite 0.31's ^0.28.)
- Dashboard SPA frontend for pairing approval + enrollment key manager (P4 UI — backend API complete)

## Pairing v2 (Sprint 28) — SECURITY-CRITICAL NOTES
Plan: `docs/plans/2026-10-17-pairing-v2-sunshine-pin.md` (P0–P7 complete, uncommitted).
Security docs: `docs/pairing-security.md` — read before touching pairing code.

- **Three client flows** on `pair`: `--code` (legacy, LAN-only), `--pin` (v2 Sunshine:
  client-generated PIN + ephemeral keypair → dashboard approve → collect w/ possession
  proof), `--enrollment-key mopk-…` (pre-approved fleet path). Certs are 90-day,
  serial-tracked, revocable at the TLS accept path; daemon auto-renews inside 30 days.
- **Never** reintroduce unauthenticated code minting (the red-team F-class failures):
  `POST /api/v1/pair/init` was reachable by anyone. v2 intake endpoints
  (`/pair/request`, `/pair/collect`, `/pair/enroll`) are intentionally unauthenticated
  but rate-limited and exempted in `auth_middleware` — keep that exemption list exact.
- **Registration-at-collection**: no DB row exists until the client proves possession.
  Do not "optimize" by registering at intake.
- **Key invariants** (tested in pairing.rs): 3-strike PIN invalidation, 15-min pending
  TTL, 5-min approved-uncollected TTL, idempotent approve, dedup-replace on same
  claimed identity + source IP, cap 50 pending.
- Enrollment keys stored **hashed only** (`mopk-` prefix, plaintext shown once);
  use-count + expiry + revocation enforced server-side (`enrollment_keys` table).
- Approver attribution in `pair_approval_audit` is `system`/`enrollment-key` —
  session-username attribution is a known gap.


## RC milestone: Interactive bash installer
**Target:** `curl -fsSL https://monopsony.dev/install.sh | bash` (like Tailscale's installer)
**Audience:** Beginner to expert. Must guide non-DBAs through PostgreSQL setup while offering experts a fast path.

### Installer flow
1. **Detect OS** (Arch, Debian/Ubuntu, Fedora, Alpine) and install monopsony-server binary + dependencies
2. **PostgreSQL setup choice:**
   - **Option A: Local PostgreSQL** — installer runs `pacman -S postgresql` / `apt install postgresql`, initializes cluster, starts service, creates `monopsony` user + db, sets random password, writes connection string to `/etc/monopsony/server.toml`
   - **Option B: Existing PostgreSQL** — user provides host/port. Installer:
     - Tests connectivity
     - Guides through: `CREATE USER monopsony WITH PASSWORD '...';`, `CREATE DATABASE monopsony OWNER monopsony;`
     - For remote PG: reminds about `pg_hba.conf` entries (host monopsony monopsony <server-ip>/32 md5) and `postgresql.conf` `listen_addresses`
     - Offers to output ready-to-paste SQL and config snippets if installer can't connect as superuser
   - **Option C: No database** — `--no-db` mode with clear warning about feature limitations (no client management, no inventory, file-only cache)
3. **TLS CA setup** — generate self-signed CA or accept existing certs
4. **Start service** — systemd unit or openrc script, enable on boot
5. **Print pairing info** — server address, first pairing code

### Key implementation notes
- The installer script should be self-contained POSIX sh (not bash-specific where possible, but bash is fine for Tailscale-style)
- Password generation: `openssl rand -base64 24` or `pwgen`
- Config file templating: write `/etc/monopsony/server.toml` with all discovered values
- Idempotent: re-running should detect existing install and offer to repair/upgrade/reconfigure
- Uninstall mode: `monopsony-install --uninstall` drops user, stops service, removes files (asks about DB)
- Architecture decision needed: whether to bundle the script in-repo (`dist/install.sh`) or generate it from a template during release

## Step 12: Distribution Mirror Support — Design

### Goal
Allow the Monopsony server to act as a local mirror for Linux distribution repositories (ISOs, package repos, container images). Clients can point their distro installers/updaters at the Monopsony server instead of public mirrors.

### Modes
- **Private mirror**: Server syncs from upstream on schedule, serves to LAN clients. Auth required (existing mTLS).
- **Public mirror**: Server acts as a public-facing mirror with optional rate limiting. No auth required for mirror content.

### Sync Mechanism
- Use existing `tokio-cron-scheduler` (already a dependency) for scheduled sync jobs
- Per-distro sync config in TOML: upstream URL, sync frequency, bandwidth limit, include/exclude patterns
- Support rsync-over-HTTP for distros that expose rsync (Debian, Ubuntu, Alpine)
- Support plain HTTP mirroring for others (Arch, Fedora)
- Initial sync can be seeded from a local ISO/directory

### Storage
- Separate `mirror_dir` from `cache_dir` — mirrors are long-lived, caches are evictable
- Mirror metadata in DB: `mirror_configs` table (distro, upstream_url, last_sync, sync_status, total_bytes)
- Checksum verification: SHA256 for ISOs, per-package checksums for repo metadata

### Server Changes
- **DONE** New `crates/monopsony-server/src/mirror.rs` module — MirrorConfig, MirrorManager, MirrorState, HTTP handlers, sync logic
- **DONE** New gRPC RPCs: `ListMirrors`, `TriggerSync`, `MirrorStatus` (HTTP API handlers ready, gRPC proto pending)
- New HTTP routes under `/mirror/` prefix (e.g., `/mirror/archlinux/`, `/mirror/ubuntu/`) — **router created, needs wiring into main proxy**
- Mirror sync runs as background tasks, report progress via DB — **background task wired in main.rs**

### Client Changes
- **DONE** New `Mirror` subcommand: rewrites `/etc/pacman.d/mirrorlist`, `/etc/apt/sources.list.d/`, `/etc/yum.repos.d/` to point at Monopsony server
- Leverages existing `ProxyConfigAction` infrastructure (WriteFile variant)

### Distro Support (initial)
1. Arch Linux (`archlinux.org`) — ISO + repo packages
2. Debian/Ubuntu — ISO + `dists/` + `pool/`
3. Alpine Linux — ISO + `apks/`
4. Fedora — ISO + `Everything/`
5. NixOS — ISO + channel tarballs

### Security Considerations
- Mirror content must be verified against upstream checksums before serving
- Private mirrors: existing mTLS auth applies
- Public mirrors: need new auth bypass for mirror routes (configurable)
- No user-supplied content — only synced from trusted upstreams
