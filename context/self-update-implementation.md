# Monopsony Self-Update Implementation - Phase 2 Complete

## Summary

The self-update mechanism allows both server and client binaries to check for
and apply updates from GitHub releases. Both `self-update check` and
`self-update apply` are fully wired and functional.

## What Was Built

### 1. Core Self-Update Module (`crates/monopsony-client/src/self_update.rs`)
- `check_for_update()` — Queries GitHub API for latest release, compares versions
- `apply_update()` — Downloads asset, extracts binary from archive, backs up, atomic swap
- `select_asset_for_platform()` — Platform-aware asset selection (os/arch/libc)
- `extract_binary_from_archive()` — Extracts binary from .tar.gz or .zip
- `detect_platform()` — Runtime platform detection (linux/windows/freebsd + arch + libc)
- Version parsing with semver comparison (handles pre-release suffixes like `-rc1`)
- 404 handling: GitHub returns 404 when no releases exist — treated as "up to date"

### 2. Server Self-Update (`crates/monopsony-server/src/self_update.rs`)
- Reuses client crate's `check_for_update` and `apply_update` via library dep
- `SelfUpdateMode` enum added to server's CLI (Check, Apply)
- Wired into `main()` command dispatch

### 3. CLI Integration
- **Client**: `monopsony-client self-update check` / `self-update apply`
- **Server**: `monopsony-server self-update check` / `self-update apply`
- Auto-detects binary type from argv[0] (client-side)
- Exit codes: 0=up to date, 1=update available, 2=check failed, 3=no asset for platform, 4=apply failed

### 4. Library Refactor
- `monopsony-client` now has both `src/lib.rs` and `src/main.rs`
- `lib.rs` exposes `self_update` module for cross-crate reuse
- `monopsony-server` depends on `monopsony-client` crate

## Usage

```bash
# Check for updates (client)
monopsony-client self-update check

# Check for updates (server)
monopsony-server self-update check

# Apply update (downloads, backs up, atomic swap)
monopsony-client self-update apply
monopsony-server self-update apply
```

## Asset Naming Convention

Expected release asset pattern: `monopsony-{arch}-{target}.{ext}`

| Platform | Asset Name |
|----------|-----------|
| Linux glibc x86_64 | `monopsony-x86_64-unknown-linux-gnu.tar.gz` |
| Linux musl x86_64 | `monopsony-x86_64-unknown-linux-musl.tar.gz` |
| Linux glibc aarch64 | `monopsony-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `monopsony-x86_64-pc-windows-gnu.zip` |
| FreeBSD x86_64 | `monopsony-x86_64-unknown-freebsd.tar.gz` |

## Status

- [x] Platform-aware asset selection
- [x] Archive extraction (tar.gz + zip)
- [x] Atomic binary swap with backup
- [x] 404 handling (no releases yet)
- [x] Server + client CLI wired
- [x] All tests passing

## Remaining (Future Phase)

- Health check before/after update (verify service responds)
- Automatic rollback on health check failure
- Auto-update scheduler integration (cron-based, using existing auto_update module)
- Dashboard UI integration ("update available" banner)
