//! Personal OCI registry — Sprint 25 Slice 1: pull-back (read-only).
//!
//! Serves pushed content from CacheStorage over the OCI distribution-spec
//! end-points this slice implements: version check (end-1), blob GET/HEAD
//! (end-2), manifest GET/HEAD (end-3), tags list (end-8a). The write path
//! (upload sessions, manifest push, tagless auto-version) lands in S2/S3.
//!
//! Runs on its OWN listener (`--registry-bind`, default :3131): the `/v2`
//! route namespace collides head-on with `docker.rs` pull-through routes and
//! docker clients cannot be prefix-routed. All routes ride the same
//! `auth_middleware`/`AuthState` as the proxy router (keyed in v1).
//!
//! Plan: `docs/plans/sprint25-oci-registry/slice-1-pullback.md`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use monopsony_db::Database;
use percent_encoding::percent_decode_str;

use crate::cache_storage::CacheStorage;
use crate::proxy::ProxyState;

/// Storage-key namespace for registry content. Disjoint from the distro /
/// language-PM / `docker/v2` cache namespaces so retention and scrub treat
/// the registry as its own class (S4 wires its budget).
const REGISTRY_PREFIX: &str = "registry";

// ---------------------------------------------------------------------------
// Public entry points (construction site — called from monopsony-server)
// ---------------------------------------------------------------------------

/// Run the registry listener. Self-contained like the mirror router: own
/// state, own auth state, plain `axum::serve` (registry traffic never uses
/// CONNECT — pulls are GET/HEAD, pushes are POST/PATCH/PUT).
pub async fn run_registry(
    addr: SocketAddr,
    storage: Arc<dyn CacheStorage>,
    db: Option<Arc<Database>>,
    auth_mode: monopsony_common::config::AuthMode,
) -> anyhow::Result<()> {
    let state = Arc::new(ProxyState::new(storage, db.clone()));
    let auth_state = Arc::new(monopsony_control::auth::AuthState { auth_mode, db });
    let app = create_registry_router(state, auth_state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("OCI registry listening on {}", addr);
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

/// Registry router. Auth is layered with its own state (same pattern as
/// `create_proxy_router`), then the handler state is attached.
pub fn create_registry_router(
    state: Arc<ProxyState>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
) -> Router {
    Router::new()
        .route("/v2", axum::routing::any(handle_v2))
        .route("/v2/", axum::routing::any(handle_v2))
        .route("/v2/*path", axum::routing::any(handle_v2))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            monopsony_control::auth::auth_middleware,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Request routing
// ---------------------------------------------------------------------------

/// One handler for the whole `/v2` tree: repo names contain slashes, so the
/// request path is parsed manually instead of relying on route params. The
/// wildcard capture is unused — we read the raw URI so percent-decoding stays
/// under our control (decode each segment AFTER splitting, RFC 3986 order).
async fn handle_v2(
    State(state): State<Arc<ProxyState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let raw = uri.path();
    let rest = raw.strip_prefix("/v2").unwrap_or(raw);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return version_check();
    }

    let segments: Vec<&str> = rest.split('/').collect();
    // Minimum shape: <repo-segment> + <blobs|manifests|tags> + <ref|list>
    if segments.len() < 3 {
        return unsupported();
    }
    let (repo_raw, kind_raw) = segments.split_at(segments.len() - 2);
    let (kind, ref_raw) = (kind_raw[0], kind_raw[1]);

    // Decode + validate the repo BEFORE anything else (traversal guard).
    let mut repo = String::new();
    for seg in repo_raw {
        let Ok(decoded) = percent_decode_str(seg).decode_utf8() else {
            return oci_error(StatusCode::BAD_REQUEST, "NAME_INVALID", "malformed repository name");
        };
        if !is_valid_repo_segment(&decoded) {
            return oci_error(StatusCode::BAD_REQUEST, "NAME_INVALID", "invalid repository name");
        }
        if !repo.is_empty() {
            repo.push('/');
        }
        repo.push_str(&decoded);
    }

    match (kind, ref_raw) {
        ("tags", "list") => tags_list(&state, &repo).await,
        ("blobs", digest_raw) => {
            let Ok(digest) = percent_decode_str(digest_raw).decode_utf8() else {
                return oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
            };
            serve_blob(&state, &method, &headers, &repo, &digest).await
        }
        ("manifests", reference_raw) => {
            let Ok(reference) = percent_decode_str(reference_raw).decode_utf8() else {
                return oci_error(StatusCode::BAD_REQUEST, "MANIFEST_UNKNOWN", "malformed reference");
            };
            serve_manifest(&state, &method, &repo, &reference).await
        }
        _ => unsupported(),
    }
}

fn version_check() -> Response {
    (
        StatusCode::OK,
        [(
            "Docker-Distribution-API-Version",
            HeaderValue::from_static("registry/2.0"),
        )],
        axum::Json(serde_json::json!({})),
    )
        .into_response()
}

fn unsupported() -> Response {
    oci_error(StatusCode::NOT_FOUND, "UNSUPPORTED", "unsupported registry path")
}

fn oci_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "errors": [{"code": code, "message": message, "detail": ""}]
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Blob serving (end-2)
// ---------------------------------------------------------------------------

/// Complete a response from a builder, falling back to 500 if a header value
/// was invalid (cannot happen with our validated digests/numeric ranges, but
/// a handler must not panic).
fn finish(builder: axum::http::response::Builder, body: axum::body::Body) -> Response {
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn serve_blob(
    state: &ProxyState,
    method: &Method,
    headers: &HeaderMap,
    repo: &str,
    digest: &str,
) -> Response {
    if !is_valid_digest(digest) {
        return oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
    }
    let key = format!("{REGISTRY_PREFIX}/{repo}/blobs/{digest}");
    let (stream, total) = match state.storage.read_stream(&key, 0..u64::MAX).await {
        Ok(pair) => pair,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob not found"),
    };

    if method == Method::HEAD {
        drop(stream);
        return finish(
            blob_response_headers(digest, total, None),
            axum::body::Body::empty(),
        );
    }

    // Single range only; malformed or multi-range requests are served in
    // full (RFC 9110 permits ignoring Range).
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range_spec)
        .and_then(|spec| resolve_range(spec, total));

    match range {
        None => finish(
            blob_response_headers(digest, total, None),
            axum::body::Body::from_stream(stream),
        ),
        Some((start, end)) => {
            // Range requested but unsatisfiable (start past EOF): 416.
            if start == u64::MAX {
                let mut resp = oci_error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "BLOB_UNKNOWN",
                    "requested range not satisfiable",
                );
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{total}")) {
                    resp.headers_mut().insert("Content-Range", v);
                }
                return resp;
            }
            let length = end - start + 1;
            let (range_stream, _) = match state.storage.read_stream(&key, start..end + 1).await {
                Ok(pair) => pair,
                Err(_) => {
                    return oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob not found")
                }
            };
            finish(
                blob_response_headers(digest, total, Some((start, end, length))),
                axum::body::Body::from_stream(range_stream),
            )
        }
    }
}

fn blob_response_headers(
    digest: &str,
    total: u64,
    range: Option<(u64, u64, u64)>,
) -> axum::http::response::Builder {
    let mut builder = axum::http::response::Response::builder()
        .header("Accept-Ranges", "bytes")
        .header("Content-Type", "application/octet-stream")
        .header(
            "Docker-Content-Digest",
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        );
    builder = match range {
        None => builder.header("Content-Length", total).status(StatusCode::OK),
        Some((start, end, length)) => builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Content-Length", length)
            .header(
                "Content-Range",
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
                    .unwrap_or(HeaderValue::from_static("bytes */0")),
            ),
    };
    builder
}

/// Parsed `Range` header. `LastN` is the suffix form `bytes=-n`.
enum RangeSpec {
    FromTo(u64, u64),
    From(u64),
    LastN(u64),
}

fn parse_range_spec(spec: &str) -> Option<RangeSpec> {
    let spec = spec.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range: ignore, serve full
    }
    let (start, end) = spec.split_once('-')?;
    match (start.is_empty(), end.is_empty()) {
        (false, false) => {
            let s: u64 = start.parse().ok()?;
            let e: u64 = end.parse().ok()?;
            if e < s {
                return None;
            }
            Some(RangeSpec::FromTo(s, e))
        }
        (false, true) => Some(RangeSpec::From(start.parse().ok()?)),
        (true, false) => {
            let n: u64 = end.parse().ok()?;
            if n == 0 {
                return None;
            }
            Some(RangeSpec::LastN(n))
        }
        (true, true) => None,
    }
}

/// Resolve a parsed range against the object size. Returns the inclusive
/// `(start, end)` byte window, or `Some((u64::MAX, _))` as the unsatisfiable
/// sentinel (start past EOF), or `None` when no usable range was given
/// (caller serves the full object).
fn resolve_range(spec: RangeSpec, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return Some((u64::MAX, 0));
    }
    match spec {
        RangeSpec::FromTo(start, end) => {
            if start >= total {
                Some((u64::MAX, 0))
            } else {
                Some((start, end.min(total - 1)))
            }
        }
        RangeSpec::From(start) => {
            if start >= total {
                Some((u64::MAX, 0))
            } else {
                Some((start, total - 1))
            }
        }
        RangeSpec::LastN(n) => Some((total.saturating_sub(n), total - 1)),
    }
}

// ---------------------------------------------------------------------------
// Manifest serving (end-3) + tags list (end-8a)
// ---------------------------------------------------------------------------

async fn serve_manifest(
    state: &ProxyState,
    method: &Method,
    repo: &str,
    reference: &str,
) -> Response {
    // A reference containing ':' is a digest; anything else is a tag.
    let digest = if reference.contains(':') {
        if !is_valid_digest(reference) {
            return oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
        }
        reference.to_string()
    } else {
        if !is_valid_tag(reference) {
            return oci_error(StatusCode::BAD_REQUEST, "TAG_INVALID", "invalid tag");
        }
        match resolve_tag(state, repo, reference).await {
            Some(d) => d,
            None => {
                return oci_error(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "manifest not found for tag",
                )
            }
        }
    };

    let key = format!("{REGISTRY_PREFIX}/{repo}/manifests/{digest}");
    let bytes = match state.storage.read(&key).await {
        Ok(b) => b,
        Err(_) => {
            return oci_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest not found")
        }
    };
    let length = bytes.len() as u64;
    if method == Method::HEAD {
        return manifest_response(&digest, length, None);
    }
    manifest_response(&digest, length, Some(bytes))
}

fn manifest_response(digest: &str, length: u64, body: Option<Vec<u8>>) -> Response {
    let builder = axum::http::response::Response::builder()
        .status(StatusCode::OK)
        .header(
            "Content-Type",
            // S3 persists the real media type alongside the manifest; until
            // then the OCI manifest media type is the correct default for
            // everything the push path (S2/S3) will store.
            "application/vnd.oci.image.manifest.v1+json",
        )
        .header("Content-Length", length)
        .header(
            "Docker-Content-Digest",
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        );
    match body {
        Some(bytes) => finish(builder, axum::body::Body::from(bytes)),
        None => finish(builder, axum::body::Body::empty()),
    }
}

/// Resolve a tag to its manifest digest via the tag-index sidecar
/// (`registry/<repo>/tags.json`, written by the S3 push path).
async fn resolve_tag(state: &ProxyState, repo: &str, tag: &str) -> Option<String> {
    let key = format!("{REGISTRY_PREFIX}/{repo}/tags.json");
    let bytes = state.storage.read(&key).await.ok()?;
    let tags: HashMap<String, String> = serde_json::from_slice(&bytes).ok()?;
    tags.get(tag).cloned()
}

async fn tags_list(state: &ProxyState, repo: &str) -> Response {
    let key = format!("{REGISTRY_PREFIX}/{repo}/tags.json");
    let bytes = match state.storage.read(&key).await {
        Ok(b) => b,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository not found"),
    };
    let tags: HashMap<String, String> = match serde_json::from_slice(&bytes) {
        Ok(t) => t,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository not found"),
    };
    let mut names: Vec<String> = tags.into_keys().collect();
    names.sort();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "name": repo, "tags": names })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Input validation — the traversal guard. Storage keys are built ONLY from
// strings that passed these checks, so `..`, `/`, and `:` can never reach a
// key path (slashes are split away before decode; the grammars reject the
// rest).
// ---------------------------------------------------------------------------

/// One repository path segment: `^[a-z0-9]+([._-][a-z0-9]+)*$` (OCI grammar).
fn is_valid_repo_segment(seg: &str) -> bool {
    if seg.is_empty() || seg.len() > 255 {
        return false;
    }
    let mut on_separator = true; // must start with alnum
    for b in seg.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => on_separator = false,
            b'.' | b'_' | b'-' => {
                if on_separator {
                    return false;
                }
                on_separator = true;
            }
            _ => return false,
        }
    }
    !on_separator // must end with alnum
}

/// Digest: `^[a-z0-9]+:[a-f0-9]{32,128}$` (covers sha256/sha512 lowercase hex).
fn is_valid_digest(s: &str) -> bool {
    let Some((alg, encoded)) = s.split_once(':') else {
        return false;
    };
    if alg.is_empty() || !alg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
        return false;
    }
    (32..=128).contains(&encoded.len())
        && encoded.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Tag: `^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$` (docker tag grammar).
fn is_valid_tag(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    let bytes = s.as_bytes();
    if !(bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store_backend::ObjectStoreBackend;
    use axum::body::Body;

    const BLOB_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const MANIFEST_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const BLOB_BODY: &[u8] = b"layer-bytes-here"; // 16 bytes
    const MANIFEST_BODY: &[u8] = b"{\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\"}";

    fn blob_digest() -> String {
        format!("sha256:{BLOB_HEX}")
    }
    fn manifest_digest() -> String {
        format!("sha256:{MANIFEST_HEX}")
    }

    fn open_auth() -> Arc<monopsony_control::auth::AuthState> {
        Arc::new(monopsony_control::auth::AuthState {
            auth_mode: monopsony_common::config::AuthMode::Open,
            db: None,
        })
    }

    /// Fresh empty-registry state (own tempdir so seeds never leak between tests).
    async fn fresh_state() -> (Arc<ProxyState>, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn CacheStorage> = Arc::new(
            ObjectStoreBackend::local(dir.path().to_path_buf(), 0).unwrap(),
        );
        (Arc::new(ProxyState::new(storage, None)), dir)
    }

    /// Router seeded with one blob, one manifest, and a two-entry tag index.
    async fn seeded_router() -> (Router, tempfile::TempDir) {
        let (state, dir) = fresh_state().await;
        state
            .storage
            .write(&format!("registry/test/app/blobs/{}", blob_digest()), BLOB_BODY)
            .await
            .unwrap();
        state
            .storage
            .write(
                &format!("registry/test/app/manifests/{}", manifest_digest()),
                MANIFEST_BODY,
            )
            .await
            .unwrap();
        let tags = serde_json::json!({ "latest": manifest_digest(), "v1": manifest_digest() });
        state
            .storage
            .write(
                "registry/test/app/tags.json",
                &serde_json::to_vec(&tags).unwrap(),
            )
            .await
            .unwrap();
        (create_registry_router(state, open_auth()), dir)
    }

    async fn req(router: &Router, method: &str, uri: &str) -> Response {
        use tower::ServiceExt;
        router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn body_bytes(resp: Response) -> bytes::Bytes {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn version_check_answers_200_on_empty_registry() {
        let (state, _dir) = fresh_state().await;
        let router = create_registry_router(state, open_auth());
        for uri in ["/v2", "/v2/"] {
            let resp = req(&router, "GET", uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "uri {uri}");
            assert_eq!(
                resp.headers()["docker-distribution-api-version"],
                "registry/2.0"
            );
            assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(b"{}"));
        }
    }

    #[tokio::test]
    async fn blob_head_and_get_serve_digest_and_length() {
        let (router, _dir) = seeded_router().await;
        let uri = format!("/v2/test/app/blobs/{}", blob_digest());
        let head = req(&router, "HEAD", &uri).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()["content-length"], "16");
        assert_eq!(head.headers()["docker-content-digest"], blob_digest());
        let get = req(&router, "GET", &uri).await;
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(body_bytes(get).await, bytes::Bytes::from_static(BLOB_BODY));
    }

    #[tokio::test]
    async fn missing_blob_404s_with_blob_unknown() {
        let (router, _dir) = seeded_router().await;
        let other = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let resp = req(&router, "GET", &format!("/v2/test/app/blobs/{other}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("BLOB_UNKNOWN"));
    }

    #[tokio::test]
    async fn blob_range_serves_206_clamped_and_416_unsatisfiable() {
        let (router, _dir) = seeded_router().await;
        let uri = format!("/v2/test/app/blobs/{}", blob_digest());

        // Clamped end range: 6-999 -> 6-15.
        let resp = axum::http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("range", "bytes=6-999")
            .body(Body::empty())
            .unwrap();
        use tower::ServiceExt;
        let resp = router.clone().oneshot(resp).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["content-range"], "bytes 6-15/16");
        assert_eq!(resp.headers()["content-length"], "10");
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(b"bytes-here"));

        // Suffix form: last 5 bytes.
        let resp = axum::http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("range", "bytes=-5")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(resp).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["content-range"], "bytes 11-15/16");

        // Unsatisfiable: start past EOF.
        let resp = axum::http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("range", "bytes=100-200")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(resp).await.unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers()["content-range"], "bytes */16");
    }

    #[tokio::test]
    async fn manifest_serves_by_digest_and_by_tag() {
        let (router, _dir) = seeded_router().await;
        for reference in [manifest_digest(), "latest".to_string(), "v1".to_string()] {
            let resp = req(&router, "GET", &format!("/v2/test/app/manifests/{reference}")).await;
            assert_eq!(resp.status(), StatusCode::OK, "ref {reference}");
            assert_eq!(
                resp.headers()["docker-content-digest"],
                manifest_digest()
            );
            assert_eq!(
                body_bytes(resp).await,
                bytes::Bytes::from_static(MANIFEST_BODY)
            );
        }
    }

    #[tokio::test]
    async fn unknown_reference_and_unknown_repo_404() {
        let (router, _dir) = seeded_router().await;
        let resp = req(&router, "GET", "/v2/test/app/manifests/nosuchtag").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("MANIFEST_UNKNOWN"));

        let resp = req(&router, "GET", "/v2/other/repo/tags/list").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("NAME_UNKNOWN"));
    }

    #[tokio::test]
    async fn tags_list_serves_sorted_names() {
        let (router, _dir) = seeded_router().await;
        let resp = req(&router, "GET", "/v2/test/app/tags/list").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["name"], "test/app");
        assert_eq!(parsed["tags"], serde_json::json!(["latest", "v1"]));
    }

    #[tokio::test]
    async fn invalid_digest_and_repo_are_rejected_before_storage() {
        let (router, _dir) = seeded_router().await;
        // Traversal attempt: decoded segment contains '/' and '.'.
        let resp = req(&router, "GET", "/v2/test/app/blobs/..%2F..%2Fetc%2Fpasswd").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("DIGEST_INVALID"));
        // Bad digest charset (uppercase hex, wrong length).
        let resp = req(&router, "GET", "/v2/test/app/blobs/sha256:ZZZZ").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Uppercase repo segment violates the OCI repo grammar.
        let resp = req(
            &router,
            "GET",
            &format!("/v2/Test/app/blobs/{}", blob_digest()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("NAME_INVALID"));
        // Invalid tag (leading dot).
        let resp = req(&router, "GET", "/v2/test/app/manifests/.bad").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("TAG_INVALID"));
    }

    #[tokio::test]
    async fn percent_encoded_digest_segment_resolves() {
        let (router, _dir) = seeded_router().await;
        let resp = req(
            &router,
            "GET",
            &format!("/v2/test/app/blobs/sha256%3A{BLOB_HEX}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(BLOB_BODY));
    }

    #[tokio::test]
    async fn keyed_auth_rejects_without_credentials() {
        let (state, _dir) = fresh_state().await;
        let auth = Arc::new(monopsony_control::auth::AuthState {
            auth_mode: monopsony_common::config::AuthMode::Token,
            db: None, // fail-closed: no key store configured
        });
        let router = create_registry_router(state, auth);
        let resp = req(&router, "GET", "/v2/").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
