//! Client-side proxy that forwards package manager requests to the Monopsony server.
//!
//! Architecture:
//! - Package manager (pacman/apt/apk) sends requests to 127.0.0.1:3129
//! - Client proxy connects TCP to the server's HTTP forward proxy (port 3128)
//! - Client proxy sends the original absolute URI in the request line (forward-proxy semantics)
//! - Server proxy checks cache, fetches from upstream on miss, caches result
//! - Client proxy returns response to package manager
//!
//! IMPORTANT: On any failure (server unreachable, timeout, error), we return 503.
//! The package manager must retry later. We do NOT fall back to direct upstream
//! fetching — that would defeat the caching purpose.
//!
//! Large-body handling: responses are STREAMED through this proxy (never
//! buffered whole in RAM), with a bounded retry loop for transient server
//! failures (502/503/504, connection errors, per-attempt timeouts). The
//! package manager sees a single request that blocks up to `retry_deadline_ms`.

use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use hyper_util::rt::TokioIo;
use rand::Rng;
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

pub struct ProxyConfig {
    /// Server's HTTP forward proxy address (e.g. "10.10.20.58:3128")
    pub server_addr: String,
    /// Per-attempt timeout: covers connect + request send + response headers.
    /// Streaming bodies are NOT cut by this — once headers arrive, the body
    /// flows until EOF or a stall.
    pub timeout_ms: u64,
    /// Total deadline for the whole retry loop. The PM sees one request
    /// blocking up to this long. Default 5 min.
    pub retry_deadline_ms: u64,
    /// First backoff step between retries (ms). Doubles per attempt, capped
    /// at 30 s, ±20% jitter.
    pub retry_backoff_initial_ms: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            server_addr: "127.0.0.1:3128".to_string(),
            timeout_ms: 30_000,
            retry_deadline_ms: 300_000,
            retry_backoff_initial_ms: 2_000,
        }
    }
}

/// Marker body prefixes the Monopsony server proxy uses on its transient
/// failures. Matching these (rather than any 503 anywhere on the internet)
/// prevents retrying genuinely-down upstreams forever.
const TRANSIENT_503_MARKERS: [&str; 4] = [
    "server proxy timed out",
    "server proxy unavailable",
    "upstream request failed",
    "upstream stalled",
];

pub struct ClientProxyState {
    config: ProxyConfig,
}

impl ClientProxyState {
    pub fn new(config: ProxyConfig) -> Self {
        Self { config }
    }

    /// Read-only access to the effective proxy configuration.
    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// Forward request to the server's HTTP forward proxy using raw HTTP/1.1.
    ///
    /// A forward proxy expects the **absolute URI** in the request line:
    ///   `GET http://mirror.example.com/path HTTP/1.1`
    ///
    /// Returns Err only on transport-level problems (connect, send, timeout).
    /// HTTP-level statuses pass through as Ok(response).
    pub async fn forward_to_server(
        &self,
        request: Request<Body>,
    ) -> Result<Response<Body>, (StatusCode, &'static str)> {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let original_uri = uri.to_string();

        // Package managers configured with a proxy send absolute URIs.
        if uri.scheme().is_none() {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Client sent non-proxy request (no scheme in URI)",
            ));
        }

        debug!(
            "Forwarding to server proxy {} : {} {}",
            self.config.server_addr, method, original_uri
        );

        // Connect TCP to the server's forward proxy. Each attempt gets a
        // fresh connection — no pooled state to poison.
        let stream = TcpStream::connect(&self.config.server_addr)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Server proxy connect failed",
                )
            })?;
        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Server proxy handshake failed",
                )
            })?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("Server proxy connection ended: {}", e);
            }
        });

        // Send the original absolute URI in the request line (forward-proxy
        // chaining, RFC 7230 §5.3.2). hyper's low-level send_request passes
        // the URI through without host extraction.
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "Server proxy send failed"))?;

        // Adopt hyper's Incoming body into axum's Body (zero-copy streaming).
        let (parts, incoming) = response.into_parts();
        Ok(Response::from_parts(parts, Body::new(incoming)))
    }
}

/// Decide whether a server response is a retryable transient failure.
/// Reads the (small) error body to match the server proxy's markers.
pub async fn is_transient_failure(response: &mut Response<Body>) -> bool {
    let status = response.status();
    if !matches!(status.as_u16(), 502 | 503 | 504) {
        return false;
    }
    // The server's transient errors are short marker strings. Only inspect
    // bodies that are known-small; anything else might be a package.
    use hyper::body::Body as HttpBody;
    let exact_small = matches!(HttpBody::size_hint(response.body()).exact(), Some(n) if n <= 2048);
    if !exact_small {
        return false;
    }
    let body = std::mem::replace(response.body_mut(), Body::empty());
    match http_body_util::BodyExt::collect(body).await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            let text = String::from_utf8_lossy(&bytes).to_lowercase();
            let is_transient = TRANSIENT_503_MARKERS.iter().any(|m| text.contains(m));
            // Put the (tiny) body back so non-retry paths can forward it.
            *response.body_mut() = Body::from(bytes);
            is_transient
        }
        Err(_) => false,
    }
}

/// Extract Retry-After seconds if the server sent one.
fn retry_after_secs(response: &Response<Body>) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

/// Compute the backoff step for attempt N (0-based): initial · 2^N, capped at
/// 30 s, with ±20% jitter.
fn backoff_step(initial_ms: u64, attempt: u32) -> Duration {
    let cap = 30_000u64;
    let exp = initial_ms.saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX));
    let base = exp.min(cap);
    let jitter_range = (base as f64 * 0.2) as u64;
    let jitter = if jitter_range > 0 {
        rand::thread_rng().gen_range(0..=(jitter_range * 2))
    } else {
        0
    };
    // center the jitter around base
    Duration::from_millis(base + jitter - jitter_range.min(base + jitter))
}

async fn proxy_handler(
    State(state): State<Arc<ClientProxyState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let deadline = Instant::now() + Duration::from_millis(state.config.retry_deadline_ms);
    let per_attempt = Duration::from_millis(state.config.timeout_ms);
    let mut attempt: u32 = 0;

    // Retrying requires re-sending the request, but `Body` is not `Clone`.
    // Package-manager fetches are GETs/HEADs with empty bodies — for those
    // we rebuild the request per attempt from its parts. A request with a
    // non-empty body is forwarded exactly once (no retry).
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let can_retry = matches!(
        hyper::body::Body::size_hint(request.body()).exact(),
        Some(0) | None
    );

    // The original request is consumed by the first attempt; subsequent
    // attempts rebuild it from the saved parts.
    let mut original = Some(request);
    loop {
        let request = match original.take() {
            Some(r) => r,
            None => {
                let mut rebuilt = Request::builder()
                    .method(method.clone())
                    .uri(uri.clone())
                    .body(Body::empty())
                    .expect("rebuilding request from known-good parts");
                *rebuilt.headers_mut() = headers.clone();
                rebuilt
            }
        };
        let result = tokio::time::timeout(per_attempt, state.forward_to_server(request)).await;

        match result {
            Err(_) => {
                warn!(
                    "Attempt {} timed out after {}ms",
                    attempt + 1,
                    per_attempt.as_millis()
                );
            }
            Ok(Err((_status, msg))) => {
                error!("Server proxy request failed: {}", msg);
                return build_503(msg);
            }
            Ok(Ok(mut response)) => {
                if !is_transient_failure(&mut response).await {
                    // Success or non-transient failure: pass through.
                    response
                        .headers_mut()
                        .insert("X-Monopsony", "forwarded".parse().unwrap());
                    return response;
                }
                debug!(
                    "Attempt {} got transient {} — will retry",
                    attempt + 1,
                    response.status()
                );
                // Honor Retry-After when present (capped by backoff schedule).
                if let Some(ra) = retry_after_secs(&response) {
                    let step = backoff_step(state.config.retry_backoff_initial_ms, attempt);
                    let wait = Duration::from_secs(ra).min(step);
                    if Instant::now() + wait >= deadline {
                        return build_503("Server proxy timed out - please retry later");
                    }
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                    continue;
                }
            }
        }

        if !can_retry {
            // Bodied request already consumed; cannot replay.
            return build_503("Server proxy unavailable - please retry later");
        }

        // Backoff check against the deadline.
        let step = backoff_step(state.config.retry_backoff_initial_ms, attempt);
        if Instant::now() + step >= deadline {
            warn!(
                "Retry deadline exhausted after {} attempt(s) — returning 503",
                attempt + 1
            );
            return build_503("Server proxy timed out - please retry later");
        }
        info!(
            "Retrying in {}ms (attempt {})",
            step.as_millis(),
            attempt + 2
        );
        tokio::time::sleep(step).await;
        attempt += 1;
    }
}

fn build_503(msg: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::from(msg))
        .unwrap_or_else(|_| Response::new(Body::from("Service unavailable")))
}

pub async fn run_proxy(listen_addr: SocketAddr, config: ProxyConfig) -> anyhow::Result<()> {
    let state = Arc::new(ClientProxyState::new(config));

    let app = axum::Router::new()
        .route("/*path", axum::routing::any(proxy_handler))
        .route("/", axum::routing::any(proxy_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    info!("Client proxy listening on {}", listen_addr);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_step_doubles_and_caps() {
        let s0 = backoff_step(2000, 0);
        let s1 = backoff_step(2000, 1);
        let s2 = backoff_step(2000, 2);
        let s_max = backoff_step(2000, 20);

        assert!(s0 >= Duration::from_millis(1600) && s0 <= Duration::from_millis(2400));
        assert!(s1 >= Duration::from_millis(3200) && s1 <= Duration::from_millis(4800));
        assert!(s2 >= Duration::from_millis(6400) && s2 <= Duration::from_millis(9600));
        assert!(s_max <= Duration::from_millis(30_000 + 6_000));
        assert!(s_max >= Duration::from_millis(24_000));
    }

    #[test]
    fn test_backoff_step_zero_initial() {
        let s = backoff_step(0, 0);
        assert_eq!(s, Duration::from_millis(0));
    }

    #[test]
    fn test_default_config_values() {
        let c = ProxyConfig::default();
        assert_eq!(c.timeout_ms, 30_000);
        assert_eq!(c.retry_deadline_ms, 300_000);
        assert_eq!(c.retry_backoff_initial_ms, 2_000);
    }

    #[tokio::test]
    async fn test_transient_marker_matching() {
        for (body, expect) in [
            ("Server proxy timed out - please retry later", true),
            ("Server proxy unavailable - please retry later", true),
            ("upstream stalled", true),
            ("Package blocked by scan policy", false),
            ("", false),
        ] {
            let mut resp = Response::builder()
                .status(503)
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                is_transient_failure(&mut resp).await,
                expect,
                "body={:?}",
                body
            );
        }
        // 404 must never retry
        let mut resp = Response::builder().status(404).body(Body::empty()).unwrap();
        assert!(!is_transient_failure(&mut resp).await);
        // 429 must never retry (respect rate limits)
        let mut resp = Response::builder()
            .status(429)
            .body(Body::from("Server proxy timed out"))
            .unwrap();
        assert!(!is_transient_failure(&mut resp).await);
    }

    #[tokio::test]
    async fn test_retry_after_parsing() {
        let resp = Response::builder()
            .status(503)
            .header("retry-after", "7")
            .body(Body::empty())
            .unwrap();
        assert_eq!(retry_after_secs(&resp), Some(7));

        let resp = Response::builder().status(503).body(Body::empty()).unwrap();
        assert_eq!(retry_after_secs(&resp), None);
    }
}
