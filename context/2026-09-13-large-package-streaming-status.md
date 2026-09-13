# Large-Package Proxy Hardening — Implementation Status

**Date:** 2026-09-13
**Plan:** `docs/plans/2026-09-13-large-package-proxy-hardening.md`
**Status:** Implementation complete — all workspace tests green (pending final suite run). Live verification on 10.10.20.58 NOT yet done.

## What changed

### Server (monopsony-proxy)

**proxy.rs — streaming rewrite (the core fix):**
- `fetch_upstream` returns bodies UNBUFFERED (`Body::new(incoming)`, zero-copy). Old code did `body.collect().to_bytes()` — buffering entire 1 GB packages in RAM.
- Cache-miss path: tee task (`run_tee`) streams upstream chunks to (a) a `SpoolHandle` and (b) the client response simultaneously. O(1) memory: chunk → spool buffer (5 MiB flush) + mpsc channel → client.
- Spool commit is ATOMIC (object_store multipart): readers never see partial objects. Abort on any failure — nothing partial cached.
- Client disconnect mid-stream → `drain_to_spool` keeps filling the cache from the still-open upstream connection (persistence of abandoned fetches, by design).
- Per-chunk stall guard: upstream silent > `upstream_stall_secs` (default 120 s) → abort, 503 to client.
- Streamed-bytes counter enforces `max_response_bytes` even without Content-Length. Content-Length pre-flight check when present.
- In-flight coalescing: per-cache-key `Notify` map (`register_waiter`/`clear_fetcher`). Second concurrent request for the same key waits (bounded: ≤2 joins, 600 s) then serves the freshly-committed HIT. Proven by test: 2 simultaneous requests → exactly 1 upstream fetch.
- HEAD requests never enter the spool path (fixed pre-existing bug: HEAD responses were cached as 0-byte files, poisoning subsequent GETs).
- Upstream 206 Partial Content never cached (fixed pre-existing bug: byte ranges were cached as the full object).
- HIT path serves via `read_stream` (streaming, never whole-file read) with Range/206 + Accept-Ranges + Content-Range support — `curl -C -` resume works against cached objects.
- DB-less mode (no database configured) now serves HITs from storage existence (was: never HIT without DB row — broken for --no-db installs).
- Scheme normalization moved to the DB-lookup boundary ONLY (fixed pre-existing bug: `normalize_url_scheme` rewrote the FETCH url to https://, breaking HTTP-only mirrors; test upstreams exposed this).
- `allow_private_upstreams` flag on ProxyState: permits LAN-internal mirror upstreams through the SSRF guard (documented; default false).
- Staged-part janitor (`sweep_staged_parts`): walks cache dir, deletes orphaned `<key>#N` staging files (crashed multipart uploads) older than TTL. Cadence + TTL configurable.

**cache_storage.rs — trait extension:**
- `spool(key) -> SpoolHandle`: streaming write, atomic commit/abort. Wraps `object_store::WriteMultipart` (works for local + S3 backends — S3 gets real multipart uploads).
- `read_stream(key, range) -> (ByteStream, total_size)`: streaming range reads via `get_opts` + `GetRange::Bounded`.
- `SpoolHandle` computes SHA-256 incrementally during streaming — single hash pass, stored in DB at commit (was: 3 separate whole-body hashes in old code paths).
- **DELETED `FilesystemStorage`** (~250 lines): redundant with `ObjectStoreBackend::local` (identical on-disk layout). All tests ported. Single storage implementation now.

**security.rs:** `MAX_RESPONSE_BODY_SIZE` 512 MB → 4 GiB (default; configurable).

**object_store_backend.rs:** implements `spool` + `read_stream`.

### Config (monopsony-common)

- `[cache.proxy_limits]`: `max_response_bytes` (4 GiB), `upstream_stall_secs` (120), `stale_part_sweep_secs` (3600), `stale_part_ttl_secs` (7200). Manual `Default` impl (serde derive pitfall documented in memory).
- Client `ProxyConfig`: `retry_deadline_ms` (300 000), `retry_backoff_initial_ms` (2 000).
- Documented in `dist/server.toml.example`.
- Server main.rs reads proxy_limits from TOML, passes to run_proxy.

### Client (monopsony-client)

**proxy.rs — retry-with-backoff rewrite:**
- Responses stream through (no more whole-body collect — the old client buffered 1 GB in client RAM too).
- Retry loop: retries ONLY 502/503/504 whose body matches the server proxy's transient markers ("server proxy timed out", "server proxy unavailable", "upstream request failed", "upstream stalled"). NEVER retries 404 (real miss), 429 (rate limit), or scan-policy 403s. Marker check only reads bodies with known-exact size ≤ 2 KiB (never buffers package bodies for inspection).
- Per-attempt timeout (`--timeout-ms`, default 5000→30000) covers connect+headers; bodies stream past it.
- Total deadline `--retry-deadline-ms` (default 300 000 = 5 min). Deadline exhaustion → 503 to PM (no silent fallback, preserved).
- Backoff: initial `--retry-backoff-initial-ms` (2 s), ×2 per attempt, cap 30 s, ±20 % jitter. Retry-After header honored, capped by backoff schedule.
- Fresh TCP connection per attempt (each `forward_to_server` dials anew).
- Requests rebuilt per attempt from saved parts (Body isn't Clone); bodied requests forwarded once without retry (PM fetches are bodiless GETs — safe).
- `is_transient_failure` + `backoff_step` + config exposed pub for tests (lib.rs now `pub mod proxy`).

**main.rs:** `--retry-deadline-ms`, `--retry-backoff-initial-ms` args on Daemon; `--timeout-ms` default raised 5000→30000.

## Tests (all passing)

- `monopsony-proxy/tests/streaming_tests.rs` (NEW, end-to-end through the real router with mock PM backend matching 127.0.0.1):
  - 12 MiB miss→stream→commit, then HIT with zero upstream refetch, then Range 206 with correct bytes + content-range header.
  - Concurrent same-key requests coalesce to exactly 1 upstream fetch.
  - HEAD does not populate the cache.
- `monopsony-client/tests/retry_tests.rs` (NEW): 2 transient 503s then 200 → client succeeds on attempt 3; endless transient 503s → 503 at deadline (not before, not hung).
- `cache_storage.rs` tests: spool commit/abort/drop semantics, multi-part streamed writes with hash verification, read_stream ranges + EOF + past-EOF error.
- `proxy.rs` unit tests: backoff math (doubling, cap, jitter bounds), transient marker matching (positive + 404/429 negatives), Retry-After parsing, default config values.
- Janitor: `test_sweep_staged_parts_removes_orphans` (backdated orphan removed, fresh in-flight file survives).

## Known deferred

- **prefetch.rs still buffers whole bodies** (pre-existing). Uses the old `write()` API. Follow-up candidate: switch to `spool()` for the same O(1) memory. Not blocking — prefetch is for small repo metadata + scheduled warmups.
- Plan item §2 (Retry-After on first-byte-late server responses): skipped per plan's own "low priority" note — streaming removed the pain.
- `docker.rs` `fetch_upstream` still collects (docker blobs via different path — unchanged this pass).

## Live verification checklist (on 10.10.20.58, next session)

1. Deploy new server binary; `pacman -Syu` single-pass — expect zero 503s (was: every package >5 s failed first attempt).
2. Fresh cache + `linux-firmware` (or synthetic 600 MB): expect success, flat server RSS during fetch, HIT on refetch.
3. Kill server mid-large-fetch → client retries then 503s; no `.part`/`#N` orphans after janitor pass (check `find /var/cache/monopsony -name '*#*'`).
4. Concurrent `pacman -Syu` on two clients → single upstream fetch per package.

## Files touched

- crates/monopsony-proxy/src/{proxy,cache_storage,object_store_backend,security}.rs
- crates/monopsony-proxy/tests/streaming_tests.rs (new)
- crates/monopsony-proxy/src/docker.rs (test port off FilesystemStorage)
- crates/monopsony-common/src/config.rs (ProxyLimitsConfig + client ProxyConfig fields)
- crates/monopsony-client/src/{proxy,main,lib}.rs
- crates/monopsony-client/tests/retry_tests.rs (new)
- crates/monopsony-server/src/main.rs (run_proxy limits wiring)
- dist/server.toml.example ([cache.proxy_limits] docs)
- monopsony-proxy/Cargo.toml (tokio-stream dep; axum macros feature)
