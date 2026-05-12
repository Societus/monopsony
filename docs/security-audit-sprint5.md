# Sprint 5 Security Audit Report

**Date:** 2026-05-12
**Auditor:** Hermes (automated review)
**Scope:** All network-facing code in monopsony-server and monopsony-tracker

---

## Findings Summary

| # | Severity | File | Finding |
|---|----------|------|---------|
| 1 | HIGH | mirror.rs | No SSRF validation on upstream mirror fetches |
| 2 | HIGH | mirror.rs | No response body size limit on mirror sync |
| 3 | MEDIUM | auth.rs | API key comparison not constant-time |
| 4 | MEDIUM | mirror.rs | `proxy_to_upstream` exposes error details to clients |
| 5 | LOW | mirror.rs | `_content_length` is read but never enforced |
| 6 | INFO | grpc.rs | mTLS is enforced (good) |
| 7 | INFO | mirror.rs | Path traversal protection exists (good) |
| 8 | INFO | proxy.rs | SSRF validation exists for proxy fetches (good) |
| 9 | INFO | proxy.rs | Response body size limit enforced (good) |

---

## Detailed Findings

### Finding 1: No SSRF validation on mirror upstream fetches [HIGH]

**File:** `mirror.rs:671`, `mirror.rs:754`, `mirror.rs:833`

**Description:** The `sync_mirror()` function and `proxy_to_upstream()` function fetch from user-configured upstream URLs without calling `validate_url_host()`. An admin who configures a mirror with an upstream URL pointing to an internal service (e.g., `http://169.254.169.254/latest/meta-data/` on AWS, or `http://localhost:6379/` for Redis) can trigger SSRF. While the mirror config is admin-controlled (not arbitrary user input), defense-in-depth says we should still validate.

**Fix:** Add `validate_url_host()` call in `sync_mirror()` before the upstream request, and in `proxy_to_upstream()`.

**Status:** TODO

---

### Finding 2: No response body size limit on mirror sync [HIGH]

**File:** `mirror.rs:854`

**Description:** The mirror sync engine reads the entire response body into memory with no size check:
```rust
let bytes = body.collect().await.map_err(|e| ...)?.to_bytes();
```
A malicious or compromised upstream mirror could serve an infinitely large response, causing OOM. The proxy has `MAX_RESPONSE_BODY_SIZE` (512 MB) but the mirror sync does not.

**Fix:** Add body size limit check after reading bytes, similar to proxy.rs:484. A reasonable limit for mirror files is 4 GB (large ISOs exist), but the current unbounded approach is dangerous.

**Status:** TODO

---

### Finding 3: API key comparison not constant-time [MEDIUM]

**File:** `auth.rs`, `db.rs:585-590`

**Description:** The token auth flow hashes the incoming key with SHA-256, then looks up the hash in the database via `db.validate_api_key()`. The lookup is a SQL query (`WHERE key_hash = $2`), which is not constant-time. However, since the comparison happens on the *hash* (not the key itself), and the hash is not reversible, this is a lower severity than direct key comparison. An attacker who can time database queries might learn whether a hash prefix matches, but this is impractical over network latency.

**Fix (Sprint 6):** For full compliance, add `constant_time_eq` for the hash comparison in application code, not relying on the database query.

**Status:** Deferred to Sprint 6 (token auth full implementation)

---

### Finding 4: `proxy_to_upstream` exposes error details to clients [MEDIUM]

**File:** `mirror.rs:600-625`

**Description:** Error messages from upstream fetch failures are returned directly to the client:
```rust
Err(e) => {
    (StatusCode::BAD_GATEWAY, format!("Upstream fetch failed: {}", e)).into_response()
}
```
This leaks internal network details (e.g., DNS resolution errors, connection refused messages) to any client hitting the mirror endpoint.

**Fix:** Return generic error messages to clients, log detailed errors server-side.

**Status:** TODO

---

### Finding 5: `_content_length` is read but never enforced [LOW]

**File:** `mirror.rs:843`

**Description:** The content-length header is read into `_content_length` but the variable is unused (underscore prefix). It was intended for size validation but never implemented.

**Fix:** Either enforce the content-length limit or remove the dead code.

**Status:** TODO (part of Finding 2 fix)

---

## Positive Findings (Verified Good)

### mTLS enforced on gRPC [INFO]
`grpc.rs:360` configures TLS with `client_ca_root`, requiring client certificates. All gRPC endpoints are behind mTLS.

### Path traversal protection exists [INFO]
`mirror.rs:548-563` canonicalizes both mirror dir and requested file, then checks `starts_with()`. This prevents `../../etc/passwd` traversal.

### SSRF validation on proxy fetches [INFO]
`proxy.rs:208` calls `validate_url_host()` before fetching from upstream. The proxy is protected.

### Rate limiting enforced [INFO]
`proxy.rs:167-183` checks both request rate and concurrent connections per IP. Rate limiter is wired into the proxy router.

### Auth middleware bypasses health check [INFO]
`auth.rs:31-33` skips auth for `/health` endpoint, which is correct (health checks must work without auth).

---

## Remediation Plan

1. **Finding 1 (SSRF in mirror):** Add `validate_url_host()` to `sync_mirror()` and `proxy_to_upstream()` in mirror.rs
2. **Finding 2 (body size limit):** Add `MAX_MIRROR_FILE_SIZE` constant (4 GB) and enforce it in mirror sync
3. **Finding 4 (error leakage):** Replace detailed error messages with generic ones in `proxy_to_upstream()`
4. **Finding 5 (dead code):** Remove `_content_length` or use it for the size limit
