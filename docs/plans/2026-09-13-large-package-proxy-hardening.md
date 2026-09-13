# Large-Package Proxy Hardening — retrying client, streaming server

**Date:** 2026-09-13 (drafted after live caching-serve test on 2026-09-12)
**Status:** Plan — ready for layered implementation after pairing work lands
**Layer:** monopsony-client (proxy), monopsony-proxy (server), monopsony-common
**Priority:** P1 — blocks bare-metal serving of kernel/firmware images

## Proven failure (live, 2026-09-12)

`pacman -Syu` through the cache: every package whose upstream fetch took
longer than ~5 s returned 503 to the client (rust 82 MB / llvm-libs /
postgresql ~90 MB each failed on first attempt; succeeded only after a
second request warmed the cache). Code attribution after reading both
proxies:

- The 5 s cutoff is **client-side**: `monopsony-client daemon
  --timeout-ms` defaults to 5000 (`crates/monopsony-client/src/main.rs:79`)
  and wraps ONE attempt in `tokio::time::timeout`
  (`proxy.rs:111-133`), returning "Server proxy timed out" on expiry.
- The **server** (`monopsony-proxy`, port 3128) has no upstream timeout at
  all — it fetched rust from upstream for 28 s and cached it fine. Server
  logs show zero timeouts; the client gave up first.
- So today's failure is: server works on big files but slow; client
  abandons the request at 5 s. Each client retry then hits a cache that
  may or may not have been filled by the *abandoned* server-side fetch
  (the server keeps fetching after the client disconnects, which is why
  retries eventually succeed).

## Bare-metal sizing (the requirement that shapes this plan)

Containers today pull small packages. Bare-metal clients pulling `linux`,
`linux-firmware`, `llvm`, `rust` hit **0.5–1 GB+ single files**. At
upstream speeds of 2–20 MB/s that is **1–8+ minutes per file**. Therefore:

- Any fixed timeout on the *client wait* must be minutes, not seconds.
- The server must **stream**, not buffer: current code collects the whole
  body into RAM (`body.collect().to_bytes()` in `fetch_upstream`) and
  rejects anything over `MAX_RESPONSE_BODY_SIZE = 512 MB`
  (`crates/monopsony-proxy/src/security.rs:15`). linux-firmware fails
  both constraints today.
- pacman runs `ParallelDownloads = 5` by default: five concurrent
  streaming 1 GB fetches must not multiply memory (streaming fixes this;
  buffering five 1 GB bodies would OOM small boxes).

## Design

### 1. Server: stream upstream → client, tee to a temp file (monopsony-proxy)

Replace collect-then-cache with stream-and-tee:

1. `fetch_upstream` returns the response with its body **unbuffered**
   (drop the `body.collect().to_bytes()` call; `MAX_RESPONSE_BODY_SIZE`
   check moves from RAM-size to `Content-Length` header when present).
2. In the cache-miss path, spawn a writer task: each body chunk is
   appended to `<cache_dir>/tmp/<key>.part` while simultaneously flowing
   to the client. On successful completion (upstream EOF + client got
   everything), atomically rename `.part` → final cache path and write
   `CacheEntry` metadata as today. On error or client disconnect:
   delete `.part` (nothing cached, no metadata — same correctness as
   today's "only cache successful responses").
3. Concurrency guard: per-cache-key in-flight map so N clients requesting
   the same 1 GB file share one upstream fetch (they all tee from the
   same in-flight stream) instead of five parallel upstream downloads.
4. Keep the no-fallback 503 behavior unchanged.

Config: `[proxy] temp_dir`, `max_response_bytes` (raise default to e.g.
4 GiB, checked via Content-Length + streamed-bytes counter instead of
RAM), `stale_part_ttl` for janitoring orphaned `.part` files.

### 2. Server: Retry-After on expected-slow responses (small, optional)

When a cache miss begins a large fetch (Content-Length > some threshold,
say 100 MB), the server MAY emit `Retry-After: <estimated-seconds>` on
its *first-byte-late* responses. With streaming (item 1) the first byte
arrives quickly, so this mostly matters when upstream itself is slow to
first byte. Low priority — streaming removes most of the pain.

### 3. Client: retry-with-backoff inside the client proxy (monopsony-client)

The client proxy (:3129) retries **503-from-server-proxy only**, within a
total deadline, while holding the package manager's connection open:

- `ProxyConfig` gains `retry_deadline_ms` (default **300 000** = 5 min)
  and `retry_backoff_initial_ms` (default 2 000, ×2 per attempt, cap
  30 s, ±20 % jitter).
- Retry only when: response status is 503 AND body matches the server
  proxy's transient markers ("Server proxy timed out", "Server proxy
  unavailable"), or the forward itself errored/timed out. **Never retry
  404** (real upstream miss) or 429 (rate limit — respect it).
- Honor `Retry-After` header if present (caps the backoff step).
- Each attempt gets a fresh connection (`forward_to_server` already
  connects per attempt) and the per-attempt wait must exceed the server's
  own upstream budget once item 1 lands — with streaming the server only
  503s on *connect* failure or upstream error, so deadline exhaustion
  means the server is genuinely down: return 503 to the PM and let the
  transaction fail as today.
- CLI: `--retry-deadline-ms` alongside existing `--timeout-ms`
  (`--timeout-ms` becomes the per-attempt first-byte window; default
  raised 5 000 → 30 000 so the *first* attempt alone survives multi-second
  server cache hits).

pacman interplay: `ParallelDownloads = 5` × 5-min deadlines is fine —
the deadline is per request, and streaming (item 1) means server memory
doesn't scale with parallelism. `curl -C -` in the stock XferCommand
resumes; with `--fail` (in use today) a final 503 still fails the
transaction, preserving no-silent-fallback.

apt/dnf/apk: same benefit automatically since they all point at :3129;
document apt's `Acquire::http::Timeout` (120 s default) as the upper
bound the retry deadline must stay under for apt hosts — 5 min default
is for pacman-first; apt hosts can set `--retry-deadline-ms 90000`.

### 4. Regression tests

- Server: 600 MB synthetic upstream — memory stays flat (streamed), file
  lands in cache, metadata written, second request is a HIT.
- Server: client disconnects mid-stream → `.part` removed, no cache entry.
- Server: two concurrent requests for same uncached key → one upstream
  fetch (count upstream hits in a mock), both clients get full bodies.
- Client: mock server returns 503-timeout twice then 200 → client proxy
  returns 200, two retries observed in log, total latency ≈ backoff sum.
- Client: mock returns 404 → no retry, single attempt, 404 passed through.
- Client: 503 forever → 503 to PM after `retry_deadline_ms`, not before.

## File-by-file (layered on current working tree)

| File | Change |
|---|---|
| `crates/monopsony-proxy/src/cache_storage.rs` | **Trait change (load-bearing):** add streaming variants `write_spool(&self, key) -> SpoolHandle` (AsyncWrite + commit/abort) and `read_stream(&self, key) -> impl AsyncRead`. The current `write(&[u8])` / `read() -> Vec<u8>` whole-blob API is what forces RAM buffering — without these, `proxy.rs` cannot be made O(1)-memory. Local backend: file Appends/pread. `ObjectStoreBackend`: multipart upload (S3) / incremental file (Local) — same trait, farm-ready. |
| `crates/monopsony-proxy/src/proxy.rs` | stream+tee cache-miss path (fixed 256 KiB–1 MiB chunk buffer), per-key in-flight map (late joiners attach to the spooling stream), Content-Length size check, Range/pread serving for cached files (curl `-C -` resume support) |
| `crates/monopsony-proxy/src/security.rs` | `MAX_RESPONSE_BODY_SIZE` → 4 GiB (or config) |
| `crates/monopsony-proxy/src/purge.rs` (or new `janitor.rs`) | orphaned `.part` cleanup task |
| `crates/monopsony-common/src/config.rs` (server) | `[proxy]` section: `temp_dir`, `max_response_bytes`, `stale_part_ttl` — manual `Default` impl per Pitfall: derive(Default) bypass |
| `crates/monopsony-client/src/proxy.rs` | retry loop w/ backoff+jitter, 503-marker match, Retry-After |
| `crates/monopsony-client/src/main.rs` | `--retry-deadline-ms` arg, `--timeout-ms` default 5000→30000 |

## Verification (live, post-implementation)

1. Container-style: repeat the 2026-09-12 `pacman -Syu` — expect zero
   503s in `/tmp/syu*.txt`, single-pass transaction.
2. Bare-metal-style: craft a >600 MB file on a throttled upstream (or
   `linux-firmware` when mirrors update next), fresh cache, single client
   fetch — expect streaming success, flat server RSS, cache HIT on refetch.
3. Kill server mid-large-fetch → client returns 503 after retries, no
   `.part` orphans, transaction fails loudly (no fallback).

## Ordering

Server streaming (1) first — it's the bare-metal blocker on its own
(512 MB cap + RAM buffering reject 1 GB files regardless of timeouts).
Client retry (3) second — it converts remaining transient 503s into
patience. Both land layered after the pairing work, same as the recipe.
