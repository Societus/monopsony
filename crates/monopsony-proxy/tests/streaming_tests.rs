//! Streaming large-package proxy integration tests (plan §4).
//!
//! Validates the 2026-09-13 hardening end-to-end through the REAL proxy
//! router: a mock package-manager backend matches 127.0.0.1 URLs so the
//! cached code path is exercised against a local upstream.
//!
//! - Large bodies stream through the cache-miss path (spool + tee) and land
//!   atomically in the cache; a second request is a HIT.
//! - Concurrent requests for the same uncached key coalesce to one fetch.
//! - Range requests against cached objects return 206 + the right bytes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use http_body_util::BodyExt;
use hyper::http::{Request, StatusCode};
use monopsony_common::package_manager::{
    PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern,
};
use monopsony_proxy::cache_storage::CacheStorage;
use monopsony_proxy::object_store_backend::ObjectStoreBackend;
use monopsony_proxy::proxy::{create_proxy_router, ProxyState};

/// Test PM backend: matches any 127.0.0.1 URL ending in .pkg so the local
/// test upstream routes through the real cached-path logic.
struct TestBackend;

#[async_trait]
impl PackageManagerBackend for TestBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Pacman
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn list_installed(&self) -> monopsony_common::Result<Vec<PackageRef>> {
        Ok(vec![])
    }
    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![UrlPattern::new(
            r"https?://127\.0\.0\.1:[0-9]+/.*\.pkg",
            100,
            "test upstream",
        )
        .expect("valid regex")]
    }
    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        let name = parsed.path_segments()?.last()?.to_string();
        Some(format!("test/{}", name))
    }
    fn proxy_config(&self, _proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::None
    }
    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::None
    }
}

/// Synthetic upstream serving `size` bytes of 0xAB in 256 KiB chunks.
/// Counts upstream fetches for coalescing assertions.
async fn spawn_pattern_upstream(size: usize, fetches: Arc<AtomicUsize>) -> SocketAddr {
    use axum::routing::get;
    use std::io;

    let app = axum::Router::new().route(
        "/file.pkg",
        get(move || {
            let fetches = fetches.clone();
            async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                let chunk = vec![0xABu8; 256 * 1024];
                let mut chunks: Vec<Result<bytes::Bytes, io::Error>> = Vec::new();
                let mut sent = 0usize;
                while sent < size {
                    let take = chunk.len().min(size - sent);
                    chunks.push(Ok(bytes::Bytes::copy_from_slice(&chunk[..take])));
                    sent += take;
                }
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-length", size)
                    .header("content-type", "application/octet-stream")
                    .body(Body::from_stream(futures::stream::iter(chunks)))
                    .unwrap()
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn make_state(cache_dir: &std::path::Path) -> Arc<ProxyState> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let storage: Arc<dyn CacheStorage> =
        Arc::new(ObjectStoreBackend::local(cache_dir.to_path_buf(), 0).unwrap());
    let mut state = ProxyState::new(storage, None);
    state.backends = vec![Box::new(TestBackend)];
    state.allow_private_upstreams = true; // test upstreams live on 127.0.0.1
    Arc::new(state)
}

async fn spawn_proxy_router(state: Arc<ProxyState>) -> SocketAddr {
    use monopsony_common::config::AuthMode;
    let auth_state = Arc::new(monopsony_control::auth::AuthState {
        auth_mode: AuthMode::Open,
        db: None,
    });
    let app = create_proxy_router(state, None, auth_state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// Raw forward-proxy request: dial `proxy`, send absolute `url` in the
/// request line (exactly what monopsony-client does).
async fn proxy_get(
    proxy: SocketAddr,
    url: &str,
    range: Option<&str>,
) -> (StatusCode, bytes::Bytes, axum::http::HeaderMap) {
    use hyper_util::rt::TokioIo;
    let stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut builder = Request::builder().method("GET").uri(url);
    if let Some(r) = range {
        builder = builder.header("range", format!("bytes={}", r));
    }
    let resp = sender
        .send_request(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, body, headers)
}

#[tokio::test]
async fn test_large_stream_miss_then_hit() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(dir.path());
    let proxy_addr = spawn_proxy_router(state.clone()).await;

    // 12 MiB — spans multiple 5 MiB spool parts.
    let size = 12 * 1024 * 1024;
    let fetches = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_pattern_upstream(size, fetches.clone()).await;
    let url = format!("http://127.0.0.1:{}/file.pkg", upstream.port());

    // First request: streaming miss through the tee.
    let (status, body, headers) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("MISS"),
        "first request must be a streaming miss"
    );
    assert_eq!(body.len(), size);
    assert!(body.iter().all(|&b| b == 0xAB));
    assert_eq!(fetches.load(Ordering::SeqCst), 1);

    // Second request: HIT from the spooled cache, no upstream refetch.
    let (status, body, headers) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(body.len(), size);
    assert!(body.iter().all(|&b| b == 0xAB));
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "cached HIT must not refetch upstream"
    );

    // Range request against the cached object → 206 + correct bytes.
    let (status, body, headers) = proxy_get(proxy_addr, &url, Some("1024-2047")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body.len(), 1024);
    assert!(body.iter().all(|&b| b == 0xAB));
    assert_eq!(
        headers
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split('/').next().map(|s| s.to_string())),
        Some("bytes 1024-2047".to_string())
    );
}

#[tokio::test]
async fn test_concurrent_requests_coalesce() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(dir.path());
    let proxy_addr = spawn_proxy_router(state).await;

    let size = 8 * 1024 * 1024;
    let fetches = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_pattern_upstream(size, fetches.clone()).await;
    let url = format!("http://127.0.0.1:{}/file.pkg", upstream.port());

    // Two simultaneous requests for the same uncached URL.
    let url_owned = url.clone();
    let a = tokio::spawn(async move { proxy_get(proxy_addr, &url_owned, None).await });
    let b = tokio::spawn(async move { proxy_get(proxy_addr, &url, None).await });
    let (sa, ba, ha) = a.await.unwrap();
    let (sb, bb, _) = b.await.unwrap();
    assert_eq!(sa, StatusCode::OK);
    assert_eq!(sb, StatusCode::OK);
    assert_eq!(ba.len(), size);
    assert_eq!(bb.len(), size);
    // The first to register wins the fetch; the second joins. Depending on
    // timing the joiner may see HIT (fetcher committed first) or MISS-join.
    let cache_states: Vec<&str> = ha
        .get("x-cache")
        .and_then(|v| v.to_str().ok())
        .into_iter()
        .collect();
    assert!(
        cache_states.iter().all(|s| *s == "MISS" || *s == "HIT"),
        "unexpected X-Cache on concurrent request: {:?}",
        cache_states
    );
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "concurrent requests for the same key must coalesce to one upstream fetch"
    );
}

#[tokio::test]
async fn test_head_request_does_not_poison_cache() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(dir.path());
    let proxy_addr = spawn_proxy_router(state.clone()).await;

    let size = 1024;
    let fetches = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_pattern_upstream(size, fetches.clone()).await;
    let url = format!("http://127.0.0.1:{}/file.pkg", upstream.port());

    // HEAD first — must not create a cache entry.
    use hyper_util::rt::TokioIo;
    let stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender
        .send_request(
            Request::builder()
                .method("HEAD")
                .uri(&url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The storage must NOT contain the object.
    let cached = state.storage.exists("test/file.pkg").await.unwrap();
    assert!(!cached, "HEAD must not populate the cache");

    // Follow-up GET works normally.
    let (status, body, _) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), size);
}
