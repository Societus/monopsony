//! Pre-fetch scheduler for proactive package caching
//!
//! Periodically fetches packages from upstream based on client inventory reports.
//! Uses the package manager URL patterns to identify which upstream URLs need caching,
//! prioritized by how many clients need each package.

use std::path::{Path, PathBuf};
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
use tracing::{debug, error, info, warn, trace};

use crate::db::Database;

/// Boxed body type for hyper client requests
type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

fn empty_body() -> BoxBody {
    http_body_util::combinators::BoxBody::new(
        http_body_util::Empty::<Bytes>::new().map_err(|_| unreachable!()),
    )
}

/// Configuration for the pre-fetch scheduler
#[derive(Debug, Clone)]
pub struct PrefetchConfig {
    /// Path to cache directory
    pub cache_dir: PathBuf,
    /// How often to run pre-fetch cycles (seconds)
    pub interval_secs: u64,
    /// Maximum concurrent fetches
    pub max_concurrent: usize,
    /// Per-request timeout (seconds)
    pub request_timeout_secs: u64,
    /// Bandwidth limit in bytes/sec (0 = unlimited)
    pub bandwidth_limit_bps: u64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from("/var/cache/monopsony"),
            interval_secs: 3600, // 1 hour
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
    #[allow(dead_code)] url: String,
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

    let client: Client<HttpConnector, BoxBody> =
        Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(config.max_concurrent)
            .build_http();

    let bandwidth_remaining = Arc::new(tokio::sync::Semaphore::new(
        if config.bandwidth_limit_bps > 0 { 1 } else { config.max_concurrent }
    ));

    // Fetch with bounded concurrency
    let results: Vec<PrefetchResult> = stream::iter(candidates)
        .map(|candidate| {
            let client = client.clone();
            let cache_dir = config.cache_dir.clone();
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

                match prefetch_url(&client, &candidate.url, &cache_dir, timeout_secs).await {
                    Ok(size) => {
                        debug!(
                            "Pre-fetched: {} ({} bytes, {} clients)",
                            candidate.url, size, candidate.client_count
                        );

                        // Record in DB
                        let now = chrono::Utc::now().timestamp();
                        let cache_path = url_to_cache_path(&candidate.url, &cache_dir);
                        if let Err(e) = db.record_cache_miss(&crate::db::CacheMissConfig {
                            url: candidate.url.clone(),
                            cache_path,
                            content_type: None,
                            content_length: size as i64,
                            etag: None,
                            last_modified: None,
                            cached_at: now,
                        }).await {
                            trace!("Failed to record pre-fetch in DB: {}", e);
                        }

                        // Bandwidth throttle: sleep proportional to size
                        if bw_limit > 0 && size > 0 {
                            let expected_time = (size as f64 / bw_limit as f64).ceil() as u64;
                            if expected_time > 0 {
                                tokio::time::sleep(Duration::from_millis(
                                    expected_time.min(60) * 1000
                                )).await;
                            }
                        }

                        Some(PrefetchResult { url: candidate.url, size })
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
/// Joins client_packages → packages → cache_entries, ordered by demand.
async fn find_uncached_packages(db: &Database) -> anyhow::Result<Vec<PrefetchCandidate>> {
    let rows = sqlx::query(
        "SELECT p.upstream_url, p.manager, COUNT(DISTINCT cp.client_id) AS client_count
         FROM packages p
         INNER JOIN client_packages cp ON p.id = cp.package_id
         LEFT JOIN cache_entries ce ON p.upstream_url = ce.url
         WHERE ce.url IS NULL
           AND p.upstream_url IS NOT NULL
           AND p.upstream_url != ''
         GROUP BY p.upstream_url, p.manager
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
    client: &Client<HttpConnector, BoxBody>,
    url: &str,
    cache_dir: &Path,
    timeout_secs: u64,
) -> Result<u64, String> {
    let cache_path = url_to_cache_path(url, cache_dir);

    // Skip if already cached on disk
    if Path::new(&cache_path).exists() {
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
    let response = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        client.request(req),
    )
    .await
    .map_err(|_| format!("Request timed out after {}s", timeout_secs))?
    .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    // Enforce max response body size
    let (parts, body) = response.into_parts();
    let content_length: u64 = parts.headers
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
        return Err(format!(
            "Response body exceeds max: {} bytes",
            bytes.len()
        ));
    }

    // Write to cache atomically
    if let Some(parent) = Path::new(&cache_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create dir: {}", e))?;
    }

    let tmp_path = {
        let mut p = cache_path.clone();
        let stem = Path::new(&p)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        p = format!("{}/{}.tmp", Path::new(&p).parent().unwrap_or(Path::new(".")).display(), stem);
        p
    };

    tokio::fs::write(&tmp_path, &bytes)
        .await
        .map_err(|e| format!("Failed to write cache: {}", e))?;

    tokio::fs::rename(&tmp_path, &cache_path)
        .await
        .map_err(|e| format!("Failed to rename tmp file: {}", e))?;

    // Write metadata sidecar
    let meta = serde_json::json!({
        "url": url,
        "cache_path": cache_path,
        "content_type": parts.headers.get("content-type").and_then(|v| v.to_str().ok()),
        "content_length": bytes.len(),
        "cached_at": chrono::Utc::now().timestamp(),
        "prefetched": true,
    });

    let meta_path = format!("{}.json", cache_path);
    if let Some(parent) = Path::new(&meta_path).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).map_err(|e| format!("Failed to serialize metadata: {}", e))?)
        .await
        .ok();

    Ok(bytes.len() as u64)
}

/// Convert a URL to a filesystem cache path
fn url_to_cache_path(url: &str, cache_dir: &Path) -> String {
    let parsed = url::Url::parse(url).unwrap_or_else(|_| url::Url::parse("http://invalid").unwrap());
    let host = parsed.host_str().unwrap_or("unknown");
    let path = parsed.path().trim_start_matches('/');
    cache_dir.join("prefetch").join(host).join(path).to_string_lossy().to_string()
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
    fn test_url_to_cache_path() {
        let cache_dir = PathBuf::from("/var/cache/monopsony");
        let path = url_to_cache_path(
            "https://repo.archlinux.org/os/x86_64/linux-6.1.pkg.tar.zst",
            &cache_dir,
        );
        assert!(path.contains("prefetch"));
        assert!(path.contains("repo.archlinux.org"));
        assert!(path.contains("linux-6.1.pkg.tar.zst"));
    }

    #[test]
    fn test_url_to_cache_path_complex() {
        let cache_dir = PathBuf::from("/tmp/cache");
        let path = url_to_cache_path(
            "http://archive.ubuntu.com/ubuntu/pool/main/l/linux/linux-image_5.15.0-1_amd64.deb",
            &cache_dir,
        );
        assert!(path.starts_with("/tmp/cache/prefetch/archive.ubuntu.com/"));
        assert!(path.contains("linux-image_5.15.0-1_amd64.deb"));
    }
}
