//! Docker Registry HTTP API V2 caching proxy.
//!
//! Implements three endpoints that allow the proxy to act as a Docker
//! registry mirror (pull-through cache):
//!
//! - `GET /v2/` — API version check, returns `{}`
//! - `GET /v2/:name/manifests/:reference` — proxy + cache manifest
//! - `GET /v2/:name/blobs/:digest` — proxy + cache blob layer
//!
//! Multi-registry routing: if the first path segment of `:name` contains
//! `.` or `:`, it is treated as a custom registry host (e.g.
//! `ghcr.io/owner/repo`). Otherwise the default `registry-1.docker.io` is
//! used.
//!
//! Auth token passthrough: the client's `Authorization` header is forwarded
//! verbatim to the upstream registry.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, Response, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use tracing::{debug, error, info, warn};

use crate::cache_storage::CacheStorage;

/// Default upstream Docker registry when the image name doesn't include a host.
const DEFAULT_REGISTRY: &str = "registry-1.docker.io";

/// Shared state for the Docker mirror.
///
/// Contains a `reqwest` client for upstream fetches and a handle to the
/// cache storage backend. This is injected into the router via
/// `Router::with_state`.
#[derive(Clone)]
pub struct DockerState {
    pub client: reqwest::Client,
    pub storage: Arc<dyn CacheStorage>,
}

impl DockerState {
    pub fn new(storage: Arc<dyn CacheStorage>) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("failed to build reqwest client for Docker mirror");
        Self { client, storage }
    }
}

/// Build the Docker registry V2 router.
///
/// Mount this on the main proxy's router:
/// ```ignore
/// let app = Router::new()
///     .merge(docker::docker_routes().with_state(docker_state));
/// ```
pub fn docker_routes() -> Router<DockerState> {
    Router::new()
        .route("/v2/", get(api_check))
        .route("/v2/:name/manifests/:reference", get(get_manifest))
        .route("/v2/:name/blobs/:digest", get(get_blob))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the upstream registry host and path prefix from an image name.
///
/// Docker image names can be:
/// - `library/nginx`           → default registry, name = `library/nginx`
/// - `nginx`                   → default registry, name = `nginx`
/// - `ghcr.io/owner/repo`     → `ghcr.io` registry, name = `owner/repo`
/// - `localhost:5000/myimage` → `localhost:5000` registry, name = `myimage`
///
/// Returns `(registry_host, image_path_on_registry)`.
fn resolve_registry(name: &str) -> (&str, &str) {
    // Check if the first segment looks like a registry host
    // (contains '.' for domains or ':' for ports)
    if let Some(idx) = name.find('/') {
        let first_segment = &name[..idx];
        if first_segment.contains('.') || first_segment.contains(':') {
            return (&name[..idx], &name[idx + 1..]);
        }
    }
    (DEFAULT_REGISTRY, name)
}

/// Build the cache key for a Docker V2 resource.
fn cache_key(resource: &str) -> String {
    format!("docker/v2/{}", resource)
}

/// Extract the `Authorization` header from the client request and forward it.
fn auth_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

/// Fetch a resource from the upstream registry, returning the raw bytes,
/// status, and response headers.
async fn fetch_upstream(
    state: &DockerState,
    registry: &str,
    path: &str,
    auth: Option<&str>,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("https://{}/v2/{}", registry, path);
    debug!("Docker upstream fetch: {}", url);

    let mut req = state.client.get(&url);
    if let Some(token) = auth {
        req = req.header(header::AUTHORIZATION, token);
    }
    req.send().await
}

/// Attempt to serve from cache. Returns `Some(response)` on cache hit.
async fn try_cache(state: &DockerState, key: &str) -> Option<Response<Body>> {
    if !state.storage.exists(key).await.unwrap_or(false) {
        return None;
    }

    let data = match state.storage.read(key).await {
        Ok(d) => d,
        Err(e) => {
            warn!("Docker cache read failed for {}: {}", key, e);
            return None;
        }
    };

    // Try to load content-type from metadata sidecar
    let content_type = state
        .storage
        .load_meta_json(key)
        .await
        .and_then(|raw| serde_json::from_slice::<DockerCacheMeta>(&raw).ok())
        .and_then(|m| m.content_type)
        .unwrap_or_else(|| "application/octet-stream".to_string());

    debug!("Docker cache HIT: {}", key);

    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &content_type)
        .header(header::CONTENT_LENGTH, data.len())
        .header("X-Cache", "HIT")
        .header("Docker-Distribution-Api-Version", "registry/2.0")
        .body(Body::from(data))
        .unwrap_or_else(|_| Response::new(Body::empty()));

    Some(resp)
}

/// Write response data to cache and return a response to the client.
///
/// Success bodies are streamed through a `SpoolHandle` (O(chunk) memory even
/// for multi-GB image layers): upstream chunks are spooled, the spool is
/// committed atomically, then the response is served via `read_stream` — the
/// same serving path cache hits use. Registry error bodies (non-success) are
/// small JSON and pass through buffered, as before.
async fn cache_and_respond(
    state: &DockerState,
    key: &str,
    upstream: reqwest::Response,
) -> Response<Body> {
    use futures::StreamExt;

    let status = upstream.status();
    let headers = upstream.headers().clone();

    if !status.is_success() {
        // Non-success: pass through without caching. Registry error bodies
        // are small JSON documents, so buffering here is fine.
        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                error!("Docker upstream body read failed: {}", e);
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("Failed to read upstream body"))
                    .unwrap_or_else(|_| Response::new(Body::empty()));
            }
        };

        let mut builder = Response::builder()
            .status(status)
            .header("Docker-Distribution-Api-Version", "registry/2.0");

        if let Some(ct) = headers.get(header::CONTENT_TYPE) {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }

        return builder
            .body(Body::from(bytes))
            .unwrap_or_else(|_| Response::new(Body::empty()));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Stream the upstream body into a spool — never buffer a whole layer.
    let mut spool = match state.storage.spool(key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Docker cache spool open failed for {}: {}", key, e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Failed to open cache spool"))
                .unwrap_or_else(|_| Response::new(Body::empty()));
        }
    };

    let mut stream = upstream.bytes_stream();
    let mut fetch_err: Option<reqwest::Error> = None;
    loop {
        match stream.next().await {
            None => break, // clean EOF
            Some(Err(e)) => {
                fetch_err = Some(e);
                break;
            }
            Some(Ok(chunk)) => spool.write(&chunk),
        }
    }

    if let Some(e) = fetch_err {
        error!("Docker upstream body read failed for {}: {}", key, e);
        spool.abort().await;
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("Failed to read upstream body"))
            .unwrap_or_else(|_| Response::new(Body::empty()));
    }

    // Clean EOF — publish atomically.
    let meta = match spool.commit().await {
        Ok(m) => m,
        Err(e) => {
            error!("Docker cache spool commit failed for {}: {}", key, e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Failed to commit cached body"))
                .unwrap_or_else(|_| Response::new(Body::empty()));
        }
    };
    info!("Docker cached: {} ({} bytes)", key, meta.size_bytes);

    // Save content-type metadata sidecar
    if let Some(ref ct) = content_type {
        let sidecar = DockerCacheMeta {
            content_type: Some(ct.clone()),
        };
        if let Ok(raw) = serde_json::to_vec(&sidecar) {
            let _ = state.storage.save_meta_json(key, &raw).await;
        }
    }

    // Serve the freshly cached object via read_stream — identical serving
    // path to cache hits, so misses stream to the client chunk-by-chunk too.
    let (body_stream, total) = match state.storage.read_stream(key, 0..u64::MAX).await {
        Ok(v) => v,
        Err(e) => {
            error!("Docker cache read-back failed for {}: {}", key, e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Failed to read cached body"))
                .unwrap_or_else(|_| Response::new(Body::empty()));
        }
    };

    let mut builder = Response::builder()
        .status(status)
        .header("X-Cache", "MISS")
        .header("Docker-Distribution-Api-Version", "registry/2.0")
        .header(header::CONTENT_LENGTH, total);

    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    // Forward digest-related headers
    for h in &[
        "docker-content-digest",
        "etag",
        header::LAST_MODIFIED.as_str(),
    ] {
        if let Some(v) = headers.get(*h) {
            builder = builder.header(*h, v);
        }
    }

    builder
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Metadata stored alongside cached Docker resources.
#[derive(serde::Serialize, serde::Deserialize)]
struct DockerCacheMeta {
    content_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /v2/` — Docker Registry API version check.
///
/// Returns `{}` with 200 and the required headers. No upstream call is made.
async fn api_check() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("Docker-Distribution-Api-Version", "registry/2.0"),
            ("X-Cache", "HIT"),
        ],
        "{}",
    )
}

/// `GET /v2/:name/manifests/:reference`
///
/// Proxies and caches image manifests. The `Accept` header from the client
/// is forwarded so that the upstream returns the correct media type.
async fn get_manifest(
    State(state): State<DockerState>,
    Path((name, reference)): Path<(String, String)>,
    client_headers: HeaderMap,
) -> Response<Body> {
    let (registry, image_path) = resolve_registry(&name);
    let upstream_path = format!("{}/manifests/{}", image_path, reference);
    let key = cache_key(&format!("{}/manifests/{}", name, reference));

    debug!(
        "Manifest request: name={}, ref={}, registry={}",
        name, reference, registry
    );

    // Check cache
    if let Some(cached) = try_cache(&state, &key).await {
        return cached;
    }

    // Fetch from upstream
    let auth = auth_header(&client_headers);
    let upstream_response = match fetch_upstream(&state, registry, &upstream_path, auth).await {
        Ok(r) => r,
        Err(e) => {
            error!(
                "Docker manifest upstream fetch failed for {}/{}: {}",
                registry, upstream_path, e
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("Docker-Distribution-Api-Version", "registry/2.0")
                .body(Body::from("Upstream registry request failed"))
                .unwrap_or_else(|_| Response::new(Body::empty()));
        }
    };

    cache_and_respond(&state, &key, upstream_response).await
}

/// `GET /v2/:name/blobs/:digest`
///
/// Proxies and caches image layer blobs.
async fn get_blob(
    State(state): State<DockerState>,
    Path((name, digest)): Path<(String, String)>,
    client_headers: HeaderMap,
) -> Response<Body> {
    let (registry, image_path) = resolve_registry(&name);
    let upstream_path = format!("{}/blobs/{}", image_path, digest);
    let key = cache_key(&format!("{}/blobs/{}", name, digest));

    debug!(
        "Blob request: name={}, digest={}, registry={}",
        name, digest, registry
    );

    // Check cache
    if let Some(cached) = try_cache(&state, &key).await {
        return cached;
    }

    // Fetch from upstream
    let auth = auth_header(&client_headers);
    let upstream_response = match fetch_upstream(&state, registry, &upstream_path, auth).await {
        Ok(r) => r,
        Err(e) => {
            error!(
                "Docker blob upstream fetch failed for {}/{}: {}",
                registry, upstream_path, e
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("Docker-Distribution-Api-Version", "registry/2.0")
                .body(Body::from("Upstream registry request failed"))
                .unwrap_or_else(|_| Response::new(Body::empty()));
        }
    };

    cache_and_respond(&state, &key, upstream_response).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_default_registry() {
        let (host, path) = resolve_registry("library/nginx");
        assert_eq!(host, DEFAULT_REGISTRY);
        assert_eq!(path, "library/nginx");
    }

    #[test]
    fn test_resolve_custom_registry() {
        let (host, path) = resolve_registry("ghcr.io/owner/repo");
        assert_eq!(host, "ghcr.io");
        assert_eq!(path, "owner/repo");
    }

    #[test]
    fn test_resolve_localhost_port() {
        let (host, path) = resolve_registry("localhost:5000/myimage");
        assert_eq!(host, "localhost:5000");
        assert_eq!(path, "myimage");
    }

    #[test]
    fn test_resolve_single_name_no_slash() {
        // "nginx" with no slash — entire thing is the image name, default registry
        let (host, path) = resolve_registry("nginx");
        assert_eq!(host, DEFAULT_REGISTRY);
        assert_eq!(path, "nginx");
    }

    #[test]
    fn test_cache_key_format() {
        assert_eq!(
            cache_key("library/nginx/manifests/latest"),
            "docker/v2/library/nginx/manifests/latest"
        );
    }

    #[tokio::test]
    async fn test_api_check_returns_200() {
        let response = api_check().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Docker-Distribution-Api-Version")
                .unwrap(),
            "registry/2.0"
        );
    }

    #[tokio::test]
    async fn test_try_cache_miss_then_hit() {
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn CacheStorage> = Arc::new(
            crate::object_store_backend::ObjectStoreBackend::local(dir.path().to_path_buf(), 0)
                .unwrap(),
        );
        let state = DockerState::new(storage);

        let key = "docker/v2/test/blobs/sha256:abc";

        // Miss
        assert!(try_cache(&state, key).await.is_none());

        // Write some data
        state.storage.write(key, b"blob data").await.unwrap();
        let meta = DockerCacheMeta {
            content_type: Some("application/octet-stream".to_string()),
        };
        let raw = serde_json::to_vec(&meta).unwrap();
        state.storage.save_meta_json(key, &raw).await.unwrap();

        // Hit
        let resp = try_cache(&state, key).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("X-Cache").unwrap(), "HIT");
    }
}
