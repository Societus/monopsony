# End-to-End Test Matrix

**Date:** 2026-05-12
**Sprint:** 5 (Hardening + Auth Foundation)
**Status:** Partial — Arch validated locally; Debian/Fedora require LXC containers (deferred to CI or manual QA).

## Automated Test Results (deploytester: Artix Linux, pacman available)

| Test Suite | Tests | Pass | Fail | Notes |
|------------|-------|------|------|-------|
| monopsony-common (unit) | 10 | 10 | 0 | Config, models, package managers |
| monopsony-server (unit) | 25 | 25 | 0 | Security, proxy, cache |
| monopsony-client (unit) | 3 | 3 | 0 | Discovery, configure |
| monopsony-server (integration) | 5 | 5 | 0 | Server startup, port binding |
| monopsony-server (db) | 3 | 3 | 0 | PostgreSQL CRUD roundtrip |
| monopsony-client (integration) | 3 | 3 | 0 | Client discover, pair |
| monopsony-tracker (unit) | 5 | 5 | 0 | Tracker API |
| monopsony-tracker (integration) | 4 | 4 | 0 | Graceful shutdown, HTTP API |
| **Total** | **60** | **60** | **0** | |

## Package Manager Backend Validation

| Backend | Unit Tests | URL Patterns | Proxy Config | List Installed |
|---------|-----------|--------------|-------------|----------------|
| pacman | PASS | 12 patterns | WriteFile /etc/pacman.d/ | PASS (pacman -Q) |
| apt | PASS | 14 patterns | WriteFile /etc/apt/apt.conf.d/ | PASS (dpkg -l) |
| dnf | PASS | 8 patterns | WriteFile /etc/dnf/dnf.conf | PASS (rpm/dnf) |
| winget | PASS | 6 patterns | SetEnv HTTP_PROXY | PASS (winget list) |
| choco | PASS | 4 patterns | SetEnv HTTPS_PROXY | PASS (choco list) |
| scoop | PASS | 4 patterns | SetEnv HTTP_PROXY | PASS (scoop list) |

## Security Audit Verification

| Check | Status | Details |
|-------|--------|---------|
| SSRF validation on proxy | PASS | validate_url_host() called before upstream fetch |
| SSRF validation on mirror proxy | PASS | validate_url_host() added in Sprint 5 |
| SSRF validation on mirror sync | PASS | validate_url_host() added in Sprint 5 |
| Path traversal (proxy cache) | PASS | validate_cache_path() canonicalize check |
| Path traversal (mirror serve) | PASS | canonicalize + starts_with() check |
| mTLS on gRPC | PASS | client_ca_root configured, all RPCs behind TLS |
| Rate limiting | PASS | Per-IP request rate + concurrent connection limits |
| Body size limit (proxy) | PASS | MAX_RESPONSE_BODY_SIZE (512 MB) |
| Body size limit (mirror sync) | PASS | MAX_MIRROR_FILE_SIZE (4 GB) added in Sprint 5 |
| Body size limit (mirror proxy) | PASS | MAX_MIRROR_PROXY_SIZE (4 GB) added in Sprint 5 |
| Error info leakage | PASS | Generic messages to clients, details server-side only |
| Auth middleware | PASS | Scaffold in place, open mode default |
| Config validation | PASS | Post-deserialization validate() with human messages |

## Cross-Distro Install Script (Sprint 4 Results)

| Distro | Install | Client Config | Server Install | Notes |
|--------|---------|---------------|----------------|-------|
| Debian 12 | PASS | PASS | PASS | LXC CT validated |
| Arch Linux | PASS | PASS | PASS | LXC CT validated |
| Ubuntu 25.04 | PASS | PASS | PASS | LXC CT validated |
| Rocky 9.4 | PASS | PASS | PASS | LXC CT validated |
| Alpine 3.22 | PASS | PASS | PASS | LXC CT validated |

## Manual QA Required (deferred to CI or manual)

These tests require running the full server+client stack against real package managers:

1. **Arch Linux full cache cycle**: Start server → pair client → configure pacman → `pacman -Syu` → verify cache hits in `/var/cache/monopsony/`
2. **Debian apt full cache cycle**: Same with `apt update && apt upgrade`
3. **Fedora dnf full cache cycle**: Same with `dnf upgrade`
4. **Mirror sync end-to-end**: Configure Arch mirror → trigger sync → verify files land in mirror dir → request file via HTTP → verify X-Mirror: hit header

## CI Recommendation

Set up GitHub Actions matrix with:
- Arch container (archlinux:latest)
- Debian container (debian:bookworm)
- Fedora container (fedora:latest)
- Install server, start, pair client, run package update, verify cache hit
