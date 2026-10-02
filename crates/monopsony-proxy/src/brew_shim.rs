//! Homebrew server shim (Sprint 26 Phase 1 slice 2).
//!
//! Two plain-http shim routes that brew's env-var mirror domains point at
//! (`HOMEBREW_API_DOMAIN`, `HOMEBREW_BOTTLE_DOMAIN`):
//!
//! - `/brew/api/*` — formula/cask/JWS metadata from formulae.brew.sh. Rides
//!   the normal cached path (`handle_request_uri`) as `LangMetadata`; etag
//!   TTL revalidation is automatic.
//! - `/brew/bottles/*` — two sub-routes:
//!   - `<name>/manifests/<version>-<rebuild>`: ghcr manifests. The anonymous
//!     ghcr token is fetched (and cached ~10 min per formula name), a
//!     `Bearer` + OCI `Accept` header pair is injected into the request, and
//!     the canonical ghcr URL flows through `handle_request_uri`
//!     (`LangArtifact`, immutable — avoids the revalidation-auth gap).
//!     After the response resolves, a manifest-LRU entry is inserted so the
//!     following blob request can resolve its ambiguous filename.
//!   - `<name>-<version>.<platform>.bottle.<rebuild>.tar.gz`: bottle blobs.
//!     Bespoke handler — the ghcr blob GET 307-redirects to a signed CDN URL
//!     and the proxy's hyper client does not follow redirects, so the fetch
//!     uses reqwest (redirect-following), mimicking docker.rs's
//!     try_cache / fetch_upstream / cache_and_respond streaming-spool shape.
//!
//! On any 404 brew silently falls back to origin (mirror-list semantics) —
//! never breaks installs.
//!
//! Wire facts bench-verified 2026-10-01; see
//! docs/plans/sprint26-phase1-homebrew/context-pack-common.md.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use monopsony_common::config::CacheClass;

use crate::cache_storage::CacheStorage;
use crate::proxy::{handle_request_uri, ProxyState};
use crate::security::RateLimiter;

/// How long a ghcr anonymous token is reused before re-fetching.
const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a manifest-LRU entry stays valid (brew requests the manifest
/// immediately before the blob; 1h comfortably covers an install).
const MANIFEST_LRU_TTL: Duration = Duration::from_secs(60 * 60);
/// Cap on the manifest-LRU (a full `brew upgrade` touches hundreds of
/// formulae at most; bounded memory).
const MANIFEST_LRU_CAP: usize = 512;
/// Content-type bottle blobs are stored/served with.
const BOTTLE_CONTENT_TYPE: &str = "application/gzip";
/// Upper bound on formula JSONs scanned in the list_keys fallback.
const FORMULA_SCAN_LIMIT: usize = 1000;

// ---------------------------------------------------------------------------
// BrewShimState
// ---------------------------------------------------------------------------

/// Shared state for the brew shim: a reqwest client for ghcr token/blob
/// fetches (the proxy's `state.client` is a hyper client — wrong type and it
/// does not follow redirects), an in-process token cache, and the
/// manifest-LRU used to disambiguate bottle filenames.
///
/// Mirrors the `DockerState` pattern in docker.rs. `ghcr_base` is
/// `https://ghcr.io` in production and overridable so tests can point at a
/// local mock registry (keeps the module's tests network-free).
pub struct BrewShimState {
    pub client: reqwest::Client,
    /// ghcr base URL (scheme + host[:port]). Token and blob URLs derive
    /// from it.
    ghcr_base: String,
    /// formulae.brew.sh base URL — the self-fetch fallback source for
    /// per-formula JSON (brew 7 clients may use only the JWS aggregate and
    /// never request per-formula JSON through the shim).
    formulae_base: String,
    /// name → (token, fetched_at)
    tokens: Mutex<HashMap<String, (String, Instant)>>,
    /// "{name}-{version}." (trailing dot) → (name, inserted_at). Key shape
    /// from the context pack: the trailing dot prevents `jq-1.8.2` matching
    /// `jq-1.8.20`.
    manifest_lru: Mutex<HashMap<String, (String, Instant)>>,
}

impl BrewShimState {
    pub fn new() -> Self {
        Self::with_bases(
            "https://ghcr.io".to_string(),
            "https://formulae.brew.sh".to_string(),
        )
    }

    /// Test constructor: point the token/blob dance at a local mock.
    pub fn with_ghcr_base(ghcr_base: String) -> Self {
        Self::with_bases(ghcr_base, "https://formulae.brew.sh".to_string())
    }

    /// Test constructor: mock BOTH upstreams (ghcr + formulae.brew.sh).
    pub fn with_bases(ghcr_base: String, formulae_base: String) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("failed to build reqwest client for brew shim");
        Self {
            client,
            ghcr_base,
            formulae_base,
            tokens: Mutex::new(HashMap::new()),
            manifest_lru: Mutex::new(HashMap::new()),
        }
    }

    /// Fetch (or reuse) the anonymous ghcr pull token for a formula.
    /// Cached ~10 min keyed by formula name.
    async fn get_token(&self, name: &str) -> anyhow::Result<String> {
        let now = Instant::now();
        {
            let map = self.tokens.lock().await;
            if let Some((token, at)) = map.get(name) {
                if now.duration_since(*at) < TOKEN_TTL {
                    return Ok(token.clone());
                }
            }
        }

        let url = format!(
            "{}/token?scope=repository:homebrew/core/{}:pull&service=ghcr.io",
            self.ghcr_base, name
        );
        debug!("Brew shim token fetch: {}", url);
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("ghcr token endpoint returned {}", resp.status());
        }
        let json: serde_json::Value = resp.json().await?;
        let token = json
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("ghcr token response missing 'token'"))?
            .to_string();

        let mut map = self.tokens.lock().await;
        map.insert(name.to_string(), (token.clone(), now));
        Ok(token)
    }

    /// Record that a manifest for `{name}-{version}` was just served — the
    /// blob request brew makes next will start with `"{name}-{version}."`.
    async fn insert_manifest_prefix(&self, name: &str, version: &str, now: Instant) {
        let key = format!("{}-{}.", name, version);
        let mut map = self.manifest_lru.lock().await;
        // Prune expired entries on every insert (cheap; map is small).
        map.retain(|_, (_, at)| now.duration_since(*at) < MANIFEST_LRU_TTL);
        // Bounded size: evict the oldest entry when at capacity.
        while map.len() >= MANIFEST_LRU_CAP {
            match map.iter().min_by_key(|(_, (_, at))| *at) {
                Some((oldest, _)) => {
                    let oldest = oldest.clone();
                    map.remove(&oldest);
                }
                None => break,
            }
        }
        map.insert(key, (name.to_string(), now));
    }

    /// Resolve `(name, version)` for a bottle filename by scanning the
    /// manifest-LRU for the longest unexpired prefix key the filename starts
    /// with. Longest-match wins: `jq-1.` (version "1") also prefixes
    /// `jq-1.8.2.arm64...`, so a shorter key must not shadow a longer one.
    async fn lru_lookup(&self, filename: &str, now: Instant) -> Option<(String, String)> {
        let map = self.manifest_lru.lock().await;
        let mut best: Option<(&String, &String)> = None;
        for (key, (name, at)) in map.iter() {
            if now.duration_since(*at) < MANIFEST_LRU_TTL
                && filename.starts_with(key.as_str())
                && best.as_ref().is_none_or(|(bk, _)| key.len() > bk.len())
            {
                best = Some((key, name));
            }
        }
        best.map(|(key, name)| {
            // key = "{name}-{version}." — recover the version slice safely.
            let start = name.len() + 1;
            let end = key.len() - 1;
            let version = if start <= end && end <= key.len() {
                key[start..end].to_string()
            } else {
                String::new()
            };
            (name.clone(), version)
        })
    }
}

impl Default for BrewShimState {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide shim state for the hub-mounted axum handlers. Per-request
/// state (token cache, manifest-LRU) is inherently server-lifetime, not
/// per-request; tests construct their own `BrewShimState` instead.
pub fn global_shim_state() -> Arc<BrewShimState> {
    static SHIM: OnceLock<Arc<BrewShimState>> = OnceLock::new();
    SHIM.get_or_init(|| Arc::new(BrewShimState::new())).clone()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /brew/api/*` — formula/cask/JWS metadata shim.
///
/// Strips the `/brew/api` prefix and re-enters the normal cached pipeline
/// with the canonical `https://formulae.brew.sh/api{path}` upstream URL.
/// Origin-form requests (no Host) already work — the upstream URL is built
/// here, never from the request URI alone.
pub async fn handle_brew_api_shim(
    State((state, rate_limiter)): State<(Arc<ProxyState>, Option<Arc<RateLimiter>>)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let pq = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let upstream = api_upstream_url(&pq);
    handle_request_uri(state, rate_limiter, addr, request, upstream)
        .await
        .into_response()
}

/// Map a `/brew/api...` path (with optional query) to the canonical
/// formulae.brew.sh upstream URL.
fn api_upstream_url(path_and_query: &str) -> String {
    let path = path_and_query
        .strip_prefix("/brew/api")
        .unwrap_or(path_and_query);
    format!("https://formulae.brew.sh/api{}", path)
}

/// `GET /brew/bottles/*` — ghcr manifests + bottle blobs.
///
/// Dispatches on path shape:
/// - `<name>/manifests/<version>-<rebuild>` → manifest sub-route
/// - `<filename>.tar.gz` (flat) → bespoke blob handler
pub async fn handle_brew_bottles_shim(
    State((state, rate_limiter)): State<(Arc<ProxyState>, Option<Arc<RateLimiter>>)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let path = request.uri().path().to_string();
    let inner = path
        .strip_prefix("/brew/bottles")
        .unwrap_or(&path)
        .trim_start_matches('/');
    let segments: Vec<&str> = inner.split('/').collect();

    if segments.len() == 3 && segments[1] == "manifests" {
        handle_manifest_request(state, rate_limiter, addr, request, segments[0], segments[2]).await
    } else if segments.len() == 1 && !segments[0].is_empty() {
        let brew = global_shim_state();
        handle_blob_request(&state, &brew, segments[0]).await
    } else {
        warn!("Brew bottles shim: unrecognized path shape: {}", path);
        not_found_response()
    }
}

/// Manifest sub-route: token dance → header injection → cached ghcr fetch →
/// post-response LRU insert.
async fn handle_manifest_request(
    state: Arc<ProxyState>,
    rate_limiter: Option<Arc<RateLimiter>>,
    addr: SocketAddr,
    mut request: Request<Body>,
    name: &str,
    reference: &str,
) -> Response<Body> {
    if !valid_formula_name(name) || !valid_manifest_reference(reference) {
        warn!(
            "Brew manifest shim: rejecting invalid name/reference: {}/{}",
            name, reference
        );
        return not_found_response();
    }

    let brew = global_shim_state();

    let token = match brew.get_token(name).await {
        Ok(t) => t,
        Err(e) => {
            error!("Brew shim ghcr token fetch failed for {}: {}", name, e);
            return bad_gateway_response("Failed to fetch registry token");
        }
    };

    // Inject auth + OCI Accept BEFORE handle_request_uri — the fetch path
    // copies request headers upstream (skipping only hop-by-hop).
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", token)) {
        request.headers_mut().insert(header::AUTHORIZATION, v);
    }
    request.headers_mut().insert(
        header::ACCEPT,
        HeaderValue::from_static(
            "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json",
        ),
    );

    let upstream = format!(
        "{}/v2/homebrew/core/{}/manifests/{}",
        brew.ghcr_base, name, reference
    );

    // Insert the manifest-LRU entry at REQUEST time, before the upstream
    // fetch: brew 7 fetches the manifest and bottle CONCURRENTLY — the blob
    // request arrives while the manifest fetch is still in flight, so a
    // post-response insert always loses the race (fleet-caught 2026-10-02:
    // blob attempts at .24–.39s vs manifest cached at .80s). The name is
    // authoritative from the request path; nothing waits on the response.
    // Key "{name}-{version}." with the trailing dot (context pack);
    // reference = "{version}-{rebuild}", rebuild is the last '-'-separated
    // token.
    let version = reference
        .rsplit_once('-')
        .map(|(v, _)| v)
        .unwrap_or(reference);
    brew.insert_manifest_prefix(name, version, Instant::now())
        .await;

    let response = handle_request_uri(state, rate_limiter, addr, request, upstream)
        .await
        .into_response();

    response
}

/// Blob sub-route: bespoke handler (ghcr blob GETs 307 to a signed CDN URL
/// the hyper client cannot follow).
///
/// Flow: shape check → PM enablement (disabled ⇒ 404, brew origin-fallback)
/// → name/version resolution (manifest-LRU, then list_keys formula scan)
/// → HIT: stream from cache → MISS: reqwest blob GET + streaming spool with
/// sha256 verification against the formula digest (mismatch ⇒ abort + 502).
async fn handle_blob_request(
    state: &ProxyState,
    brew: &BrewShimState,
    filename: &str,
) -> Response<Body> {
    // Shape check: must be a flat bottle filename, no path separators.
    if !is_bottle_filename(filename) {
        warn!(
            "Brew blob shim: rejecting non-bottle filename: {}",
            filename
        );
        return not_found_response();
    }

    // Enablement gate — the bespoke path bypasses handle_request_uri's gate,
    // so check it here. Disabled ⇒ 404 (brew falls back to origin).
    if !brew_pm_enabled(state).await {
        info!("Brew blob shim: PM disabled — 404 for {}", filename);
        return not_found_response();
    }

    // Resolve the ambiguous name/version split. Tier-2/3 resolvers carry
    // the expected sha256 along; then the separate digest lookup is only
    // needed for LRU-resolved (tier-1) names.
    let Some((name, version, sha_opt)) = resolve_name_version(state, brew, filename).await else {
        warn!("Brew blob shim: unresolvable filename: {}", filename);
        return not_found_response();
    };

    let key = format!("brew/bottles/{}/{}/{}", name, version, filename);
    debug!("Brew blob request: {} → key {}", filename, key);

    // HIT path.
    if state.storage.exists(&key).await.unwrap_or(false) {
        monopsony_common::metrics::cache_request_hit();
        match state.storage.read_stream(&key, 0..u64::MAX).await {
            Ok((stream, total)) => {
                let content_type = stored_content_type(state.storage.as_ref(), &key).await;
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_LENGTH, total)
                    .header("X-Cache", "HIT")
                    .body(Body::from_stream(stream))
                    .unwrap_or_else(|_| Response::new(Body::empty()));
            }
            Err(e) => {
                warn!(
                    "Brew blob cache read failed for {}: {} — refetching",
                    key, e
                );
            }
        }
    }

    monopsony_common::metrics::cache_request_miss();

    // MISS path: expected digest comes from the formula JSON — carried
    // along by tier-2/3 resolution, or (LRU-resolved tier-1 names) looked
    // up separately, self-fetching origin if the formula was never cached
    // (brew 7 JWS-only clients). No formula ⇒ we cannot verify origin
    // integrity ⇒ 404 (origin fallback), never a blind fetch.
    let info = match sha_opt {
        Some(sha) => BottleFileInfo {
            digest_ref: format!("sha256:{sha}"),
            sha256_hex: sha,
        },
        None => match load_bottle_info(state.storage.as_ref(), brew, &name, filename).await {
            Some(info) => info,
            None => {
                warn!(
                    "Brew blob shim: no cached formula JSON (or no matching platform) for {} — 404",
                    filename
                );
                return not_found_response();
            }
        },
    };

    let token = match brew.get_token(&name).await {
        Ok(t) => t,
        Err(e) => {
            error!("Brew shim ghcr token fetch failed for {}: {}", name, e);
            return bad_gateway_response("Failed to fetch registry token");
        }
    };

    let url = format!(
        "{}/v2/homebrew/core/{}/blobs/{}",
        brew.ghcr_base, name, info.digest_ref
    );
    debug!("Brew blob upstream fetch: {}", url);

    let upstream = match brew
        .client
        .get(&url)
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Brew blob upstream fetch failed for {}: {}", url, e);
            return bad_gateway_response("Upstream registry request failed");
        }
    };

    // Non-success: pass through without caching (registry error bodies are
    // small JSON; buffering is fine — same as docker.rs).
    if !upstream.status().is_success() {
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let ct = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let bytes = upstream.bytes().await.unwrap_or_default();
        let mut builder = Response::builder().status(status);
        if let Some(ct) = ct {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        return builder
            .body(Body::from(bytes))
            .unwrap_or_else(|_| Response::new(Body::empty()));
    }

    // Stream the body into a spool — never buffer a whole bottle. Verify
    // the running sha256 against the formula digest BEFORE commit so a
    // corrupted/truncated fetch never becomes cache-visible.
    let mut spool = match state.storage.spool(&key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Brew blob spool open failed for {}: {}", key, e);
            return bad_gateway_response("Failed to open cache spool");
        }
    };
    spool.set_cache_class(CacheClass::LangArtifact);

    let mut hasher = Sha256::new();
    let mut stream = upstream.bytes_stream();
    let mut fetch_err: Option<reqwest::Error> = None;
    loop {
        match stream.next().await {
            None => break,
            Some(Err(e)) => {
                fetch_err = Some(e);
                break;
            }
            Some(Ok(chunk)) => {
                hasher.update(&chunk);
                spool.write(&chunk);
            }
        }
    }

    if let Some(e) = fetch_err {
        error!("Brew blob upstream read failed for {}: {}", key, e);
        spool.abort().await;
        return bad_gateway_response("Failed to read upstream body");
    }

    let got_sha = hex::encode(hasher.finalize());
    if got_sha != info.sha256_hex {
        error!(
            "Brew blob checksum mismatch for {}: expected sha256:{}, got sha256:{} — aborting",
            key, info.sha256_hex, got_sha
        );
        spool.abort().await;
        return bad_gateway_response("Bottle checksum mismatch");
    }

    let meta = match spool.commit().await {
        Ok(m) => m,
        Err(e) => {
            error!("Brew blob spool commit failed for {}: {}", key, e);
            return bad_gateway_response("Failed to commit cached body");
        }
    };
    info!("Brew cached bottle {} ({} bytes)", key, meta.size_bytes);

    // Content-type sidecar (same pattern as docker.rs).
    let sidecar = BottleCacheMeta {
        content_type: Some(BOTTLE_CONTENT_TYPE.to_string()),
    };
    if let Ok(raw) = serde_json::to_vec(&sidecar) {
        let _ = state.storage.save_meta_json(&key, &raw).await;
    }

    // Serve the freshly cached object via read_stream — identical serving
    // path to cache hits.
    let (body_stream, total) = match state.storage.read_stream(&key, 0..u64::MAX).await {
        Ok(v) => v,
        Err(e) => {
            error!("Brew blob read-back failed for {}: {}", key, e);
            return bad_gateway_response("Failed to read cached body");
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("X-Cache", "MISS")
        .header(header::CONTENT_TYPE, BOTTLE_CONTENT_TYPE)
        .header(header::CONTENT_LENGTH, total)
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

// ---------------------------------------------------------------------------
// Resolution helpers
// ---------------------------------------------------------------------------

/// Per-PM enablement for the bespoke blob path. `Ok(false)` ⇒ disabled;
/// errors fail OPEN (a settings-read failure must not take the proxy path
/// down — same class as handle_request_uri's gate). DB-less mode resolves
/// enabled (no settings table to gate on).
async fn brew_pm_enabled(state: &ProxyState) -> bool {
    match crate::language_artifacts::resolve_pm_enabled(state, "brew").await {
        Ok(enabled) => enabled,
        Err(e) => {
            debug!("Brew PM enablement check failed: {}", e);
            true
        }
    }
}

/// Resolve the ambiguous name/version split for a bottle filename.
/// Tiers:
/// 1. manifest-LRU (name authoritative from a prior manifest request),
/// 2. cached-formula scan (exact filename reconstruction),
/// 3. origin self-fetch (fleet-caught 2026-10-02): brew 7 clients that
///    rely on the JWS aggregate never request per-formula JSON through
///    the mirror, so tiers 1-2 can both miss on a FIRST fetch. Derive
///    name candidates from the formula from formulae.brew.sh until one
///    reconstructs the filename exactly. The sha256 rides along, so the
///    caller can skip the separate digest lookup.
///
/// Returns `(name, version, sha256_hex-if-resolved-via-formula)`.
async fn resolve_name_version(
    state: &ProxyState,
    brew: &BrewShimState,
    filename: &str,
) -> Option<(String, String, Option<String>)> {
    if let Some(hit) = brew.lru_lookup(filename, Instant::now()).await {
        return Some((hit.0, hit.1, None));
    }
    if let Some((name, version, sha)) =
        scan_formulas_for_filename(state.storage.as_ref(), filename).await
    {
        return Some((name, version, Some(sha)));
    }
    origin_resolve_filename(brew, filename).await
}

/// Digest info pulled from a cached formula JSON.
struct BottleFileInfo {
    /// Bare hex sha256 from `bottle.stable.files.<platform>.sha256`.
    sha256_hex: String,
    /// `sha256:<hex>` blob reference for the ghcr URL.
    digest_ref: String,
}

/// Load the cached formula JSON for `name` and find the bottle file entry
/// whose reconstructed filename matches. Returns the expected sha256 +
/// digest reference.
///
/// Fleet-caught (2026-10-02): brew 7 clients may rely solely on the JWS
/// aggregate bundle and NEVER request per-formula JSON through the API
/// shim — the cache can legitimately lack the formula. On cache miss,
/// self-fetch `formulae.brew.sh/api/formula/<name>.json` (via the shim's
/// reqwest client, NOT the proxy's cached path) and use it ephemerally.
/// Writing it into the cache here would bypass TTL bookkeeping; the normal
/// path populates it when a client asks for it.
async fn load_bottle_info(
    storage: &dyn CacheStorage,
    brew: &BrewShimState,
    name: &str,
    filename: &str,
) -> Option<BottleFileInfo> {
    let key = format!("brew/api/formula/{}.json", name);
    let value: serde_json::Value = match storage.read(&key).await {
        Ok(body) => serde_json::from_slice(&body).ok()?,
        Err(_) => {
            // Cache miss: self-fetch the formula from origin (ephemeral).
            let url = format!("{}/api/formula/{}.json", brew.formulae_base, name);
            debug!("Brew shim self-fetching formula: {}", url);
            let resp = brew.client.get(&url).send().await.ok()?;
            if !resp.status().is_success() {
                warn!(
                    "Brew shim formula self-fetch failed for {}: {}",
                    name,
                    resp.status()
                );
                return None;
            }
            serde_json::from_slice(&resp.bytes().await.ok()?).ok()?
        }
    };
    let (_, sha) = formula_matches_filename(&value, name, filename)?;
    Some(BottleFileInfo {
        digest_ref: format!("sha256:{}", sha),
        sha256_hex: sha,
    })
}

/// list_keys fallback: scan cached `brew/api/formula/*.json` entries and
/// reconstruct each bottle filename from `versions.stable` +
/// `bottle.stable.rebuild` + platform keys; return the (name, version,
/// sha256) whose reconstruction matches.
async fn scan_formulas_for_filename(
    storage: &dyn CacheStorage,
    filename: &str,
) -> Option<(String, String, String)> {
    let keys = storage.list_keys().await.ok()?;
    let mut formula_keys: Vec<String> = keys
        .into_iter()
        .filter(|k| k.starts_with("brew/api/formula/") && k.ends_with(".json"))
        .collect();
    // Deterministic order (shortest key first is arbitrary but stable).
    formula_keys.sort_by_key(|k| k.len());

    for key in formula_keys.iter().take(FORMULA_SCAN_LIMIT) {
        let name = key
            .strip_prefix("brew/api/formula/")
            .and_then(|s| s.strip_suffix(".json"))
            .unwrap_or_default();
        if !valid_formula_name(name) {
            continue;
        }
        let Ok(body) = storage.read(key).await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
            continue;
        };
        if let Some((version, sha)) = formula_matches_filename(&value, name, filename) {
            debug!(
                "Brew blob filename resolved via formula scan: {} → {}",
                filename, name
            );
            return Some((name.to_string(), version, sha));
        }
    }
    None
}

/// Tier-3 (fleet-caught 2026-10-02): brew 7 clients that rely solely on the
/// JWS aggregate never request per-formula JSON through the mirror, so on a
/// FIRST fetch neither the manifest-LRU nor the cached-formula scan can
/// resolve the ambiguous `<name>-<version>` split. Derive candidate names
/// from the filename itself and self-fetch each candidate's formula from
/// formulae.brew.sh until one reconstructs the filename exactly.
async fn origin_resolve_filename(
    brew: &BrewShimState,
    filename: &str,
) -> Option<(String, String, Option<String>)> {
    for candidate in filename_name_candidates(filename) {
        let url = format!("{}/api/formula/{}.json", brew.formulae_base, candidate);
        debug!("Brew shim tier-3 formula probe: {}", url);
        let Ok(resp) = brew.client.get(&url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(bytes) = resp.bytes().await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if let Some((version, sha)) = formula_matches_filename(&value, &candidate, filename) {
            info!(
                "Brew blob filename resolved via origin formula fetch: {} → {}",
                filename, candidate
            );
            return Some((candidate, version, Some(sha)));
        }
    }
    None
}

/// Derive candidate formula names from a bottle filename
/// `<name>-<version>.<platform>.bottle[.<rebuild>].tar.gz`.
///
/// The version token always starts with an ASCII digit; formula names may
/// (`openssl@3`) or may not (`gnu-tar`) contain digits, so the PRIMARY
/// candidate is everything before the first digit-leading `-` token, with
/// shorter prefixes as fallbacks (for names whose own hyphens confuse the
/// split), capped at 4. Exact confirmation happens via filename
/// reconstruction in `origin_resolve_filename`, so over- or
/// under-generating here only costs an extra probe, never a wrong match.
fn filename_name_candidates(filename: &str) -> Vec<String> {
    let stem = filename.strip_suffix(".tar.gz").unwrap_or(filename);
    let parts: Vec<&str> = stem.split('-').collect();
    let mut candidates: Vec<String> = Vec::new();
    // Primary: join all parts before the first digit-leading part.
    for (i, part) in parts.iter().enumerate() {
        if i > 0 && part.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            candidates.push(parts[..i].join("-"));
            break;
        }
    }
    // Fallbacks: shorter prefixes (handles `-` inside names).
    if let Some(primary) = candidates.first().cloned() {
        let mut short: Vec<&str> = primary.split('-').collect();
        while short.len() > 1 && candidates.len() < 4 {
            short.pop();
            let c = short.join("-");
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }
    }
    candidates
        .into_iter()
        .filter(|c| valid_formula_name(c))
        .collect()
}

/// Check a parsed formula JSON against a bottle filename. Returns
/// `(version, sha256_hex)` when `versions.stable` + platform + rebuild
/// reconstruct the filename exactly.
///
/// Fleet-caught (2026-10-02, brew 7.0.7): rebuild-0 formulas request
/// `<name>-<version>.<platform>.bottle.tar.gz` — NO rebuild segment. Only
/// rebuild >= 1 carries `.bottle.<rebuild>.tar.gz`. Both shapes must match.
fn formula_matches_filename(
    value: &serde_json::Value,
    name: &str,
    filename: &str,
) -> Option<(String, String)> {
    let stable = value.get("versions")?.get("stable")?.as_str()?;
    // Fleet-caught (2026-10-02, wget 1.25.0_2): formulas with a top-level
    // `revision` render the bottle version as `{stable}_{revision}`; the
    // manifest ref and filename both carry the joined form.
    let revision = value.get("revision").and_then(|r| r.as_i64()).unwrap_or(0);
    let version = if revision > 0 {
        format!("{stable}_{revision}")
    } else {
        stable.to_string()
    };
    let rebuild = value
        .get("bottle")
        .and_then(|b| b.get("stable"))
        .and_then(|s| s.get("rebuild"))
        .and_then(|r| r.as_i64())
        .unwrap_or(0);
    let files = value
        .get("bottle")?
        .get("stable")?
        .get("files")?
        .as_object()?;
    for (platform, info) in files {
        let Some(sha) = info.get("sha256").and_then(|s| s.as_str()) else {
            continue;
        };
        // rebuild-0: segment omitted entirely; rebuild>=1: `.bottle.<rebuild>.`
        let candidate = if rebuild == 0 {
            format!("{}-{}.{}.bottle.tar.gz", name, version, platform)
        } else {
            format!(
                "{}-{}.{}.bottle.{}.tar.gz",
                name, version, platform, rebuild
            )
        };
        if candidate == filename {
            return Some((version, sha.to_string()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Bottle filename shape: flat `<name>-<version>.<platform>.bottle.<rebuild>.tar.gz`.
fn is_bottle_filename(filename: &str) -> bool {
    filename.ends_with(".tar.gz")
        && filename.contains(".bottle.")
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains("..")
        && filename.len() > ".tar.gz".len()
}

/// Formula names: ASCII alphanumerics plus `-_.+@` (e.g. `openssl@3`),
/// no leading dot. Guards the token URL and cache keys against injection.
fn valid_formula_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'@'))
}

/// Manifest references: `{version}-{rebuild}` — alphanumerics plus `-._+`.
fn valid_manifest_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 128
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
}

fn not_found_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn bad_gateway_response(msg: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::from(msg))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Metadata stored alongside cached bottle blobs (content-type sidecar,
/// same pattern as docker.rs's DockerCacheMeta).
#[derive(serde::Serialize, serde::Deserialize)]
struct BottleCacheMeta {
    content_type: Option<String>,
}

async fn stored_content_type(storage: &dyn CacheStorage, key: &str) -> String {
    storage
        .load_meta_json(key)
        .await
        .and_then(|raw| serde_json::from_slice::<BottleCacheMeta>(&raw).ok())
        .and_then(|m| m.content_type)
        .unwrap_or_else(|| BOTTLE_CONTENT_TYPE.to_string())
}

// ---------------------------------------------------------------------------
// Tests (self-contained — mock upstreams on 127.0.0.1:0, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store_backend::ObjectStoreBackend;
    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn install_ring() {
        // ProxyState::new() builds an HTTPS client which needs the ring
        // CryptoProvider (context-pack pitfall).
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    async fn test_proxy_state() -> ProxyState {
        install_ring();
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn CacheStorage> =
            Arc::new(ObjectStoreBackend::local(dir.path().to_path_buf(), 0).unwrap());
        // Keep the tempdir alive for the test's duration by leaking it —
        // the storage backend only borrows the path.
        std::mem::forget(dir);
        ProxyState::new(storage, None)
    }

    /// Mock ghcr: `/token` + `/v2/homebrew/core/:name/blobs/:digest`.
    #[derive(Default)]
    struct MockGhcr {
        token_hits: AtomicUsize,
        blob_hits: AtomicUsize,
    }

    async fn spawn_mock_ghcr() -> (String, Arc<MockGhcr>) {
        let mock = Arc::new(MockGhcr::default());
        let m = mock.clone();

        async fn mock_token(State(m): State<Arc<MockGhcr>>) -> impl IntoResponse {
            m.token_hits.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({"token": "test-token"}))
        }

        async fn mock_blob(
            State(m): State<Arc<MockGhcr>>,
            Path((name, _digest)): Path<(String, String)>,
            headers: HeaderMap,
        ) -> Response<Body> {
            let auth_ok = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "Bearer test-token")
                .unwrap_or(false);
            if !auth_ok {
                return Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from("missing bearer token"))
                    .unwrap();
            }
            m.blob_hits.fetch_add(1, Ordering::SeqCst);
            // Deterministic payload keyed on the formula name (NOT the
            // requested digest) so tests can stage a matching or mismatching
            // formula sha without a hash fixed-point problem.
            let payload = format!("blob-for:{}\0\0trailing", name);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(payload))
                .unwrap()
        }

        let app = axum::Router::new()
            .route("/token", get(mock_token))
            .route("/v2/homebrew/core/:name/blobs/:digest", get(mock_blob))
            .with_state(m);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        (format!("http://{}", addr), mock)
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    async fn stage_formula(
        storage: &dyn CacheStorage,
        name: &str,
        version: &str,
        rebuild: i64,
        platform: &str,
        sha_hex: &str,
    ) {
        let url = format!(
            "https://ghcr.io/v2/homebrew/core/{}/blobs/sha256:{}",
            name, sha_hex
        );
        let json = serde_json::json!({
            "versions": {"stable": version},
            "bottle": {"stable": {"rebuild": rebuild, "files": {
                platform: {"url": url, "sha256": sha_hex}
            }}}
        });
        storage
            .write(
                &format!("brew/api/formula/{}.json", name),
                &serde_json::to_vec(&json).unwrap(),
            )
            .await
            .unwrap();
    }

    /// stage_formula variant with a top-level `revision` (formula-revision
    /// bottles render the version as `{stable}_{revision}` on the wire).
    async fn stage_formula_revised(
        storage: &dyn CacheStorage,
        name: &str,
        stable: &str,
        revision: i64,
        rebuild: i64,
        platform: &str,
        sha_hex: &str,
    ) {
        let url = format!(
            "https://ghcr.io/v2/homebrew/core/{}/blobs/sha256:{}",
            name, sha_hex
        );
        let json = serde_json::json!({
            "versions": {"stable": stable},
            "revision": revision,
            "bottle": {"stable": {"rebuild": rebuild, "files": {
                platform: {"url": url, "sha256": sha_hex}
            }}}
        });
        storage
            .write(
                &format!("brew/api/formula/{}.json", name),
                &serde_json::to_vec(&json).unwrap(),
            )
            .await
            .unwrap();
    }

    // 1. API shim URL construction (query preserved, prefix stripped).
    #[test]
    fn test_api_shim_upstream_url_mappings() {
        assert_eq!(
            api_upstream_url("/brew/api/formula/jq.json"),
            "https://formulae.brew.sh/api/formula/jq.json"
        );
        assert_eq!(
            api_upstream_url("/brew/api/internal/packages.arm64_tahoe.jws.json?v=2"),
            "https://formulae.brew.sh/api/internal/packages.arm64_tahoe.jws.json?v=2"
        );
    }

    // 2. Manifest-LRU: insert → blob lookup hits; trailing dot prevents
    //    jq-1.8.2 matching jq-1.8.20.
    #[tokio::test]
    async fn test_manifest_lru_insert_then_blob_lookup() {
        let brew = BrewShimState::new();
        let now = Instant::now();
        brew.insert_manifest_prefix("jq", "1.8.2", now).await;

        assert_eq!(
            brew.lru_lookup("jq-1.8.2.arm64_tahoe.bottle.1.tar.gz", now)
                .await,
            Some(("jq".to_string(), "1.8.2".to_string()))
        );
        // Trailing-dot guard: version 1.8.20 must NOT match key "jq-1.8.2.".
        assert!(brew
            .lru_lookup("jq-1.8.20.arm64_tahoe.bottle.1.tar.gz", now)
            .await
            .is_none());
    }

    // 3. Manifest-LRU entries age out after the TTL.
    #[tokio::test]
    async fn test_manifest_lru_expiry() {
        let brew = BrewShimState::new();
        let now = Instant::now();
        let stale = now.checked_sub(Duration::from_secs(7200)).unwrap();
        brew.insert_manifest_prefix("jq", "1.8.2", stale).await;

        assert!(brew
            .lru_lookup("jq-1.8.2.arm64_tahoe.bottle.1.tar.gz", now)
            .await
            .is_none());
        // Re-insert fresh — lookup works again.
        brew.insert_manifest_prefix("jq", "1.8.2", now).await;
        assert!(brew
            .lru_lookup("jq-1.8.2.x86_64_linux.bottle.1.tar.gz", now)
            .await
            .is_some());
    }

    // 4. Longest-prefix wins: "jq-1." must not shadow "jq-1.8.2.".
    #[tokio::test]
    async fn test_manifest_lru_longest_prefix_wins() {
        let brew = BrewShimState::new();
        let now = Instant::now();
        brew.insert_manifest_prefix("jq", "1", now).await;
        brew.insert_manifest_prefix("jq", "1.8.2", now).await;

        assert_eq!(
            brew.lru_lookup("jq-1.8.2.arm64_tahoe.bottle.1.tar.gz", now)
                .await,
            Some(("jq".to_string(), "1.8.2".to_string()))
        );
    }

    // 5. Ambiguous filename resolved via the list_keys formula fallback
    //    (restart mid-install: LRU empty, formula JSON cached).
    #[tokio::test]
    async fn test_filename_resolution_via_list_keys_fallback() {
        let state = test_proxy_state().await;
        stage_formula(
            state.storage.as_ref(),
            "jq",
            "1.8.2",
            1,
            "arm64_tahoe",
            &sha256_hex(b"whatever"),
        )
        .await;
        // Decoy formula with a similar name — must not match.
        stage_formula(
            state.storage.as_ref(),
            "jq-light",
            "1.8.2",
            1,
            "arm64_tahoe",
            &sha256_hex(b"other"),
        )
        .await;

        let brew = BrewShimState::new(); // empty LRU → fallback path
        let resolved =
            resolve_name_version(&state, &brew, "jq-1.8.2.arm64_tahoe.bottle.1.tar.gz").await;
        assert_eq!(
            resolved,
            Some((
                "jq".to_string(),
                "1.8.2".to_string(),
                Some(sha256_hex(b"whatever"))
            ))
        );
    }

    // 5b. Fleet-caught regression (2026-10-02): rebuild-0 formulas omit the
    // rebuild segment — `tree-2.3.2.arm64_tahoe.bottle.tar.gz` (no `.0.`).
    // The reconstruction must match BOTH shapes or every rebuild-0 formula
    // falls back to origin.
    #[tokio::test]
    async fn test_filename_resolution_rebuild_zero_shape() {
        let state = test_proxy_state().await;
        stage_formula(
            state.storage.as_ref(),
            "tree",
            "2.3.2",
            0,
            "arm64_tahoe",
            &sha256_hex(b"tree-bytes"),
        )
        .await;

        let brew = BrewShimState::new(); // empty LRU → fallback path
        let resolved =
            resolve_name_version(&state, &brew, "tree-2.3.2.arm64_tahoe.bottle.tar.gz").await;
        assert_eq!(
            resolved,
            Some((
                "tree".to_string(),
                "2.3.2".to_string(),
                Some(sha256_hex(b"tree-bytes"))
            ))
        );

        // load_bottle_info must ALSO resolve through the rebuild-0 shape so
        // the MISS path can find the digest.
        let brew_state = BrewShimState::new();
        let info = load_bottle_info(
            state.storage.as_ref(),
            &brew_state,
            "tree",
            "tree-2.3.2.arm64_tahoe.bottle.tar.gz",
        )
        .await;
        assert!(info.is_some(), "rebuild-0 digest lookup must resolve");
        assert_eq!(info.unwrap().sha256_hex, sha256_hex(b"tree-bytes"));
    }

    // 5c. Fleet-caught (2026-10-02, wget 1.25.0_2): formulas with a
    // top-level `revision` render the bottle version as `{stable}_{revision}`
    // — the manifest ref, filename, AND digest lookup must all carry the
    // joined form.
    #[tokio::test]
    async fn test_filename_resolution_formula_revision_shape() {
        let state = test_proxy_state().await;
        stage_formula_revised(
            state.storage.as_ref(),
            "wget",
            "1.25.0",
            2,
            0,
            "arm64_tahoe",
            &sha256_hex(b"wget-bytes"),
        )
        .await;

        let brew = BrewShimState::new(); // empty LRU → fallback path
        let resolved =
            resolve_name_version(&state, &brew, "wget-1.25.0_2.arm64_tahoe.bottle.tar.gz").await;
        assert_eq!(
            resolved,
            Some((
                "wget".to_string(),
                "1.25.0_2".to_string(),
                Some(sha256_hex(b"wget-bytes"))
            ))
        );
    }

    // 6. Unresolvable filename (no LRU entry, no cached formula) → 404.
    #[tokio::test]
    async fn test_unresolvable_filename_returns_404() {
        let state = test_proxy_state().await;
        // Dead bases: tier-3 origin probing fails fast (connection refused)
        // instead of touching the real formulae.brew.sh from unit tests.
        let brew = BrewShimState::with_bases(
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:1".to_string(),
        );
        let resp =
            handle_blob_request(&state, &brew, "ghost-9.9.9.arm64_tahoe.bottle.3.tar.gz").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // 6b. Tier-3 candidate derivation (pure function, no network).
    #[test]
    fn test_filename_name_candidates_shapes() {
        // Plain name, revisioned version (wget 1.25.0_2): primary = "wget".
        assert_eq!(
            filename_name_candidates("wget-1.25.0_2.arm64_tahoe.bottle.tar.gz"),
            vec!["wget".to_string()]
        );
        // Multi-hyphen name (gnu-tar style): primary keeps full name,
        // shorter prefixes follow as fallbacks.
        assert_eq!(
            filename_name_candidates("gnu-tar-1.35.arm64_tahoe.bottle.tar.gz"),
            vec!["gnu-tar".to_string(), "gnu".to_string(),]
        );
        // Digit-bearing name token (openssl@3): the '@' token does not
        // start with a digit, so the primary stays correct.
        assert_eq!(
            filename_name_candidates("openssl@3-3.5.7.arm64_tahoe.bottle.tar.gz"),
            vec!["openssl@3".to_string()]
        );
        // Rebuild-1 shape resolves identically.
        assert_eq!(
            filename_name_candidates("jq-1.8.2.arm64_tahoe.bottle.1.tar.gz"),
            vec!["jq".to_string()]
        );
    }

    // 7. Non-bottle path shapes rejected 404 without touching storage.
    #[tokio::test]
    async fn test_blob_shape_validation_404() {
        let state = test_proxy_state().await;
        let brew = BrewShimState::new();
        for bad in [
            "not-a-bottle.txt",
            "a/b.tar.gz",
            "plain.tar.gz",
            "../escape.tar.gz",
            "no-bottle-marker-1.0.tar.gz",
        ] {
            let resp = handle_blob_request(&state, &brew, bad).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "shape: {}", bad);
        }
    }

    // 8. Enablement: DB-less mode resolves enabled (no gate possible);
    //    the decision mapping itself is fail-open on error.
    #[tokio::test]
    async fn test_enablement_db_none_resolves_enabled() {
        let state = test_proxy_state().await;
        assert!(state.db.is_none());
        assert!(brew_pm_enabled(&state).await);
    }

    // 9. Happy-path roundtrip: MISS fetches from the mock ghcr (validating
    //    the Bearer dance), verifies sha256, caches; second call is a HIT
    //    with no additional upstream hits.
    #[tokio::test]
    async fn test_blob_miss_then_hit_roundtrip() {
        let (base, mock) = spawn_mock_ghcr().await;
        let state = test_proxy_state().await;
        let brew = BrewShimState::with_ghcr_base(base);

        let filename = "jq-1.8.2.arm64_tahoe.bottle.1.tar.gz";
        // The mock's payload is keyed on the formula name; stage the formula
        // with that payload's sha256 so the commit-time verify passes.
        let payload = "blob-for:jq\0\0trailing";
        stage_formula(
            state.storage.as_ref(),
            "jq",
            "1.8.2",
            1,
            "arm64_tahoe",
            &sha256_hex(payload.as_bytes()),
        )
        .await;

        // Seed the LRU so resolution is deterministic (name/version split).
        brew.insert_manifest_prefix("jq", "1.8.2", Instant::now())
            .await;

        // MISS
        let resp = handle_blob_request(&state, &brew, filename).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-cache").unwrap(), "MISS");
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            BOTTLE_CONTENT_TYPE
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], payload.as_bytes());

        // Cached under the segmented key.
        let key = format!("brew/bottles/jq/1.8.2/{}", filename);
        assert!(state.storage.exists(&key).await.unwrap());

        // HIT — same bytes, no extra upstream hits.
        let resp = handle_blob_request(&state, &brew, filename).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-cache").unwrap(), "HIT");
        let body2 = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body, body2);

        assert_eq!(mock.blob_hits.load(Ordering::SeqCst), 1);
        assert_eq!(mock.token_hits.load(Ordering::SeqCst), 1);
    }

    // 10. Digest mismatch: mock serves bytes whose sha256 differs from the
    //     formula digest → abort (nothing cached) + 502.
    #[tokio::test]
    async fn test_blob_digest_mismatch_aborts_and_502() {
        let (base, mock) = spawn_mock_ghcr().await;
        let state = test_proxy_state().await;
        let brew = BrewShimState::with_ghcr_base(base);

        // Real wire shape for rebuild-0: no rebuild segment (fleet-verified
        // 2026-10-02 — `.bottle.0.tar.gz` never appears on the wire).
        let filename = "wget-1.24.0.arm64_tahoe.bottle.tar.gz";
        // Formula claims a sha the mock's payload does NOT have.
        stage_formula(
            state.storage.as_ref(),
            "wget",
            "1.24.0",
            0,
            "arm64_tahoe",
            &sha256_hex(b"definitely-not-the-payload"),
        )
        .await;
        brew.insert_manifest_prefix("wget", "1.24.0", Instant::now())
            .await;

        let resp = handle_blob_request(&state, &brew, filename).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        // Aborted: nothing committed to storage.
        let key = format!("brew/bottles/wget/1.24.0/{}", filename);
        assert!(!state.storage.exists(&key).await.unwrap());
        assert_eq!(mock.blob_hits.load(Ordering::SeqCst), 1);
    }

    // 11. Token caching: a second call within the TTL does not re-fetch
    //     (mock counts token endpoint hits).
    #[tokio::test]
    async fn test_token_cached_across_calls() {
        let (base, mock) = spawn_mock_ghcr().await;
        let brew = BrewShimState::with_ghcr_base(base);

        let t1 = brew.get_token("jq").await.unwrap();
        let t2 = brew.get_token("jq").await.unwrap();
        assert_eq!(t1, t2);
        assert_eq!(mock.token_hits.load(Ordering::SeqCst), 1);

        // Different formula → separate token fetch.
        let _ = brew.get_token("wget").await.unwrap();
        assert_eq!(mock.token_hits.load(Ordering::SeqCst), 2);
    }
}
