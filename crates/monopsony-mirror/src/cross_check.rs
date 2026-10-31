//! Cross-mirror hash comparison.
//!
//! After a successful sync, sample locally-stored objects and fetch the same
//! paths from tracker-listed peer mirrors; a SHA-256 mismatch means the
//! mirrors diverged (poisoning, tampering, or lag). Outcomes are REPORTED
//! via metrics + logs — nothing is auto-quarantined (deliberate v1 policy).
//!
//! Peer endpoints come from the operator's own tracker (`/discover`), the
//! same fleet data the announce loop feeds. The private-IP SSRF guard is
//! intentionally NOT applied here: tracker-supplied endpoints are
//! operator-controlled infrastructure, and blocking RFC1918 would disable
//! the feature for every LAN deployment. Risk accepted: read-only GETs,
//! responses are hashed locally and discarded, response size is capped.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Cross-mirror comparison settings (server.toml [mirror]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossCheckConfig {
    /// Enable comparison after each successful sync. Default: true.
    pub enabled: bool,
    /// Objects sampled per sync: index files (newest first) plus recently
    /// synced files. Default: 4.
    pub sample: usize,
    /// Per-peer fetch timeout in seconds. Default: 30.
    pub timeout_secs: u64,
}

/// Outcome summary for one distro's comparison run.
#[derive(Debug, Default, Clone)]
pub struct CrossCheckOutcome {
    pub peers_seen: usize,
    pub compared: usize,
    pub matched: usize,
    pub diverged: usize,
    pub unreachable: usize,
}

/// Tracker /discover response (subset of fields we consume).
#[derive(Debug, Clone, Deserialize, Serialize)]
struct DiscoverResponse {
    mirrors: Vec<DiscoveredMirror>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DiscoveredMirror {
    mirror_id: String,
    endpoint: String,
}

/// Cap on a single peer-fetch body during comparison (an index or package
/// larger than this is skipped, not verified).
const CROSS_CHECK_MAX_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Compare sampled local objects against peer mirrors discovered via the
/// tracker. Returns per-outcome counts; every comparison emits
/// `monopsony_mirror_cross_check_total{distro,result}`.
pub async fn cross_check_distro(
    distro: &str,
    local_dir: &Path,
    tracker_urls: &[String],
    cfg: &CrossCheckConfig,
) -> CrossCheckOutcome {
    let mut outcome = CrossCheckOutcome::default();
    if !cfg.enabled {
        monopsony_common::metrics::mirror_cross_check(distro, "skipped");
        return outcome;
    }

    // 1. Discover peer mirrors for this distro.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.timeout_secs.max(1)))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            warn!(distro, "cross-check: failed to build HTTP client: {}", e);
            return outcome;
        }
    };

    let mut peers: Vec<String> = Vec::new();
    let self_id = format!("{}-", hostname());
    for tracker in tracker_urls {
        let url = format!(
            "{}/discover?distro={}",
            tracker.trim_end_matches('/'),
            distro
        );
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<DiscoverResponse>().await {
                    Ok(discovered) => {
                        for m in discovered.mirrors {
                            // Skip ourselves (mirror_id = "{server_id}-{distro}").
                            if m.mirror_id.starts_with(&self_id) {
                                continue;
                            }
                            let endpoint = m.endpoint.trim_end_matches('/').to_string();
                            if (endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                                && !peers.contains(&endpoint)
                            {
                                peers.push(endpoint);
                            }
                        }
                    }
                    Err(e) => {
                        debug!(distro, tracker, "cross-check: bad discover response: {}", e)
                    }
                }
            }
            Ok(resp) => debug!(
                distro,
                tracker,
                "cross-check: discover HTTP {}",
                resp.status()
            ),
            Err(e) => debug!(distro, tracker, "cross-check: discover failed: {}", e),
        }
    }
    outcome.peers_seen = peers.len();
    if peers.is_empty() {
        monopsony_common::metrics::mirror_cross_check(distro, "skipped");
        return outcome;
    }

    // 2. Sample local objects: newest-modified files in the mirror dir.
    let samples = sample_files(local_dir, cfg.sample);
    if samples.is_empty() {
        monopsony_common::metrics::mirror_cross_check(distro, "skipped");
        return outcome;
    }

    // 3. Fetch the same relative paths from each peer and compare hashes.
    // One peer per file is enough for divergence detection; round-robin so
    // sampling spreads across peers.
    for (idx, (rel_path, local_hash)) in samples.iter().enumerate() {
        let endpoint = &peers[idx % peers.len()];
        let url = format!("{}/{}", endpoint.trim_end_matches('/'), rel_path);
        let fetch = tokio::time::timeout(
            std::time::Duration::from_secs(cfg.timeout_secs.max(1)),
            fetch_and_hash(&client, &url),
        )
        .await;
        let peer_hash = match fetch {
            Err(_) => {
                outcome.unreachable += 1;
                monopsony_common::metrics::mirror_cross_check(distro, "unreachable");
                debug!(distro, url, "cross-check: peer fetch timed out");
                continue;
            }
            Ok(Err(e)) => {
                outcome.unreachable += 1;
                monopsony_common::metrics::mirror_cross_check(distro, "unreachable");
                debug!(distro, url, "cross-check: peer fetch failed: {}", e);
                continue;
            }
            Ok(Ok(None)) => {
                outcome.unreachable += 1;
                monopsony_common::metrics::mirror_cross_check(distro, "unreachable");
                debug!(distro, url, "cross-check: peer object missing or oversized");
                continue;
            }
            Ok(Ok(Some(h))) => h,
        };
        outcome.compared += 1;
        if &peer_hash == local_hash {
            outcome.matched += 1;
            monopsony_common::metrics::mirror_cross_check(distro, "match");
        } else {
            outcome.diverged += 1;
            monopsony_common::metrics::mirror_cross_check(distro, "divergence");
            warn!(
                distro,
                object = %rel_path,
                peer = %endpoint,
                local = %local_hash,
                peer_hash = %peer_hash,
                "cross-mirror hash MISMATCH"
            );
        }
    }

    outcome
}

/// GET a URL and return its lowercase hex sha256. None on HTTP failure,
/// oversized bodies, or read errors (mapped to unreachable upstream).
async fn fetch_and_hash(client: &reqwest::Client, url: &str) -> Result<Option<String>, String> {
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    if let Some(len) = resp.content_length() {
        if len > CROSS_CHECK_MAX_BYTES {
            return Ok(None);
        }
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() as u64 > CROSS_CHECK_MAX_BYTES {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(Some(format!("{:x}", hasher.finalize())))
}

/// Newest-first bounded walk of the mirror directory (depth ≤ 3). Local
/// hashes are STREAMED (packages run to gigabytes — never buffer whole
/// objects); objects past the size cap are skipped, not hashed.
fn sample_files(local_dir: &Path, sample: usize) -> Vec<(String, String)> {
    if sample == 0 {
        return Vec::new();
    }
    let mut candidates: Vec<(std::time::SystemTime, u64, PathBufUtf8)> = Vec::new();
    collect_files(local_dir, 0, &mut candidates);
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .take(sample)
        .filter_map(|(_mtime, size, path)| {
            if size > CROSS_CHECK_MAX_BYTES {
                return None;
            }
            let rel = path
                .strip_prefix(local_dir)
                .ok()?
                .to_string_lossy()
                .to_string();
            let file = std::fs::File::open(&path).ok()?;
            let mut hasher = Sha256::new();
            let mut reader = std::io::BufReader::new(file);
            std::io::copy(&mut reader, &mut hasher).ok()?;
            Some((rel, format!("{:x}", hasher.finalize())))
        })
        .collect()
}

type PathBufUtf8 = std::path::PathBuf;

fn collect_files(
    dir: &Path,
    depth: usize,
    out: &mut Vec<(std::time::SystemTime, u64, PathBufUtf8)>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_files(&path, depth + 1, out),
            Ok(t) if t.is_file() => {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        out.push((mtime, meta.len(), path));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Best-effort hostname, matching the announce loop's server_id format.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .map_err(|_| ())
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detects_divergence_and_match_against_local_peers() {
        // Local mirror dir with two objects.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("InRelease"), b"trusted bytes").unwrap();
        std::fs::write(dir.path().join("core.db"), b"database bytes").unwrap();

        // Fake peer: an axum server serving the same paths — one matches,
        // one diverges. Spin two servers: honest peer + poisoned peer.
        let honest = spawn_test_peer(vec![
            ("/mirror/testdistro/InRelease", "trusted bytes".to_string()),
            ("/mirror/testdistro/core.db", "database bytes".to_string()),
        ])
        .await;
        let poisoned = spawn_test_peer(vec![
            ("/mirror/testdistro/InRelease", "trusted bytes".to_string()),
            ("/mirror/testdistro/core.db", "EVIL DATABASE".to_string()),
        ])
        .await;

        // Tracker fake: returns both peers on /discover.
        let tracker = spawn_test_tracker(&[honest, poisoned]).await;

        let cfg = CrossCheckConfig {
            enabled: true,
            sample: 4,
            timeout_secs: 5,
        };
        let outcome = cross_check_distro("testdistro", dir.path(), &[tracker], &cfg).await;

        assert_eq!(outcome.peers_seen, 2);
        assert_eq!(outcome.compared, 2);
        assert_eq!(outcome.matched, 1);
        assert_eq!(outcome.diverged, 1);
    }

    #[tokio::test]
    async fn disabled_config_skips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"data").unwrap();
        let cfg = CrossCheckConfig {
            enabled: false,
            sample: 4,
            timeout_secs: 5,
        };
        let outcome = cross_check_distro(
            "testdistro",
            dir.path(),
            &["http://127.0.0.1:1".to_string()],
            &cfg,
        )
        .await;
        assert_eq!(outcome.compared, 0);
        assert_eq!(outcome.peers_seen, 0);
    }

    /// Spawn an axum server on an ephemeral port serving (path, body) pairs.
    async fn spawn_test_peer(routes: Vec<(&'static str, String)>) -> String {
        use axum::routing::get;

        let app = axum::Router::new();
        let app = routes.into_iter().fold(app, |router, (path, body)| {
            router.route(
                path,
                get(move || {
                    let body = body.clone();
                    async move { body }
                }),
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{}/mirror/testdistro", addr)
    }

    /// Spawn a fake tracker answering /discover with the given peer endpoints.
    async fn spawn_test_tracker(peer_endpoints: &[String]) -> String {
        use axum::routing::get;
        use axum::Json;

        let mirrors: Vec<DiscoveredMirror> = peer_endpoints
            .iter()
            .enumerate()
            .map(|(i, endpoint)| DiscoveredMirror {
                mirror_id: format!("peer-{}-testdistro", i),
                endpoint: endpoint.clone(),
            })
            .collect();

        let app = axum::Router::new().route(
            "/discover",
            get(move || {
                let mirrors = mirrors.clone();
                async move { Json(DiscoverResponse { mirrors }) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{}", addr)
    }
}
