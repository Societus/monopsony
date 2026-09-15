# Slice 1 — Server: pin capture at spool commit + pin headers on HIT

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-proxy/src/proxy.rs — regions ONLY: `struct ProxyState` (~line 67-95),
   `async fn run_tee` (~line 855-1100, focus on the "Clean EOF — commit" section and the DB
   writes after `spool.commit()`), `async fn serve_cache_hit` (~line 443-590, focus on the
   X-Cache/X-Scan-Status header area), and the imports at the top.

## File set (touch NOTHING else)
- crates/monopsony-proxy/src/proxy.rs

## Design to implement
### A. Capture origin integrity at commit (in run_tee, after meta sidecar save, before/next to the DB block)
New helper fns (private, bottom of file near other helpers):
```rust
/// Extract an origin-declared sha256 (hex) for url from response headers.
fn origin_hash_from_headers(headers: &reqwest::header::HeaderMap) -> Option<(String, monopsony_common::pin_store::PinOrigin)>
```
Check (case-insensitive) header names: `x-checksum-sha256`, `x-goog-hash` (take only when the
value is `sha256=<hex>` form — skip crc32c/md5 entries), `x-amz-meta-sha256`, `digest`
(only `sha256=<hex>` form per RFC 3230). Return (hex, PinOrigin::Header) on first hit. Validate
hex is exactly 64 lowercase-able chars.
```rust
/// Try to fetch `<url>.sha256` (and `.sha512`) from the same origin using
/// state.client; return (hex-64 sha256, PinOrigin::Sidecar) if found.
async fn origin_hash_from_sidecar(state: &ProxyState, url: &str) -> Option<(String, monopsony_common::pin_store::PinOrigin)>
```
Append `.sha256` to the full URL. GET with a SHORT timeout (use
`tokio::time::timeout(Duration::from_secs(5), ...)`) so commit latency stays bounded;
treat non-200, body > 4 KiB, or parse failure as None. Content forms accepted: a bare hex
line, or `<hex>  <filename>` (sha256sum format — take the first token). If `.sha256`
missing, try `.sha512`: the sidecar is 128 hex chars → hash ALGORITHM differs. In that case
compute nothing — sidecars give the origin's declared hash of the object; a 128-hex sidecar is
a sha512 of the object, NOT comparable to our sha256 pin. SKIP `.sha512` (return None) and
leave a `// Phase 4: sha512 sidecars` comment.
In run_tee, gated on `ctx.state.integrity.pinning_enabled && ctx.state.pin_key.is_some()`:
1. Determine origin: headers first (headers of the UPSTREAM response — TeeContext does not
   carry them; the response headers ARE available where run_tee is invoked... if response
   headers are NOT reachable inside run_tee, capture them at the call site is FORBIDDEN (that
   is a different file's slice); instead do sidecar/shim/unverified inside run_tee and add a
   TODO for header capture — see "Report deviations" below).
2. Compute: `origin = header.or(sidecar).unwrap_or((meta.checksum_sha256.clone(), PinOrigin::Unverified))`.
3. Build `PinAttestation{ url: normalize_url_scheme(&ctx.url), expected_sha256, origin,
   key_id: "", observed_at: now_secs, signature: "" }`, `.sign(key)`.
4. Persist: `ctx.state.save_meta` already wrote CacheEntry; ALSO write a sidecar JSON file
   `<cache_dir>/pins/<cache_key>.pin` containing `pin_store::to_json(&att)` (create_dir_all
   the pins dir once). Use std::fs via tokio::fs.
5. If DB present: upsert into cache_pins via a NEW small db method — DO NOT add it; the db
   slice owns db.rs. Instead, call it through an optional closure-free direct sqlx? NO —
   db.rs is the other slice's file. Write the DB upsert call as a one-line TODO comment in
   run_tee marked `// DB upsert: slice-2 wires db.record_pin()` and SKIP it.
### B. X-Monopsony-Pin header on HIT (in serve_cache_hit, next to X-Cache: HIT)
Read `<cache_dir>/pins/<cache_key>.pin` (cache_dir from state.cache_dir, cache_key param is
in scope). If present AND the pin verifies under `state.pin_key`'s verifying key AND
`pin.expected_sha256` matches the storage-side recorded sha (get via
`state.storage.get_checksum(&cache_key)` if it returns Some, else skip comparison): attach
header `X-Monopsony-Pin: <to_json(att)>`. If the pin file exists but does NOT verify → attach
`X-Monopsony-Pin-Status: invalid` and a warn! log. Do not block serving at the server
(enforcement is the client's job).
### C. Tests (append to the existing `#[cfg(test)] mod tests` at the bottom of proxy.rs — it exists)
- `test_origin_hash_from_headers_prefers_x_checksum` — build HeaderMap, assert (hex,
  PinOrigin::Header).
- `test_origin_hash_from_headers_ignores_non_sha256_goog_hash` — `x-goog-hash: crc32c=xxx`
  only → None.
- `test_pin_sidecar_write_and_header_roundtrip` — if the existing tests have a ProxyState
  fixture pattern (they do — see test module), write a pin file to the temp cache dir's
  pins/ subdir and call the new serve-path helper OR, if serve_cache_hit is too heavy to call
  directly, extract the header attach into a small helper fn
  `fn attach_pin_header(builder, state, cache_key) -> builder` (axum Builder type is fine)
  and unit-test THAT. Prefer the helper extraction.

## Do NOT
- Do not modify run_proxy's signature (hub already did).
- Do not modify pin_store.rs, lib.rs, Cargo.toml, db.rs, or any client file.
- Do not implement the header capture if response headers are genuinely unreachable in
  run_tee — leave the TODO and report the deviation. Do not refactor TeeContext to add them
  unless it is a <=6-line change (add `upstream_headers: Option<HashMap<String,String>>` to
  TeeContext, populate at the ONE construction site in this file, use it). That refactor IS
  allowed and preferred.

## Verify (ONCE, from /home/societus/projects/monopsony)
`cargo check -p monopsony-proxy && cargo test -p monopsony-proxy pin -- --nocapture` must end
with the new tests passing.
