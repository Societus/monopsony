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
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, error, info, trace, warn};

use monopsony_common::{PackageManagerBackend, PackageManagerKind};

use crate::cache_storage::CacheStorage;
use monopsony_db::Database;
use crate::security::RateLimiter;

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
        }
    }

    /// Find a backend that matches the given URL
    pub fn find_backend_for_url(&self, url: &str) -> Option<(PackageManagerKind, &dyn PackageManagerBackend)> {
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
        self.url_to_cache_key(url).and_then(|key| {
            self.storage.base_dir().map(|base| base.join(&key))
        })
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
                if let Err(e) = db.record_cache_hit(url, now as i64).await {
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
pub async fn handle_proxy_request(
    State((state, rate_limiter)): State<(Arc<ProxyState>, Option<Arc<crate::security::RateLimiter>>)>,
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

    // Get the full URL from the request
    let raw_url = if uri.scheme().is_some() {
        uri.to_string()
    } else {
        // For proxy requests, the URI should be absolute
        format!("http://{}", uri)
    };

    // Normalize scheme to HTTPS for package URLs.
    //
    // Forward-proxy semantics deliver absolute-URI request lines, and package
    // managers configured via XferCommand/proxy often send http:// even though
    // upstreams serve over HTTPS. The prefetch module stores URLs as https://
    // (from upstream_packages.download_url). Without normalization, a request
    // for http://X never matches a prefetched https://X entry in the DB,
    // causing duplicate fetches and cache misses.
    let url = normalize_url_scheme(&raw_url);

    // SSRF prevention: validate URL host doesn't resolve to private IP
    if let Ok(parsed) = url::Url::parse(&url) {
        if let Err(e) = crate::security::validate_url_host(&parsed) {
            warn!("SSRF blocked: {} — {}", url, e);
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("Request blocked: destination resolves to private network"))
                .unwrap_or_else(|_| {
                    Response::new(Body::from("Request blocked: destination resolves to private network"))
                });
        }
    }

    // Check if this URL matches a package manager pattern
    let is_package = state.find_backend_for_url(&url).is_some();

    let response = if is_package {
        debug!("Package URL detected: {}", url);
        handle_cached_request(&state, request, &url).await
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
    state: &ProxyState,
    request: Request<Body>,
    url: &str,
) -> Response<Body> {
    let cache_key = match state.url_to_cache_key(url) {
        Some(key) => key,
        None => {
            warn!("Could not determine cache key for URL: {}", url);
            return proxy_passthrough(state, request).await;
        }
    };

    let cache_path = state.storage.base_dir()
        .map(|base| base.join(&cache_key));

    // Check DB first for authoritative cache state
    let db_entry = if let Some(db) = &state.db {
        match db.get_cache_entry(url).await {
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

    // Validate cache path doesn't escape cache directory (path traversal prevention, filesystem only)
    let exists = state.storage.exists(&cache_key).await.unwrap_or(false);
    let path_valid = if let Some(ref path) = cache_path {
        db_cached && exists && crate::security::validate_cache_path(&path.to_string_lossy(), &state.cache_dir)
    } else {
        // S3 backend: no path traversal concern
        db_cached && exists
    };
    
    // Serve from storage if cached
    if path_valid {
        debug!("Cache hit for: {}", url);
        monopsony_common::metrics::cache_request_hit();
        
        state.update_access(&cache_key, url).await;

        // Load metadata for content type
        let meta = state.load_meta(&cache_key).await;
        let content_type = meta
            .as_ref()
            .and_then(|m| m.content_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        // Read cached file via storage backend
        match state.storage.read(&cache_key).await {
            Ok(data) => {
                // Tamper detection: verify SHA256 if we have a stored hash
                if let Some(ref entry) = db_entry {
                    if let Some(ref stored_hash) = entry.sha256 {
                        use sha2::{Sha256, Digest};
                        let mut hasher = Sha256::new();
                        hasher.update(&data);
                        let actual_hash = format!("{:x}", hasher.finalize());
                        if actual_hash != *stored_hash {
                            warn!(
                                "Tamper detected for {}: stored hash {} != actual {}",
                                url, stored_hash, actual_hash
                            );
                            monopsony_common::metrics::cache_request_miss();
                            return Response::builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .header("X-Tamper-Detected", "true")
                                .body(Body::from("Cached content integrity check failed"))
                                .unwrap_or_else(|_| {
                                    Response::new(Body::from("Integrity check failed"))
                                });
                        }
                    }
                }

                let mut builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &content_type)
                    .header(header::CONTENT_LENGTH, data.len());

                // Add cache headers
                if let Some(m) = &meta {
                    if let Some(etag) = &m.etag {
                        builder = builder.header(header::ETAG, etag);
                    }
                    if let Some(lm) = &m.last_modified {
                        builder = builder.header(header::LAST_MODIFIED, lm);
                    }
                    builder = builder.header("X-Cache", "HIT");
                }

                // Scan policy enforcement: query DB for scan status on HIT
                if let Some(db) = &state.db {
                    if let Ok(Some(scan_row)) =
                        db.get_latest_scan_result(&cache_key).await
                    {
                        builder =
                            builder.header("X-Scan-Status", &scan_row.scan_status);

                        if let Some(ref scan_cfg) = state.scan_config {
                            let yara_rules: Vec<String> = scan_row
                                .yara_matches
                                .as_ref()
                                .and_then(|v| {
                                    v.as_array().map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| {
                                                v.get("rule")
                                                    .and_then(|r| r.as_str())
                                                    .map(String::from)
                                            })
                                            .collect()
                                    })
                                })
                                .unwrap_or_default();

                            let scan_info =
                                crate::scan_policy::ScanInfo {
                                    status: scan_row.scan_status.clone(),
                                    clamav_result: scan_row.clamav_result,
                                    yara_rules,
                                    max_entropy: scan_row.max_entropy,
                                    cves: vec![],
                                    cves_in_installed: vec![],
                                };

                            let client_ctx =
                                crate::scan_policy::ClientContext {
                                    installed_version: None,
                                    force: false,
                                };

                            let decision = crate::scan_policy::evaluate_policy(
                                &scan_info,
                                &client_ctx,
                                scan_cfg,
                            );

                            match &decision {
                                crate::scan_policy::PolicyDecision::Block(
                                    reason,
                                ) => {
                                    monopsony_common::metrics::scan_policy_decision("block");
                                    return builder
                                        .status(StatusCode::FORBIDDEN)
                                        .header(
                                            "X-Scan-Block-Reason",
                                            format!("{:?}", reason),
                                        )
                                        .body(Body::from(format!(
                                            "Package blocked by scan policy: {:?}",
                                            reason
                                        )))
                                        .unwrap_or_else(|_| {
                                            Response::new(Body::from(
                                                "Package blocked by scan policy",
                                            ))
                                        });
                                }
                                crate::scan_policy::PolicyDecision::Warn(
                                    warning,
                                ) => {
                                    monopsony_common::metrics::scan_policy_decision("warn");
                                    builder = builder.header(
                                        "X-Scan-Warning",
                                        format!("{:?}", warning),
                                    );
                                }
                                crate::scan_policy::PolicyDecision::Allow => {}
                            }
                        }
                    }
                }

                monopsony_common::metrics::cache_bytes_served(data.len() as u64);
                return builder.body(Body::from(data)).unwrap_or_else(|_| {
                    Response::new(Body::from("Failed to build response"))
                });
            }
            Err(e) => {
                warn!("Failed to read cached file {}: {}", cache_key, e);
            }
        }
    }

    debug!("Cache miss for: {}", url);
    monopsony_common::metrics::cache_request_miss();

    // Fetch from upstream
    let upstream_response = match fetch_upstream(state, request, url).await {
        Ok(response) => response,
        Err(e) => {
            error!("Upstream request failed for {}: {}", url, e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Upstream request failed"))
                .unwrap_or_else(|_| {
                    Response::new(Body::from("Upstream request failed"))
                });
        }
    };

    // Only cache successful responses
    if !upstream_response.status().is_success() {
        debug!("Upstream returned non-success status, not caching");
        return upstream_response;
    }

    // Extract response headers for metadata
    let headers = upstream_response.headers();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let etag = headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = headers
        .get(header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Collect response body
    let (parts, body) = upstream_response.into_parts();
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read upstream response body: {}", e);
            return Response::from_parts(parts, Body::from("Failed to read upstream response"));
        }
    };

    // Save to cache via storage backend
    match state.storage.write(&cache_key, &bytes).await {
        Ok(_meta) => {
            info!("Cached: {} -> {}", url, cache_key);
            monopsony_common::metrics::cache_bytes_cached(bytes.len() as u64);

            // Save metadata
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let entry = CacheEntry {
                url: url.to_string(),
                cache_path: cache_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| cache_key.clone()),
                content_type,
                content_length,
                etag,
                last_modified,
                cached_at: now,
                last_accessed: now,
                hit_count: 1,
            };

            if let Err(e) = state.save_meta(&cache_key, &entry).await {
                warn!("Failed to save cache metadata: {}", e);
            }

            if let Some(db) = &state.db {
                if let Err(e) = db
                    .record_cache_miss(&monopsony_db::db::CacheMissConfig {
                        url: url.to_string(),
                        cache_path: entry.cache_path.clone(),
                        content_type: entry.content_type.clone(),
                        content_length: entry.content_length as i64,
                        etag: entry.etag.clone(),
                        last_modified: entry.last_modified.clone(),
                        cached_at: entry.cached_at as i64,
                    })
                    .await
                {
                    trace!("Failed to record cache miss in DB for {}: {}", url, e);
                }
            }

            // Fire-and-forget malware scan dispatch
            if let Some(ref scan_cfg) = state.scan_config {
                if scan_cfg.enabled {
                    let scan_bytes = bytes.clone();
                    let scan_path = cache_path
                        .as_ref()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| state.cache_dir.join(&cache_key));
                    let scan_config = scan_cfg.clone();
                    let scan_db = state.db.clone();
                    let scan_storage = state.storage.clone();
                    let scan_cache_key = cache_key.clone();
                    let scan_url = url.to_owned();
                    let retention_days = state.node_config.as_ref()
                        .and_then(|n| n.cache.s3.as_ref())
                        .map(|s3| s3.retention_days)
                        .unwrap_or(0);

                    tokio::spawn(async move {
                        let (result, record) = crate::scan::run_scan(
                            &scan_bytes,
                            &scan_path,
                            None, // package_name extracted from URL in future sprint
                            None, // package_version
                            &scan_config,
                        )
                        .await;

                        if let Some(db) = &scan_db {
                            if let Err(e) = db.record_scan_result(&record).await {
                                tracing::warn!("Failed to record scan result: {}", e);
                            }
                            // Compute and store SHA256 for tamper detection
                            use sha2::{Sha256, Digest};
                            let mut hasher = Sha256::new();
                            hasher.update(&scan_bytes);
                            let hash = format!("{:x}", hasher.finalize());
                            if let Err(e) = db.update_cache_sha256(&scan_url, &hash).await {
                                tracing::warn!("Failed to store SHA256 for {}: {}", scan_url, e);
                            }
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
                                if let Err(e) = scan_storage.set_retention(&scan_cache_key, until).await {
                                    tracing::warn!(
                                        "Failed to set Object Lock retention on {}: {}",
                                        scan_cache_key, e
                                    );
                                } else {
                                    tracing::debug!(
                                        "Object Lock set for {} days on {}",
                                        retention_days, scan_cache_key
                                    );
                                }
                            }
                        }
                    });
                }
            }
        }
        Err(e) => {
            warn!("Failed to write cache file {}: {}", cache_key, e);
        }
    }

    // Build response with cache header
    let mut response = Response::from_parts(parts, Body::from(bytes));
    response.headers_mut().insert("X-Cache", "MISS".parse().unwrap());
    if let Some(ref scan_cfg) = state.scan_config {
        if scan_cfg.enabled {
            response.headers_mut().insert(
                "X-Scan-Status",
                "pending".parse().unwrap(),
            );
        }
    }
    response
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
                .unwrap_or_else(|_| {
                    Response::new(Body::from("Upstream request failed"))
                })
        }
    }
}

/// Fetch a request from upstream
async fn fetch_upstream(
    state: &ProxyState,
    request: Request<Body>,
    url: &str,
) -> Result<Response<Body>, anyhow::Error> {
    let mut upstream_request = Request::builder()
        .method(request.method().clone())
        .uri(url);

    // Copy relevant headers
    for (name, value) in request.headers() {
        // Skip hop-by-hop headers
        if matches!(
            name.as_str(),
            "host" | "connection" | "keep-alive" | "proxy-authenticate" 
            | "proxy-authorization" | "te" | "trailers" | "transfer-encoding" | "upgrade"
        ) {
            continue;
        }
        upstream_request = upstream_request.header(name, value);
    }

    let upstream_request = upstream_request.body(request.into_body())?;

    debug!("Fetching from upstream: {}", url);
    let response = state.client.request(upstream_request).await?;

    // Convert response body with size limit
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await?.to_bytes();

    // Enforce max response body size
    if bytes.len() as u64 > crate::security::MAX_RESPONSE_BODY_SIZE {
        warn!(
            "Response body too large: {} bytes (max: {})",
            bytes.len(),
            crate::security::MAX_RESPONSE_BODY_SIZE
        );
        return Err(anyhow::anyhow!(
            "Response body exceeds maximum allowed size ({} bytes)",
            crate::security::MAX_RESPONSE_BODY_SIZE
        ));
    }

    Ok(Response::from_parts(parts, Body::from(bytes)))
}

/// Start the HTTP proxy server
pub async fn run_proxy(
    addr: SocketAddr,
    storage: Arc<dyn CacheStorage>,
    db: Option<Arc<Database>>,
    auth_mode: monopsony_common::config::AuthMode,
    scan_config: Option<monopsony_common::config::ScanConfig>,
    node_config: Option<monopsony_common::config::ServerConfig>,
) -> anyhow::Result<()> {
    storage.init_dirs().await?;

    let mut state = Arc::new(ProxyState::new(storage, db.clone()));
    // Wire scan config into proxy state
    if let Some(ref cfg) = scan_config {
        Arc::get_mut(&mut state)
            .expect("state should be uniquely owned at this point")
            .scan_config = Some(cfg.clone());
    }
    // Wire node config (for Object Lock retention)
    if let Some(ref cfg) = node_config {
        Arc::get_mut(&mut state)
            .expect("state should be uniquely owned at this point")
            .node_config = Some(cfg.clone());
    }
    
    // Create rate limiter and spawn cleanup task
    let limiter = crate::security::create_default_rate_limiter();
    let limiter_clone = limiter.clone();
    tokio::spawn(crate::security::run_rate_limiter_cleanup(limiter_clone, 120));

    // Create auth middleware state
    let auth_state = Arc::new(monopsony_control::auth::AuthState {
        auth_mode,
        db,
    });

    let app = create_proxy_router(state, Some(limiter), auth_state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("HTTP proxy listening on {}", addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_storage::FilesystemStorage;

    #[test]
    fn test_url_to_cache_path() {
        // ProxyState::new() builds an HTTPS client which needs the ring CryptoProvider
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = std::env::temp_dir().join("monopsony-test-cache");
        let storage = Arc::new(FilesystemStorage::new(temp_dir));
        let state = ProxyState::new(storage, None);

        // Test with a known apt URL pattern
        let url = "http://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-image_5.15.0-1_amd64.deb";
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
            if entry.file_type().await.map(|ft| ft.is_dir()).unwrap_or(false) {
                dir_entries.push(entry.path());
            }
        }
    }

    // Now scan each subdirectory (backend dirs)
    for dir in &dir_entries {
        let backend_name = dir.file_name()
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

    let oldest_entry = all_entries.iter()
        .min_by_key(|e| e.cached_at)
        .cloned();
    let newest_entry = all_entries.iter()
        .max_by_key(|e| e.cached_at)
        .cloned();

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