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

use monopsony_common::peer_blacklist::BlacklistEntry;
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
    /// Signed blacklist entries picked up from peers' `/peers` responses
    /// (mirror-to-mirror gossip hop). The caller merges them into the
    /// manager's store through the pinned-key policy.
    pub ingested_entries: Vec<BlacklistEntry>,
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

/// Max peers queried for blacklist gossip per cross-check run (the peers
/// endpoint is already bounded; this keeps a large tracker response from
/// fanning out).
const BLACKLIST_GOSSIP_MAX_PEERS: usize = 8;

/// Cap on a single peers-response body during blacklist gossip ingestion.
/// Blacklist exports are tiny (bounded entry counts); without a cap a hostile
/// peer could force an unbounded parse before per-entry rejection. Much
/// tighter than [`CROSS_CHECK_MAX_BYTES`] because the payload shape is known.
const PEER_BLACKLIST_MAX_BYTES: u64 = 1024 * 1024; // 1 MiB

/// Read a response body with a HARD running-total cap (chunked — a hostile
/// peer cannot buffer us past `max_bytes` even if it lies about
/// Content-Length). Errors are strings per the cross-check house style.
async fn read_body_capped(
    mut resp: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(format!("body length {} exceeds cap {}", len, max_bytes));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(format!("body exceeds cap {} while streaming", max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Compare sampled local objects against peer mirrors discovered via the
/// tracker. Returns per-outcome counts; every comparison emits
/// `monopsony_mirror_cross_check_total{distro,result}`.
///
/// `banned_mirror_ids` are skipped as comparison anchors (a poisoned anchor
/// corrupts the divergence signal). Peers' `/peers` responses are additionally
/// fetched (bounded) so their signed blacklist arrays can ride back as gossip.
pub async fn cross_check_distro(
    distro: &str,
    local_dir: &Path,
    tracker_urls: &[String],
    cfg: &CrossCheckConfig,
    banned_mirror_ids: &[String],
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
                            // Skip blacklisted peers as anchors.
                            if is_banned_anchor(&m.mirror_id, banned_mirror_ids) {
                                debug!(
                                    distro,
                                    mirror_id = %m.mirror_id,
                                    "cross-check: skipping blacklisted peer anchor"
                                );
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

    // 1b. Gossip hop: pull signed blacklist arrays from peers' /peers
    // responses (bounded). Ingest failures never block the comparison.
    outcome.ingested_entries = ingest_peer_blacklists(&client, &peers).await;

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

/// Subset of the peers response consumed for gossip (the peers list itself is
/// not used here).
#[derive(Debug, Default, Deserialize)]
struct PeerBlacklistBody {
    #[serde(default)]
    blacklist: Vec<BlacklistEntry>,
}

/// Fetch each peer's `/.well-known/monopsony/peers` (bounded) and collect the
/// signed blacklist entries they advertise. Bodies are read under a hard
/// size cap ([`PEER_BLACKLIST_MAX_BYTES`]) — per-peer failures are silent at
/// debug level; gossip must never block the comparison.
async fn ingest_peer_blacklists(
    client: &reqwest::Client,
    peer_endpoints: &[String],
) -> Vec<BlacklistEntry> {
    let mut entries = Vec::new();
    for endpoint in peer_endpoints.iter().take(BLACKLIST_GOSSIP_MAX_PEERS) {
        let Some(origin) = peer_origin(endpoint) else {
            continue;
        };
        let url = format!("{}/.well-known/monopsony/peers", origin);
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match read_body_capped(
                resp,
                PEER_BLACKLIST_MAX_BYTES,
            )
            .await
            {
                Ok(bytes) => match serde_json::from_slice::<PeerBlacklistBody>(&bytes) {
                    Ok(body) => entries.extend(body.blacklist),
                    Err(e) => debug!(peer = %origin, "cross-check: bad peers body: {}", e),
                },
                Err(e) => debug!(peer = %origin, "cross-check: peers body rejected: {}", e),
            },
            Ok(resp) => debug!(peer = %origin, "cross-check: peers HTTP {}", resp.status()),
            Err(e) => debug!(peer = %origin, "cross-check: peers fetch failed: {}", e),
        }
    }
    entries
}

/// Derive the server origin (scheme://host:port) from a peer mirror endpoint
/// like `http://host:3129/<distro>`.
fn peer_origin(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))?;
    let host_port = rest.split('/').next()?;
    if host_port.is_empty() {
        return None;
    }
    let scheme = if endpoint.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    Some(format!("{}://{}", scheme, host_port))
}

/// Anchor-eligible check: mirror_id in the banned set (pure helper, unit-testable).
fn is_banned_anchor(mirror_id: &str, banned_mirror_ids: &[String]) -> bool {
    banned_mirror_ids.iter().any(|b| b.as_str() == mirror_id)
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

        // Fake peer: an axum server serving the same paths. The poisoned peer
        // diverges on EVERY object, so the round-robin pairing in
        // cross_check_distro (sample[i] -> peers[i % len]) yields exactly one
        // match + one divergence regardless of mtime/readdir sample order.
        let honest = spawn_test_peer(vec![
            ("/testdistro/InRelease", "trusted bytes".to_string()),
            ("/testdistro/core.db", "database bytes".to_string()),
        ])
        .await;
        let poisoned = spawn_test_peer(vec![
            ("/testdistro/InRelease", "EVIL INRELEASE".to_string()),
            ("/testdistro/core.db", "EVIL DATABASE".to_string()),
        ])
        .await;

        // Tracker fake: returns both peers on /discover.
        let tracker = spawn_test_tracker(&[honest, poisoned]).await;

        let cfg = CrossCheckConfig {
            enabled: true,
            sample: 4,
            timeout_secs: 5,
        };
        let outcome = cross_check_distro("testdistro", dir.path(), &[tracker], &cfg, &[]).await;

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
            &[],
        )
        .await;
        assert_eq!(outcome.compared, 0);
        assert_eq!(outcome.peers_seen, 0);
    }

    #[tokio::test]
    async fn capped_body_read_rejects_oversized_content_length() {
        // reqwest derives content_length from the body's exact size hint, so
        // an oversized BODY is the honest stand-in for an oversized
        // content-length response (the early-reject branch fires before any
        // chunk is read).
        let resp = reqwest::Response::from(
            axum::http::Response::builder()
                .body(vec![b'x'; 2048])
                .unwrap(),
        );
        assert!(
            read_body_capped(resp, 1024).await.is_err(),
            "body over the cap must reject before ingesting"
        );
    }

    #[tokio::test]
    async fn capped_body_read_rejects_oversized_stream() {
        // No exact size hint (streaming body): the running-total cap trips
        // while streaming — a peer cannot bypass the cap by omitting
        // content-length.
        use futures::stream;
        let body = reqwest::Body::wrap_stream(stream::iter(vec![
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from(vec![b'x'; 800])),
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from(vec![b'x'; 800])),
        ]));
        let resp = reqwest::Response::from(axum::http::Response::new(body));
        assert!(
            read_body_capped(resp, 1024).await.is_err(),
            "1600 streamed bytes must trip the 1024-byte running-total cap"
        );
    }

    #[tokio::test]
    async fn capped_body_read_accepts_under_cap() {
        let resp = reqwest::Response::from(axum::http::Response::new(b"{}".to_vec()));
        let bytes = read_body_capped(resp, 1024).await.unwrap();
        assert_eq!(bytes, b"{}");
    }

    #[tokio::test]
    async fn ingest_peer_blacklists_caps_hostile_body() {
        use monopsony_common::peer_blacklist::BlacklistAction;

        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let mut entry = BlacklistEntry {
            subject: "mirror:m-arch".to_string(),
            action: BlacklistAction::Ban,
            reason: "gossip".to_string(),
            originator: String::new(),
            seq: 1,
            created_at: 1_793_500_000,
            expires_at: None,
            signature: String::new(),
        };
        entry.sign(&key);
        let honest_body = serde_json::json!({ "blacklist": [entry] }).to_string();

        let honest = spawn_test_peer(vec![(
            "/.well-known/monopsony/peers",
            honest_body,
        )])
        .await;
        // Hostile peer: body just over the gossip cap.
        let oversized = "x".repeat(PEER_BLACKLIST_MAX_BYTES as usize + 64);
        let hostile = spawn_test_peer(vec![(
            "/.well-known/monopsony/peers",
            oversized,
        )])
        .await;

        let client = reqwest::Client::new();
        let entries = ingest_peer_blacklists(&client, &[honest, hostile]).await;
        assert_eq!(
            entries.len(),
            1,
            "hostile oversized body must be dropped; honest peer's entries ingested"
        );
        assert_eq!(
            entries[0].originator,
            hex::encode(key.verifying_key().as_bytes())
        );
    }

    #[test]
    fn banned_anchors_are_filtered() {
        assert!(is_banned_anchor(
            "badhost-arch",
            &["badhost-arch".to_string()]
        ));
        assert!(!is_banned_anchor(
            "goodhost-arch",
            &["badhost-arch".to_string()]
        ));
        assert!(!is_banned_anchor("goodhost-arch", &[]));
    }

    #[test]
    fn peer_origin_derivation() {
        assert_eq!(
            peer_origin("http://host:3129/archlinux").as_deref(),
            Some("http://host:3129")
        );
        assert_eq!(
            peer_origin("https://host/debian").as_deref(),
            Some("https://host")
        );
        assert_eq!(peer_origin("ftp://host/x"), None);
        assert_eq!(peer_origin("http:///path"), None);
    }

    #[test]
    fn peers_body_parses_blacklist_array_and_tolerates_absence() {
        // The full Approach A peers response shape parses; the legacy shape
        // (no blacklist key) yields an empty vec via #[serde(default)].
        let legacy = r#"{"source":"local","peers":[]}"#;
        let parsed: PeerBlacklistBody = serde_json::from_str(legacy).unwrap();
        assert!(parsed.blacklist.is_empty());

        let key = ed25519_dalek::SigningKey::from_bytes(&[6u8; 32]);
        let mut entry = BlacklistEntry {
            subject: "mirror:badhost-arch".to_string(),
            action: monopsony_common::peer_blacklist::BlacklistAction::Ban,
            reason: "poisoning".to_string(),
            originator: String::new(),
            seq: 1,
            created_at: 1_793_500_000,
            expires_at: None,
            signature: String::new(),
        };
        entry.sign(&key);
        let full = serde_json::json!({
            "source": "local",
            "peers": [],
            "blacklist": [entry]
        });
        let parsed: PeerBlacklistBody = serde_json::from_value(full).unwrap();
        assert_eq!(parsed.blacklist.len(), 1);
        assert!(parsed.blacklist[0].verify_signature().is_ok());
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
        format!("http://{}/testdistro", addr)
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
