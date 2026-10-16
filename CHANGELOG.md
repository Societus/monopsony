# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0-rc1] — 2026-05-19

### Added

#### Core System (Sprint 5)
- Auth modes: open, token-based, and mTLS (always required on gRPC)
- Built-in private TLS CA with automatic client certificate generation
- mTLS wired into gRPC (port 50051) with SHA-256 fingerprinting
- Config validation for all TOML-based configuration
- Prometheus metrics skeleton (defined, not yet wired to exporters)
- 6 package manager backends: apt, dnf, pacman, winget, choco, scoop
- Interactive bash installer (`dist/install.sh`) with OS detection
- systemd and OpenRC service units for both server and client

#### Distribution Mirrors (Sprint 12)
- Mirror HTTP router (:3129) serving synced distro repositories
- Per-distro sync configuration (upstream URL, frequency, bandwidth limit)
- Mirror sync as background task with progress tracking via DB
- gRPC MirrorControl service: ListMirrors, TriggerMirrorSync, GetMirrorStatus, UpsertMirrorConfig, DeleteMirrorConfig
- Client `Mirror` subcommand: rewrites distro mirror lists to point at Monopsony
- Signed mirror manifests (Ed25519)
- Per-IP rate limiting (token bucket)
- Prometheus metrics wired into proxy, gRPC, prefetch, and mirror handlers

#### Tracker Service (Sprint 11)
- Standalone tracker crate (`monopsony-tracker`) with SQLite backend
- `POST /announce` and `GET /discover` endpoints
- Haversine geo-proximity scoring with freshness, reliability, and capacity factors
- Auth middleware: open (default), token, JWT modes
- Nostr-style operator identity (Ed25519 npub)
- Operator reputation system with anti-malice dispute mechanism
- Client report protocol (`POST /report`): hash mismatch, slow response, stale mirror, malicious content
- Auto-blacklisting: reputation threshold and report-count triggers with 7-day TTL
- Manual blacklisting (permanent) via CLI
- V4V conditional schema (gated by `--v4v-enabled` flag)
- Server metrics push (`POST /v1/metrics/push`) and public aggregate endpoint (`GET /v1/metrics/public`)
- Graceful shutdown with cooperative cancellation
- STUN-based NAT detection for mirrors

#### Shadow Packages Phase 1 (Sprint 7)
- GitHub release source with asset selection by platform/arch
- Ed25519 recipe signing and verification (ed25519-dalek 2.2)
- TOML recipe parsing, validation, and template generation
- Package manager conflict detection (apt/dnf/pacman/apk) with masking
- Content-addressed blob store with platform index and disk-budget GC
- Atomic extract-and-swap install with pre/post hooks
- CLI surface: Register, Unregister, List, Inspect, Check, Update, Rollback, Recipe

#### Shadow Packages Phase 2 (Sprint 8)
- Git build source: bare clone, fetch, tags, worktree, GC
- Script install method (build-from-source)
- Direct URL source (arbitrary HTTPS assets)
- Pre/post update hooks with failure rollback
- Health check system (HTTP, TCP, command, systemd)

#### Shadow Packages Phase 3 — Federation + V4V (Sprint 9)
- Tracker recipe storage with quarantine/reinstate admin routes
- Recipe publishing from server to tracker
- Dual signature verification (author + operator)
- Update annotation protocol (info/warning/breaking severity)
- Voting system weighted by install count
- Malicious report flow (3-report quarantine threshold)
- Namespace collision handling with interactive variant selection
- Client trust enforcement: trusted-server-only, trusted-authors, paranoid policies

#### Shadow Packages Phase 4 (Sprint 10)
- Rollback chains: keep last N backups (configurable, default 3), rollback to any previous version
- Scheduled auto-update windows: cron-based scheduling (7-field format), timezone support, severity filtering
- Multi-binary packages: `BinaryEntry` mapping from archive paths to install targets with chmod
- Custom version detection: command (run + regex), file (read + regex), HTTP header methods
- Build sandboxing research design doc (bubblewrap, cgroups v2, Landlock, seccomp, Podman evaluation)

### Changed
- Version bumped from 0.1.0-rc1 to 1.0.0-rc1
- Repository URL updated to Forgejo (base.societus.club)

### Test Coverage
- 238 tests passing across 10 test suites
- 0 compile errors, warnings only (dead_code in unwired server shadow modules)

### Known Limitations
- Server shadow modules compile but are not yet wired to gRPC handlers
- V4V schema exists but logic is not built (post-v1.0)
- Metrics crate types defined but never called from production code
- Windows backends (winget, choco, scoop) untested on actual Windows — **actively seeking a Windows contributor** for end-to-end implementation and verification
- macOS builds as a BSD/Darwin app, not a native Mac application; **seeking a Swift/Shell contributor** to move toward a proper macOS app

---

## [0.1.0-rc1] — 2026-04-14

### Added
- Workspace skeleton with 4 crates: common, server, client, tracker
- Server HTTP caching proxy with file-based cache and JSON sidecar metadata
- PostgreSQL integration (sqlx 0.8) with migrations and CRUD
- mTLS with built-in private CA (rustls + rcgen)
- Client local HTTP proxy with timeout and upstream fallback
- gRPC basic: register, heartbeat, inventory reporting, bidirectional ControlStream
- Pairing mechanism: 6-char code exchange on plaintext TCP port 50052
- Client daemon mode with exponential backoff reconnect
- mDNS discovery (mdns-sd)
- Interactive bash installer with OS detection and PostgreSQL setup
- systemd and OpenRC service units

[1.0.0-rc1]: https://base.societus.club/societus/monopsony/releases/tag/v1.0.0-rc1
[0.1.0-rc1]: https://base.societus.club/societus/monopsony/releases/tag/v0.1.0-rc1
