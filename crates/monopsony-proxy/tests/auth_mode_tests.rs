//! Token-mode auth regression tests (Wave B item B3 — F10 lesson).
//!
//! Open-mode test suites cannot catch a dropped auth layer: with
//! `AuthMode::Open` every request passes, so a regression that removes the
//! auth middleware entirely stays green. These tests drive the REAL serve
//! path (`serve_proxy_router`, the manual hyper-util loop with CONNECT
//! interception) with raw absolute-URI GET requests and lock in:
//!
//! 1. Token mode + missing key  -> 401 (not 500, not pass-through)
//! 2. Token mode + garbage key  -> 401 (fail-closed with db: None)
//! 3. Open mode + no key        -> NOT 401 (layer must not over-block)

use std::net::SocketAddr;
use std::sync::Arc;

use monopsony_common::config::AuthMode;
use monopsony_proxy::cache_storage::CacheStorage;
use monopsony_proxy::object_store_backend::ObjectStoreBackend;
use monopsony_proxy::proxy::{create_proxy_router, ProxyState};

/// Build a ProxyState over a temp dir-backed object store (same shape as
/// streaming_tests::make_state). Private upstreams allowed so the Open-mode
/// pass-through can reach a 127.0.0.1 test upstream.
fn make_state(cache_dir: &std::path::Path) -> Arc<ProxyState> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let storage: Arc<dyn CacheStorage> =
        Arc::new(ObjectStoreBackend::local(cache_dir.to_path_buf(), 0).unwrap());
    let mut state = ProxyState::new(storage, None);
    state.allow_private_upstreams = true; // test upstream lives on 127.0.0.1
    Arc::new(state)
}

/// Spawn the REAL proxy serve path on 127.0.0.1:0 with the given auth mode.
async fn spawn_proxy_with_auth(state: Arc<ProxyState>, mode: AuthMode) -> SocketAddr {
    let auth_state = Arc::new(monopsony_control::auth::AuthState {
        auth_mode: mode,
        db: None, // fail-closed: no key store configured
    });
    let app = create_proxy_router(state.clone(), None, auth_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        monopsony_proxy::proxy::serve_proxy_router(listener, app, state, None, auth_state)
            .await
            .unwrap();
    });
    addr
}

/// Plain pass-through upstream for the Open-mode test (no PM backend pattern
/// matches it, so the proxy streams it through uncached).
async fn spawn_upstream() -> SocketAddr {
    let app = axum::Router::new().route(
        "/hello",
        axum::routing::get(|| async {
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/plain")],
                "hello",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Raw forward-proxy GET over a bare TcpStream: absolute URI in the request
/// line (exactly what monopsony-client sends), optional X-Monopsony-Key
/// header. Returns the response status code parsed from the status line.
async fn raw_proxy_get(proxy: SocketAddr, url: &str, key: Option<&str>) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let host = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("127.0.0.1")
        .to_string();

    let mut req = format!("GET {url} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(k) = key {
        req.push_str(&format!("X-Monopsony-Key: {k}\r\n"));
    }
    req.push_str("\r\n");

    let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Read until we have a full status line; we only need the code.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or_default();
    // "HTTP/1.1 401 Unauthorized"
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("unparseable status line: {status_line:?}"))
}

#[tokio::test]
async fn token_mode_missing_key_returns_401() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(dir.path());
    let proxy = spawn_proxy_with_auth(state, AuthMode::Token).await;

    // Absolute-URI GET with NO X-Monopsony-Key header. Upstream is bogus —
    // it must never be reached; auth rejects first.
    let status = raw_proxy_get(proxy, "http://127.0.0.1:1/never-reached.pkg", None).await;
    assert_eq!(
        status, 401,
        "Token mode must reject keyless requests with 401 (got {status})"
    );
}

#[tokio::test]
async fn token_mode_garbage_key_returns_401() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(dir.path());
    let proxy = spawn_proxy_with_auth(state, AuthMode::Token).await;

    // A well-formed but unknown key: db is None (fail-closed branch), so the
    // middleware must 401 — not 500, not pass-through.
    let status = raw_proxy_get(
        proxy,
        "http://127.0.0.1:1/never-reached.pkg",
        Some("garbage-key-that-exists-nowhere"),
    )
    .await;
    assert_eq!(
        status, 401,
        "Token mode must reject unknown keys with 401 (got {status})"
    );
}

#[tokio::test]
async fn open_mode_keyless_request_is_not_401() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(dir.path());
    let proxy = spawn_proxy_with_auth(state, AuthMode::Open).await;
    let upstream = spawn_upstream().await;

    // Same keyless absolute-URI GET in Open mode: must sail through to the
    // upstream (200), proving the auth gate does not over-block.
    let url = format!("http://127.0.0.1:{}/hello", upstream.port());
    let status = raw_proxy_get(proxy, &url, None).await;
    assert_eq!(status, 200, "Open mode must pass through (got {status})");
    assert_ne!(status, 401, "Open mode must never 401 keyless requests");
}
