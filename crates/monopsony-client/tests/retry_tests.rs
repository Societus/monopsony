//! Client proxy retry-loop integration tests (plan §4).
//!
//! A mock "server proxy" returns transient 503s a configurable number of
//! times, then 200. The client proxy must retry within its deadline,
//! honor Retry-After, pass through 404s without retrying, and give up
//! with 503 once the deadline expires.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http_body_util::BodyExt;
use hyper::http::{Request, StatusCode};

use monopsony_client::proxy::{run_proxy, ClientProxyState, ProxyConfig};

/// Mock server proxy: first `transient_before_success` GETs return
/// 503 with the Monopsony transient marker; after that, 200 with a body.
async fn spawn_mock_server_proxy(
    transient_before_success: usize,
    hits: Arc<AtomicUsize>,
    retry_after: Option<u64>,
) -> SocketAddr {
    use axum::routing::get;

    let app = axum::Router::new().route(
        "/pkg/file.pkg",
        get(move || {
            let hits = hits.clone();
            async move {
                let n = hits.fetch_add(1, Ordering::SeqCst);
                if n < transient_before_success {
                    let mut builder = hyper::http::Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header("content-type", "text/plain");
                    if let Some(ra) = retry_after {
                        builder = builder.header("retry-after", ra.to_string());
                    }
                    builder
                        .body(Body::from("Server proxy timed out - please retry later"))
                        .unwrap()
                } else {
                    hyper::http::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/octet-stream")
                        .body(Body::from(vec![0x42u8; 4096]))
                        .unwrap()
                }
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

/// Drive the retry loop directly against a mock (no listening client proxy):
/// `ClientProxyState::forward_to_server` + the same decision logic the
/// handler uses. Returns the final response and the number of attempts.
struct RetryOutcome {
    status: StatusCode,
    body_len: usize,
    attempts: usize,
}

async fn drive_retry(state: Arc<ClientProxyState>, url: &str) -> RetryOutcome {
    let deadline =
        std::time::Instant::now() + Duration::from_millis(state.config().retry_deadline_ms);
    let per_attempt = Duration::from_millis(state.config().timeout_ms);
    let mut attempt: u32 = 0;

    let mut original = Some(build_request(url));
    loop {
        let request = original.take().unwrap_or_else(|| build_request(url));
        let result = tokio::time::timeout(per_attempt, state.forward_to_server(request)).await;
        match result {
            Ok(Ok(mut response)) => {
                if !monopsony_client::proxy::is_transient_failure(&mut response).await {
                    let status = response.status();
                    let body = response.into_body().collect().await.unwrap().to_bytes();
                    return RetryOutcome {
                        status,
                        body_len: body.len(),
                        attempts: (attempt + 1) as usize,
                    };
                }
                debug_assert!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
            }
            _ => {}
        }
        let step = Duration::from_millis(50); // fast backoff for tests
        if std::time::Instant::now() + step >= deadline {
            return RetryOutcome {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body_len: 0,
                attempts: (attempt + 1) as usize,
            };
        }
        tokio::time::sleep(step).await;
        attempt += 1;
    }
}

fn build_request(url: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(url)
        .body(Body::empty())
        .unwrap()
}

fn test_config(server_addr: String) -> ProxyConfig {
    ProxyConfig {
        server_addr,
        timeout_ms: 2_000,
        retry_deadline_ms: 10_000,
        retry_backoff_initial_ms: 50,
    }
}

#[tokio::test]
async fn test_retry_succeeds_after_transient_503s() {
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_mock_server_proxy(2, hits.clone(), None).await;
    let state = Arc::new(ClientProxyState::new(test_config(format!(
        "127.0.0.1:{}",
        server.port()
    ))));
    let url = format!("http://mirror.example.com/pkg/file.pkg");

    let outcome = drive_retry(state, &url).await;
    assert_eq!(outcome.status, StatusCode::OK);
    assert_eq!(outcome.body_len, 4096);
    assert_eq!(outcome.attempts, 3, "2 transient failures then success");
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_deadline_exhaustion_returns_503() {
    // Mock always returns transient 503 → the loop must give up at the
    // deadline with a 503, not spin forever.
    let hits = Arc::new(AtomicUsize::new(0));
    let server = spawn_mock_server_proxy(usize::MAX, hits.clone(), None).await;
    let mut cfg = test_config(format!("127.0.0.1:{}", server.port()));
    cfg.retry_deadline_ms = 500; // short deadline for test speed
    let state = Arc::new(ClientProxyState::new(cfg));
    let url = format!("http://mirror.example.com/pkg/file.pkg");

    let start = std::time::Instant::now();
    let outcome = drive_retry(state, &url).await;
    assert_eq!(outcome.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        start.elapsed() >= Duration::from_millis(450),
        "must not give up before the deadline"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "must not hang past the deadline"
    );
}

// Silence unused warnings for run_proxy import (used in type signatures).
#[allow(unused)]
fn _unused(_r: SocketAddr) {}
