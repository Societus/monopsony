//! HTTP caching proxy implementation
//!
//! This module implements a forward HTTP proxy that intercepts package downloads,
//! caches them locally, and serves cached packages to subsequent requests.

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};

/// HTTPS-capable hyper client type (rustls + ring).
type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

/// Build an HTTPS-capable hyper client connector (rustls + native roots).
///
/// `Client::builder().build_http()` only does plaintext HTTP. This wrapper
/// upgrades the connector to support TLS so we can reach HTTPS upstreams.
fn build_https_client() -> HttpClient {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("Failed to load native root certs")
        .https_or_http()
        .enable_http2()
        .build();
    Client::builder(TokioExecutor::new()).build(connector)
}
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use parking_lot::Mutex;
use tracing::{debug, error, info, trace, warn};

use monopsony_common::{PackageManagerBackend, PackageManagerKind};

use crate::cache_storage::{ByteRange, CacheStorage, SpoolHandle};
use crate::security::RateLimiter;
use monopsony_db::Database;

/// Cache metadata for a cached package
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub url: String,
    pub cache_path: String,
    pub content_type: Option<String>,
    pub content_length: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cached_at: u64,
    pub last_accessed: u64,
    pub hit_count: u64,
}

/// Shared state for the proxy
pub struct ProxyState {
    pub client: HttpClient,
    pub cache_dir: PathBuf,
    pub storage: Arc<dyn CacheStorage>,
    pub backends: Vec<Box<dyn PackageManagerBackend>>,
    pub db: Option<Arc<Database>>,
    pub scan_config: Option<monopsony_common::config::ScanConfig>,
    pub node_config: Option<monopsony_common::config::ServerConfig>,
    /// Per-cache-key in-flight fetch coordination. A second request for the
    /// same key waits for the winner's fetch and then serves from cache.
    pub inflight: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// Streaming limits (max body size, upstream stall timeout, janitor knobs).
    pub limits: monopsony_common::config::ProxyLimitsConfig,
    /// Permit upstreams on private/loopback IPs (SSRF guard bypass). Needed
    /// when Monopsony caches from LAN-internal mirrors. Default: false.
    pub allow_private_upstreams: bool,
    /// HTTP CONNECT pass-through settings (enabled flag, deny-list, private
    /// upstreams). Default: disabled.
    pub connect_config: monopsony_common::config::ProxyServerConfig,
}

impl ProxyState {
    pub fn new(storage: Arc<dyn CacheStorage>, db: Option<Arc<Database>>) -> Self {
        let client = build_https_client();
        let cache_dir = storage.base_dir().unwrap_or_default();
        let backends = monopsony_common::package_manager::get_all_backends();

        Self {
            client,
            cache_dir,
            storage,
            backends,
            db,
            scan_config: None,
            node_config: None,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            limits: monopsony_common::config::ProxyLimitsConfig::default(),
            allow_private_upstreams: false,
            connect_config: monopsony_common::config::ProxyServerConfig::default(),
        }
    }

    /// Find a backend that matches the given URL
    pub fn find_backend_for_url(
        &self,
        url: &str,
    ) -> Option<(PackageManagerKind, &dyn PackageManagerBackend)> {
        for backend in &self.backends {
            for pattern in backend.upstream_url_patterns() {
                if pattern.matches(url) {
                    return Some((backend.kind(), backend.as_ref()));
                }
            }
        }
        None
    }

    /// Convert a URL to a storage key (relative path within the cache).
    /// This key is used with the CacheStorage trait.
    pub fn url_to_cache_key(&self, url: &str) -> Option<String> {
        if let Some((_, backend)) = self.find_backend_for_url(url) {
            let relative = backend.url_to_cache_path(url)?;
            Some(relative)
        } else {
            // Generic cache path for unmatched URLs
            let parsed = url::Url::parse(url).ok()?;
            let host = parsed.host_str()?;
            let path = parsed.path().trim_start_matches('/');
            Some(format!("other/{}/{}", host, path))
        }
    }

    /// Convert a URL to a cache file path (filesystem only, for path traversal validation).
    pub fn url_to_cache_path(&self, url: &str) -> Option<PathBuf> {
        self.url_to_cache_key(url)
            .and_then(|key| self.storage.base_dir().map(|base| base.join(&key)))
    }

    /// Load cache metadata via storage backend
    pub async fn load_meta(&self, cache_key: &str) -> Option<CacheEntry> {
        let content = self.storage.load_meta_json(cache_key).await?;
        serde_json::from_slice(&content).ok()
    }

    /// Save cache metadata via storage backend
    pub async fn save_meta(&self, cache_key: &str, entry: &CacheEntry) -> anyhow::Result<()> {
        let content = serde_json::to_vec_pretty(entry)?;
        self.storage.save_meta_json(cache_key, &content).await
    }

    /// Update last accessed time and hit count
    pub async fn update_access(&self, cache_key: &str, url: &str) {
        if let Some(mut meta) = self.load_meta(cache_key).await {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            meta.last_accessed = now;
            meta.hit_count += 1;
            let _ = self.save_meta(cache_key, &meta).await;

            if let Some(db) = &self.db {
                // The row was recorded under the scheme-normalized lookup URL;
                // count the hit against the same canonical form.
                if let Err(e) = db
                    .record_cache_hit(&normalize_url_scheme(url), now as i64)
                    .await
                {
                    trace!("Failed to record cache hit in DB for {}: {}", url, e);
                }
            }
        }
    }
}

/// Create the HTTP proxy router with optional rate limiter and auth middleware
pub fn create_proxy_router(
    state: Arc<ProxyState>,
    rate_limiter: Option<Arc<RateLimiter>>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
) -> axum::Router {
    axum::Router::new()
        .route("/health", axum::routing::get(health_check))
        .route("/*path", axum::routing::any(handle_proxy_request))
        .route("/", axum::routing::any(handle_proxy_request))
        // No `.fallback` here: CONNECT is intercepted in the hyper Service
        // (ProxyHyperService) before the Router, and `/*path` + `/` already
        // matched every routable request pre-CONNECT. A fallback only widened
        // matching — malformed paths matchit rejects reached the handler
        // instead of 404ing.
        //
        // Auth gates every Router-dispatched request (GET/HEAD cache path).
        // CONNECT never reaches the Router — it is auth'd separately in
        // `handle_connect_request` before the tunnel is established.
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            monopsony_control::auth::auth_middleware,
        ))
        .with_state((state, rate_limiter))
}

/// Health check endpoint for monitoring and installer verification
async fn health_check() -> &'static str {
    "ok"
}

/// Normalize a URL's scheme to HTTPS.
///
/// Package managers configured via forward proxy (XferCommand/proxy env) send
/// `http://` URLs even when upstreams serve HTTPS. The prefetch module stores
/// URLs as `https://`. Without normalization, an `http://` request never
/// matches a prefetched `https://` entry, causing duplicate fetches and cache
/// misses.
fn normalize_url_scheme(url: &str) -> String {
    if url.starts_with("http://") {
        format!("https://{}", &url[7..])
    } else {
        url.to_string()
    }
}

/// Handle HTTP proxy requests
#[axum::debug_handler]
pub async fn handle_proxy_request(
    State((state, rate_limiter)): State<(
        Arc<ProxyState>,
        Option<Arc<crate::security::RateLimiter>>,
    )>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> impl IntoResponse {
    let method = request.method().clone();
    let uri = request.uri().clone();

    debug!("Proxy request: {} {}", method, uri);

    // Check rate limit if a limiter is configured
    let limiter_clone = rate_limiter.clone();
    let needs_release = if let Some(limiter) = &rate_limiter {
        let ip = addr.ip();
        if !limiter.check_rate(&ip) {
            warn!("Rate limit exceeded for {}", ip);
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from("Rate limit exceeded"))
                .unwrap_or_else(|_| Response::new(Body::from("Rate limit exceeded")));
        }
        if !limiter.acquire_concurrent(&ip, crate::security::DEFAULT_MAX_CONCURRENT_PER_IP) {
            warn!("Concurrent connection limit exceeded for {}", ip);
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from("Too many concurrent connections"))
                .unwrap_or_else(|_| Response::new(Body::from("Too many concurrent connections")));
        }
        true
    } else {
        false
    };

    // Federation gate: reject peer requests on private-tier nodes
    if request.headers().contains_key("x-federated-peer") {
        if let Some(ref node_cfg) = state.node_config {
            if node_cfg.node.tier == monopsony_common::config::NodeTier::Private {
                warn!("Rejecting federated peer request on private-tier node");
                return Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header("X-Federation-Rejected", "private-tier")
                    .body(Body::from("This node does not accept federated data"))
                    .unwrap_or_else(|_| Response::new(Body::from("Federation rejected")));
            }
        }
    }

    // Only handle GET and HEAD for caching
    if method != Method::GET && method != Method::HEAD {
        return proxy_passthrough(&state, request).await;
    }

    // Get the full URL from the request. The scheme is preserved as-sent:
    // the fetch must honor http-only mirrors. Scheme normalization happens
    // only at DB-lookup boundaries (prefetch stores https:// URLs).
    let url = if uri.scheme().is_some() {
        uri.to_string()
    } else {
        // For proxy requests, the URI should be absolute
        format!("http://{}", uri)
    };

    // SSRF prevention: validate URL host doesn't resolve to private IP
    if let Ok(parsed) = url::Url::parse(&url) {
        if let Err(e) = crate::security::validate_url_host(&parsed) {
            if !state.allow_private_upstreams {
                warn!("SSRF blocked: {} — {}", url, e);
                return Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Body::from(
                        "Request blocked: destination resolves to private network",
                    ))
                    .unwrap_or_else(|_| {
                        Response::new(Body::from(
                            "Request blocked: destination resolves to private network",
                        ))
                    });
            }
            debug!(
                "Private upstream allowed (allow_private_upstreams): {} — {}",
                url, e
            );
        }
    }

    // Check if this URL matches a package manager pattern
    let is_package = state.find_backend_for_url(&url).is_some();

    let response = if is_package {
        debug!("Package URL detected: {}", url);
        handle_cached_request(state.clone(), request, &url).await
    } else {
        debug!("Non-package URL, passing through: {}", url);
        proxy_passthrough(&state, request).await
    };

    // Release the concurrent connection slot
    if needs_release {
        if let Some(limiter) = &limiter_clone {
            limiter.release_concurrent(&addr.ip());
        }
    }

    response
}

/// Handle a request with caching
async fn handle_cached_request(
    state: Arc<ProxyState>,
    request: Request<Body>,
    url: &str,
) -> Response<Body> {
    let cache_key = match state.url_to_cache_key(url) {
        Some(key) => key,
        None => {
            warn!("Could not determine cache key for URL: {}", url);
            return proxy_passthrough(&state, request).await;
        }
    };

    let cache_path = state.storage.base_dir().map(|base| base.join(&cache_key));

    // Join loop: serve from cache when possible; if another request is
    // already fetching this key, wait for it and re-check (bounded so a
    // pathological sequence cannot spin forever).
    let mut joins: u8 = 0;
    loop {
        // Check DB first for authoritative cache state. The lookup URL is
        // scheme-normalized because prefetch stores https:// URLs while PMs
        // configured via proxy env may send http:// for the same object.
        let db_entry = if let Some(db) = &state.db {
            let lookup_url = normalize_url_scheme(url);
            match db.get_cache_entry(&lookup_url).await {
                Ok(Some(entry)) => {
                    debug!("DB cache entry found for: {}", url);
                    Some(entry)
                }
                Ok(None) => {
                    debug!("DB: no entry for: {}", url);
                    None
                }
                Err(e) => {
                    trace!("DB lookup failed for {}: {}", url, e);
                    None
                }
            }
        } else {
            None
        };
        let db_cached = db_entry.is_some();

        // Validate cache path doesn't escape cache directory (path traversal prevention, filesystem only).
        // DB-less mode (no database configured) falls back to storage existence.
        let exists = state.storage.exists(&cache_key).await.unwrap_or(false);
        let authoritative = db_cached || state.db.is_none();
        let path_valid = if let Some(ref path) = cache_path {
            authoritative
                && exists
                && crate::security::validate_cache_path(&path.to_string_lossy(), &state.cache_dir)
        } else {
            // S3 backend: no path traversal concern
            authoritative && exists
        };

        if path_valid {
            // Parse Range before the await-heavy serve path: `&Request<Body>`
            // is not Send and cannot be held across awaits.
            let range = parse_range_header(request.headers().get(header::RANGE));
            if let Some(response) = serve_cache_hit(&state, range, &cache_key, url).await {
                return response;
            }
            // read_stream failed below the DB/exists checks — treat as a
            // miss and fall through to fetch.
        }

        monopsony_common::metrics::cache_request_miss();
        debug!("Cache miss for: {}", url);

        // In-flight coalescing: if another request is already fetching this
        // key, wait for it and re-check the cache instead of starting a
        // duplicate upstream download. Bounded: after 2 joins we fetch
        // ourselves regardless.
        if joins < 2 {
            if let Some(waiter) = register_waiter(&state.inflight, &cache_key) {
                info!("Joining in-flight fetch for {}", url);
                wait_for_fetcher(&waiter, Duration::from_secs(600)).await;
                joins += 1;
                continue;
            }
        }

        // We are the fetcher for this key.
        return fetch_and_stream_cache_miss(state, request, url, cache_key, cache_path).await;
    }
}

/// Serve a validated cache hit as a streaming response. Returns None when the
/// storage read fails or a security policy blocks serving — the caller then
/// treats the request as a miss.
///
/// `range` is pre-parsed (the caller extracts it from the request because
/// `&Request<Body>` is not Send and cannot be held across awaits here).
async fn serve_cache_hit(
    state: &ProxyState,
    range: ByteRange,
    cache_key: &str,
    url: &str,
) -> Option<Response<Body>> {
    debug!("Cache hit for: {}", url);
    monopsony_common::metrics::cache_request_hit();

    state.update_access(cache_key, url).await;

    // Load metadata for content type
    let meta = state.load_meta(cache_key).await;
    let content_type = meta
        .as_ref()
        .and_then(|m| m.content_type.clone())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Open a streaming read for the requested range (full object when no
    // Range header). This replaces the old whole-file `read()` — serving
    // a 1 GB cached package must not buffer it in RAM.
    match state.storage.read_stream(cache_key, range.clone()).await {
        Ok((stream, total)) => {
            let partial = stream_requested_partial(&range, total);
            let serve_len = if partial {
                range.end.min(total) - range.start
            } else {
                total
            };

            let mut builder = Response::builder()
                .status(if partial {
                    StatusCode::PARTIAL_CONTENT
                } else {
                    StatusCode::OK
                })
                .header(header::CONTENT_TYPE, &content_type)
                .header(header::CONTENT_LENGTH, serve_len)
                .header(header::ACCEPT_RANGES, "bytes");

            if partial {
                builder = builder.header(
                    header::CONTENT_RANGE,
                    format!(
                        "bytes {}-{}/{}",
                        range.start,
                        range.end.min(total).saturating_sub(1),
                        total
                    ),
                );
            }

            // Add cache headers
            if let Some(m) = &meta {
                if let Some(etag) = &m.etag {
                    builder = builder.header(header::ETAG, etag);
                }
                if let Some(lm) = &m.last_modified {
                    builder = builder.header(header::LAST_MODIFIED, lm);
                }
            }
            builder = builder.header("X-Cache", "HIT");

            // Scan policy enforcement: query DB for scan status on HIT
            if let Some(db) = &state.db {
                if let Ok(Some(scan_row)) = db.get_latest_scan_result(&cache_key).await {
                    builder = builder.header("X-Scan-Status", &scan_row.scan_status);

                    if let Some(ref scan_cfg) = state.scan_config {
                        let yara_rules: Vec<String> = scan_row
                            .yara_matches
                            .as_ref()
                            .and_then(|v| {
                                v.as_array().map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| {
                                            v.get("rule").and_then(|r| r.as_str()).map(String::from)
                                        })
                                        .collect()
                                })
                            })
                            .unwrap_or_default();

                        let scan_info = crate::scan_policy::ScanInfo {
                            status: scan_row.scan_status.clone(),
                            clamav_result: scan_row.clamav_result,
                            yara_rules,
                            max_entropy: scan_row.max_entropy,
                            cves: vec![],
                            cves_in_installed: vec![],
                        };

                        let client_ctx = crate::scan_policy::ClientContext {
                            installed_version: None,
                            force: false,
                        };

                        let decision =
                            crate::scan_policy::evaluate_policy(&scan_info, &client_ctx, scan_cfg);

                        match &decision {
                            crate::scan_policy::PolicyDecision::Block(reason) => {
                                monopsony_common::metrics::scan_policy_decision("block");
                                return Some(
                                    builder
                                        .status(StatusCode::FORBIDDEN)
                                        .header("X-Scan-Block-Reason", format!("{:?}", reason))
                                        .body(Body::from(format!(
                                            "Package blocked by scan policy: {:?}",
                                            reason
                                        )))
                                        .unwrap_or_else(|_| {
                                            Response::new(Body::from(
                                                "Package blocked by scan policy",
                                            ))
                                        }),
                                );
                            }
                            crate::scan_policy::PolicyDecision::Warn(warning) => {
                                monopsony_common::metrics::scan_policy_decision("warn");
                                builder =
                                    builder.header("X-Scan-Warning", format!("{:?}", warning));
                            }
                            crate::scan_policy::PolicyDecision::Allow => {}
                        }
                    }
                }
            }

            monopsony_common::metrics::cache_bytes_served(serve_len);
            let body = Body::from_stream(stream);
            return Some(
                builder
                    .body(body)
                    .unwrap_or_else(|_| Response::new(Body::from("Failed to build response"))),
            );
        }
        Err(e) => {
            warn!("Failed to open cached object {}: {}", cache_key, e);
            // Signal the caller to treat this as a miss and refetch —
            // the entry may be corrupt or the range malformed.
            None
        }
    }
}

/// Fetch from upstream and stream to the client while teeing to a spool.
/// Called only when we won the in-flight race for this cache key.
#[allow(clippy::too_many_arguments)]
async fn fetch_and_stream_cache_miss(
    state: Arc<ProxyState>,
    request: Request<Body>,
    url: &str,
    cache_key: String,
    cache_path: Option<PathBuf>,
) -> Response<Body> {
    // We are the fetcher for this key. On EVERY return path below, the
    // in-flight entry must be cleared so waiters wake and future requests
    // can fetch. `clear_fetcher` notifies waiters.

    // HEAD must never populate the cache (a HEAD response has no body —
    // caching it would poison the entry with a 0-byte file).
    if request.method() == Method::HEAD {
        clear_fetcher(&state.inflight, &cache_key);
        match fetch_upstream(&state, request, url).await {
            Ok(mut response) => {
                response
                    .headers_mut()
                    .insert("X-Cache", "MISS".parse().unwrap());
                response
            }
            Err(e) => {
                error!("Upstream HEAD failed for {}: {}", url, e);
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("Upstream request failed"))
                    .unwrap_or_else(|_| Response::new(Body::from("Upstream request failed")))
            }
        }
    } else {
        fetch_and_stream_get(state, request, url, cache_key, cache_path).await
    }
}

/// GET/other-body path of `fetch_and_stream_cache_miss`.
#[allow(clippy::too_many_arguments)]
async fn fetch_and_stream_get(
    state: Arc<ProxyState>,
    request: Request<Body>,
    url: &str,
    cache_key: String,
    cache_path: Option<PathBuf>,
) -> Response<Body> {
    let _ = &request; // request body is forwarded by fetch_upstream below

    // Fetch from upstream with the body UNBUFFERED.
    let upstream_response = match fetch_upstream(&state, request, url).await {
        Ok(response) => response,
        Err(e) => {
            error!("Upstream request failed for {}: {}", url, e);
            clear_fetcher(&state.inflight, &cache_key);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Upstream request failed"))
                .unwrap_or_else(|_| Response::new(Body::from("Upstream request failed")));
        }
    };

    // Only cache plain 200s. A 206 Partial Content is NOT the full object —
    // caching it as the full key would poison the cache with a byte range.
    // Other non-success statuses pass through uncached.
    if upstream_response.status() != StatusCode::OK {
        debug!(
            "Upstream returned {} for {} — passing through without caching",
            upstream_response.status(),
            url
        );
        clear_fetcher(&state.inflight, &cache_key);
        let mut response = upstream_response;
        response
            .headers_mut()
            .insert("X-Cache", "MISS".parse().unwrap());
        return response;
    }

    // Extract response headers for metadata
    let headers = upstream_response.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let upstream_content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let etag = headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = headers
        .get(header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Pre-flight size check via Content-Length when present. Bodies without
    // Content-Length are checked by the streamed-bytes counter in the tee task.
    if let Some(cl) = upstream_content_length {
        if cl > state.limits.max_response_bytes {
            warn!(
                "Response too large for {}: {} bytes (max {})",
                url, cl, state.limits.max_response_bytes
            );
            clear_fetcher(&state.inflight, &cache_key);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("X-Size-Rejected", "true")
                .body(Body::from(
                    "Response exceeds maximum cacheable size and is not served through the cache",
                ))
                .unwrap_or_else(|_| Response::new(Body::from("Response too large")));
        }
    }

    // Begin the spool: everything streams upstream → spool + client with
    // O(1) memory. The object becomes visible in the cache only on commit.
    let spool = match state.storage.spool(&cache_key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to open spool for {}: {}", cache_key, e);
            clear_fetcher(&state.inflight, &cache_key);
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Cache storage unavailable"))
                .unwrap_or_else(|_| Response::new(Body::from("Cache storage unavailable")));
        }
    };

    let (parts, upstream_body) = upstream_response.into_parts();

    // Tee task: reads the upstream body frame by frame, appending each chunk
    // to the spool while forwarding it to the client. On clean EOF it commits
    // the spool (atomic publish) and records metadata + DB + scan dispatch.
    // On any error (upstream abort, stall, oversize, client disconnect) it
    // aborts the spool — nothing partial is ever cached.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
    let tee_ctx = TeeContext {
        state: state.clone(),
        cache_key: cache_key.clone(),
        cache_path: cache_path.clone(),
        url: url.to_string(),
        content_type,
        upstream_content_length,
        etag,
        last_modified,
    };
    tokio::spawn(run_tee(upstream_body, spool, tx, tee_ctx));

    // Build the client response: original upstream headers, body piped from
    // the tee channel.
    let mut response = Response::from_parts(
        parts,
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
    );
    response
        .headers_mut()
        .insert("X-Cache", "MISS".parse().unwrap());
    if let Some(ref scan_cfg) = state.scan_config {
        if scan_cfg.enabled {
            response
                .headers_mut()
                .insert("X-Scan-Status", "pending".parse().unwrap());
        }
    }
    response
}

// ---------------------------------------------------------------------------
// In-flight fetch coalescing
// ---------------------------------------------------------------------------

/// Registers the caller as fetcher for `key` if nobody else has; otherwise
/// returns the waiter to await. Race-free under the mutex.
fn register_waiter(
    inflight: &Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
    key: &str,
) -> Option<Arc<tokio::sync::Notify>> {
    let mut map = inflight.lock();
    if let Some(existing) = map.get(key) {
        Some(existing.clone())
    } else {
        map.insert(key.to_string(), Arc::new(tokio::sync::Notify::new()));
        None
    }
}

/// Removes this key's in-flight entry and wakes all waiters.
fn clear_fetcher(inflight: &Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>, key: &str) {
    if let Some(notify) = inflight.lock().remove(key) {
        notify.notify_waiters();
    }
}

/// Waits for the fetcher to finish. The timeout bounds the wait; on timeout
/// the caller re-checks the cache and may become the fetcher itself.
async fn wait_for_fetcher(waiter: &tokio::sync::Notify, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, waiter.notified())
        .await
        .is_ok()
}

// ---------------------------------------------------------------------------
// Range request parsing
// ---------------------------------------------------------------------------

/// Parse a Range header into a byte range. Only single `bytes=N-M` ranges
/// are supported (that's what pacman `curl -C -` and apt send). Returns
/// `0..u64::MAX` when absent or unparseable — serving the whole object.
fn parse_range_header(value: Option<&axum::http::HeaderValue>) -> ByteRange {
    use crate::cache_storage::ByteRange;

    let Some(v) = value else { return 0..u64::MAX };
    let Some(s) = v.to_str().ok() else {
        return 0..u64::MAX;
    };

    let spec = match s.trim().strip_prefix("bytes=") {
        Some(spec) => spec,
        None => return 0..u64::MAX,
    };
    let (start_s, end_s) = match spec.split_once('-') {
        Some(pair) => pair,
        None => return 0..u64::MAX,
    };

    match (start_s.trim().parse::<u64>(), end_s.trim().parse::<u64>()) {
        (Ok(start), Ok(end)) if start <= end => start..(end + 1),
        (Ok(start), Err(_)) => start..u64::MAX, // bytes=N-
        _ => 0..u64::MAX,
    }
}

/// Whether the parsed range plus actual object size means a 206 response.
fn stream_requested_partial(range: &ByteRange, total: u64) -> bool {
    range.start > 0 || range.end < total
}

// ---------------------------------------------------------------------------
// Tee: upstream body → spool + client
// ---------------------------------------------------------------------------

/// Everything the tee task needs to finish the cache write after the body
/// streams through.
struct TeeContext {
    state: Arc<ProxyState>,
    cache_key: String,
    cache_path: Option<PathBuf>,
    url: String,
    content_type: Option<String>,
    upstream_content_length: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Tee task body: pump upstream chunks to the spool and the client channel.
///
/// Termination semantics:
/// - Upstream EOF + all chunks forwarded → commit spool, write metadata,
///   record DB rows, dispatch scan, clear in-flight.
/// - Upstream error / stall > `upstream_stall_secs` / size over limit /
///   client disconnect (send fails) → abort spool (nothing cached), clear
///   in-flight.
#[allow(clippy::too_many_arguments)]
async fn run_tee(
    body: axum::body::Body,
    mut spool: SpoolHandle,
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    ctx: TeeContext,
) {
    let stall_timeout = Duration::from_secs(ctx.state.limits.upstream_stall_secs);
    let max_bytes = ctx.state.limits.max_response_bytes;
    let mut bytes_seen: u64 = 0;
    let mut last_progress = Instant::now();

    let mut body = body.into_data_stream();
    loop {
        // Per-chunk stall guard: if upstream sends nothing for
        // `upstream_stall_secs`, give up (bounded by select).
        let next = tokio::time::timeout(stall_timeout, body.next()).await;
        match next {
            Err(_) => {
                warn!(
                    "Upstream stalled >{}s for {} — aborting fetch",
                    stall_timeout.as_secs(),
                    ctx.url
                );
                spool.abort().await;
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream stalled",
                    )))
                    .await;
                clear_fetcher(&ctx.state.inflight, &ctx.cache_key);
                return;
            }
            Ok(None) => break, // clean EOF
            Ok(Some(Err(e))) => {
                warn!("Upstream body error for {}: {}", ctx.url, e);
                spool.abort().await;
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "upstream body error",
                    )))
                    .await;
                clear_fetcher(&ctx.state.inflight, &ctx.cache_key);
                return;
            }
            Ok(Some(Ok(chunk))) => {
                bytes_seen += chunk.len() as u64;
                if bytes_seen > max_bytes {
                    warn!(
                        "Streamed body exceeded max {} bytes for {} — aborting",
                        max_bytes, ctx.url
                    );
                    spool.abort().await;
                    let _ = tx
                        .send(Err(std::io::Error::new(
                            std::io::ErrorKind::FileTooLarge,
                            "response too large",
                        )))
                        .await;
                    clear_fetcher(&ctx.state.inflight, &ctx.cache_key);
                    return;
                }
                spool.write(&chunk);
                if tx.send(Ok(chunk)).await.is_err() {
                    // Client disconnected. Keep spooling to completion —
                    // the cache still benefits from the full fetch — but
                    // stop forwarding.
                    debug!("Client disconnected from tee for {}", ctx.url);
                    if let Err(e) = drain_to_spool(
                        &mut body,
                        &mut spool,
                        stall_timeout,
                        max_bytes,
                        &mut bytes_seen,
                        &mut last_progress,
                    )
                    .await
                    {
                        warn!(
                            "Drain after client disconnect failed for {}: {}",
                            ctx.url, e
                        );
                        spool.abort().await;
                        clear_fetcher(&ctx.state.inflight, &ctx.cache_key);
                        return;
                    }
                    break;
                }
                last_progress = Instant::now();
            }
        }
    }

    // Clean EOF — commit.
    let meta = match spool.commit().await {
        Ok(m) => m,
        Err(e) => {
            error!("Spool commit failed for {}: {}", ctx.url, e);
            clear_fetcher(&ctx.state.inflight, &ctx.cache_key);
            return;
        }
    };

    info!(
        "Cached: {} -> {} ({} bytes, sha256 {})",
        ctx.url,
        ctx.cache_key,
        meta.size_bytes,
        &meta.checksum_sha256[..12.min(meta.checksum_sha256.len())]
    );
    monopsony_common::metrics::cache_bytes_cached(meta.size_bytes);

    // Filesystem metadata sidecar (content-type, etag, last-modified, hits)
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entry = CacheEntry {
        url: ctx.url.clone(),
        cache_path: ctx
            .cache_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ctx.cache_key.clone()),
        content_type: ctx.content_type.clone(),
        content_length: meta.size_bytes,
        etag: ctx.etag.clone(),
        last_modified: ctx.last_modified.clone(),
        cached_at: now,
        last_accessed: now,
        hit_count: 1,
    };
    if let Err(e) = ctx.state.save_meta(&ctx.cache_key, &entry).await {
        warn!("Failed to save cache metadata: {}", e);
    }

    if let Some(db) = &ctx.state.db {
        // Canonicalize the DB key to match the HIT-path lookup, which
        // scheme-normalizes before querying. Writing the raw fetch URL here
        // (http://) makes freshly committed objects invisible to lookups.
        if let Err(e) = db
            .record_cache_miss(&monopsony_db::db::CacheMissConfig {
                url: normalize_url_scheme(&ctx.url),
                cache_path: entry.cache_path.clone(),
                content_type: entry.content_type.clone(),
                content_length: entry.content_length as i64,
                etag: entry.etag.clone(),
                last_modified: entry.last_modified.clone(),
                cached_at: entry.cached_at as i64,
            })
            .await
        {
            trace!("Failed to record cache miss in DB for {}: {}", ctx.url, e);
        }
        // Store the spool-computed hash for tamper detection on HITs
        // (canonical scheme, matching how the row was recorded).
        if let Err(e) = db
            .update_cache_sha256(&normalize_url_scheme(&ctx.url), &meta.checksum_sha256)
            .await
        {
            trace!("Failed to store SHA256 for {}: {}", ctx.url, e);
        }
    }

    // Fire-and-forget malware scan. The scanner API takes a buffer; for
    // large objects it reads the committed file from disk instead of RAM.
    if let Some(scan_cfg) = ctx.state.scan_config.clone() {
        if scan_cfg.enabled {
            let scan_state = ctx.state.clone();
            let scan_key = ctx.cache_key.clone();
            let scan_path = ctx
                .cache_path
                .clone()
                .unwrap_or_else(|| scan_state.cache_dir.join(&scan_key));
            let scan_url = ctx.url.clone();
            let sha = meta.checksum_sha256.clone();
            let retention_days = scan_state
                .node_config
                .as_ref()
                .and_then(|n| n.cache.s3.as_ref())
                .map(|s3| s3.retention_days)
                .unwrap_or(0);

            tokio::spawn(async move {
                // Read the committed object back for scanning. Objects up to
                // SCAN_BUFFER_LIMIT are read whole; larger objects skip
                // ClamAV/entropy scanning (YARA scans the file path directly).
                const SCAN_BUFFER_LIMIT: u64 = 256 * 1024 * 1024;
                let scan_bytes: Option<Vec<u8>> = if meta.size_bytes <= SCAN_BUFFER_LIMIT {
                    scan_state.storage.read(&scan_key).await.ok()
                } else {
                    debug!(
                        "Object {} ({} bytes) exceeds scan buffer limit — YARA-only scan",
                        scan_url, meta.size_bytes
                    );
                    None
                };

                let (result, record) = match scan_bytes {
                    Some(ref bytes) => {
                        crate::scan::run_scan(
                            bytes, &scan_path,
                            None, // package_name extracted from URL in future sprint
                            None, // package_version
                            &scan_cfg,
                        )
                        .await
                    }
                    None => {
                        // Large-object path: YARA + CVE only (ClamAV/entropy
                        // need the buffer). run_scan with empty data gives
                        // ClamAV/entropy vacuous results; YARA scans the file.
                        crate::scan::run_scan(&[], &scan_path, None, None, &scan_cfg).await
                    }
                };

                if let Some(db) = &scan_state.db {
                    if let Err(e) = db.record_scan_result(&record).await {
                        tracing::warn!("Failed to record scan result: {}", e);
                    }
                    let _ = sha; // hash already stored at commit time
                }

                // Object Lock: set retention on clean/flagged scans
                if retention_days > 0 {
                    let should_lock = matches!(
                        result.status,
                        crate::scan::ScanStatus::Clean | crate::scan::ScanStatus::Flagged
                    );
                    if should_lock {
                        let until = std::time::SystemTime::now()
                            + std::time::Duration::from_secs(retention_days as u64 * 86400);
                        if let Err(e) = scan_state.storage.set_retention(&scan_key, until).await {
                            tracing::warn!(
                                "Failed to set Object Lock retention on {}: {}",
                                scan_key,
                                e
                            );
                        }
                    }
                }
            });
        }
    }

    clear_fetcher(&ctx.state.inflight, &ctx.cache_key);
}

/// Continue spooling after the client has disconnected. Same guards as the
/// main loop (stall, size) minus the forwarding.
async fn drain_to_spool(
    body: &mut (impl futures::Stream<Item = Result<bytes::Bytes, axum::Error>> + Unpin),
    spool: &mut SpoolHandle,
    stall_timeout: Duration,
    max_bytes: u64,
    bytes_seen: &mut u64,
    _last_progress: &mut Instant,
) -> anyhow::Result<()> {
    loop {
        let next = tokio::time::timeout(stall_timeout, body.next()).await;
        match next {
            Err(_) => anyhow::bail!("upstream stalled during drain"),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => anyhow::bail!("upstream error during drain: {}", e),
            Ok(Some(Ok(chunk))) => {
                *bytes_seen += chunk.len() as u64;
                if *bytes_seen > max_bytes {
                    anyhow::bail!("body exceeded max size during drain");
                }
                spool.write(&chunk);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Staged-part janitor
// ---------------------------------------------------------------------------

/// Remove orphaned multipart staging files (`<dest>#<N>`) older than `ttl`.
///
/// object_store's local backend writes spooled data to a staged sibling file
/// and atomically renames on commit. A crashed or killed upload leaves the
/// staged file behind forever — this sweep reclaims that space. Only applies
/// to filesystem-backed caches (S3 aborts clean up server-side).
async fn sweep_staged_parts(state: &ProxyState, ttl: Duration) -> anyhow::Result<()> {
    let Some(root) = state.storage.base_dir() else {
        return Ok(()); // S3 backend: multipart aborts handle cleanup
    };
    let cutoff = std::time::SystemTime::now() - ttl;
    let mut removed = 0usize;
    let mut reclaimed: u64 = 0;

    sweep_dir(&root, cutoff, &mut removed, &mut reclaimed).await?;

    if removed > 0 {
        info!(
            "Janitor removed {} orphaned staged part(s), {:.1} MiB reclaimed",
            removed,
            reclaimed as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

async fn sweep_dir(
    dir: &std::path::Path,
    cutoff: std::time::SystemTime,
    removed: &mut usize,
    reclaimed: &mut u64,
) -> anyhow::Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            Box::pin(sweep_dir(&path, cutoff, removed, reclaimed)).await?;
        } else {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            // Staged uploads look like `pkg.tar.zst#1` (or #2, #3 on retry).
            if let Some(idx) = name_str.rfind('#') {
                let suffix = &name_str[idx + 1..];
                if !suffix.bytes().all(|b| b.is_ascii_digit()) || suffix.is_empty() {
                    continue;
                }
                if let Ok(meta) = entry.metadata().await {
                    if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
                        let size = meta.len();
                        if tokio::fs::remove_file(&path).await.is_ok() {
                            *removed += 1;
                            *reclaimed += size;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Pass through request to upstream without caching
async fn proxy_passthrough(state: &ProxyState, request: Request<Body>) -> Response<Body> {
    let uri = request.uri().clone();
    let url = if uri.scheme().is_some() {
        uri.to_string()
    } else {
        format!("http://{}", uri)
    };

    match fetch_upstream(state, request, &url).await {
        Ok(response) => response,
        Err(e) => {
            error!("Proxy passthrough failed for {}: {}", url, e);
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Upstream request failed"))
                .unwrap_or_else(|_| Response::new(Body::from("Upstream request failed")))
        }
    }
}

/// Fetch a request from upstream
async fn fetch_upstream(
    state: &ProxyState,
    request: Request<Body>,
    url: &str,
) -> Result<Response<Body>, anyhow::Error> {
    let mut upstream_request = Request::builder().method(request.method().clone()).uri(url);

    // Copy relevant headers
    for (name, value) in request.headers() {
        // Skip hop-by-hop headers
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
        ) {
            continue;
        }
        upstream_request = upstream_request.header(name, value);
    }

    let upstream_request = upstream_request.body(request.into_body())?;

    debug!("Fetching from upstream: {}", url);
    let response = state.client.request(upstream_request).await?;

    // Return the response with its body UNBUFFERED. Size limits are enforced
    // by the callers: the cached path checks Content-Length up front and the
    // streamed-bytes counter in the tee task; non-package passthrough wraps
    // the body in a Limited adapter with the global cap.
    //
    // `Incoming` (hyper's response body) implements the http-body traits
    // directly; `Body::new` adopts it with zero copies.
    let (parts, incoming) = response.into_parts();
    Ok(Response::from_parts(parts, Body::new(incoming)))
}

/// Releases an acquired rate-limiter concurrent slot exactly once when
/// dropped. The CONNECT path holds its slot for the tunnel's lifetime (not
/// per byte) across every exit path: early rejects drop it locally, and a
/// spawned tunnel task drops it when the relay finishes.
struct ConnectSlotGuard {
    limiter: Option<Arc<crate::security::RateLimiter>>,
    ip: std::net::IpAddr,
    armed: bool,
}

impl Drop for ConnectSlotGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(l) = &self.limiter {
                l.release_concurrent(&self.ip);
            }
        }
    }
}

/// Rate-limited, authenticated entry point for CONNECT requests that bypasses
/// the axum Router (the serve loop calls this directly — see
/// `serve_proxy_router`).
///
/// The per-IP rate-limit slot is acquired INSIDE the auth-wrapped passthrough
/// service, i.e. only after auth passes: an auth-rejected CONNECT (401/501)
/// never acquires a slot, and the `ConnectSlotGuard` created in the same scope
/// as the acquire is passed into `handle_connect`, which moves it into the
/// tunnel task. Every early return drops the guard → releases the slot, so the
/// slot cannot leak.
///
/// Auth mirrors `monopsony_control::auth::auth_middleware` exactly: it is
/// invoked via a `Next` whose inner service runs the CONNECT handler, so the
/// real middleware (not a copy) gates this path.
async fn handle_connect_request(
    state: Arc<ProxyState>,
    rate_limiter: Option<Arc<crate::security::RateLimiter>>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
    addr: SocketAddr,
    req: Request<Body>,
) -> Response<Body> {
    // Same auth the Router would have run: the real auth_middleware is
    // invoked through its FromFnLayer, wrapped around a passthrough service
    // that runs the CONNECT handler (axum 0.7 exposes no Next constructor,
    // so middleware-from-a-fn must be called via its layer).
    let authority = req.uri().authority().map(|a| a.to_string());
    let connect_cfg = state.connect_config.clone();
    let connect_limiter = rate_limiter.clone();
    let connect_addr = addr;
    let svc = tower::util::BoxCloneService::new(tower::service_fn(
        move |req: Request<Body>| {
            let authority = authority.clone();
            let connect_cfg = connect_cfg.clone();
            let limiter = connect_limiter.clone();
            async move {
                // Auth passed. NOW acquire the concurrent slot (F1): before
                // this, an auth reject leaked the slot because the guard only
                // existed inside handle_connect.
                let ip = connect_addr.ip();
                if let Some(limiter) = &limiter {
                    if !limiter.check_rate(&ip) {
                        warn!("Rate limit exceeded for {}", ip);
                        return Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(StatusCode::TOO_MANY_REQUESTS)
                                .body(Body::from("Rate limit exceeded"))
                                .unwrap_or_else(|_| {
                                    Response::new(Body::from("Rate limit exceeded"))
                                })
                                .into_response(),
                        );
                    }
                    if !limiter.acquire_concurrent(&ip, crate::security::DEFAULT_MAX_CONCURRENT_PER_IP)
                    {
                        warn!("Concurrent connection limit exceeded for {}", ip);
                        return Ok(
                            Response::builder()
                                .status(StatusCode::TOO_MANY_REQUESTS)
                                .body(Body::from("Too many concurrent connections"))
                                .unwrap_or_else(|_| {
                                    Response::new(Body::from(
                                        "Too many concurrent connections",
                                    ))
                                })
                                .into_response(),
                        );
                    }
                }
                // Guard created in the same scope as the acquire: every early
                // return from here on releases the slot via Drop.
                let slot = ConnectSlotGuard {
                    limiter,
                    ip,
                    armed: true,
                };
                Ok(handle_connect(slot, ip, authority, connect_cfg, req)
                    .await
                    .into_response())
            }
        },
    ));
    let middleware = axum::middleware::from_fn_with_state(
        auth_state,
        monopsony_control::auth::auth_middleware,
    );
    let mut authed = tower::Layer::layer(&middleware, svc);
    use tower::Service as _;
    // Error type is Infallible, so this unwrap_or_else is total.
    authed.call(req).await.unwrap_or_else(|e| match e {})
}

/// Handle an HTTP CONNECT request: open an opaque TCP tunnel to the
/// authority's host:port and relay raw bytes in both directions.
///
/// The tunnel is deliberately opaque: no caching, no tee/spool, no body
/// inspection, no X-Cache header. The concurrent slot was acquired by the
/// caller (after auth) and arrives as `slot`; the guard moves into the tunnel
/// task, releasing the slot exactly once when the relay ends.
async fn handle_connect(
    slot: ConnectSlotGuard,
    ip: std::net::IpAddr,
    authority: Option<String>,
    connect_cfg: monopsony_common::config::ProxyServerConfig,
    request: Request<Body>,
) -> Response<Body> {
    if !connect_cfg.connect_enabled {
        warn!("CONNECT rejected: connect_enabled=false in [proxy] config");
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::from("CONNECT pass-through is disabled on this proxy"))
            .unwrap_or_else(|_| Response::new(Body::from("CONNECT disabled")));
    }

    let authority = match authority {
        Some(a) => a,
        None => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("CONNECT requires an authority (host:port)"))
                .unwrap_or_else(|_| Response::new(Body::from("CONNECT requires an authority")));
        }
    };

    // Parse "host:port" — the url crate handles bare hosts and [IPv6]:port.
    let parsed = match url::Url::parse(&format!("http://{}", authority)) {
        Ok(u) => u,
        Err(e) => {
            warn!("CONNECT invalid authority '{}': {}", authority, e);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("CONNECT invalid authority"))
                .unwrap_or_else(|_| Response::new(Body::from("CONNECT invalid authority")));
        }
    };
    let host = parsed.host_str().unwrap_or_default().to_string();
    if host.is_empty() {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("CONNECT authority has no host"))
            .unwrap_or_else(|_| Response::new(Body::from("CONNECT authority has no host")));
    }
    let port = parsed.port().unwrap_or(443);

    // Host reject-list: normalized case-insensitive match at a label
    // boundary (F8). `evil.com` denies `evil.com`, `EVIL.COM.`, and
    // `api.evil.com` — plain exact-match missed both.
    if let Some(deny) = &connect_cfg.connect_deny {
        let h = host.trim_end_matches('.').to_ascii_lowercase();
        if deny.iter().any(|d| {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            h == d || h.ends_with(&format!(".{d}"))
        }) {
            warn!("CONNECT to denied host '{}' rejected", host);
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("CONNECT destination is denied"))
                .unwrap_or_else(|_| Response::new(Body::from("CONNECT destination is denied")));
        }
    }

    // SSRF guard WITHOUT a DNS-rebinding TOCTOU (F2): resolve the hostname
    // ONCE, validate every resolved address against the private-IP check,
    // and dial ONLY validated addresses, in order. The hostname is never
    // dialed again, so rebinding between check and dial is impossible.
    // Resolution failure is a reject (not "let the fetch fail naturally"),
    // so a resolver blackhole cannot bypass the guard either.
    let resolved: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            warn!("CONNECT DNS resolve for '{}' failed: {}", host, e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("CONNECT to {} failed to resolve: {}", host, e)))
                .unwrap_or_else(|_| Response::new(Body::from("CONNECT upstream unreachable")));
        }
    };
    let candidates: Vec<SocketAddr> = if connect_cfg.allow_private_upstreams {
        resolved
    } else {
        resolved
            .into_iter()
            .filter(|a| {
                let private = crate::security::is_private_ip(&a.ip());
                if private {
                    warn!("SSRF blocked CONNECT to '{}': resolves to private IP {}", host, a);
                }
                !private
            })
            .collect()
    };
    if candidates.is_empty() {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from(
                "CONNECT destination resolves to a private network",
            ))
            .unwrap_or_else(|_| {
                Response::new(Body::from(
                    "CONNECT destination resolves to a private network",
                ))
            });
    }

    // Dial upstream BEFORE acknowledging: on dial failure the client sees a
    // clean 502 and no interim 200 was ever sent. The dial is bounded by
    // connect_timeout_secs (F3) — previously a blackholed upstream hung on
    // the OS's ~2min connect timeout.
    let dial_timeout = Duration::from_secs(connect_cfg.connect_timeout_secs);
    let mut upstream = None;
    let mut last_err: Option<std::io::Error> = None;
    for addr in &candidates {
        match tokio::time::timeout(dial_timeout, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                upstream = Some(s);
                break;
            }
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("dial to {} timed out after {}s", addr, connect_cfg.connect_timeout_secs),
                ));
            }
        }
    }
    let mut upstream = match upstream {
        Some(s) => s,
        None => {
            let e = last_err.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "no validated addresses to dial")
            });
            warn!("CONNECT upstream dial {}:{} failed: {}", host, port, e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!(
                    "CONNECT to {}:{} failed: {}",
                    host, port, e
                )))
                .unwrap_or_else(|_| Response::new(Body::from("CONNECT upstream unreachable")));
        }
    };
    // Disable Nagle on the tunnel legs (F4): the manual serve loop replaced
    // axum::serve, which set nodelay by default. Without it, tunnels suffer
    // Nagle/delayed-ACK (~40ms per direction switch).
    let _ = upstream.set_nodelay(true);

    // Tunnel lifecycle: return 200, then relay raw bytes until either side
    // closes (or the F3 total-lifetime cap expires). The slot guard MOVES
    // INTO the task so the concurrent slot is held for the tunnel's lifetime
    // (not per byte) and released exactly once when the relay ends.
    let lifetime = Duration::from_secs(connect_cfg.tunnel_max_lifetime_secs);
    let max_lifetime_secs = connect_cfg.tunnel_max_lifetime_secs;
    debug!("CONNECT tunnel open {}:{} for client {}", host, port, ip);
    tokio::spawn(async move {
        let _slot = slot; // moved: dropped at task end → releases the slot
        match hyper::upgrade::on(request).await {
            Ok(upgraded) => {
                // hyper 1.x `Upgraded` implements hyper::rt::Read/Write, not
                // tokio's traits. TokioIo bridges the trait families both
                // ways, giving copy_bidirectional a tokio AsyncRead+Write.
                let mut client_io = hyper_util::rt::TokioIo::new(upgraded);
                match tokio::time::timeout(
                    lifetime,
                    tokio::io::copy_bidirectional(&mut client_io, &mut upstream),
                )
                .await
                {
                    Ok(Ok((to_upstream, to_client))) => {
                        monopsony_common::metrics::proxy_connect_bytes(to_upstream + to_client);
                        debug!(
                            "CONNECT tunnel {}:{} closed ({}B up, {}B down)",
                            host, port, to_upstream, to_client
                        );
                    }
                    Ok(Err(e)) => warn!("CONNECT tunnel {}:{} relay error: {}", host, port, e),
                    Err(_) => warn!(
                        "CONNECT tunnel {}:{} exceeded max lifetime {}s — closing",
                        host, port, max_lifetime_secs
                    ),
                }
            }
            Err(e) => {
                warn!("CONNECT upgrade for {}:{} failed: {}", host, port, e)
            }
        }
        // `slot` drops here — releases the concurrent slot exactly once.
    });

    Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Thin hyper service: intercepts CONNECT before the axum Router, forwards
/// everything else into the Router unchanged.
///
/// Why this exists: axum 0.7's `RouteFuture` injects `Content-Length` into
/// every top-level route/fallback response, and hyper's h1 encoder FORBIDS
/// Content-Length/Transfer-Encoding on a 2xx response to CONNECT (RFC 7231)
/// — the encode error closes the connection and cancels the upgrade, killing
/// every tunnel. The injection happens outside any Router-level middleware,
/// so it cannot be stripped with `.layer(...)`; bypassing the Router for
/// CONNECT is the only correct fix.
struct ProxyHyperService {
    router: axum::Router,
    state: Arc<ProxyState>,
    rate_limiter: Option<Arc<crate::security::RateLimiter>>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
    /// The peer address of the connection this service instance serves; used
    /// for rate limiting and logging on the CONNECT path (CONNECT never goes
    /// through axum's ConnectInfo extractor because it bypasses the Router).
    peer_addr: SocketAddr,
}

impl hyper::service::Service<Request<hyper::body::Incoming>> for ProxyHyperService {
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Self::Response, Self::Error>>
                + Send,
        >,
    >;

    fn call(&self, req: Request<hyper::body::Incoming>) -> Self::Future {
        if req.method() == Method::CONNECT {
            // Tunnel path: bypass the Router entirely (its RouteFuture would
            // inject Content-Length and kill the tunnel). Convert Incoming ->
            // axum Body and run the CONNECT handler on the raw request.
            let (parts, incoming) = req.into_parts();
            let req = Request::from_parts(parts, Body::new(incoming));
            let fut = handle_connect_request(
                self.state.clone(),
                self.rate_limiter.clone(),
                self.auth_state.clone(),
                self.peer_addr,
                req,
            );
            Box::pin(async move { Ok(fut.await) })
        } else {
            // Normal path: through the axum Router (routes + middleware +
            // cache logic untouched). The Router's handler extracts
            // ConnectInfo<SocketAddr>; axum::serve normally inserts that
            // extension via into_make_service_with_connect_info, which this
            // manual loop replaces — so insert it here.
            let (mut parts, incoming) = req.into_parts();
            parts
                .extensions
                .insert(axum::extract::ConnectInfo(self.peer_addr));
            let req = Request::from_parts(parts, Body::new(incoming));
            let mut router = self.router.clone();
            Box::pin(async move {
                use tower::Service as _; // Router<()>: tower Service, Error = Infallible
                let res = router.call(req).await.unwrap_or_else(|e| match e {});
                Ok(res)
            })
        }
    }
}

/// Manual hyper-util serve loop for the proxy Router. Replaces `axum::serve`:
/// CONNECT is intercepted by [`ProxyHyperService`] before the Router, and
/// `serve_connection_with_upgrades` delivers h1 upgrades for the tunnel.
pub async fn serve_proxy_router(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    state: Arc<ProxyState>,
    rate_limiter: Option<Arc<crate::security::RateLimiter>>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        // axum::serve set TCP_NODELAY by default; this manual loop replaces
        // it, so set it explicitly or tunnels suffer Nagle/delayed-ACK (F4).
        let _ = stream.set_nodelay(true);
        let svc = ProxyHyperService {
            router: app.clone(),
            state: state.clone(),
            rate_limiter: rate_limiter.clone(),
            auth_state: auth_state.clone(),
            peer_addr: peer,
        };
        tokio::spawn(async move {
            let _ = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            )
            .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), svc)
            .await;
        });
    }
}

/// Start the HTTP proxy server
pub async fn run_proxy(
    addr: SocketAddr,
    storage: Arc<dyn CacheStorage>,
    db: Option<Arc<Database>>,
    auth_mode: monopsony_common::config::AuthMode,
    scan_config: Option<monopsony_common::config::ScanConfig>,
    node_config: Option<monopsony_common::config::ServerConfig>,
    limits: Option<monopsony_common::config::ProxyLimitsConfig>,
    connect_config: monopsony_common::config::ProxyServerConfig,
) -> anyhow::Result<()> {
    storage.init_dirs().await?;

    let mut state = Arc::new(ProxyState::new(storage, db.clone()));
    // Wire scan config into proxy state
    if let Some(ref cfg) = scan_config {
        Arc::get_mut(&mut state)
            .expect("state should be uniquely owned at this point")
            .scan_config = Some(cfg.clone());
    }
    // Wire node config (for Object Lock retention).
    if let Some(ref cfg) = node_config {
        Arc::get_mut(&mut state)
            .expect("state should be uniquely owned at this point")
            .node_config = Some(cfg.clone());
    }
    // Wire CONNECT ([proxy] section) config. Passed explicitly by the caller:
    // monopsony-server's main reads TOML per-section and historically never
    // constructed a full ServerConfig, so piggybacking on node_config left
    // connect_enabled stuck at its default (false).
    {
        Arc::get_mut(&mut state)
            .expect("state should be uniquely owned at this point")
            .connect_config = connect_config;
    }
    // Wire streaming limits (max body size, stall timeout, janitor cadence)
    if let Some(ref cfg) = limits {
        Arc::get_mut(&mut state)
            .expect("state should be uniquely owned at this point")
            .limits = cfg.clone();
    }

    // Spawn the staged-part janitor: sweeps orphaned `#N` multipart staging
    // files left behind by crashed uploads.
    let janitor_state = state.clone();
    tokio::spawn(async move {
        let sweep_secs = janitor_state.limits.stale_part_sweep_secs.max(60);
        let sweep_every = Duration::from_secs(sweep_secs);
        let ttl = Duration::from_secs(janitor_state.limits.stale_part_ttl_secs.max(sweep_secs));
        let mut interval = tokio::time::interval(sweep_every);
        loop {
            interval.tick().await;
            if let Err(e) = sweep_staged_parts(&janitor_state, ttl).await {
                warn!("Staged-part janitor sweep failed: {}", e);
            }
        }
    });

    // Create rate limiter and spawn cleanup task
    let limiter = crate::security::create_default_rate_limiter();
    let limiter_clone = limiter.clone();
    tokio::spawn(crate::security::run_rate_limiter_cleanup(
        limiter_clone,
        120,
    ));

    // Create auth middleware state
    let auth_state = Arc::new(monopsony_control::auth::AuthState { auth_mode, db });

    let app = create_proxy_router(state.clone(), Some(limiter.clone()), auth_state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("HTTP proxy listening on {}", addr);

    serve_proxy_router(listener, app, state, Some(limiter), auth_state).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store_backend::ObjectStoreBackend;

    #[test]
    fn test_url_to_cache_path() {
        // ProxyState::new() builds an HTTPS client which needs the ring CryptoProvider
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = std::env::temp_dir().join("monopsony-test-cache");
        std::fs::create_dir_all(&temp_dir).expect("create test cache dir");
        let storage = Arc::new(
            ObjectStoreBackend::local(temp_dir, 0).expect("local backend should construct"),
        );
        let state = ProxyState::new(storage, None);

        // Test with a known apt URL pattern
        let url =
            "http://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-image_5.15.0-1_amd64.deb";
        let path = state.url_to_cache_path(url);

        // Should produce a cache path
        assert!(path.is_some());
    }
}

/// Aggregate statistics about the cache directory
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheStats {
    pub cache_dir: String,
    pub total_entries: u64,
    pub total_size_bytes: u64,
    pub total_meta_files: u64,
    /// Per-backend breakdown
    pub by_backend: Vec<BackendStats>,
    /// Top 10 entries by size
    pub top_by_size: Vec<CacheEntry>,
    /// Top 10 entries by hit count
    pub top_by_hits: Vec<CacheEntry>,
    /// Oldest and newest entries
    pub oldest_entry: Option<CacheEntry>,
    pub newest_entry: Option<CacheEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendStats {
    pub backend: String,
    pub entry_count: u64,
    pub total_size_bytes: u64,
}

/// Scan the cache directory and return aggregate statistics.
/// Scans for .json metadata files and their corresponding cache files.
pub async fn scan_cache_stats(cache_dir: &std::path::Path) -> CacheStats {
    let mut total_entries: u64 = 0;
    let mut total_size_bytes: u64 = 0;
    let mut total_meta_files: u64 = 0;
    let mut by_backend: Vec<BackendStats> = Vec::new();
    let mut all_entries: Vec<CacheEntry> = Vec::new();

    // Walk the cache directory
    let mut dir_entries: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(mut read_dir) = tokio::fs::read_dir(cache_dir).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            if entry
                .file_type()
                .await
                .map(|ft| ft.is_dir())
                .unwrap_or(false)
            {
                dir_entries.push(entry.path());
            }
        }
    }

    // Now scan each subdirectory (backend dirs)
    for dir in &dir_entries {
        let backend_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let mut entry_count: u64 = 0;
        let mut backend_size: u64 = 0;

        if let Ok(mut sub_stream) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(entry)) = sub_stream.next_entry().await {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    total_meta_files += 1;
                    if let Ok(content) = tokio::fs::read(&path).await {
                        if let Ok(meta) = serde_json::from_slice::<CacheEntry>(&content) {
                            total_entries += 1;
                            total_size_bytes += meta.content_length;
                            entry_count += 1;
                            backend_size += meta.content_length;
                            all_entries.push(meta);
                        }
                    }
                } else if path.is_file() {
                    if let Ok(meta) = tokio::fs::metadata(&path).await {
                        backend_size += meta.len();
                    }
                }
            }
        }

        if entry_count > 0 {
            by_backend.push(BackendStats {
                backend: backend_name.to_string(),
                entry_count,
                total_size_bytes: backend_size,
            });
        }
    }

    // Sort entries for top lists
    all_entries.sort_by(|a, b| b.content_length.cmp(&a.content_length));
    let top_by_size = all_entries.iter().take(10).cloned().collect();

    let mut entries_by_hits = all_entries.clone();
    entries_by_hits.sort_by(|a, b| b.hit_count.cmp(&a.hit_count));
    let top_by_hits = entries_by_hits.iter().take(10).cloned().collect();

    let oldest_entry = all_entries.iter().min_by_key(|e| e.cached_at).cloned();
    let newest_entry = all_entries.iter().max_by_key(|e| e.cached_at).cloned();

    CacheStats {
        cache_dir: cache_dir.to_string_lossy().to_string(),
        total_entries,
        total_size_bytes,
        total_meta_files,
        by_backend,
        top_by_size,
        top_by_hits,
        oldest_entry,
        newest_entry,
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn test_scan_cache_stats_empty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = std::env::temp_dir().join("monopsony-test-cache-stats");
        let stats = rt.block_on(scan_cache_stats(&temp_dir));
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.total_size_bytes, 0);
    }
}
