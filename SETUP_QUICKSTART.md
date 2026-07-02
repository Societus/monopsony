# Quick Commands for New Machine Setup
**Repo:** base.societus.club/societus/monopsony.git  
**Local:** ~/projects/monopsony/

---

## 1. Clone & Setup
```bash
cd ~/projects
git clone https://github.com/Societus/monopsony.git
cd monopsony

# Install protoc if needed (Alpine)
# pacman -S protobuf
# Or use uv tool:
uv tool install grpcio-tools
export PROTOC=~/.local/bin/python-grpc-tools-protoc

# Verify environment
cargo check --workspace
cargo test --workspace          # Should be 363 tests passing
cargo clippy --workspace       # Should be 0 errors, 1 warning
```

## 2. Verify Uncommitted Work
```bash
# Check file changes
git diff HEAD --stat
git diff HEAD -- crates/monopsony-control/src/grpc.rs | head -20

# Run full test suite
cargo test --workspace

# Build and test shadow integration
cargo test -p monopsony-shadow
cargo test -p monopsony-control
```

## 3. Commit Sprint 7c
```bash
# Add shadow RPC work
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
- Add accessor methods to ShadowPackageManager"

# Add container deployment
git add Dockerfile docker-compose.yml .dockerignore \
        dist/container/ docs/container-deploy.md
git commit -m "feat: Add container deployment (Docker/Podman)
- Multi-stage Alpine + Rust build
- Runtime detection (Docker vs Podman)
- Auto-config generation from example + env overrides
- Rootless Podman support with keep-id namespace
- OCI-compliant layering ready for registry push"
```

## 4. Verify Everything Works
```bash
# Full build and test
cargo build --workspace
cargo test --workspace --release

# Container build test
docker build -t monopsony-test .

# Run container (quick test)
export POSTGRES_PASSWORD=***
docker compose up -d
sleep 10
curl http://localhost:3128/
docker compose down
```

## 5. Next Steps (After Commit)
- **Sprint 8d:** Wire GitMirrorManager config (2 small tasks)
- **Config wiring:** Hook [scan] and [node] sections to main.rs
- **Release:** `git tag -s v1.0.0` (requires Forgejo secrets)

---

## Critical Files to Review
1. `crates/monopsony-control/src/grpc.rs` - Shadow RPC implementations
2. `crates/monopsony-server/src/main.rs` - Shadow manager init
3. `dist/container/entrypoint.sh` - Container runtime detection
4. `docker-compose.yml` - Service configuration

---

## Common Issues & Fixes
- **protoc error:** Set PROTOC env var or install system protobuf
- **Docker build fails:** Ensure all source files are committed
- **Container permission:** Use `userns: keep-id` in compose for rootless Podman
- **Test failures:** Run `cargo test --workspace -- --exact shadow` for shadow-specific tests