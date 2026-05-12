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
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, error, info, trace, warn};

use monopsony_common::{PackageManagerBackend, PackageManagerKind};

use crate::db::Database;
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
    pub client: Client<HttpConnector, Body>,
    pub cache_dir: PathBuf,
    pub meta_dir: PathBuf,
    pub backends: Vec<Box<dyn PackageManagerBackend>>,
    pub db: Option<Arc<Database>>,
}

impl ProxyState {
    pub fn new(cache_dir: PathBuf, db: Option<Arc<Database>>) -> Self {
        let client = Client::builder(TokioExecutor::new()).build_http();

        let meta_dir = cache_dir.join("meta");
        let backends = monopsony_common::package_manager::get_available_backends();

        Self {
            client,
            cache_dir,
            meta_dir,
            backends,
            db,
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

    /// Convert a URL to a cache file path
    pub fn url_to_cache_path(&self, url: &str) -> Option<PathBuf> {
        if let Some((_, backend)) = self.find_backend_for_url(url) {
            let relative = backend.url_to_cache_path(url)?;
            Some(self.cache_dir.join(relative))
        } else {
            // Generic cache path for unmatched URLs
            let parsed = url::Url::parse(url).ok()?;
            let host = parsed.host_str()?;
            let path = parsed.path().trim_start_matches('/');
            Some(self.cache_dir.join("other").join(host).join(path))
        }
    }

    /// Get the metadata path for a cache entry
    pub fn meta_path_for_cache(&self, cache_path: &Path) -> PathBuf {
        let relative = cache_path.strip_prefix(&self.cache_dir).unwrap_or(cache_path);
        let mut meta_path = self.meta_dir.join(relative);
        meta_path.set_extension("json");
        meta_path
    }

    /// Load cache metadata
    pub async fn load_meta(&self, cache_path: &Path) -> Option<CacheEntry> {
        let meta_path = self.meta_path_for_cache(cache_path);
        let content = tokio::fs::read(&meta_path).await.ok()?;
        serde_json::from_slice(&content).ok()
    }

    /// Save cache metadata
    pub async fn save_meta(&self, cache_path: &Path, entry: &CacheEntry) -> anyhow::Result<()> {
        let meta_path = self.meta_path_for_cache(cache_path);
        if let Some(parent) = meta_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let content = serde_json::to_vec_pretty(entry)?;
        tokio::fs::write(&meta_path, content).await?;
        Ok(())
    }

    /// Update last accessed time and hit count
    pub async fn update_access(&self, cache_path: &Path, url: &str) {
        if let Some(mut meta) = self.load_meta(cache_path).await {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            meta.last_accessed = now;
            meta.hit_count += 1;
            let _ = self.save_meta(cache_path, &meta).await;

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
    auth_state: Arc<crate::auth::AuthState>,
) -> axum::Router {
    axum::Router::new()
        .route("/health", axum::routing::get(health_check))
        .route("/*path", axum::routing::any(handle_proxy_request))
        .route("/", axum::routing::any(handle_proxy_request))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::auth::auth_middleware,
        ))
        .with_state((state, rate_limiter))
}

/// Health check endpoint for monitoring and installer verification
async fn health_check() -> &'static str {
    "ok"
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
    if let Some(limiter) = &rate_limiter {
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
    }

    // Only handle GET and HEAD for caching
    if method != Method::GET && method != Method::HEAD {
        return proxy_passthrough(&state, request).await;
    }

    // Get the full URL from the request
    let url = if uri.scheme().is_some() {
        uri.to_string()
    } else {
        // For proxy requests, the URI should be absolute
        format!("http://{}", uri)
    };

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

    if is_package {
        debug!("Package URL detected: {}", url);
        handle_cached_request(&state, request, &url).await
    } else {
        debug!("Non-package URL, passing through: {}", url);
        proxy_passthrough(&state, request).await
    }
}

/// Handle a request with caching
async fn handle_cached_request(
    state: &ProxyState,
    request: Request<Body>,
    url: &str,
) -> Response<Body> {
    let cache_path = match state.url_to_cache_path(url) {
        Some(path) => path,
        None => {
            warn!("Could not determine cache path for URL: {}", url);
            return proxy_passthrough(state, request).await;
        }
    };

    // Check DB first for authoritative cache state
    let db_cached = if let Some(db) = &state.db {
        match db.get_cache_entry(url).await {
            Ok(Some(_entry)) => {
                debug!("DB cache entry found for: {}", url);
                true
            }
            Ok(None) => {
                debug!("DB: no entry for: {}", url);
                false
            }
            Err(e) => {
                trace!("DB lookup failed for {}: {}", url, e);
                // Fall through to filesystem check
                false
            }
        }
    } else {
        // No DB — always check filesystem
        true
    };

    // Validate cache path doesn't escape cache directory (path traversal prevention)
    let path_valid = db_cached && cache_path.exists() && crate::security::validate_cache_path(&cache_path.to_string_lossy(), &state.cache_dir);
    
    // Serve from filesystem if cached (either DB confirms or no DB available)
    if path_valid {
        debug!("Cache hit for: {}", url);
        monopsony_common::metrics::cache_request_hit();
        
        state.update_access(&cache_path, url).await;

        // Load metadata for content type
        let meta = state.load_meta(&cache_path).await;
        let content_type = meta
            .as_ref()
            .and_then(|m| m.content_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        // Read cached file
        match tokio::fs::read(&cache_path).await {
            Ok(data) => {
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
                    // Add X-Cache header to indicate cache hit
                    builder = builder.header("X-Cache", "HIT");
                }

                monopsony_common::metrics::cache_bytes_served(data.len() as u64);
                return builder.body(Body::from(data)).unwrap_or_else(|_| {
                    Response::new(Body::from("Failed to build response"))
                });
            }
            Err(e) => {
                warn!("Failed to read cached file {:?}: {}", cache_path, e);
                // Fall through to fetch from upstream
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

    // Save to cache
    if let Some(parent) = cache_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!("Failed to create cache directory {:?}: {}", parent, e);
        }
    }

    if let Err(e) = tokio::fs::write(&cache_path, &bytes).await {
        warn!("Failed to write cache file {:?}: {}", cache_path, e);
    } else {
        info!("Cached: {} -> {:?}", url, cache_path);
        monopsony_common::metrics::cache_bytes_cached(bytes.len() as u64);

        // Save metadata
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let entry = CacheEntry {
            url: url.to_string(),
            cache_path: cache_path.to_string_lossy().to_string(),
            content_type,
            content_length,
            etag,
            last_modified,
            cached_at: now,
            last_accessed: now,
            hit_count: 1,
        };

        if let Err(e) = state.save_meta(&cache_path, &entry).await {
            warn!("Failed to save cache metadata: {}", e);
        }

        if let Some(db) = &state.db {
            if let Err(e) = db
                .record_cache_miss(&crate::db::CacheMissConfig {
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
    }

    // Build response with cache header
    let mut response = Response::from_parts(parts, Body::from(bytes));
    response.headers_mut().insert("X-Cache", "MISS".parse().unwrap());
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
    cache_dir: PathBuf,
    db: Option<Arc<Database>>,
    auth_mode: monopsony_common::config::AuthMode,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::create_dir_all(cache_dir.join("meta")).await?;
    tokio::fs::create_dir_all(cache_dir.join("other")).await?;

    let state = Arc::new(ProxyState::new(cache_dir, db.clone()));
    
    // Create rate limiter and spawn cleanup task
    let limiter = crate::security::create_default_rate_limiter();
    let limiter_clone = limiter.clone();
    tokio::spawn(crate::security::run_rate_limiter_cleanup(limiter_clone, 120));

    // Create auth middleware state
    let auth_state = Arc::new(crate::auth::AuthState {
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

    #[test]
    fn test_url_to_cache_path() {
        let temp_dir = std::env::temp_dir().join("monopsony-test-cache");
        let state = ProxyState::new(temp_dir, None);

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