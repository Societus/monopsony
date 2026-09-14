//! Scoop bucket shim (Wave A slice A1)
//!
//! Serves scoop bucket JSON from `http://<server>:3129/scoop/<bucket>/...`
//! out of `<mirror_dir>/scoop/bucket/<bucket>/`. Manifest `url` fields (and
//! `url.<arch>` variants) are rewritten at SYNC time to absolute-URIs through
//! the caching proxy (`http://<proxy_base>/<original-upstream-url>`), so
//! `scoop bucket add monopsony http://...:3129/scoop/main` gives Windows
//! clients cache-backed installs with zero new serve-time machinery.
//!
//! This module is the template the winget (A2) and choco (A3) shims copy:
//! `pub fn router(state: MirrorState) -> axum::Router` (self-contained state,
//! no shared middleware — the hub nests it inside `create_mirror_router`),
//! a `pub async fn sync(...)` writing plain files under `state.mirror_dir`,
//! upstream fetches via reqwest (https), and SSRF validation through
//! `crate::url_validation::validate_url_host`.

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::mirror::MirrorState;
use crate::url_validation::validate_url_host;

/// Default base address of the caching proxy (absolute-URI requests).
pub const DEFAULT_PROXY_BASE: &str = "http://127.0.0.1:3128";

/// Storage layout root for scoop bucket files under `mirror_dir`.
fn bucket_root(mirror_dir: &std::path::Path, bucket: &str) -> std::path::PathBuf {
    mirror_dir.join("scoop").join("bucket").join(bucket)
}

/// Create the scoop bucket router.
///
/// Routes (mounted by the hub at `/scoop`):
/// - `GET /:bucket/*path` — bucket JSON files from
///   `<mirror_dir>/scoop/bucket/<bucket>/<path>` (plain static serving,
///   same pattern as `serve_mirror_file`; rewriting happened at sync time).
/// - `GET /:bucket` — bucket root manifest if present, else 404.
pub fn router(state: MirrorState) -> axum::Router {
    axum::Router::new()
        .route("/:bucket/*path", axum::routing::get(serve_bucket_file))
        .route("/:bucket", axum::routing::get(serve_bucket_root))
        .with_state(state)
}

/// GET /:bucket/*path — serve a file from the local bucket mirror.
async fn serve_bucket_file(
    State(state): State<MirrorState>,
    AxumPath((bucket, file_path)): AxumPath<(String, String)>,
) -> Response {
    serve_from_bucket(&state, &bucket, &file_path).await
}

/// GET /:bucket — serve the bucket root manifest if present.
async fn serve_bucket_root(
    State(state): State<MirrorState>,
    AxumPath(bucket): AxumPath<String>,
) -> Response {
    serve_from_bucket(&state, &bucket, "").await
}

/// Shared static-file serving with path-traversal protection.
async fn serve_from_bucket(state: &MirrorState, bucket: &str, file_path: &str) -> Response {
    // Sanitize bucket name: single path component, no traversal
    if bucket.is_empty()
        || bucket.contains('/')
        || bucket.contains('\\')
        || bucket == "."
        || bucket == ".."
        || bucket.contains("..")
    {
        return (StatusCode::BAD_REQUEST, "Invalid bucket name").into_response();
    }

    let root = bucket_root(&state.mirror_dir, bucket);
    let canonical_root = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "Bucket not found").into_response();
        }
    };

    let local_file = if file_path.is_empty() {
        root.join("bucket.json")
    } else {
        root.join(&file_path)
    };

    let canonical_file = match local_file.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    if !canonical_file.starts_with(&canonical_root) {
        return (StatusCode::FORBIDDEN, "Path traversal blocked").into_response();
    }

    if !canonical_file.is_file() {
        return (StatusCode::NOT_FOUND, "Not a file").into_response();
    }

    match tokio::fs::read(&canonical_file).await {
        Ok(data) => {
            let content_type = mime_guess::from_path(&canonical_file)
                .first_or_octet_stream()
                .to_string();
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", content_type)
                .header("Content-Length", data.len())
                .header("X-Mirror", "hit")
                .body(axum::body::Body::from(data))
                .unwrap()
        }
        Err(e) => {
            warn!("scoop shim: failed reading {}: {}", canonical_file.display(), e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Read failed").into_response()
        }
    }
}

/// Rewrite a scoop manifest JSON in place:
/// - top-level `url` (string) → `<proxy_base>/<original-url>`
/// - top-level `url.<arch>` keys (string values) → same rewrite
/// - `hash`, `hash.<arch>`, and all other fields left untouched
/// - non-string `url` values (rare multi-url arrays) left untouched (v1)
fn rewrite_manifest_urls(manifest: &mut Value, proxy_base: &str) -> usize {
    let mut rewritten = 0;
    let Some(obj) = manifest.as_object_mut() else {
        return 0;
    };
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let is_url_key = key == "url" || key.starts_with("url.");
        if !is_url_key {
            continue;
        }
        if let Some(url_str) = obj.get(&key).and_then(|v| v.as_str()) {
            if url_str.starts_with("http://") || url_str.starts_with("https://") {
                let new_url = format!("{}/{}", proxy_base.trim_end_matches('/'), url_str);
                obj.insert(key, Value::String(new_url));
                rewritten += 1;
            }
        }
    }
    rewritten
}

/// Sync a scoop bucket from an upstream GitHub-style repository.
///
/// `upstream` must be an https GitHub repo URL whose raw file listing is
/// fetchable via the GitHub REST tree API
/// (`https://api.github.com/repos/<owner>/<repo>/git/trees/HEAD?recursive=1`).
/// Each `.json` file is fetched raw, rewritten (`url`/`url.<arch>` →
/// absolute-URI through the caching proxy at `proxy_base`), and written under
/// `<mirror_dir>/scoop/bucket/<bucket>/<file>`.
///
/// The upstream host is SSRF-validated before any fetch.
pub async fn sync(
    state: &MirrorState,
    bucket: &str,
    upstream: &str,
    proxy_base: &str,
) -> anyhow::Result<()> {
    // Sanitize bucket name (same rules as serving)
    if bucket.is_empty()
        || bucket.contains('/')
        || bucket.contains('\\')
        || bucket.contains("..")
        || bucket == "."
    {
        anyhow::bail!("invalid bucket name: {:?}", bucket);
    }

    // Normalize: strip trailing slash
    let upstream = upstream.trim_end_matches('/');

    // Parse + SSRF-validate the upstream URL
    let parsed = url::Url::parse(upstream)?;
    if parsed.scheme() != "https" {
        anyhow::bail!("upstream must be https: {}", upstream);
    }
    if let Err(e) = validate_url_host(&parsed) {
        anyhow::bail!("SSRF validation failed for {}: {}", upstream, e);
    }

    // Extract owner/repo from a GitHub repo URL
    let (owner, repo) = parse_github_repo(&parsed)
        .ok_or_else(|| anyhow::anyhow!("not a GitHub repo URL: {}", upstream))?;

    let api_url = format!(
        "https://api.github.com/repos/{}/{}/git/trees/HEAD?recursive=1",
        owner, repo
    );
    let api_parsed = url::Url::parse(&api_url)?;
    if let Err(e) = validate_url_host(&api_parsed) {
        anyhow::bail!("SSRF validation failed for {}: {}", api_url, e);
    }

    let client = reqwest::Client::builder()
        .user_agent("monopsony-mirror-scoop-shim")
        .build()?;
    let tree_resp = client.get(&api_url).send().await?;
    if !tree_resp.status().is_success() {
        anyhow::bail!("GitHub tree API {}: {}", tree_resp.status(), api_url);
    }
    let tree: Value = tree_resp.json().await?;
    let truncated = tree.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false);
    if truncated {
        warn!("scoop shim sync: GitHub tree listing truncated for {}/{}", owner, repo);
    }

    let root = bucket_root(&state.mirror_dir, bucket);
    tokio::fs::create_dir_all(&root).await?;

    let raw_base = format!("https://raw.githubusercontent.com/{}/{}/HEAD/", owner, repo);
    let raw_parsed = url::Url::parse(&raw_base)?;

    let mut synced = 0usize;
    if let Some(entries) = tree.get("tree").and_then(|v| v.as_array()) {
        for entry in entries {
            let path = match entry.get("path").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => continue,
            };
            // v1: top-level .json manifest files only (common case)
            if path.contains('/')
                || !path.ends_with(".json")
                || entry.get("type").and_then(|v| v.as_str()) != Some("blob")
            {
                continue;
            }
            let file_url = format!("{}{}", raw_base, path);
            if let Err(e) = validate_url_host(&raw_parsed) {
                warn!("scoop shim: skipping {}: {}", path, e);
                continue;
            }
            let resp = client.get(&file_url).send().await?;
            if !resp.status().is_success() {
                warn!("scoop shim sync: {} {}: skipping", resp.status(), path);
                continue;
            }
            let text = resp.text().await?;
            let mut manifest: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!("scoop shim sync: {} not valid JSON ({}): skipping", path, e);
                    continue;
                }
            };
            let rewritten = rewrite_manifest_urls(&mut manifest, proxy_base);
            let out = root.join(path);
            if let Some(parent) = out.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&out, serde_json::to_vec_pretty(&manifest)?).await?;
            synced += 1;
            debug!("scoop shim sync: {} ({} url(s) rewritten)", path, rewritten);
        }
    }

    info!(
        "scoop shim sync: bucket '{}' from {}/{}: {} manifest(s) synced",
        bucket, owner, repo, synced
    );
    Ok(())
}

/// Parse `owner/repo` out of a GitHub repo URL
/// (accepts `.../<owner>/<repo>` with optional `/tree/<ref>...` suffix).
fn parse_github_repo(u: &url::Url) -> Option<(String, String)> {
    let host = u.host_str()?;
    if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") {
        return None;
    }
    let mut segs = u.path_segments()?.filter(|s| !s.is_empty());
    let owner = segs.next()?.to_string();
    let repo = segs.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt; // oneshot

    /// Build a MirrorState backed by a temp dir (no DB, no config file).
    fn test_state() -> (MirrorState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let manager = crate::mirror::MirrorManager::new(dir.path().to_path_buf(), None);
        let state = MirrorState::new(manager, dir.path().to_path_buf());
        (state, dir)
    }

    const FIXTURE: &str = r#"{
        "version": "1.2.3",
        "url": "https://example.com/downloads/myapp-1.2.3.exe",
        "url.x64": "https://example.com/downloads/myapp-1.2.3-x64.exe",
        "url.arm64": "https://example.com/downloads/myapp-1.2.3-arm64.exe",
        "hash": "sha256:abcdef0123456789",
        "hash.x64": "sha64:fedcba9876543210",
        "extract_dir": "myapp-1.2.3",
        "bin": "myapp.exe",
        "checkver": { "github": "https://github.com/example/myapp" }
    }"#;

    #[test]
    fn test_rewrite_urls_hash_untouched() {
        let mut manifest: Value = serde_json::from_str(FIXTURE).unwrap();
        let rewritten = rewrite_manifest_urls(&mut manifest, "http://192.168.1.10:3128");
        assert_eq!(rewritten, 3); // url + url.x64 + url.arm64

        let obj = manifest.as_object().unwrap();
        assert_eq!(
            obj["url"],
            "http://192.168.1.10:3128/https://example.com/downloads/myapp-1.2.3.exe"
        );
        assert_eq!(
            obj["url.x64"],
            "http://192.168.1.10:3128/https://example.com/downloads/myapp-1.2.3-x64.exe"
        );
        assert_eq!(
            obj["url.arm64"],
            "http://192.168.1.10:3128/https://example.com/downloads/myapp-1.2.3-arm64.exe"
        );

        // hash fields untouched
        assert_eq!(obj["hash"], "sha256:abcdef0123456789");
        assert_eq!(obj["hash.x64"], "sha64:fedcba9876543210");

        // non-url fields untouched
        assert_eq!(obj["version"], "1.2.3");
        assert_eq!(obj["extract_dir"], "myapp-1.2.3");
        assert_eq!(obj["bin"], "myapp.exe");
        assert_eq!(
            obj["checkver"]["github"],
            "https://github.com/example/myapp"
        );
    }

    #[test]
    fn test_rewrite_skips_non_http_and_non_string() {
        let mut manifest: Value = serde_json::from_str(r#"{
            "url": "ftp://example.com/x.exe",
            "url.x86": 42,
            "url.extra": "not-a-url",
            "myurl": "https://example.com/should-not-rewrite.exe"
        }"#)
        .unwrap();
        let rewritten = rewrite_manifest_urls(&mut manifest, "http://p:3128");
        assert_eq!(rewritten, 0);
        let obj = manifest.as_object().unwrap();
        assert_eq!(obj["url"], "ftp://example.com/x.exe");
        assert_eq!(obj["url.x86"], 42);
        assert_eq!(obj["url.extra"], "not-a-url");
        assert_eq!(obj["myurl"], "https://example.com/should-not-rewrite.exe");
    }

    #[test]
    fn test_rewrite_non_object_manifest_noop() {
        let mut manifest: Value = Value::Array(vec![Value::String("https://x/y".into())]);
        assert_eq!(rewrite_manifest_urls(&mut manifest, "http://p:3128"), 0);
    }

    #[tokio::test]
    async fn test_router_serves_bucket_file() {
        let (state, dir) = test_state();
        let bucket_dir = dir.path().join("scoop").join("bucket").join("main");
        tokio::fs::create_dir_all(&bucket_dir).await.unwrap();
        let manifest = r#"{"version":"1.0","url":"http://p:3128/https://example.com/a.exe","hash":"sha256:aa"}"#;
        tokio::fs::write(bucket_dir.join("myapp.json"), manifest)
            .await
            .unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/main/myapp.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["version"], "1.0");
        assert!(parsed["url"]
            .as_str()
            .unwrap()
            .starts_with("http://p:3128/https://"));
    }

    #[tokio::test]
    async fn test_router_bucket_root_manifest_and_404() {
        let (state, dir) = test_state();
        let bucket_dir = dir.path().join("scoop").join("bucket").join("main");
        tokio::fs::create_dir_all(&bucket_dir).await.unwrap();
        tokio::fs::write(bucket_dir.join("bucket.json"), r#"{"ok":true}"#)
            .await
            .unwrap();

        let app = router(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/main")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // missing bucket → 404
        let app2 = router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri("/nosuch")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_router_blocks_path_traversal() {
        let (state, dir) = test_state();
        let bucket_dir = dir.path().join("scoop").join("bucket").join("main");
        tokio::fs::create_dir_all(&bucket_dir).await.unwrap();
        let secret = dir.path().join("secret.txt");
        tokio::fs::write(&secret, "secret").await.unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/main/../../secret.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Dot-segments are normalized by the router/hyper; the security
        // property is that the out-of-bucket file is NEVER served (any
        // non-200, e.g. 404 after normalization or 403 from the explicit
        // traversal check).
        assert_ne!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_router_rejects_bad_bucket_name() {
        let (state, _dir) = test_state();
        let app = router(state);
        // ".." as bucket would need a literal path segment; use encoded form
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/%2e%2e/etc/passwd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_parse_github_repo() {
        let url = url::Url::parse("https://github.com/ScoopInstaller/Main").unwrap();
        assert_eq!(
            parse_github_repo(&url),
            Some(("ScoopInstaller".to_string(), "Main".to_string()))
        );
        let url = url::Url::parse("https://github.com/ScoopInstaller/Main.git").unwrap();
        assert_eq!(
            parse_github_repo(&url),
            Some(("ScoopInstaller".to_string(), "Main".to_string()))
        );
        let url =
            url::Url::parse("https://github.com/ScoopInstaller/Main/tree/master").unwrap();
        assert_eq!(
            parse_github_repo(&url),
            Some(("ScoopInstaller".to_string(), "Main".to_string()))
        );
        let url = url::Url::parse("https://example.com/foo/bar").unwrap();
        assert_eq!(parse_github_repo(&url), None);
    }

    // sync() end-to-end test skipped: requires network (GitHub API). The
    // rewrite + router + SSRF-relevant parsing paths are covered above.
    // validate_url_host("https://github.com") resolution cannot be reliably
    // intercepted hermetically without a DNS stub, per slice spec guidance.
}
