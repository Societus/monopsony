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
        let name = parsed.path_segments()?.next_back()?.to_string();
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
    let app = create_proxy_router(state.clone(), None, auth_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Exercise the REAL serve path (manual hyper-util loop with CONNECT
        // interception), not axum::serve — tunnels only work through it.
        monopsony_proxy::proxy::serve_proxy_router(listener, app, state, None, auth_state)
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

// ---------------------------------------------------------------------------
// CONNECT pass-through integration tests (Sprint 19 Phase 1, slice 3).
//
// Loopback-only and hermetic: the "upstream" is a local tokio echo listener.
// Raw CONNECT requests are written by hand over a tokio TcpStream — do NOT
// use reqwest/Client::request, it dials the target host directly and
// bypasses the proxy.
// ---------------------------------------------------------------------------

/// Read one HTTP/1.x response head (terminated by \r\n\r\n) plus any body
/// bytes already received with it.
async fn read_response_head(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::io::AsyncReadExt::read(stream, &mut chunk)
            .await
            .expect("read response head");
        assert!(n > 0, "connection closed before a response head arrived");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let rest = buf[pos + 4..].to_vec();
            return (head, rest);
        }
    }
}

/// Dial the proxy and send a raw CONNECT request for `authority`
/// (e.g. "127.0.0.1:12345"), returning the parsed response head.
async fn send_connect(proxy: SocketAddr, authority: &str) -> (tokio::net::TcpStream, String) {
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let req = format!("CONNECT {a} HTTP/1.1\r\nHost: {a}\r\n\r\n", a = authority);
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let (head, _rest) = read_response_head(&mut stream).await;
    (stream, head)
}

/// Tiny tokio echo server: reads whatever it gets and writes it straight
/// back until the client closes. Returns its bound address.
async fn spawn_echo_upstream() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// ProxyState with the given CONNECT configuration.
fn make_connect_state(
    cache_dir: &std::path::Path,
    cfg: monopsony_common::config::ProxyServerConfig,
) -> Arc<ProxyState> {
    // ProxyState::new() builds an HTTPS client which needs the ring
    // CryptoProvider installed process-wide (same as make_state above;
    // install_default is a no-op once another test has installed it).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let storage: Arc<dyn CacheStorage> =
        Arc::new(ObjectStoreBackend::local(cache_dir.to_path_buf(), 0).unwrap());
    let mut state = ProxyState::new(storage, None);
    state.backends = vec![Box::new(TestBackend)];
    state.allow_private_upstreams = true;
    state.connect_config = cfg;
    Arc::new(state)
}

#[tokio::test]
async fn test_connect_disabled_returns_405() {
    let dir = tempfile::tempdir().unwrap();
    // Default ProxyServerConfig: connect_enabled = false.
    let state = make_connect_state(dir.path(), Default::default());
    let proxy_addr = spawn_proxy_router(state).await;

    let (_stream, head) = send_connect(proxy_addr, "127.0.0.1:443").await;
    assert!(
        head.starts_with("HTTP/1.1 405"),
        "expected 405 for disabled CONNECT, got: {}",
        head
    );
}

#[tokio::test]
async fn test_connect_private_target_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_connect_state(
        dir.path(),
        monopsony_common::config::ProxyServerConfig {
            connect_enabled: true,
            allow_private_upstreams: false, // SSRF guard active
            connect_deny: None,
            ..Default::default()
        },
    );
    let proxy_addr = spawn_proxy_router(state).await;

    // Loopback = private → SSRF block.
    let (mut stream, head) = send_connect(proxy_addr, "127.0.0.1:9").await;
    assert!(
        head.starts_with("HTTP/1.1 403"),
        "expected 403 for private CONNECT target, got: {}",
        head
    );
    // No 200 was ever sent: exactly one response head, nothing after it.
    let mut leftover = Vec::new();
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::io::AsyncReadExt::read(&mut stream, &mut [0u8; 128]),
    )
    .await
    {
        Ok(Ok(0)) | Err(_) => {} // closed / quiet: correct
        Ok(Ok(n)) => leftover.extend_from_slice(&[0u8; 128][..n]),
        Ok(Err(_)) => {} // reset also fine
    }
    assert!(
        leftover.is_empty(),
        "expected no bytes after the 403 head, got: {:?}",
        leftover
    );
}

#[tokio::test]
async fn test_connect_end_to_end_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_connect_state(
        dir.path(),
        monopsony_common::config::ProxyServerConfig {
            connect_enabled: true,
            allow_private_upstreams: true, // loopback test upstream allowed
            connect_deny: None,
            ..Default::default()
        },
    );
    let proxy_addr = spawn_proxy_router(state).await;

    let upstream = spawn_echo_upstream().await;
    let authority = format!("127.0.0.1:{}", upstream.port());

    let (mut stream, head) = send_connect(proxy_addr, &authority).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "expected 200 Connection established, got: {}",
        head
    );

    // Tunnel established: bytes must flow both ways through the proxy.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let payload = b"hello through the monopsony tunnel";
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();

    let mut echoed = vec![0u8; payload.len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut echoed),
    )
    .await
    .expect("timed out waiting for echo")
    .expect("read echo failed");
    assert_eq!(echoed, payload, "echo must round-trip through the tunnel");

    // Client half-close → clean bidirectional shutdown.
    let _ = stream.shutdown().await;
}

#[tokio::test]
async fn test_connect_refused_upstream_502() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_connect_state(
        dir.path(),
        monopsony_common::config::ProxyServerConfig {
            connect_enabled: true,
            allow_private_upstreams: true,
            connect_deny: None,
            ..Default::default()
        },
    );
    let proxy_addr = spawn_proxy_router(state).await;

    // Bind then drop a listener to obtain a guaranteed-closed loopback port.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);

    let (_stream, head) = send_connect(proxy_addr, &format!("127.0.0.1:{}", closed_port)).await;
    assert!(
        head.starts_with("HTTP/1.1 502"),
        "expected 502 for dead upstream, got: {}",
        head
    );
}

// ---------------------------------------------------------------------------
// Sprint 19 Phase 1 review — S3 verification tests (F8/F9).
// ---------------------------------------------------------------------------

/// Full client→server CONNECT chain (F9): a raw socket dials the CLIENT
/// proxy (`monopsony_client::proxy::run_proxy`), which must tunnel the
/// CONNECT through the SERVER proxy (monopsony-proxy router) to a local
/// echo upstream. Bytes must round-trip through BOTH proxies.
///
/// `run_proxy` binds its listener internally, so we pre-bind a port, note
/// it, drop the listener, and pass the port to `run_proxy` in a spawned
/// task, then poll-connect until it accepts (small bind race, accepted).
#[tokio::test]
async fn test_connect_chain_client_to_server() {
    use monopsony_client::proxy::{run_proxy, ProxyConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // --- Server side: monopsony-proxy router with CONNECT enabled.
    let dir = tempfile::tempdir().unwrap();
    let state = make_connect_state(
        dir.path(),
        monopsony_common::config::ProxyServerConfig {
            connect_enabled: true,
            allow_private_upstreams: true, // loopback echo upstream
            connect_deny: None,
            ..Default::default()
        },
    );
    let server_addr = spawn_proxy_router(state).await;

    // --- Client side: real client proxy forwarding to the server proxy.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_port = probe.local_addr().unwrap().port();
    drop(probe);

    // Pinning not provisioned for the client harness (None key = inert).
    let pin_state = Arc::new(monopsony_client::pin_verify::PinState::new(
        None,
        dir.path().join("pin-events.jsonl"),
    ));

    let client_config = ProxyConfig {
        server_addr: format!("127.0.0.1:{}", server_addr.port()),
        timeout_ms: 2_000,
        retry_deadline_ms: 5_000,
        retry_backoff_initial_ms: 100,
        tunnel_max_lifetime_ms: 60_000,
    };
    tokio::spawn(async move {
        let addr: SocketAddr = format!("127.0.0.1:{}", client_port).parse().unwrap();
        if let Err(e) = run_proxy(
            addr,
            client_config,
            pin_state,
            None,
            "warn".to_string(),
            0,
            None,
            None,
        )
        .await
        {
            tracing::warn!("client proxy exited: {}", e);
        }
    });

    // Wait for the client proxy to come up (poll-connect, bounded).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "client proxy did not start listening within 5s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let client_addr: SocketAddr = format!("127.0.0.1:{}", client_port).parse().unwrap();

    // --- Raw CONNECT through the CLIENT proxy to the echo upstream.
    let upstream = spawn_echo_upstream().await;
    let authority = format!("127.0.0.1:{}", upstream.port());

    let (mut stream, head) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send_connect(client_addr, &authority),
    )
    .await
    .expect("timed out sending CONNECT through client proxy");
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "expected 200 through the client→server chain, got: {}",
        head
    );

    // Echo round-trip through BOTH proxies.
    let payload = b"chain: client proxy -> server proxy -> echo upstream";
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        stream.write_all(payload).await.unwrap();
        stream.flush().await.unwrap();
    })
    .await
    .expect("timed out writing into tunnel");

    let mut echoed = vec![0u8; payload.len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut echoed),
    )
    .await
    .expect("timed out waiting for chained echo")
    .expect("read chained echo failed");
    assert_eq!(
        echoed, payload,
        "echo must round-trip through client AND server proxy tunnels"
    );

    let _ = stream.shutdown().await;
}

/// F8 contract: `connect_deny` entries must block the exact host AND any
/// subdomain of it (suffix match at a label boundary), case-insensitively.
/// The deny check runs before any dial, so fake hostnames are safe — no
/// public dials happen (a not-yet-landed F8 fix fails the subdomain case;
/// this test encodes the contract).
#[tokio::test]
async fn test_connect_denied_suffix_matches() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_connect_state(
        dir.path(),
        monopsony_common::config::ProxyServerConfig {
            connect_enabled: true,
            allow_private_upstreams: true,
            connect_deny: Some(vec!["evil.example".to_string()]),
            ..Default::default()
        },
    );
    let proxy_addr = spawn_proxy_router(state).await;

    // Exact match must be denied.
    let (_s1, head_exact) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send_connect(proxy_addr, "evil.example:443"),
    )
    .await
    .expect("timed out on exact-deny CONNECT");
    assert!(
        head_exact.starts_with("HTTP/1.1 403"),
        "exact deny-list match must return 403, got: {}",
        head_exact
    );

    // Subdomain must ALSO be denied (F8 suffix semantics).
    let (_s2, head_sub) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send_connect(proxy_addr, "api.evil.example:443"),
    )
    .await
    .expect("timed out on subdomain-deny CONNECT");
    assert!(
        head_sub.starts_with("HTTP/1.1 403"),
        "subdomain of a deny-list entry must return 403 (F8 suffix semantics), got: {}",
        head_sub
    );
}

// ---------------------------------------------------------------------------
// Sprint 20 Phase 3 slice 7: tamper-injection end-to-end tests.
//
// Loopback-only: a local upstream serves fixed-pattern bytes and the REAL
// proxy router commits the object + pin through the tee. Deterministic
// Ed25519 keys (no rand), no sleeps beyond bounded commit polling.
// ---------------------------------------------------------------------------

/// Deterministic pin signing key (same construction as proxy.rs `pin_test_key`).
fn pin_e2e_key(seed: u8) -> ed25519_dalek::SigningKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[31] = seed.wrapping_mul(7);
    ed25519_dalek::SigningKey::from_bytes(&bytes)
}

/// ProxyState with pin capture provisioned: real signing key, pinning on,
/// private loopback upstreams allowed (test upstreams live on 127.0.0.1).
fn make_pin_state(cache_dir: &std::path::Path, key: ed25519_dalek::SigningKey) -> Arc<ProxyState> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let storage: Arc<dyn CacheStorage> =
        Arc::new(ObjectStoreBackend::local(cache_dir.to_path_buf(), 0).unwrap());
    let mut state = ProxyState::new(storage, None);
    state.backends = vec![Box::new(TestBackend)];
    state.allow_private_upstreams = true;
    state.pin_key = Some(key);
    state.integrity.pinning_enabled = true;
    Arc::new(state)
}

/// Upstream serving `size` bytes of 0xAB in 256 KiB chunks. When
/// `with_checksum` is true it declares `x-checksum-sha256`; otherwise no
/// integrity header at all (origin becomes Unverified at commit). Returns
/// the bound address plus the sha256 of the served bytes. The `/file.pkg.sha256`
/// sidecar path is NOT routed, so the sidecar probe 404s and the header
/// origin (or Unverified) wins cleanly.
async fn spawn_checksum_upstream(
    size: usize,
    with_checksum: bool,
    fetches: Arc<AtomicUsize>,
) -> (SocketAddr, String) {
    use axum::routing::get;
    use sha2::{Digest, Sha256};
    use std::io;

    // Hash of the deterministic 0xAB pattern, computed once up front.
    let chunk = vec![0xABu8; 256 * 1024];
    let mut hasher = Sha256::new();
    let mut sent = 0usize;
    while sent < size {
        let take = chunk.len().min(size - sent);
        hasher.update(&chunk[..take]);
        sent += take;
    }
    let hash_hex = hex::encode(hasher.finalize());
    let route_hash = hash_hex.clone();

    let app = axum::Router::new().route(
        "/file.pkg",
        get(move || {
            let fetches = fetches.clone();
            let value = route_hash.clone();
            async move {
                let hash_hex = value.clone();
                fetches.fetch_add(1, Ordering::SeqCst);
                let chunk = vec![0xABu8; 256 * 1024];
                let mut chunks: Vec<Result<bytes::Bytes, io::Error>> = Vec::new();
                let mut sent = 0usize;
                while sent < size {
                    let take = chunk.len().min(size - sent);
                    chunks.push(Ok(bytes::Bytes::copy_from_slice(&chunk[..take])));
                    sent += take;
                }
                let mut builder = axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-length", size)
                    .header("content-type", "application/octet-stream");
                if with_checksum {
                    builder = builder.header("x-checksum-sha256", hash_hex);
                }
                builder
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
    (addr, hash_hex)
}

/// The tee task commits (meta + pin) shortly after the last body byte is
/// forwarded; poll for the pin file so the follow-up HIT is deterministic.
async fn wait_for_pin_file(pin_path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !pin_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "pin file never appeared at {}",
            pin_path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn miss_then_hit_carries_signed_pin() {
    let dir = tempfile::tempdir().unwrap();
    let key = pin_e2e_key(21);
    let state = make_pin_state(dir.path(), key.clone());
    let proxy_addr = spawn_proxy_router(state.clone()).await;

    let size = 64 * 1024;
    let fetches = Arc::new(AtomicUsize::new(0));
    let (upstream, hash_hex) = spawn_checksum_upstream(size, true, fetches.clone()).await;
    let url = format!("http://127.0.0.1:{}/file.pkg", upstream.port());

    // First request: streaming MISS through the tee.
    let (status, body, headers) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("MISS"),
        "first request must be a streaming miss"
    );
    assert_eq!(body.len(), size);

    // Tee commit must have produced the pin sidecar at
    // <cache_dir>/pins/<cache_key>.pin (cache_key from url_to_cache_path).
    let cache_key = "test/file.pkg";
    let pin_path = dir.path().join("pins").join(format!("{}.pin", cache_key));
    wait_for_pin_file(&pin_path).await;

    // Second request: HIT carrying the signed pin.
    let (status, body, headers) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(body.len(), size);
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "HIT must not refetch upstream"
    );

    let pin_header = headers
        .get("x-monopsony-pin")
        .expect("HIT must carry X-Monopsony-Pin");
    let att = monopsony_common::pin_store::from_json(pin_header.to_str().unwrap())
        .expect("X-Monopsony-Pin parses as a PinAttestation");
    assert!(
        att.verify(&key.verifying_key()),
        "pin attestation must verify under the proxy's pin key"
    );
    assert_eq!(att.origin.as_str(), "header", "header origin wins");
    assert_eq!(
        att.expected_sha256, hash_hex,
        "pinned hash must equal the upstream bytes' sha256"
    );
    assert!(
        pin_path.exists(),
        "pin file must exist at <cache_dir>/pins/<cache_key>.pin"
    );
}

#[tokio::test]
async fn hit_with_forged_pin_reports_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let real_key = pin_e2e_key(31);
    let state = make_pin_state(dir.path(), real_key);
    let proxy_addr = spawn_proxy_router(state.clone()).await;

    let size = 64 * 1024;
    let fetches = Arc::new(AtomicUsize::new(0));
    // No checksum header → commit-time Unverified pin; then we forge over it.
    let (upstream, _hash) = spawn_checksum_upstream(size, false, fetches.clone()).await;
    let url = format!("http://127.0.0.1:{}/file.pkg", upstream.port());

    // Plain MISS with pinning enabled (no origin source → Unverified pin).
    let (status, body, headers) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("MISS")
    );
    assert_eq!(body.len(), size);

    let cache_key = "test/file.pkg";
    let pin_path = dir.path().join("pins").join(format!("{}.pin", cache_key));
    wait_for_pin_file(&pin_path).await;

    // Forge: re-sign the committed pin with a DIFFERENT key and write it back.
    let forger = pin_e2e_key(32);
    let original = monopsony_common::pin_store::from_json(
        &std::fs::read_to_string(&pin_path).expect("read committed pin"),
    )
    .expect("committed pin parses");
    let mut forged = original.clone();
    forged.sign(&forger);
    std::fs::write(&pin_path, monopsony_common::pin_store::to_json(&forged))
        .expect("overwrite pin file with forged attestation");

    // HIT: invalid pin must be flagged, not served, and must not block the object.
    let (status, body, headers) = proxy_get(proxy_addr, &url, None).await;
    assert_eq!(status, StatusCode::OK, "server-side pin check is advisory");
    assert_eq!(
        headers.get("x-cache").and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    assert!(
        headers.get("x-monopsony-pin").is_none(),
        "forged pin must NOT ride out as X-Monopsony-Pin"
    );
    assert_eq!(
        headers
            .get("x-monopsony-pin-status")
            .and_then(|v| v.to_str().ok()),
        Some("invalid"),
        "forged pin must surface X-Monopsony-Pin-Status: invalid"
    );
    assert_eq!(body.len(), size);
}
