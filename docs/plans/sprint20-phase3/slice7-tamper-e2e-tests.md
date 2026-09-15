# Slice 7 (wave 2) — Tamper-injection end-to-end tests

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-proxy/tests/streaming_tests.rs (the harness: local upstream + ProxyState
   with injected test backend + raw absolute-URI requests)
3. crates/monopsony-proxy/src/proxy.rs — regions ONLY: the pin helpers
   (`origin_hash_from_header_map` ~1310, `attach_pin_header` ~1355, `quarantined` — search),
   the TeeContext upstream_headers capture (~760-782, 881-893), and the pin test module
   entries at the bottom of the file (pin_test_key pattern).

## File set
- crates/monopsony-proxy/tests/streaming_tests.rs (append tests; may add helpers)
- crates/monopsony-proxy/src/proxy.rs ONLY IF a test-visible helper needs `pub(crate)`
  visibility (prefer not touching it; if you must, visibility-only changes)

## Context: what already exists (do NOT duplicate)
- Client stream wrapper truncation test: LANDED as
  `pin_verify::tests::stream_wrapper_passes_and_truncates` (client crate) — wave 1.
- Proxy unit tests for header parsing + pin-file roundtrip: LANDED in proxy.rs test module.

## Tests to add (in streaming_tests.rs)
1. `miss_then_hit_carries_signed_pin`: ProxyState with
   `pin_key = Some(test signing key)`, `integrity.pinning_enabled = true`,
   `allow_private_upstreams = true`, local upstream serving fixed bytes with response
   header `x-checksum-sha256: <sha256 of those bytes>`. Request the URL through the proxy
   (absolute-URI form) → expect 200 MISS; request again → expect 200 HIT with
   `X-Monopsony-Pin` header that parses via `monopsony_common::pin_store::from_json`,
   verifies under the test key's verifying key, origin=="header", and expected_sha256 ==
   the upstream bytes' hash. Also assert the pin file exists under
   `<cache_dir>/pins/<cache_key>.pin` (cache_key = what the PM backend's url_to_cache_path
   yields — print it if unsure).
   NOTE: response headers on the MISS/HIT responses are readable from the raw http1
   handshake response the harness already uses (hyper Response parts).
   CAVEAT: the sidecar fetch fires a GET for `<url>.sha256` — your local upstream should
   404 that path (default) so the header origin wins cleanly.
2. `hit_with_forged_pin_reports_invalid`: after a plain MISS (pinning enabled but upstream
   WITHOUT checksum headers — origin becomes unverified), hand-write a forged pin JSON into
   `<cache_dir>/pins/<cache_key>.pin` signed by a DIFFERENT key (or with a corrupted
   signature/key_id). HIT response must carry `X-Monopsony-Pin-Status: invalid` and no
   `X-Monopsony-Pin` header, and the object must still serve 200 (server-side is advisory;
   enforcement is client-side by design).
3. If time remains: `quarantine_gate_403s_tampered_url` — requires a live PG (db Some).
   SKIP if the harness has no PG fixture; report under not_done (verified on bench instead).

## Do NOT
- Do not modify production code (beyond visibility if unavoidable).
- Do not weaken or #[ignore] a failing test — report the state you saw honestly.
- Tests must be deterministic: no real network (loopback only), no sleeps > 1s.

## Environment
export PROTOC=/usr/bin/protoc before cargo. HARD 600s cap: read the harness, write both
tests, verify ONCE at the end. If short on time, land test 1 and report test 2 under
not_done rather than half-finishing both.

## Verify (ONCE, from /home/societus/projects/monopsony)
`cargo test -p monopsony-proxy --test streaming_tests -- --nocapture`
