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
    /// Pin verification (Sprint 20 Phase 3). Inert until the daemon
    /// provisions the server's mirror verify key; shared via Arc so the
    /// daemon can rotate the key without rebuilding the proxy.
    pub pin: std::sync::Arc<crate::pin_verify::PinState>,
    /// Monotonic ledger + downgrade enforcement (Sprint 20 Phase 4).
    /// None = disabled (fail-open availability; feature is off).
    pub ledger: Option<std::sync::Arc<crate::ledger::Ledger>>,
    /// Ledger policy from the [ledger] config section (warn | reject).
    pub ledger_policy: String,
    /// Warn-only index staleness threshold (0 = disabled).
    pub ledger_max_index_age_secs: u64,
    /// Oracle floor enforcement (Sprint 21 slice 21-5b).
    /// None = disabled (feature off; enforcement inert).
    pub floor_store: Option<std::sync::Arc<std::sync::RwLock<crate::floor_guard::FloorStore>>>,
}

impl ClientProxyState {
    pub fn new(config: ProxyConfig) -> Self {
        let events_path = dirs_events_path();
        Self {
            config,
            pin: std::sync::Arc::new(crate::pin_verify::PinState::new(None, events_path)),
            ledger: None,
            ledger_policy: "warn".to_string(),
            ledger_max_index_age_secs: 0,
            floor_store: None,
        }
    }

    pub fn with_pin(mut self, pin: std::sync::Arc<crate::pin_verify::PinState>) -> Self {
        self.pin = pin;
        self
    }

    /// Attach the ledger + policy knobs (Sprint 20 Phase 4). Called by the
    /// daemon when [ledger] enabled; omitted → enforcement inert.
    pub fn with_ledger(
        mut self,
        ledger: std::sync::Arc<crate::ledger::Ledger>,
        policy: String,
        max_index_age_secs: u64,
    ) -> Self {
        self.ledger = Some(ledger);
        self.ledger_policy = policy;
        self.ledger_max_index_age_secs = max_index_age_secs;
        self
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
        let head = tokio::time::timeout(Duration::from_millis(self.config.timeout_ms), async {
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
        })
        .await
        .map_err(|_| "Server proxy CONNECT timed out")??;

        // Split head from any over-read tunnel payload.
        let head_end = find_head_end(&head).unwrap_or(head.len());
        let payload = head[head_end + 4..].to_vec();

        // Parse the status line: `HTTP/1.1 200 ...`
        let status_code = std::str::from_utf8(&head)
            .ok()
            .and_then(|s| s.lines().next())
            .and_then(|line| {
                line.split_whitespace()
                    .nth(1)
                    .and_then(|c| c.parse::<u16>().ok())
            })
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
                    // Pin verification (Sprint 20 Phase 3): wrap 200 payload
                    // bodies that carry a signed pin. HEAD (no body) and
                    // 206 partial responses are skipped — a partial body's
                    // hash cannot match the whole-object pin.
                    if method != Method::HEAD && response.status() == StatusCode::OK {
                        // Ledger first (Sprint 20 Phase 4): index/payload
                        // enforcement may 503 or add headers; the surviving
                        // response then goes through the pin stream check
                        // (pin check wraps THIS function's result — for
                        // ledger-modified bodies the pin still verifies the
                        // exact bytes handed to the PM).
                        let resp = wrap_with_ledger_check(state.clone(), &uri, response).await;
                        return wrap_with_pin_check(state, &uri, resp);
                    }
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

/// Canonical URL for pin attestation + ledger repo keys — MUST mirror the
/// server proxy's `normalize_url_scheme` EXACTLY (string prefix swap on
/// the full absolute-form URL, path and query preserved) so client-side
/// URLs are byte-identical to the server's cache-entry keys. The previous
/// authority-only form for http URIs broke both pin verification and
/// repo_key derivation for http mirrors (the primary proxy deployment).
fn pin_url(uri: &hyper::Uri) -> String {
    let s = uri.to_string();
    match s.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => s,
    }
}

/// Wrap a 200 response body in the hash-checking stream when a signed pin
/// header is present, provisioned, and verifies. Outcomes:
/// - no/invalid pin header → pass through untouched (server-side decision;
///   `X-Monopsony-Pin-Status: invalid` is logged as a tamper event)
/// - pin verifies → stream-hash the body; mismatch truncates + spools event
/// - pinning unprovisioned → pass through untouched (inert)
fn wrap_with_pin_check(
    state: Arc<ClientProxyState>,
    uri: &hyper::Uri,
    response: Response<Body>,
) -> Response<Body> {
    let url = pin_url(uri);

    // Oracle floor enforcement (Sprint 21 slice 21-5b): reject downloads
    // below the oracle floor once the repo class's grace window has
    // elapsed. Fail-open: feature off / no floor match / undeterminable
    // version all pass through.
    if let Some(store) = &state.floor_store {
        // Observed age comes from the floor's own observed_at (carried from
        // the oracle delta); the wall clock only anchors `now`.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let guard = store.read().unwrap_or_else(|e| e.into_inner());
        if let crate::floor_guard::FloorVerdict::BelowFloor {
            package_key,
            version,
            floor,
        } = guard.check_url(&url, now)
        {
            warn!(
                "Floor block: {} served {} below oracle floor {}",
                url, version, floor
            );
            let body = serde_json::json!({
                "error": "below_oracle_floor",
                "package_key": package_key,
                "floor": floor,
                "version": version,
            });
            return Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| Response::new(Body::from("{\"error\":\"below_oracle_floor\"}")));
        }
    }

    let (mut parts, body) = response.into_parts();

    let Some(pin_json) = parts
        .headers
        .get("x-monopsony-pin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    else {
        // No pin on this object (origin had no integrity source, or server
        // pinning disabled) — nothing to enforce.
        return Response::from_parts(parts, body);
    };

    let att = match state.pin.verify_attestation_json(&pin_json, &url) {
        Ok(att) => att,
        Err(reason) => {
            // Invalid/unprovisioned pin. Unprovisioned is normal (feature
            // not yet paired); invalid is worth an event + warn.
            if state.pin.provisioned() {
                warn!("Invalid pin for {}: {}", url, reason);
                state.pin.record_event(&crate::pin_verify::TamperEvent {
                    url: url.clone(),
                    expected_sha256: String::new(),
                    actual_sha256: String::new(),
                    detail: format!("invalid pin attestation: {}", reason),
                    observed_at: chrono::Utc::now().timestamp(),
                });
            }
            return Response::from_parts(parts, body);
        }
    };

    let expected = att.expected_sha256.clone();
    let data_stream = body.into_data_stream();
    let checked =
        crate::pin_verify::pin_check_stream(data_stream, state.pin.clone(), url, expected);
    // Re-frame for enforcement (fleet verification 2026-09-16, Bug 2): the
    // hash verdict lands at END of stream. If `content-length` survives, the
    // h1 encoder stops polling the body once that many bytes are written —
    // the terminal poll NEVER runs and the check silently no-ops. Drop the
    // length headers; hyper re-frames an unknown-length body as chunked, so
    // a mismatch's synthetic error chunk aborts the transfer mid-flight and
    // the package manager sees a failed download instead of a complete but
    // tampered file.
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
    parts
        .headers
        .insert("X-Monopsony-Verified", "pending".parse().expect("static"));
    Response::from_parts(parts, Body::from_stream(checked))
}

/// Default JSONL spool path for tamper events under the client state dir.
fn dirs_events_path() -> std::path::PathBuf {
    let base = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/state/monopsony"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/monopsony"));
    base.join("tamper-events.jsonl")
}

// ---- Sprint 20 Phase 4: ledger enforcement (index + payload) ----

/// Hard cap on buffered index bodies (pacman .db can reach tens of MB;
/// 256 MB covers every current repo DB with margin).
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// PM identity used in ledger package keys, derived from the URL (host/path
/// shapes — NOT full backend instantiation). Unknown → enforcement skipped.
fn pm_kind_from_url(uri: &hyper::Uri) -> Option<&'static str> {
    let host = uri.host().unwrap_or("").to_ascii_lowercase();
    let path = uri.path();
    if host.contains("archlinux") || path.contains(".pkg.tar") || path.ends_with(".db") {
        return Some("pacman");
    }
    if host.contains("alpine") || path.contains("APKINDEX") {
        return Some("apk");
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last == "primary.xml.gz" || last == "primary.xml" {
        return Some("dnf");
    }
    // Release.gpg deliberately falls through: it is a signature, not an index.
    if last == "InRelease" || last == "Release" {
        return Some("apt");
    }
    None
}

/// Best-effort (name, version) from a pacman payload filename
/// (`bash-5.2.38-1-x86_64.pkg.tar.zst`). Returns None when the shape does
/// not parse unambiguously — enforcement is skipped, never guessed.
fn pacman_payload_info(path: &str) -> Option<(String, String)> {
    let fname = path.rsplit('/').next()?;
    let stem = fname.split(".pkg.tar").next()?;
    if stem == fname || stem.is_empty() {
        return None;
    }
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    let arch = parts[parts.len() - 1];
    let rel = parts[parts.len() - 2];
    let ver = parts[parts.len() - 3];
    if arch.is_empty()
        || !rel.bytes().all(|b| b.is_ascii_digit())
        || !ver.bytes().next()?.is_ascii_digit()
    {
        return None;
    }
    let name = parts[..parts.len() - 3].join("-");
    if name.is_empty() {
        return None;
    }
    // Re-join version-pkgrel so normalize() splits the release back out.
    Some((name, format!("{}-{}", ver, rel)))
}

/// Ledger enforcement for 200 GET responses (Sprint 20 Phase 4).
///
/// - Index responses (`.db`, APKINDEX): buffer (capped), parse off-thread,
///   feed every entry through the high-water ledger. Downgrades and parse
///   errors follow the configured policy: "reject" → 503 + tamper event,
///   "warn" → pass + `X-Monopsony-Downgrade` / error header + event.
/// - Payload responses parseable to (name, version): read-only cross-check
///   against the held high-water (indexes are the only writers).
/// - Ledger absent / URL unrecognized → untouched (feature inert).
///
/// Ordering with the pin check: the PIN check wraps the response this
/// returns (call site), so pinned index bodies are still stream-hashed —
/// a reconstituted body carries the same bytes.
async fn wrap_with_ledger_check(
    state: std::sync::Arc<ClientProxyState>,
    uri: &hyper::Uri,
    response: Response<Body>,
) -> Response<Body> {
    let Some(ledger) = state.ledger.clone() else {
        return response;
    };
    let pm = match pm_kind_from_url(uri) {
        Some(pm) => pm,
        None => return response,
    };

    let canonical = pin_url(uri); // https-canonical form (repo_key / source)
    let is_index = crate::index_parse::looks_like_index(uri.path());
    let payload = if is_index {
        None
    } else {
        pacman_payload_info(uri.path())
    };

    // ---- Payload cross-check (read-only; no body buffering) ----
    if let Some((name, version)) = payload {
        let key = crate::index_parse::package_key_for(pm, &name);
        let held = {
            let ledger = ledger.clone();
            let key = key.clone();
            tokio::task::spawn_blocking(move || ledger.high_water_for(&key))
                .await
                .ok()
                .flatten()
        };
        let Some(held) = held else { return response };
        let offered = monopsony_common::version_ord::normalize(&version);
        let held_norm = monopsony_common::version_ord::normalize(&held);
        if monopsony_common::version_ord::vercmp(&offered, &held_norm) != std::cmp::Ordering::Less {
            return response;
        }
        let detail = format!(
            "downgrade: payload {} offered {} below held {}",
            key, version, held
        );
        warn!("{}", detail);
        state.pin.record_event(&crate::pin_verify::TamperEvent {
            url: canonical,
            expected_sha256: String::new(),
            actual_sha256: String::new(),
            detail,
            observed_at: chrono::Utc::now().timestamp(),
        });
        if state.ledger_policy == "reject" {
            return build_503("package version rewound - held by client ledger");
        }
        let (mut parts, body) = response.into_parts();
        parts.headers.insert(
            "X-Monopsony-Downgrade",
            format!("{}={};held={}", key, version, held)
                .parse()
                .expect("header value"),
        );
        return Response::from_parts(parts, body);
    }

    if !is_index {
        return response;
    }

    // ---- Index path: buffer → parse → ledger (off-thread) ----
    let (mut parts, body) = response.into_parts();
    use http_body_util::BodyExt;
    let collected = match BodyExt::collect(body).await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            warn!("Index body read failed for {}: {}", uri, e);
            return build_503("index body read failed");
        }
    };
    // Cap AFTER collect: warn policy must be able to serve the bytes it has;
    // decompression bombs are separately bounded inside parse_index (256 MB
    // decode cap / 100k entries).
    if collected.len() as u64 > MAX_INDEX_BYTES {
        let outcome = IndexOutcome::Oversize;
        return ledger_index_verdict_response(state, outcome, parts, collected.to_vec());
    }

    let repo_key = crate::index_parse::repo_key_from_url(&canonical);
    let max_age = state.ledger_max_index_age_secs;
    let parse_bytes = collected.to_vec();
    let canonical_for_task = canonical.clone();
    let join = tokio::task::spawn_blocking(move || {
        let canonical = canonical_for_task;
        let now = chrono::Utc::now().timestamp();
        // Staleness v1: previous age BEFORE this response refreshes it.
        let prev_age = ledger.repo_age_secs(&repo_key);
        let parsed = crate::index_parse::parse_index(&parse_bytes);
        let mut apt_valid_until_expired: Option<i64> = None;
        let entries = match &parsed {
            Ok(p) => match p {
                crate::index_parse::ParsedIndex::Pacman { entries } => entries.clone(),
                crate::index_parse::ParsedIndex::Apk { entries } => entries.clone(),
                crate::index_parse::ParsedIndex::Dnf { entries } => entries.clone(),
                crate::index_parse::ParsedIndex::Apt {
                    entries,
                    valid_until,
                } => {
                    // Native apt expiry: route through the SAME downgrade
                    // machinery (policy split applies verbatim downstream).
                    if let Some(vu) = valid_until {
                        if now > *vu {
                            apt_valid_until_expired = Some(*vu);
                        }
                    }
                    entries.clone()
                }
            },
            Err(_) => Vec::new(),
        };
        let mut downgrades: Vec<(String, String, String)> = Vec::new();
        if let Some(vu) = apt_valid_until_expired {
            downgrades.push((
                "apt:valid-until".to_string(),
                format!("expired-at={}", vu),
                format!("now={}", now),
            ));
        }
        if parsed.is_ok() {
            let _ = ledger.repo_seen(&repo_key, now);
            for entry in &entries {
                let key = crate::index_parse::package_key_for(pm, &entry.name);
                let norm = monopsony_common::version_ord::normalize(&entry.version);
                if let Ok(crate::ledger::CheckVerdict::Downgrade { held }) =
                    ledger.check_and_update(&key, &entry.version, &norm, &canonical, now)
                {
                    downgrades.push((key, entry.version.clone(), held));
                }
            }
        }
        (parsed, downgrades, prev_age, now)
    })
    .await;

    let (parsed, downgrades, prev_age, now) = match join {
        Ok(v) => v,
        Err(e) => {
            // Join failure = panic inside parse; parse_index is panic-free by
            // contract, so this is a poisoned worker — fail-open (serve) and
            // record an event so it is not silent.
            warn!("Ledger task panicked: {}", e);
            state.pin.record_event(&crate::pin_verify::TamperEvent {
                url: canonical,
                expected_sha256: String::new(),
                actual_sha256: String::new(),
                detail: "ledger: index task join failure".to_string(),
                observed_at: chrono::Utc::now().timestamp(),
            });
            parts.headers.insert(
                "X-Monopsony-Index-Parse-Error",
                "1".parse().expect("static"),
            );
            return Response::from_parts(parts, Body::from(collected));
        }
    };

    // Staleness header (warn-only v1).
    if max_age > 0 {
        if let Some(age) = prev_age {
            if age > max_age as i64 {
                parts.headers.insert(
                    "X-Monopsony-Index-Stale",
                    age.to_string().parse().expect("digits"),
                );
            }
        }
    }

    let outcome = match parsed {
        Err(e) => IndexOutcome::ParseError(e.to_string()),
        Ok(_) if downgrades.is_empty() => IndexOutcome::Clean,
        Ok(_) => IndexOutcome::Downgrades(downgrades),
    };
    let _ = now;
    ledger_index_verdict_response(state, outcome, parts, collected.to_vec())
}

/// Index-path outcomes for [`ledger_index_verdict_response`].
enum IndexOutcome {
    Clean,
    Oversize,
    ParseError(String),
    Downgrades(Vec<(String, String, String)>), // (key, offered, held)
}

/// Apply the warn/reject policy to an index outcome and rebuild the response.
/// `body` is the buffered index bytes (empty for reject/oversize, which do
/// not forward the body).
fn ledger_index_verdict_response(
    state: std::sync::Arc<ClientProxyState>,
    outcome: IndexOutcome,
    mut parts: axum::http::response::Parts,
    body: Vec<u8>,
) -> Response<Body> {
    let reject = state.ledger_policy == "reject";
    match outcome {
        IndexOutcome::Clean => Response::from_parts(parts, Body::from(body)),
        IndexOutcome::Oversize => {
            // Availability policy: oversize indexes are a capacity event, not
            // proof of tampering — warn-policy passes them through unserved?
            // No: the body is already buffered; forwarding is impossible
            // without re-fetch. Treat as parse-error-class (policy split).
            let detail = "index-parse-error: index exceeds 256MB cap".to_string();
            state.pin.record_event(&crate::pin_verify::TamperEvent {
                url: String::new(),
                expected_sha256: String::new(),
                actual_sha256: String::new(),
                detail,
                observed_at: chrono::Utc::now().timestamp(),
            });
            if reject {
                build_503("package index rejected - size over ledger cap")
            } else {
                build_503("index over ledger cap - refetch with smaller index")
            }
        }
        IndexOutcome::ParseError(e) => {
            let detail = format!("index-parse-error: {}", e);
            warn!("{}", detail);
            state.pin.record_event(&crate::pin_verify::TamperEvent {
                url: String::new(),
                expected_sha256: String::new(),
                actual_sha256: String::new(),
                detail,
                observed_at: chrono::Utc::now().timestamp(),
            });
            if reject {
                build_503("package index failed parsing - rejected by ledger policy")
            } else {
                parts.headers.insert(
                    "X-Monopsony-Index-Parse-Error",
                    "1".parse().expect("static"),
                );
                Response::from_parts(parts, Body::from(body))
            }
        }
        IndexOutcome::Downgrades(list) => {
            let summary: Vec<String> = list
                .iter()
                .take(8)
                .map(|(k, o, h)| format!("{}={};held={}", k, o, h))
                .collect();
            let detail = format!("downgrade: {}", summary.join(", "));
            warn!("{}", detail);
            state.pin.record_event(&crate::pin_verify::TamperEvent {
                url: String::new(),
                expected_sha256: String::new(),
                actual_sha256: String::new(),
                detail,
                observed_at: chrono::Utc::now().timestamp(),
            });
            if reject {
                build_503("package index rewound - held by client ledger")
            } else {
                parts.headers.insert(
                    "X-Monopsony-Downgrade",
                    summary.join(",").parse().expect("header value"),
                );
                Response::from_parts(parts, Body::from(body))
            }
        }
    }
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

pub async fn run_proxy(
    listen_addr: SocketAddr,
    config: ProxyConfig,
    pin: std::sync::Arc<crate::pin_verify::PinState>,
    ledger: Option<std::sync::Arc<crate::ledger::Ledger>>,
    ledger_policy: String,
    ledger_max_index_age_secs: u64,
    floor_store: Option<
        std::sync::Arc<std::sync::RwLock<crate::floor_guard::FloorStore>>,
    >,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    run_proxy_with_listener(
        listener,
        config,
        pin,
        ledger,
        ledger_policy,
        ledger_max_index_age_secs,
        floor_store,
    )
    .await
}

/// Test/integration entry: identical to [`run_proxy`] but the caller
/// supplies an already-bound listener, so `bind(127.0.0.1:0)` tests can
/// discover the port via `local_addr()` before serving begins.
#[allow(clippy::too_many_arguments)]
pub async fn run_proxy_with_listener(
    listener: tokio::net::TcpListener,
    config: ProxyConfig,
    pin: std::sync::Arc<crate::pin_verify::PinState>,
    ledger: Option<std::sync::Arc<crate::ledger::Ledger>>,
    ledger_policy: String,
    ledger_max_index_age_secs: u64,
    floor_store: Option<std::sync::Arc<std::sync::RwLock<crate::floor_guard::FloorStore>>>,
) -> anyhow::Result<()> {
    let mut state = ClientProxyState::new(config).with_pin(pin);
    if let Some(ledger) = ledger {
        state = state.with_ledger(ledger, ledger_policy, ledger_max_index_age_secs);
    }
    state.floor_store = floor_store;
    let state = Arc::new(state);

    let app = axum::Router::new()
        .route("/*path", axum::routing::any(proxy_handler))
        .route("/", axum::routing::any(proxy_handler))
        .with_state(state.clone());

    info!("Client proxy listening on {}", listener.local_addr()?);

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
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
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
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
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

    // ---- slice 5: pm_kind_from_url dnf/apt detection ----

    fn pm_kind_for_url(url: &str) -> Option<&'static str> {
        let uri: hyper::Uri = url.parse().expect("valid test uri");
        pm_kind_from_url(&uri)
    }

    #[test]
    fn pm_kind_primary_xml_gz_is_dnf() {
        assert_eq!(
            pm_kind_for_url(
                "https://mirrors.rockylinux.org/rocky/9/BaseOS/x86_64/os/repodata/primary.xml.gz"
            ),
            Some("dnf")
        );
        assert_eq!(
            pm_kind_for_url("https://mirror.example.com/repodata/primary.xml"),
            Some("dnf")
        );
    }

    #[test]
    fn pm_kind_inrelease_and_release_are_apt() {
        assert_eq!(
            pm_kind_for_url("https://deb.debian.org/debian/dists/bookworm/InRelease"),
            Some("apt")
        );
        assert_eq!(
            pm_kind_for_url("http://archive.ubuntu.com/ubuntu/dists/noble/Release"),
            Some("apt")
        );
    }

    #[test]
    fn pm_kind_release_gpg_and_unknown_fall_through_to_none() {
        // Signature, not an index.
        assert_eq!(
            pm_kind_for_url("https://deb.debian.org/debian/dists/bookworm/Release.gpg"),
            None
        );
        assert_eq!(
            pm_kind_for_url("https://example.com/some/random/path"),
            None
        );
    }

    #[test]
    fn pm_kind_pacman_and_apk_unchanged() {
        assert_eq!(
            pm_kind_for_url("https://mirror.example.com/repo/os/x86_64/core.db"),
            Some("pacman")
        );
        assert_eq!(
            pm_kind_for_url(
                "https://dl-cdn.alpinelinux.org/alpine/v3.20/main/x86_64/APKINDEX.tar.gz"
            ),
            Some("apk")
        );
    }

    // ---- Sprint 20 Phase 3 regression tests (fleet verification 2026-09-16) ----

    /// Test helper: build a signed pin attestation like the server does.
    /// Returns the attestation AND the 32-byte verify key (public key bytes,
    /// what `mirror_verify_key` carries and PinState::set_key expects).
    fn signed_pin_att(
        url: &str,
        sha: &str,
    ) -> (monopsony_common::pin_store::PinAttestation, [u8; 32]) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut att = monopsony_common::pin_store::PinAttestation {
            url: url.to_string(),
            expected_sha256: sha.to_string(),
            origin: monopsony_common::pin_store::PinOrigin::Sidecar,
            key_id: String::new(),
            observed_at: 1_700_000_000,
            signature: String::new(),
        };
        att.sign(&key);
        (att, key.verifying_key().to_bytes())
    }

    /// Bug 2 regression: a wrapped response MUST NOT carry content-length.
    /// The hash verdict lands at end-of-stream; with a length-declared body
    /// hyper stops polling after the declared bytes and the terminal poll
    /// (the check itself) never runs. The fix strips the framing headers so
    /// the body is re-framed chunked and a mismatch aborts mid-transfer.
    #[tokio::test]
    async fn pin_wrap_strips_content_length() {
        let sha = "ab".repeat(32);
        let url = "https://example.com/pkg-1.0.pkg.tar.zst";

        let pin = std::sync::Arc::new(crate::pin_verify::PinState::new(
            None,
            std::env::temp_dir().join(format!("pin-strip-{}.jsonl", std::process::id())),
        ));
        let (att, verify_bytes) = signed_pin_att(url, &sha);
        pin.set_key(verify_bytes); // provisioned AFTER construction (Bug 1 path)
        assert!(pin.provisioned());

        let state =
            std::sync::Arc::new(ClientProxyState::new(ProxyConfig::default()).with_pin(pin));
        let uri: hyper::Uri = "http://example.com/pkg-1.0.pkg.tar.zst".parse().unwrap();
        let resp = Response::builder()
            .status(200)
            .header(
                "x-monopsony-pin",
                monopsony_common::pin_store::to_json(&att),
            )
            .header("content-length", "44562")
            .body(Body::from("junk body"))
            .unwrap();
        let wrapped = wrap_with_pin_check(state, &uri, resp);
        assert!(wrapped.headers().get("content-length").is_none());
        assert!(wrapped.headers().get("transfer-encoding").is_none());
        assert_eq!(
            wrapped.headers().get("x-monopsony-verified").unwrap(),
            "pending"
        );
    }

    /// Unpinned/unprovisioned passthrough keeps the original framing (inert).
    #[tokio::test]
    async fn pin_wrap_inert_without_key_keeps_framing() {
        let sha = "ab".repeat(32);
        let url = "https://example.com/pkg-1.0.pkg.tar.zst";
        let (att, _key) = signed_pin_att(url, &sha);

        // NOT provisioned — verification is inert, response passes through.
        let pin = std::sync::Arc::new(crate::pin_verify::PinState::new(
            None,
            std::env::temp_dir().join(format!("pin-inert-{}.jsonl", std::process::id())),
        ));
        let state =
            std::sync::Arc::new(ClientProxyState::new(ProxyConfig::default()).with_pin(pin));
        let uri: hyper::Uri = "http://example.com/pkg-1.0.pkg.tar.zst".parse().unwrap();
        let resp = Response::builder()
            .status(200)
            .header(
                "x-monopsony-pin",
                monopsony_common::pin_store::to_json(&att),
            )
            .header("content-length", "44562")
            .body(Body::from("junk body"))
            .unwrap();
        let wrapped = wrap_with_pin_check(state, &uri, resp);
        assert_eq!(
            wrapped.headers().get("content-length").unwrap(),
            "44562",
            "inert path must not re-frame"
        );
        assert!(wrapped.headers().get("x-monopsony-verified").is_none());
    }
}
