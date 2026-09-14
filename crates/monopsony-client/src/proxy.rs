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
    http::{Method, Request, Response, StatusCode},
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
    /// Total lifetime cap for a CONNECT tunnel (ms). The relay is torn down
    /// after this even if both ends are still healthy, so a forgotten
    /// tunnel can't pin a connection forever. Default 1 h.
    pub tunnel_max_lifetime_ms: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            server_addr: "127.0.0.1:3128".to_string(),
            timeout_ms: 30_000,
            retry_deadline_ms: 300_000,
            retry_backoff_initial_ms: 2_000,
            tunnel_max_lifetime_ms: 3_600_000,
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

    /// Establish a CONNECT tunnel through the server's forward proxy.
    ///
    /// Opens a raw TCP connection to the server, sends a hand-written
    /// `CONNECT {authority} HTTP/1.1` request, and reads the response head
    /// manually (hyper's http1 handshake API doesn't fit the one-shot
    /// request-then-raw-relay shape of CONNECT). The server proxy enforces
    /// auth/deny/SSRF policy; the client adds nothing.
    ///
    /// On `200` the underlying TcpStream is returned positioned at the start
    /// of the tunneled byte stream, together with any bytes over-read past
    /// the response head (usually empty — the server sends the head alone)
    /// which the caller must prepend to the relay. On any refusal/failure,
    /// Err carries a static marker matching TRANSIENT_503_MARKERS so callers
    /// 503 consistently.
    pub async fn forward_connect_to_server(
        &self,
        authority: &str,
    ) -> Result<(TcpStream, Vec<u8>), &'static str> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Connect TCP to the server's forward proxy, bounded by the
        // per-attempt timeout — a blackholed server must not hang the PM
        // for the OS default (~2 min).
        let mut stream = tokio::time::timeout(
            Duration::from_millis(self.config.timeout_ms),
            TcpStream::connect(&self.config.server_addr),
        )
        .await
        .map_err(|_| "Server proxy connect failed")?
        .map_err(|_| "Server proxy connect failed")?;

        // Tunnels are latency-sensitive (request/response pipelining over
        // TLS); disable Nagle so small h2/TLS records aren't delayed.
        let _ = stream.set_nodelay(true);

        // Send the CONNECT request (RFC 7231 §4.3.6 authority-form).
        let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| "Server proxy send failed")?;

        // Read the response head with a timeout, accumulating bytes until the
        // `\r\n\r\n` terminator. Only the head is buffered (hard-capped at
        // 16 KiB); tunnel bytes stream untouched afterwards.
        let head = tokio::time::timeout(
            Duration::from_millis(self.config.timeout_ms),
            async {
                let mut buf = Vec::with_capacity(256);
                let mut chunk = [0u8; 512];
                loop {
                    let n = stream
                        .read(&mut chunk)
                        .await
                        .map_err(|_| "Server proxy CONNECT failed")?;
                    if n == 0 {
                        // Unexpected EOF before a complete response head.
                        return Err("Server proxy CONNECT failed");
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if find_head_end(&buf).is_some() {
                        return Ok(buf);
                    }
                    if buf.len() > 16 * 1024 {
                        return Err("Server proxy CONNECT failed");
                    }
                }
            },
        )
        .await
        .map_err(|_| "Server proxy CONNECT timed out")??;

        // Split head from any over-read tunnel payload.
        let head_end = find_head_end(&head).unwrap_or(head.len());
        let payload = head[head_end + 4..].to_vec();

        // Parse the status line: `HTTP/1.1 200 ...`
        let status_code = std::str::from_utf8(&head)
            .ok()
            .and_then(|s| s.lines().next())
            .and_then(|line| line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()))
            .ok_or("Server proxy CONNECT failed")?;

        if status_code == 200 {
            debug!(
                "CONNECT tunnel to {} established via server proxy {}",
                authority, self.config.server_addr
            );
            Ok((stream, payload))
        } else {
            let msg = if (400..500).contains(&status_code) {
                "Server proxy refused CONNECT"
            } else {
                "Server proxy CONNECT failed"
            };
            warn!(
                "Server proxy rejected CONNECT to {} ({}): {}",
                authority, status_code, msg
            );
            Err(msg)
        }
    }
}

/// Find the index of the `\r\n\r\n` terminator in an HTTP response head.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
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
    // CONNECT (https tunneling) is intercepted BEFORE the deadline/retry-loop
    // setup: a tunnel is one long-lived bidirectional stream and must never be
    // rebuilt mid-flight. CONNECT URIs are authority-form (`host:port`) —
    // uri.scheme() is None by design, so this must precede any scheme checks.
    if request.method() == Method::CONNECT {
        return handle_connect(state, request).await;
    }

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

/// Handle a CONNECT request by relaying a tunnel through the server proxy.
///
/// Opaque by design: no cache, no tee/spool, no X-Cache header, no body
/// inspection. Establishment failures (server unreachable / CONNECT refused /
/// timeout) return 503 with a TRANSIENT_503_MARKERS-compatible message —
/// there is NO fallback to a direct upstream connection. The server proxy
/// owns auth/deny/SSRF policy; the client adds no filtering of its own.
async fn handle_connect(state: Arc<ClientProxyState>, request: Request<Body>) -> Response<Body> {
    // CONNECT must carry an authority-form request-target (`host:port`).
    // `uri.authority()` is None for absolute-form CONNECT (e.g.
    // `CONNECT http://host:443`), which would forward a garbage target —
    // reject it with 400 rather than relaying malformed syntax upstream.
    let Some(authority) = request.uri().authority().map(|a| a.as_str().to_string()) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from(
                "CONNECT requires authority-form request-target (host:port)",
            ))
            .unwrap_or_else(|_| build_503("Server proxy unavailable - please retry later"));
    };

    match state.forward_connect_to_server(&authority).await {
        Err(msg) => build_503(msg),
        Ok((server_stream, initial_payload)) => {
            // The 200 must be upgrade-capable (empty body); axum::serve then
            // delivers the client side of the h1 upgrade via
            // hyper::upgrade::on, same as the ws_tunnel.rs precedent.
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;

            let tunnel_max_lifetime_ms = state.config.tunnel_max_lifetime_ms;
            tokio::spawn(async move {
                match hyper::upgrade::on(request).await {
                    Ok(upgraded) => {
                        let mut client_io = TokioIo::new(upgraded);
                        // server_stream is a tokio TcpStream: copy_bidirectional
                        // wants tokio AsyncRead/Write on BOTH sides, so the
                        // server side is NOT wrapped in TokioIo. TokioIo is
                        // only needed to bridge hyper's `Upgraded` (hyper
                        // rt::Read/Write → tokio traits).
                        let mut server_io = server_stream;
                        if !initial_payload.is_empty() {
                            use tokio::io::AsyncWriteExt;
                            if let Err(e) = client_io.write_all(&initial_payload).await {
                                debug!("CONNECT tunnel: relaying initial payload failed: {}", e);
                                return;
                            }
                        }
                        // Total-lifetime cap: even a healthy tunnel is torn
                        // down after tunnel_max_lifetime_ms so it can't pin
                        // a connection (and its task) forever.
                        let lifetime = Duration::from_millis(tunnel_max_lifetime_ms);
                        match tokio::time::timeout(
                            lifetime,
                            tokio::io::copy_bidirectional(&mut client_io, &mut server_io),
                        )
                        .await
                        {
                            // Relay finished (either direction closed) inside the cap.
                            Ok(Ok((to_upstream, from_upstream))) => {
                                monopsony_common::metrics::proxy_connect_bytes(to_upstream);
                                monopsony_common::metrics::proxy_connect_bytes(from_upstream);
                                debug!(
                                    "CONNECT tunnel to {} closed ({to_upstream} up / {from_upstream} down)",
                                    authority
                                );
                            }
                            // Relay errored inside the cap.
                            Ok(Err(e)) => {
                                debug!("CONNECT tunnel to {} ended: {}", authority, e)
                            }
                            // Lifetime cap hit — tear the tunnel down.
                            Err(_) => {
                                warn!(
                                    "CONNECT tunnel to {} exceeded max lifetime ({}ms) — closing",
                                    authority, tunnel_max_lifetime_ms
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // Client (PM) never completed the upgrade — nothing
                        // to relay; drop the upstream connection.
                        debug!("CONNECT upgrade not completed by client: {}", e);
                    }
                }
            });

            response
        }
    }
}

pub async fn run_proxy(listen_addr: SocketAddr, config: ProxyConfig) -> anyhow::Result<()> {
    let state = Arc::new(ClientProxyState::new(config));

    let app = axum::Router::new()
        .route("/*path", axum::routing::any(proxy_handler))
        .route("/", axum::routing::any(proxy_handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    info!("Client proxy listening on {}", listen_addr);

    // Manual serve loop: CONNECT is intercepted before the Router (see the
    // Content-Length note in ClientHyperService); everything else goes
    // through it untouched.
    loop {
        let (stream, _peer) = listener.accept().await?;
        // Tunnels are latency-sensitive; Nagle's algorithm adds up to
        // ~40ms per direction-switch. axum::serve used to set this for us.
        let _ = stream.set_nodelay(true);
        let svc = ClientHyperService {
            router: app.clone(),
            state: state.clone(),
        };
        tokio::spawn(async move {
            let _ = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            )
            .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), svc)
            .await;
        });
    }
}

/// Thin hyper service: intercepts CONNECT before the axum Router, forwards
/// everything else into the Router unchanged. The bypass is required
/// because axum 0.7's RouteFuture injects Content-Length into every
/// response, which hyper's h1 encoder forbids on a 2xx-to-CONNECT — the
/// encode error would kill the tunnel.
struct ClientHyperService {
    router: axum::Router,
    state: Arc<ClientProxyState>,
}

impl hyper::service::Service<Request<hyper::body::Incoming>> for ClientHyperService {
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Self::Response, Self::Error>>
                + Send,
        >,
    >;

    fn call(&self, req: Request<hyper::body::Incoming>) -> Self::Future {
        if req.method() == Method::CONNECT {
            let (parts, incoming) = req.into_parts();
            let req = Request::from_parts(parts, Body::new(incoming));
            let state = self.state.clone();
            Box::pin(async move { Ok(handle_connect(state, req).await) })
        } else {
            let mut router = self.router.clone();
            Box::pin(async move {
                let (parts, incoming) = req.into_parts();
                let req = Request::from_parts(parts, Body::new(incoming));
                use tower::Service as _; // Router<()>: tower Service, Error = Infallible
                let res = router.call(req).await.unwrap_or_else(|e| match e {});
                Ok(res)
            })
        }
    }
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
