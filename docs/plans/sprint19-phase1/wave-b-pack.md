# Wave B pack — three independent backlog items

Read `wave-ab-context-pack.md` first. Your ONE item is named in your dispatch
brief. Files are exclusive per item — no overlap.

## B1 — prefetch.rs → spool() migration
File: crates/monopsony-proxy/src/prefetch.rs ONLY.
Problem: prefetch buffers whole bodies (`.bytes()` at ~L330 via
BodyExt::collect or to_bytes) before storage.write — pre-streaming API. A
multi-GB prefetched package OOMs a 4GB server.
Fix: migrate the fetch path onto the streaming API that EXISTS:
`CacheStorage::spool(&self, key) -> SpoolHandle` with
`handle.write(&[u8])` chunks + `handle.commit() -> StorageMeta` (see
cache_storage.rs L131 + proxy.rs run_tee for the canonical consumer pattern).
reqwest response → stream chunks (response.bytes_stream()) → SpoolHandle.
Keep: priority ordering, bounded concurrency, per-request timeout, SSRF
validation, atomic writes (SpoolHandle.commit is atomic), response-size limit
(enforce via written-bytes counter against the existing limit const).
READ FIRST: prefetch.rs whole file (~430 lines) + proxy.rs run_tee (~L884).
Verify: cargo test -p monopsony-proxy 2>&1 | tail -4 (114+9 baseline).

## B2 — docker.rs streaming fetch
File: crates/monopsony-proxy/src/docker.rs ONLY.
Problem: L170 `upstream.bytes().await` buffers whole layers into RAM.
Fix: same SpoolHandle migration as B1 at that site (stream via
response.chunk() or bytes_stream; commit; serve via read_stream which the
file already uses for hits — check try_cache/serve path and make miss-path
serve identical headers). Preserve auth-token forwarding and content-type
sidecar behavior.
READ FIRST: docker.rs whole file (440 lines).
Verify: cargo test -p monopsony-proxy 2>&1 | tail -4.

## B3 — token-mode auth regression test
File: crates/monopsony-proxy/tests/auth_mode_tests.rs (NEW) ONLY.
Rationale (F10 lesson): Open-mode test suites cannot catch auth regressions —
S1's dropped auth layer stayed green across 507 tests. This locks it.
Tests (loopback, hermetic, reuse streaming_tests.rs harness shapes but
self-contained file):
1. Token mode + missing key → GET via proxy router → 401 (build
   create_proxy_router with AuthState{auth_mode: Token, db: None} — db:None
   is the fail-closed branch; assert 401 not 500).
2. Token mode + garbage key → 401.
3. Open mode → same request → NOT 401 (asserts the layer doesn't over-block).
Use ProxyState::new with temp ObjectStoreBackend (CryptoProvider install —
see streaming_tests make_state pattern) + serve via
monopsony_proxy::proxy::serve_proxy_router (the REAL path incl. CONNECT
intercept wiring) on 127.0.0.1:0; raw TcpStream absolute-URI GET.
READ FIRST: streaming_tests.rs L100-130 (harness) + auth.rs (token branch).
Verify: cargo test -p monopsony-proxy --test auth_mode_tests 2>&1 | tail -4.

Common: NO COMMITS; skip-and-report; lint noise = false positives.
