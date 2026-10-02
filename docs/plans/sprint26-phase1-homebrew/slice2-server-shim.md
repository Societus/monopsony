# Slice 2 — Brew server shim (monopsony-proxy)

**Read first:** `docs/plans/sprint26-phase1-homebrew/context-pack-common.md`.

**Scope:** ONE new module `crates/monopsony-proxy/src/brew_shim.rs`. Plus 4 small hub-owned wiring edits in proxy.rs (listed at the end — DO NOT edit proxy.rs or lib.rs yourself; report done and the hub lands them).

## Routes & behavior

`pub fn router() -> axum::Router` NOT needed — the shim is two handlers
mounted into the existing `create_proxy_router` via `.route(...)` lines
(HUB-owned). You write:

### `pub async fn handle_brew_api_shim(...)`
- Strip `/brew/api` prefix; upstream = `https://formulae.brew.sh/api{path}`.
- `handle_request_uri(state, rate_limiter, addr, request, upstream).await` —
  formula/cask/JWS metadata rides the normal cached LangMetadata path (etag
  TTL revalidation is automatic).
- Origin-form requests (no Host) already work — do NOT rebuild URLs from
  request.uri() alone (see pack pitfalls).

### `pub async fn handle_brew_bottles_shim(...)`
Two sub-routes by path shape:

**A. `<name>/manifests/<version>-<rebuild>`** — upstream
`https://ghcr.io/v2/homebrew/core/{name}/manifests/{version}-{rebuild}`:
1. Token: `GET https://ghcr.io/token?scope=repository:homebrew/core/{name}:pull&service=ghcr.io`
   (parse `.token` from JSON; cache token in-process ~10 min keyed by name —
   a `tokio::sync::Mutex<HashMap<String,(String,Instant)>>` on a small
   `BrewShimState` you define, matching the crate's existing style. Use a
   fresh `reqwest::Client` held in that state — mirror the `DockerState`
   pattern in docker.rs; do NOT touch `state.client` (it is a hyper client,
   wrong type for this).
2. Inject `Authorization: Bearer <token>` + Accept
   (`application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json`)
   into `request` headers, then call `handle_request_uri` with the ghcr URL.
   Cache class LangArtifact (immutable) — the URL pattern in the backend
   already classifies it; no extra work. AFTER the response resolves, insert
   the manifest-LRU entry used by the blob route (see B.2): key
   `"{name}-{version}."` (with trailing dot — prevents `jq-1.8.2` matching
   `jq-1.8.20`), value `{name}`, 1h expiry.

**B. blob `<name>-<version>.<platform>.bottle.<rebuild>.tar.gz`** — bespoke
handler (307-redirect makes the normal path unusable):
1. Check PM enablement: reuse `crate::language_artifacts::resolve_pm_enabled(&state, "brew")`
   — disabled ⇒ respond **404** (brew origin-fallback, verified safe).
2. Cache key: `brew/bottles/{name}/{version}/{filename}` — but name/version
   are ambiguous from the filename (pack § Bottle filename ambiguity).
   Resolve via manifest-LRU: the map from sub-route A (key
   `"{name}-{version}."` with trailing dot, value `{name}`; entries expire
   after 1h). Lookup: scan map keys for one where the filename
   `starts_with` that key — the platform + `.bottle.` + rebuild suffix in the
   filename is irrelevant to the lookup. On miss →
   `storage.list_keys()` fallback over `brew/api/formula/` (parse each,
   reconstruct from `versions.stable` + `bottle.stable.rebuild` + platform
   keys) → resolve. Still miss ⇒ 404.
3. HIT path: `storage.exists(key)` → stream via
   `storage.read_stream(key, (0, u64::MAX))`, respond with stored
   content-type `application/gzip`, `X-Cache: HIT`.
4. MISS path: resolve digest from the formula JSON (see 2) → reqwest GET the
   ghcr blob URL `https://ghcr.io/v2/homebrew/core/{name}/ blobs/sha256:{hex}`
   (redirect-following) with Bearer token → streaming spool
   (`storage.spool(key)`, `set_cache_class(LangArtifact)`) teeing to client —
   **mimic docker.rs `cache_and_respond` exactly** (never buffer the whole
   body; respond 502 on spool failure; pass through non-success status).
5. Verify commit checksum vs formula `.sha256` — mismatch ⇒ abort commit
   (spool.abort()), log `error!`, 502. Match ⇒ `X-Cache: MISS`.
6. DB bookkeeping: call the same record-cache-miss/hit helpers the docker
   path uses, if any (grep `record_cache_miss(` in proxy.rs to find the
   canonical call with all 10 SQL binds + cache_class) — if the docker path
   doesn't do DB rows, skip DB for blobs and note it in your report.

## Tests (~8, self-contained — no network)

- LRU: manifest-serve insert → blob lookup hits; expiry ages entries out.
- Ambiguous filename resolution via list_keys fallback with a staged formula
  JSON in a tempdir storage (`ObjectStoreBackend::local(tempdir)`).
- Blob MISS with a local mock upstream (spawn a hyper server on
  127.0.38.99:PORT? NO — use `spawn a tokio TcpListener + axum router on
  127.0.0.1:0`, serve synthetic blob bytes; build a `ProxyState` via
  `ProxyState::new(storage, None)`; remember `rustls::ring` provider install
  in every ProxyState-building test per pack pitfalls).
- Digest mismatch → abort + 502 (mock serves wrong bytes).
- Disabled enablement → 404 (mock DB state or a stub `resolve_pm_enabled` —
  simpler: extract the check into `fn pm_enabled(state) -> bool` you can unit
  test by matching on `state.db.is_none()`).
- Manifest token caching: second call does not re-fetch (count mock hits).
- Happy-path blob MISS→cache→HIT roundtrip with sha256 match.
- Bottles handler returns 404 for unresolvable filenames.

## Verify

```
cargo test -p monopsony-proxy brew_shim 2>&1 | tail -5
cargo clippy -p monopsony-proxy --all-targets 2>&1 | grep -cE '^error'   # 0
```

## HUB-owned proxy.rs edits (do NOT touch; hub lands after review)

1. `mod brew_shim;` + `pub use` in lib.rs if applicable (check how docker.rs
   is exposed — grep `pub mod docker` in lib.rs).
2. `.route("/brew/api/*path", any(brew_shim::handle_brew_api_shim))` and
   `.route("/brew/bottles/*path", any(brew_shim::handle_brew_bottles))` in
   `create_proxy_router`.
3. Enablement arm `PackageManagerKind::Homebrew => Some("brew")` (~L548).
4. Retention `parse_brew_path` addition (separate slice/hub edit).
