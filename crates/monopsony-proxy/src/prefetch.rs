//! Pre-fetch scheduler for proactive package caching
//!
//! Periodically fetches packages from upstream based on client inventory reports.
//! Uses the package manager URL patterns to identify which upstream URLs need caching,
//! prioritized by how many clients need each package.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use sqlx::Row;
use tracing::{debug, error, info, trace, warn};

use crate::cache_storage::CacheStorage;
use monopsony_db::Database;

/// Boxed body type for hyper client requests
type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

fn empty_body() -> BoxBody {
    http_body_util::combinators::BoxBody::new(
        http_body_util::Empty::<Bytes>::new().map_err(|_| unreachable!()),
    )
}

/// Configuration for the pre-fetch scheduler
pub struct PrefetchConfig {
    /// Cache storage backend
    pub storage: Arc<dyn CacheStorage>,
    /// How often to run pre-fetch cycles (seconds)
    pub interval_secs: u64,
    /// Maximum concurrent fetches
    pub max_concurrent: usize,
    /// Per-request timeout (seconds)
    pub request_timeout_secs: u64,
    /// Bandwidth limit in bytes/sec (0 = unlimited)
    pub bandwidth_limit_bps: u64,
}

#[cfg(test)]
impl Default for PrefetchConfig {
    fn default() -> Self {
        use crate::object_store_backend::ObjectStoreBackend;
        Self {
            storage: Arc::new(ObjectStoreBackend::in_memory("memory")),
            interval_secs: 3600,
            max_concurrent: 10,
            request_timeout_secs: 300,
            bandwidth_limit_bps: 0,
        }
    }
}

/// A URL that needs pre-fetching, with priority info
struct PrefetchCandidate {
    url: String,
    #[allow(dead_code)] // For logging/debugging prefetch source
    manager: String,
    /// Number of clients that need this package (higher = more priority)
    client_count: i64,
}

/// Result of a single prefetch fetch
#[allow(dead_code)] // For logging/debugging prefetch results
struct PrefetchResult {
    #[allow(dead_code)]
    url: String,
    size: u64,
}

/// Start the pre-fetch background task
pub async fn start_prefetch_task(db: Arc<Database>, config: PrefetchConfig) -> anyhow::Result<()> {
    info!(
        "Pre-fetch scheduler started (interval: {}s, concurrency: {}, bandwidth: {} bps)",
        config.interval_secs,
        config.max_concurrent,
        if config.bandwidth_limit_bps > 0 {
            config.bandwidth_limit_bps.to_string()
        } else {
            "unlimited".to_string()
        }
    );

    let mut interval = tokio::time::interval(Duration::from_secs(config.interval_secs));

    // First cycle runs immediately
    if let Err(e) = run_prefetch_cycle(&db, &config).await {
        error!("Pre-fetch cycle failed: {}", e);
    }

    loop {
        interval.tick().await;
        if let Err(e) = run_prefetch_cycle(&db, &config).await {
            error!("Pre-fetch cycle failed: {}", e);
        }
    }
}

/// Run a single pre-fetch cycle
async fn run_prefetch_cycle(db: &Database, config: &PrefetchConfig) -> anyhow::Result<()> {
    info!("Starting pre-fetch cycle");

    // Find packages that clients have installed but aren't cached yet.
    // Prioritize by client count (most-requested first).
    let candidates = find_uncached_packages(db).await?;

    if candidates.is_empty() {
        debug!("No new packages to pre-fetch");
        return Ok(());
    }

    info!(
        "Pre-fetching {} uncached packages (sorted by client demand)",
        candidates.len()
    );

    let client: Client<hyper_rustls::HttpsConnector<HttpConnector>, BoxBody> = {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("Failed to load native root certs")
            .https_or_http()
            .enable_http2()
            .build();
        Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(config.max_concurrent)
            .build(connector)
    };

    let bandwidth_remaining = Arc::new(tokio::sync::Semaphore::new(
        if config.bandwidth_limit_bps > 0 {
            1
        } else {
            config.max_concurrent
        },
    ));

    // Fetch with bounded concurrency
    let results: Vec<PrefetchResult> = stream::iter(candidates)
        .map(|candidate| {
            let client = client.clone();
            let storage = config.storage.clone();
            let timeout_secs = config.request_timeout_secs;
            let bw_limit = config.bandwidth_limit_bps;

            let bw_sem = bandwidth_remaining.clone();

            async move {
                // SSRF check before fetching
                if let Ok(parsed) = url::Url::parse(&candidate.url) {
                    if let Err(e) = crate::security::validate_url_host(&parsed) {
                        warn!("Prefetch SSRF blocked: {} — {}", candidate.url, e);
                        return None;
                    }
                }

                // Bandwidth throttle: acquire permit, fetch, then pace release
                let _permit = bw_sem.acquire().await.ok()?;

                match prefetch_url(&client, &candidate.url, &*storage, timeout_secs).await {
                    Ok(size) => {
                        debug!(
                            "Pre-fetched: {} ({} bytes, {} clients)",
                            candidate.url, size, candidate.client_count
                        );

                        // Record in DB
                        let now = chrono::Utc::now().timestamp();
                        let cache_key = url_to_cache_key(&candidate.url);
                        let cache_path = storage
                            .base_dir()
                            .map(|base| base.join(&cache_key).to_string_lossy().to_string())
                            .unwrap_or_else(|| cache_key.clone());
                        if let Err(e) = db
                            .record_cache_miss(&monopsony_db::db::CacheMissConfig {
                                url: candidate.url.clone(),
                                cache_path,
                                content_type: None,
                                content_length: size as i64,
                                etag: None,
                                last_modified: None,
                                cached_at: now,
                            })
                            .await
                        {
                            trace!("Failed to record pre-fetch in DB: {}", e);
                        }

                        // Bandwidth throttle: sleep proportional to size
                        if bw_limit > 0 && size > 0 {
                            let expected_time = (size as f64 / bw_limit as f64).ceil() as u64;
                            if expected_time > 0 {
                                tokio::time::sleep(Duration::from_millis(
                                    expected_time.min(60) * 1000,
                                ))
                                .await;
                            }
                        }

                        Some(PrefetchResult {
                            url: candidate.url,
                            size,
                        })
                    }
                    Err(e) => {
                        warn!("Pre-fetch failed for {}: {}", candidate.url, e);
                        None
                    }
                }
            }
        })
        .buffer_unordered(config.max_concurrent)
        .filter_map(|r| async { r })
        .collect()
        .await;

    // Summarize
    let fetched = results.len() as u64;
    let bytes_cached: u64 = results.iter().map(|r| r.size).sum();

    monopsony_common::metrics::cache_bytes_cached(bytes_cached);
    monopsony_common::metrics::set_cache_entries(fetched);

    info!(
        "Pre-fetch cycle complete: {} fetched ({} MB)",
        fetched,
        bytes_cached / (1024 * 1024),
    );

    Ok(())
}

/// Find packages that clients have but aren't cached yet.
/// Joins client_packages → packages → upstream_packages → cache_entries, ordered by demand.
///
/// NOTE: The `packages` table has an empty `upstream_url` column — the actual
/// upstream download URLs live in `upstream_packages.download_url`. We join
/// client_packages → packages (by id) → upstream_packages (by name) to find
/// which HTTPS download URLs need pre-fetching.
async fn find_uncached_packages(db: &Database) -> anyhow::Result<Vec<PrefetchCandidate>> {
    let rows = sqlx::query(
        "SELECT up.download_url AS upstream_url, up.distro AS manager, COUNT(DISTINCT cp.client_id) AS client_count
         FROM upstream_packages up
         INNER JOIN packages p ON p.name = up.name
         INNER JOIN client_packages cp ON cp.package_id = p.id
         LEFT JOIN cache_entries ce ON up.download_url = ce.url
         WHERE ce.url IS NULL
           AND up.download_url IS NOT NULL
           AND up.download_url != ''
         GROUP BY up.download_url, up.distro
         ORDER BY client_count DESC
         LIMIT 500"
    )
    .fetch_all(db.pool())
    .await?;

    let candidates: Vec<PrefetchCandidate> = rows
        .iter()
        .map(|row| PrefetchCandidate {
            url: row.get("upstream_url"),
            manager: row.get("manager"),
            client_count: row.get::<i64, _>("client_count"),
        })
        .collect();

    Ok(candidates)
}

/// Fetch a single URL and cache it
async fn prefetch_url(
    client: &Client<hyper_rustls::HttpsConnector<HttpConnector>, BoxBody>,
    url: &str,
    storage: &dyn CacheStorage,
    timeout_secs: u64,
) -> Result<u64, String> {
    let cache_key = url_to_cache_key(url);

    // Skip if already cached
    if storage.exists(&cache_key).await.unwrap_or(false) {
        return Ok(0);
    }

    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| format!("Invalid URL {}: {}", url, e))?;

    let req = hyper::Request::builder()
        .uri(uri)
        .header("User-Agent", "Monopsony-Prefetch/0.1")
        .body(empty_body())
        .map_err(|e| format!("Failed to build request: {}", e))?;

    // Fetch with timeout
    let response = tokio::time::timeout(Duration::from_secs(timeout_secs), client.request(req))
        .await
        .map_err(|_| format!("Request timed out after {}s", timeout_secs))?
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    // Enforce max response body size
    let (parts, body) = response.into_parts();
    let content_length: u64 = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if content_length > crate::security::MAX_RESPONSE_BODY_SIZE {
        return Err(format!(
            "Response too large: {} bytes (max: {})",
            content_length,
            crate::security::MAX_RESPONSE_BODY_SIZE
        ));
    }

    // Collect body
    let bytes = body
        .collect()
        .await
        .map_err(|e| format!("Failed to read body: {}", e))?
        .to_bytes();

    if bytes.len() as u64 > crate::security::MAX_RESPONSE_BODY_SIZE {
        return Err(format!("Response body exceeds max: {} bytes", bytes.len()));
    }

    // Write to cache via storage backend
    storage
        .write(&cache_key, &bytes)
        .await
        .map_err(|e| format!("Failed to write cache: {}", e))?;

    // Write metadata sidecar — must match the CacheEntry struct so that
    // ProxyState::load_meta() can deserialize it for X-Cache headers.
    let now = chrono::Utc::now().timestamp();
    let cache_path = storage
        .base_dir()
        .map(|base| base.join(&cache_key).to_string_lossy().to_string())
        .unwrap_or_else(|| cache_key.clone());
    let meta = serde_json::json!({
        "url": url,
        "cache_path": cache_path,
        "content_type": parts.headers.get("content-type").and_then(|v| v.to_str().ok()),
        "content_length": bytes.len(),
        "etag": parts.headers.get("etag").and_then(|v| v.to_str().ok()),
        "last_modified": parts.headers.get("last-modified").and_then(|v| v.to_str().ok()),
        "cached_at": now,
        "last_accessed": now,
        "hit_count": 0u64,
    });

    let meta_bytes = serde_json::to_vec_pretty(&meta)
        .map_err(|e| format!("Failed to serialize metadata: {}", e))?;
    storage.save_meta_json(&cache_key, &meta_bytes).await.ok();

    Ok(bytes.len() as u64)
}

/// Convert a URL to a storage key, matching the proxy's key derivation.
///
/// Uses backend-aware path mapping (e.g., `pacman/core/os/x86_64/...`) when a
/// package manager backend matches the URL, falling back to
/// `other/{host}/{path}` for unrecognized URLs. This MUST match the key scheme
/// used by `ProxyState::url_to_cache_key` in proxy.rs, or prefetched files
/// won't be found on cache hit.
fn url_to_cache_key(url: &str) -> String {
    use monopsony_common::package_manager::get_available_backends;

    let backends = get_available_backends();
    for backend in &backends {
        for pattern in backend.upstream_url_patterns() {
            if pattern.matches(url) {
                if let Some(relative) = backend.url_to_cache_path(url) {
                    return relative;
                }
            }
        }
    }

    // Generic fallback (same logic as ProxyState::url_to_cache_key)
    let parsed =
        url::Url::parse(url).unwrap_or_else(|_| url::Url::parse("http://invalid").unwrap());
    let host = parsed.host_str().unwrap_or("unknown");
    let path = parsed.path().trim_start_matches('/');
    format!("other/{}/{}", host, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefetch_config_default() {
        let config = PrefetchConfig::default();
        assert_eq!(config.interval_secs, 3600);
        assert_eq!(config.max_concurrent, 10);
        assert_eq!(config.request_timeout_secs, 300);
        assert_eq!(config.bandwidth_limit_bps, 0);
    }

    #[test]
    fn test_url_to_cache_key() {
        // pacman backend matches archlinux.org → key starts with "pacman/"
        let key = url_to_cache_key("https://repo.archlinux.org/os/x86_64/linux-6.1.pkg.tar.zst");
        assert!(key.starts_with("pacman/"));
        assert!(key.contains("linux-6.1.pkg.tar.zst"));
    }

    #[test]
    fn test_url_to_cache_key_complex() {
        // No backend matches archive.ubuntu.com on Arch → falls back to other/{host}/{path}
        let key = url_to_cache_key(
            "http://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-image_5.15.0-1_amd64.deb",
        );
        assert!(key.starts_with("other/archive.ubuntu.com/"));
        assert!(key.contains("linux-image_5.15.0-1_amd64.deb"));
    }
}
