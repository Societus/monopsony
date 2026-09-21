//! Language artifact TTL metadata revalidation (Sprint 23 Phase A).
//!
//! Cached tarballs/registry metadata under the `lang_artifact` class get a
//! TTL: entries older than `metadata_ttl_secs` are revalidated against the
//! origin (HEAD request with If-None-Match/If-Modified-Since) before the
//! cached copy is trusted. On revalidation failure or network error the
//! plan-specified behavior is **serve-stale**: stale entries stay servable,
//! an `X-Monopsony-Stale: 1` header marks them, and revalidation state is
//! preserved so the next request retries.

use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, error, info, warn};

use crate::proxy::{CacheEntry, ProxyState};

/// Result of a TTL freshness check for a cached entry.
pub enum Freshness {
    /// Entry within TTL — serve without origin contact.
    Fresh,
    /// Entry past TTL; origin confirmed unchanged (same validators) — the
    /// TTL bookkeeping in `meta` was refreshed; persist and serve.
    Revalidated,
    /// Entry past TTL and origin indicates modification — must NOT serve
    /// from cache (caller falls back to the miss path).
    Expired,
    /// Entry past TTL but revalidation could not complete (network error,
    /// non-2xx/304). Per plan: serve stale, mark with X-Monopsony-Stale,
    /// preserve revalidation state (cached_at untouched → next req retries).
    ServeStale,
}

enum RevalidateOutcome {
    Unchanged,
    Changed,
    Unknown,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Classify an entry's freshness and refresh TTL bookkeeping on revalidation.
///
/// `meta` is the sidecar metadata (`CacheEntry`) for the cache key; the
/// caller has already loaded it. On `Revalidated`, `meta.cached_at` is
/// updated in place so the caller can persist it via `save_meta`.
pub async fn check_freshness(state: &ProxyState, meta: &mut CacheEntry) -> Freshness {
    let cfg = &state.language_artifacts;
    if !cfg.enabled || cfg.metadata_ttl_secs == 0 {
        return Freshness::Fresh;
    }
    if meta.cache_class != "lang_artifact" {
        return Freshness::Fresh;
    }

    let ttl = cfg.metadata_ttl_secs as u64;
    let age = now_secs().saturating_sub(meta.cached_at);
    if age < ttl {
        return Freshness::Fresh;
    }

    // Past TTL: revalidate with the origin via HEAD + validators.
    match revalidate(meta).await {
        RevalidateOutcome::Unchanged => {
            debug!(
                "Revalidated lang_artifact {} (age {}s >= TTL {}s): unchanged",
                meta.url, age, ttl
            );
            meta.cached_at = now_secs();
            Freshness::Revalidated
        }
        RevalidateOutcome::Changed => {
            info!("lang_artifact {} origin reports modification", meta.url);
            Freshness::Expired
        }
        RevalidateOutcome::Unknown => {
            warn!(
                "lang_artifact {} revalidation failed (serve-stale), preserving revalidation state",
                meta.url
            );
            Freshness::ServeStale
        }
    }
}

/// Verify-on-serve (Phase A): for `lang_artifact` entries, hash the cached
/// blob before serving a HIT. On mismatch: quarantine via tamper_alerts
/// (HIT path refuses to serve quarantined URLs) and report failure.
pub async fn verify_on_serve(
    state: &ProxyState,
    cache_key: &str,
    url: &str,
    entry: &CacheEntry,
) -> bool {
    if entry.cache_class != "lang_artifact" {
        return true;
    }
    let expected = match state.storage.get_checksum(cache_key).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            debug!("verify-on-serve: no stored checksum for {}, skipping", url);
            return true;
        }
        Err(e) => {
            warn!("verify-on-serve: checksum read failed for {}: {} (serve)", url, e);
            return true;
        }
    };
    match state.storage.read(cache_key).await {
        Ok(data) => {
            let digest = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&data);
                hex::encode(h.finalize())
            };
            if digest == expected {
                true
            } else {
                error!(
                    "VERIFY-FAIL {}: cached sha256 {} != stored {} — quarantining",
                    url, digest, expected
                );
                if let Some(db) = &state.db {
                    let alert = monopsony_db::db::TamperAlertInsert {
                        url: url.to_string(),
                        expected_sha256: expected.clone(),
                        actual_sha256: digest,
                        detail: Some("sprint23 verify-on-serve mismatch".to_string()),
                        client_id: None,
                        observed_at: now_secs() as i64,
                    };
                    if let Err(e) = db.record_tamper_alert(&alert).await {
                        error!("verify-on-serve: tamper alert insert failed: {}", e);
                    }
                }
                false
            }
        }
        Err(e) => {
            warn!("verify-on-serve: blob read failed for {}: {} (serve)", url, e);
            true
        }
    }
}

/// Origin HEAD with conditional validators. `Unknown` on any failure —
/// never let a broken origin turn into a cache poisoning or an outage.
async fn revalidate(meta: &CacheEntry) -> RevalidateOutcome {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return RevalidateOutcome::Unknown,
    };

    let mut req = client.head(&meta.url);
    if let Some(etag) = meta.etag.as_deref() {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(lm) = meta.last_modified.as_deref() {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_MODIFIED {
                RevalidateOutcome::Unchanged
            } else if status.is_success() {
                // Origin ignored validators or reports a newer version. A
                // changed ETag/Last-Modified = Changed; same validators =
                // Unchanged.
                let origin_etag = resp
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let origin_lm = resp
                    .headers()
                    .get(reqwest::header::LAST_MODIFIED)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if origin_etag.is_some() && origin_etag != meta.etag {
                    RevalidateOutcome::Changed
                } else if origin_lm.is_some() && origin_lm != meta.last_modified {
                    RevalidateOutcome::Changed
                } else {
                    RevalidateOutcome::Unchanged
                }
            } else {
                RevalidateOutcome::Unknown
            }
        }
        Err(_) => RevalidateOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store_backend::ObjectStoreBackend;
    use crate::proxy::{CacheEntry, ProxyState};
    use std::sync::Arc;

    fn test_entry(cache_class: &str, cached_at: u64) -> CacheEntry {
        CacheEntry {
            url: "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz".to_string(),
            cache_path: "npm/left-pad/1.3.0/blob".to_string(),
            content_type: None,
            content_length: 4,
            etag: None,
            last_modified: None,
            cached_at,
            last_accessed: cached_at,
            hit_count: 1,
            cache_class: cache_class.to_string(),
        }
    }

    fn test_state() -> (ProxyState, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(
            ObjectStoreBackend::local(temp.path().to_path_buf(), 0)
                .expect("local backend constructs"),
        );
        (ProxyState::new(storage, None), temp)
    }

    /// Pre-Sprint-23 sidecar JSON (no cache_class) must deserialize with the
    /// serde default — backward compat gate for the migration.
    #[test]
    fn test_sidecar_backward_compat_missing_cache_class() {
        let legacy = r#"{
            "url": "https://example.com/pkg.tar.gz",
            "cache_path": "other/example.com/pkg.tar.gz",
            "content_type": null,
            "content_length": 10,
            "etag": null,
            "last_modified": null,
            "cached_at": 1700000000,
            "last_accessed": 1700000000,
            "hit_count": 0
        }"#;
        let entry: CacheEntry = serde_json::from_str(legacy).expect("legacy sidecar parses");
        assert_eq!(entry.cache_class, "distro");
    }

    /// Disabled config or distro class → Fresh regardless of age.
    #[tokio::test]
    async fn test_freshness_disabled_config_and_distro_class() {
        let (state, _t) = test_state();
        // Distro class, ancient timestamp: never TTL-gated.
        let mut entry = test_entry("distro", 1);
        assert!(matches!(
            check_freshness(&state, &mut entry).await,
            Freshness::Fresh
        ));

        // lang_artifact but TTL=0 (disabled) → Fresh.
        let mut state = state;
        state.language_artifacts.metadata_ttl_secs = 0;
        let mut entry = test_entry("lang_artifact", 1);
        assert!(matches!(
            check_freshness(&state, &mut entry).await,
            Freshness::Fresh
        ));
    }

    /// Within TTL → Fresh without origin contact.
    #[tokio::test]
    async fn test_freshness_within_ttl_is_fresh() {
        let (state, _t) = test_state();
        let mut entry = test_entry("lang_artifact", now_secs() - 10);
        assert!(matches!(
            check_freshness(&state, &mut entry).await,
            Freshness::Fresh
        ));
    }

    /// Past TTL with unreachable origin → ServeStale (serve-stale semantics),
    /// and cached_at is NOT refreshed (revalidation state preserved).
    #[tokio::test]
    async fn test_freshness_past_ttl_origin_unreachable_serves_stale() {
        let (state, _t) = test_state();
        let mut entry = test_entry("lang_artifact", 1);
        entry.url = "http://127.0.0.1:1/unreachable.tgz".to_string();
        let original_cached_at = entry.cached_at;
        match check_freshness(&state, &mut entry).await {
            Freshness::ServeStale => {}
            other => panic!("expected ServeStale, got {:?}", other_status(&other)),
        }
        assert_eq!(
            entry.cached_at, original_cached_at,
            "serve-stale must preserve revalidation state"
        );
    }

    fn other_status(f: &Freshness) -> &'static str {
        match f {
            Freshness::Fresh => "Fresh",
            Freshness::Revalidated => "Revalidated",
            Freshness::Expired => "Expired",
            Freshness::ServeStale => "ServeStale",
        }
    }

    /// Verify-on-serve: distro class skips hashing entirely (no checksum
    /// present either) — pass-through.
    #[tokio::test]
    async fn test_verify_on_serve_skips_distro() {
        let (state, _t) = test_state();
        let entry = test_entry("distro", now_secs());
        assert!(verify_on_serve(&state, "other/x/y", "https://x/y", &entry).await);
    }

    /// Verify-on-serve pass: correct stored checksum + intact blob → serve.
    #[tokio::test]
    async fn test_verify_on_serve_passes_intact_blob() {
        let (state, _t) = test_state();
        let data = b"artifact-bytes";
        let key = "npm/left-pad/1.3.0/blob";
        let meta = state.storage.write(key, data).await.expect("write");
        let entry = test_entry("lang_artifact", now_secs());
        assert!(
            verify_on_serve(&state, key, &entry.url, &entry).await,
            "intact blob must pass verify-on-serve"
        );
        drop(meta);
    }

    /// Verify-on-serve quarantine: corrupted blob → refuse + tamper alert
    /// row (db=None here, so only the refusal is asserted).
    #[tokio::test]
    async fn test_verify_on_serve_refuses_corrupted_blob() {
        let (state, _t) = test_state();
        let data = b"artifact-bytes";
        let key = "npm/left-pad/1.3.0/blob";
        let meta = state.storage.write(key, data).await.expect("write");
        let entry = test_entry("lang_artifact", now_secs());

        // Corrupt the stored blob behind the checksum's back. Local backend
        // layout: blob at <root>/<key>, sidecar at <root>/.meta/<key>.
        let blob_path = _t.path().join(key);
        let blob_path = if blob_path.exists() {
            blob_path
        } else {
            // Fallback: first non-sidecar file for this key.
            let mut found = None;
            for entry_path in walkdir_walk(_t.path()) {
                let is_meta = entry_path
                    .components()
                    .any(|c| c.as_os_str() == ".meta");
                if !is_meta {
                    found = Some(entry_path);
                    break;
                }
            }
            found.expect("blob file on disk")
        };
        std::fs::write(&blob_path, b"CORRUPTED-BYTES!").expect("corrupt");

        assert!(
            !verify_on_serve(&state, key, &entry.url, &entry).await,
            "corrupted blob must fail verify-on-serve"
        );
        drop(meta);
    }

    fn walkdir_walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        out
    }
}
