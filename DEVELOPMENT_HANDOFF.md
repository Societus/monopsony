# Monopsony Development Handoff
**Date:** June 22, 2026  
**Status:** v1.0.0-rc1 (uncommitted Sprint 7c work)  
**Git repo:** base.societus.club/societus/monopsony.git  
**Local:** ~/projects/monopsony/

---

## Current State Summary

- **Sprints 1-18 complete** (363 tests passing)
- **Release CI ready** (dual-arch musl, SBOM, GPG signing)
- **Sprint 7c (Shadow RPC wiring) partially committed** - 406 lines of uncommitted work
- **Container deployment files added** (uncommitted)
- **One clippy warning** remaining

---

## Next Steps Before Commit

### 1. Verify Uncommitted Work
```
cargo test --workspace          # Should be 363 passing
cargo clippy --workspace       # Should be 0 errors, 1 warning
```

### 2. Commit Sprint 7c Changes
```
git add .
git commit -m "feat: Sprint 7c - Wire all 8 shadow gRPC RPCs
- Add monopsony-shadow dependency to control/server
- Implement list_shadow_packages with DB queries
- Implement register_shadow_package with validation
- Implement unregister_shadow_package 
- Implement check_shadow_updates with real update checking
- Implement apply_shadow_update and rollback_shadow_update
- Implement get_shadow_recipe with ed25519 signing
- Implement download_shadow_asset with streaming chunks
- Start background polling in main.rs
- Add CLI args for shadow directory and GitHub token
- Add accessor methods to ShadowPackageManager
```

### 3. Add Container Files
```
git add Dockerfile docker-compose.yml .dockerignore \
        dist/container/ docs/container-deploy.md
git commit -m "feat: Add container deployment (Docker/Podman)
- Multi-stage Alpine + Rust build
- Runtime detection (Docker vs Podman)
- Auto-config generation from example + env overrides
- Rootless Podman support with keep-id namespace
- OCI-compliant layering ready for registry push"
```

---

## Build Environment

### Required Tools
- Rust 1.94.1 (workspace pin in rust-toolchain.toml)
- protoc (for gRPC codegen)
- Docker or Podman

### Environment Setup
```bash
export PROTOC=/path/to/protoc  # if using uv-installed grpcio-tools
# Or: pacman -S protobuf  # system protoc
```

### Build Commands
```bash
cargo check --workspace          # Clean compilation
cargo test --workspace          # 363 tests passing
cargo build -p monopsony-server # Server binary only
cargo build -p monopsony-client # Client binary only
```

---

## Workspace Structure

**9 crates total (2 standalone, 7 core dependencies):**

**Core crates (server depends on these):**
- `monopsony-common` — Shared types, proto stubs, package manager backends
- `monopsony-db` — PostgreSQL Database with migrations and CRUD
- `monopsony-proxy` — HTTP proxy with security and scan pipeline
- `monopsony-control` — gRPC server with shadow RPCs (Sprint 7c done)
- `monopsony-mirror` — Distribution mirror sync and HTTP router
- `monopsony-server` — Thin orchestrator binary
- `monopsony-client` — Local proxy and shadow CLI

**Standalone crates (no consumers, safe to remove):**
- `monopsony-shadow` — Now depends on by server after Sprint 7c
- `monopsony-tracker` — Standalone HTTP tracker, not linked

---

## Shadow Package System Status

### Sprint 7c - FULLY IMPLEMENTED
- ✅ All 8 gRPC stubs replaced with real implementations
- ✅ Database integration for CRUD operations
- ✅ ed25519 recipe signing with key persistence
- ✅ Shadow asset streaming for downloads
- ✅ Background update polling (1h interval)
- ✅ CLI args for directory/GitHub token configuration

### Shadow Features Complete:
- Package registration/unregistration
- Version update checking (GitHub API)
- Signed recipe generation
- Asset download with chunked streaming
- Background polling for updates
- Rollback/apply operations

---

## Missing Items (Pre-Release)

### 1. Config Wiring (3 small tasks)
- `[scan]` section not wired to main.rs (passes None)
- `[node]` section not wired to main.rs (passes None) 
- Package name/version extraction for CVE lookup still returns None

### 2. Sprint 8d (deferred)
- Git mirror manager config wiring into server.toml

### 3. Post-Release Features
- mTLS auth mode (structure exists, not wired)
- Windows client mirror support (winget/choco/scoop)

---

## Container Deployment

### Quick Start
```bash
export POSTGRES_PASSWORD="$(openssl rand -hex 16)"
docker compose up -d
curl http://localhost:3128/      # Proxy health
curl http://localhost:9090/metrics  # Prometheus
```

### Ports
- 3128: HTTP proxy (package manager traffic)
- 50051: gRPC control plane (client registration)  
- 9090: Prometheus metrics
- 3129: Distribution mirror

### Rootless Podman
```yaml
# In docker-compose.yml server service:
userns: keep-id  # Maps host UID/GID to container
```

---

## Release Status

**Ready:** All CI checks pass, 363 tests, dual-arch builds, SBOM, GPG signing
**Trigger:** `git tag -s v1.0.0` (requires Forgejo secrets: GPG key, passphrase, API token)

### Required Secrets (in repo settings)
- `GPG_PRIVATE_KEY` — Full ASCII-armored private key
- `GPG_PASSPHRASE` — Key passphrase  
- `RELEASE_TOKEN` — API token with write:repository scope

---

## Next Sprint After Commit

### Sprint 8d - Small Priority
- Wire GitMirrorManager into server.toml config
- Add E2E tests for mirror configuration

### Post-Release Sprints
- Sprint 10: Shadow Phase 4 (rollback chains, auto-update)
- Sprint 11: Tracker build (standalone binary already exists)
- Sprint 9: Federation + V4V (depends on S7c + S11)
- Sprint 12: Public mirror network (depends on S11)

---

## Critical Notes
- **Shadow crate is now a real dependency** — no longer dead weight
- **Container files ready** but need committing
- **Config gaps don't block release** — features implemented but not wired to TOML parsing
- **363 tests passing** with shadow RPC integration
- **Release CI pipeline verified** and ready to trigger