//! Tailscale-aware authentication for the admin dashboard.
//!
//! Detects whether the server is behind `tailscale serve` (tailnet-only) or
//! `tailscale funnel` (public internet), and auto-trusts tailnet clients in
//! serve mode. Falls back to username/password session auth otherwise.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tower_cookies::{Cookie, Cookies};

use crate::AdminState;

const SESSION_COOKIE_NAME: &str = "monopsony_session";

// ─── Tailscale Detection ──────────────────────────────────────────────

/// Detected Tailscale environment state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TailscaleState {
    /// Is Tailscale installed and running on this host?
    pub available: bool,
    /// Is the server behind `tailscale serve` (tailnet-only access)?
    pub serve_mode: bool,
    /// Is the server behind `tailscale funnel` (public internet access)?
    pub funnel_mode: bool,
    /// The tailnet name (e.g., "societus.github")
    pub tailnet_name: Option<String>,
    /// This node's Tailscale IP (e.g., "100.125.219.67")
    pub self_ip: Option<String>,
    /// This node's hostname on the tailnet
    pub hostname: Option<String>,
}

/// Detector that caches Tailscale state and refreshes periodically.
pub struct TailscaleDetector {
    state: RwLock<CachedState>,
    socket_path: String,
    refresh_interval: Duration,
}

struct CachedState {
    state: TailscaleState,
    last_checked: Instant,
}

impl TailscaleDetector {
    /// Create a new detector. Tries common socket paths.
    pub fn new() -> Self {
        let socket_path = Self::find_socket();
        Self {
            state: RwLock::new(CachedState {
                state: TailscaleState::default(),
                last_checked: Instant::now() - Duration::from_secs(3600),
            }),
            socket_path,
            refresh_interval: Duration::from_secs(60),
        }
    }

    fn find_socket() -> String {
        let candidates = [
            "/var/run/tailscale/tailscaled.sock",
            "/run/tailscale/tailscaled.sock",
            "/var/run/tailscaled.sock",
        ];
        for path in &candidates {
            if Path::new(path).exists() {
                return path.to_string();
            }
        }
        // Default to the standard path even if not found
        "/var/run/tailscale/tailscaled.sock".to_string()
    }

    /// Get current Tailscale state, refreshing if stale.
    pub async fn detect(&self) -> TailscaleState {
        {
            let cached = self.state.read().await;
            if cached.last_checked.elapsed() < self.refresh_interval {
                return cached.state.clone();
            }
        }

        // Need to refresh — acquire write lock
        let mut cached = self.state.write().await;

        // Double-check after acquiring lock
        if cached.last_checked.elapsed() < self.refresh_interval {
            return cached.state.clone();
        }

        let new_state = self.do_detect().await;
        *cached = CachedState {
            state: new_state.clone(),
            last_checked: Instant::now(),
        };
        new_state
    }

    async fn do_detect(&self) -> TailscaleState {
        if !Path::new(&self.socket_path).exists() {
            return TailscaleState::default();
        }

        // Query Tailscale local API for status
        let status = match self.query_local_api("/localapi/v0/status").await {
            Ok(body) => body,
            Err(_) => return TailscaleState::default(),
        };

        let parsed = match serde_json::from_str::<serde_json::Value>(&status) {
            Ok(v) => v,
            Err(_) => return TailscaleState::default(),
        };

        let available = true;
        let tailnet_name = parsed
            .pointer("/CurrentTailnet/Name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let self_ip = parsed
            .pointer("/Self/TailscaleIPs/0")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let hostname = parsed
            .pointer("/Self/HostName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Check serve/funnel status via `tailscale serve status --json`
        let (serve_mode, funnel_mode) = self.detect_serve_funnel(&hostname, &tailnet_name).await;

        TailscaleState {
            available,
            serve_mode,
            funnel_mode,
            tailnet_name,
            self_ip,
            hostname,
        }
    }

    /// Detect whether the server is in serve or funnel mode by querying
    /// the serve-config local API endpoint.
    async fn detect_serve_funnel(
        &self,
        _hostname: &Option<String>,
        _tailnet: &Option<String>,
    ) -> (bool, bool) {
        // Query the serve status from the local API
        let serve_status = match self.query_local_api("/localapi/v0/serve-config").await {
            Ok(body) => body,
            Err(_) => return (false, false),
        };

        let parsed = match serde_json::from_str::<serde_json::Value>(&serve_status) {
            Ok(v) => v,
            Err(_) => return (false, false),
        };

        // If there are web handlers configured, the server is in serve mode.
        // We check for Funnel eligibility by looking at the AllowFunnel flag.
        let web = parsed.get("Web");
        if web.is_none()
            || web
                .unwrap()
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(true)
        {
            return (false, false);
        }

        // Check if any handler has funnel enabled
        let mut has_funnel = false;
        if let Some(web_obj) = web.and_then(|w| w.as_object()) {
            for (_host, config) in web_obj {
                if let Some(handlers) = config.pointer("/Handlers").and_then(|h| h.as_object()) {
                    for (_path, handler) in handlers {
                        // Check for Funnel flag in the serve config
                        // The serve-config API returns funnel info at the host level
                        if handler
                            .get("Funnel")
                            .and_then(|f| f.as_bool())
                            .unwrap_or(false)
                        {
                            has_funnel = true;
                        }
                    }
                }
            }
        }

        // Also check the top-level Funnel field
        if parsed
            .get("Funnel")
            .and_then(|f| f.as_bool())
            .unwrap_or(false)
        {
            has_funnel = true;
        }

        if has_funnel {
            (false, true) // funnel mode
        } else {
            (true, false) // serve mode (tailnet only)
        }
    }

    /// Returns true when the server is publicly exposed (tailscale funnel
    /// mode). Funnel == reachable from the public internet, so it is the
    /// exposure signal used to gate remote assignment.
    pub async fn detect_public_exposure(&self) -> bool {
        exposure_from_modes(self.detect().await.funnel_mode)
    }

    /// Query the Tailscale local API via Unix socket.
    async fn query_local_api(&self, path: &str) -> Result<String, std::io::Error> {
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixStream;

        let stream = UnixStream::connect(&self.socket_path).await?;

        // HTTP/1.1 request over Unix socket
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: local-tailscaled.sock\r\n\
             Connection: close\r\n\
             \r\n",
            path
        );

        let (mut reader, mut writer) = stream.into_split();
        use tokio::io::AsyncWriteExt;
        writer.write_all(request.as_bytes()).await?;
        writer.flush().await?;

        let mut response = String::new();
        reader.read_to_string(&mut response).await?;

        // Extract body from HTTP response (skip headers)
        if let Some(pos) = response.find("\r\n\r\n") {
            Ok(response[pos + 4..].to_string())
        } else {
            Ok(response)
        }
    }
}

impl Default for TailscaleDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Session Management ───────────────────────────────────────────────

/// In-memory session store.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, Session>>,
}

struct Session {
    #[allow(dead_code)] // reserved for audit fields in session responses
    username: String,
    #[allow(dead_code)] // reserved: expiry cleanup pass reads this
    created_at: Instant,
    expires_at: Instant,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub async fn create(&self, username: &str, timeout_hours: u64) -> String {
        let token = generate_session_token();
        let now = Instant::now();
        let session = Session {
            username: username.to_string(),
            created_at: now,
            expires_at: now + Duration::from_secs(timeout_hours * 3600),
        };
        let mut sessions = self.sessions.write().await;
        sessions.insert(token.clone(), session);
        // Lazy eviction: bound memory without a background task. Preserves
        // unexpired sessions even past the threshold.
        if sessions.len() > 10_000 {
            sessions.retain(|_, s| Instant::now() < s.expires_at);
        }
        token
    }

    pub async fn validate(&self, token: &str) -> bool {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(token) {
            return Instant::now() < session.expires_at;
        }
        false
    }

    pub async fn remove(&self, token: &str) {
        self.sessions.write().await.remove(token);
    }

    /// Evict expired sessions. Called periodically.
    #[allow(dead_code)]
    pub async fn evict_expired(&self) {
        let now = Instant::now();
        self.sessions
            .write()
            .await
            .retain(|_, s| now < s.expires_at);
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

fn generate_session_token() -> String {
    let bytes = uuid::Uuid::new_v4();
    let hash = Sha256::digest(bytes.as_bytes());
    hex::encode(hash)
}

// ─── Auth Context ─────────────────────────────────────────────────────

/// Result of auth evaluation for a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthContext {
    pub authenticated: bool,
    pub method: AuthMethod,
    pub tailscale: TailscaleState,
    pub client_is_tailnet: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Auto-trusted via Tailscale tailnet in serve mode
    TailnetAutoTrust,
    /// Authenticated via username/password session
    Session,
    /// Not authenticated
    None,
}

// ─── Axum Handlers ────────────────────────────────────────────────────

/// True when the given Tailscale modes indicate public exposure.
/// Funnel mode == the server is reachable from the public internet.
pub fn exposure_from_modes(funnel_mode: bool) -> bool {
    funnel_mode
}

/// Evaluate auth context for a request.
pub async fn evaluate_auth(state: &AdminState, client_ip: &IpAddr) -> AuthContext {
    let ts = state.tailscale.detect().await;
    let client_is_tailnet = crate::is_tailscale_ip(client_ip);

    // Auto-trust: client is on tailnet AND server is in serve mode (not funnel)
    // AND dashboard config doesn't force login
    let auto_trust = client_is_tailnet
        && ts.serve_mode
        && !ts.funnel_mode
        && !state.dashboard_config.always_require_login;

    if auto_trust {
        return AuthContext {
            authenticated: true,
            method: AuthMethod::TailnetAutoTrust,
            tailscale: ts,
            client_is_tailnet,
        };
    }

    AuthContext {
        authenticated: false,
        method: AuthMethod::None,
        tailscale: ts,
        client_is_tailnet,
    }
}

/// Whether an endpoint is a "write" operation that always requires auth.
///
/// Write operations: POST, PUT, DELETE on any path, plus GET on config
/// endpoints (which expose sensitive data like DB URLs).
fn is_write_endpoint(method: &axum::http::Method, path: &str) -> bool {
    // All non-GET methods are writes
    if method != axum::http::Method::GET {
        return true;
    }

    // Config reads expose sensitive data — always require auth
    if path.starts_with("/api/v1/config") {
        return true;
    }

    false
}

/// Whether the endpoint should always require auth regardless of mode.
fn is_sensitive_read(path: &str) -> bool {
    // Client inventory contains system info
    path.starts_with("/api/v1/clients/")
}

/// Auth middleware with tiered access:
///
/// - Write endpoints (POST/PUT/DELETE, config reads): **always require auth**
/// - Read endpoints (GET health/stats/cache/shadow/mirrors/tracker/features):
///   - Funnel mode: **always require auth**
///   - Tailnet serve mode or Direct LAN: **optional** unless `always_require_login`
///     is set in config
pub async fn auth_middleware(
    State(state): State<AdminState>,
    cookies: Cookies,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let method = req.method().clone();

    // Allow auth endpoints through without authentication
    if path.starts_with("/api/v1/auth/") {
        return next.run(req).await;
    }

    // Pairing intake (/api/v1/pair/request), cert collection
    // (/api/v1/pair/collect), and enrollment-key pairing (/api/v1/pair/enroll)
    // are client-facing and intentionally unauthenticated — the client has no
    // session cookie. They are rate-limited by the outer middleware instead
    // (pairing-v2 D8). Admin-only pair routes (approve/deny/pending) fall
    // through to auth.
    if path.starts_with("/api/v1/pair/request")
        || path.starts_with("/api/v1/pair/collect")
        || path.starts_with("/api/v1/pair/enroll")
    {
        return next.run(req).await;
    }

    // Get client IP from connection info or X-Forwarded-For
    let client_ip = extract_client_ip(&req, &state.trusted_proxies);
    let ctx = evaluate_auth(&state, &client_ip).await;

    // Check if this is a write/sensitive endpoint
    let is_write = is_write_endpoint(&method, path);
    let is_sensitive = is_sensitive_read(path);

    // Determine if auth is strictly required for this request
    let auth_strictly_required = if ctx.tailscale.funnel_mode {
        // Funnel mode: everything requires auth
        true
    } else if is_write || is_sensitive {
        // Write/sensitive endpoints always require auth
        true
    } else {
        // Read endpoints: optional unless always_require_login is set
        state.dashboard_config.always_require_login
    };

    // Auto-trust via tailnet (serve mode, not funnel)
    if ctx.authenticated {
        return next.run(req).await;
    }

    // Check for valid session cookie (pairing-v2 Phase 0: the token must
    // exist in the SessionStore — previously any non-empty cookie passed)
    if let Some(cookie) = cookies.get(SESSION_COOKIE_NAME) {
        let token = cookie.value();
        if !token.is_empty() && state.sessions.validate(token).await {
            return next.run(req).await;
        }
    }

    if !auth_strictly_required {
        // Allow the request through — read access in non-funnel mode
        return next.run(req).await;
    }

    // Not authenticated for a strictly-required endpoint
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "unauthorized",
            "message": "Authentication required",
            "auth_method": ctx.method,
            "tailscale_available": ctx.tailscale.available,
            "funnel_mode": ctx.tailscale.funnel_mode,
            "write_endpoint": is_write,
        })),
    )
        .into_response()
}

/// Trusted reverse proxies whose `X-Forwarded-For` / `X-Real-IP` headers are honored
/// for observed-identity display and per-IP rate limiting.
///
/// Entries are parsed as CIDR (`"10.0.0.0/8"`) or single IPs (`"10.0.0.1"` = /32 or /128).
/// When empty, forwarded headers are never trusted and the direct peer IP is used.
///
/// This is the red-team F5 control: a forged `X-Forwarded-For` behind Cloudflare/Caddy
/// must not let an attacker spoof their observed source IP.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    /// `(network_ip, prefix_len)`. Single IPs are stored as /32 (IPv4) or /128 (IPv6).
    entries: Vec<(IpAddr, u8)>,
}

impl TrustedProxies {
    /// Parse config strings into a matchable set. Malformed entries are skipped.
    pub fn from_config(strings: &[String]) -> Self {
        let mut entries = Vec::new();
        for s in strings {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some((net, prefix)) = trimmed.split_once('/') {
                if let (Ok(net_ip), Ok(p)) = (
                    net.trim().parse::<IpAddr>(),
                    prefix.trim().parse::<u8>(),
                ) {
                    let max_prefix = match net_ip {
                        IpAddr::V4(_) => 32,
                        IpAddr::V6(_) => 128,
                    };
                    if p <= max_prefix {
                        entries.push((apply_mask(net_ip, p), p));
                    }
                }
            } else if let Ok(ip) = trimmed.parse::<IpAddr>() {
                let prefix = match ip {
                    IpAddr::V4(_) => 32,
                    IpAddr::V6(_) => 128,
                };
                entries.push((ip, prefix));
            }
        }
        Self { entries }
    }

    /// Returns true when `ip` falls within any configured trusted network.
    pub(crate) fn contains(&self, ip: &IpAddr) -> bool {
        self.entries
            .iter()
            .any(|(net, prefix)| cidr_contains(*net, *prefix, *ip))
    }
}

/// Apply a CIDR prefix mask to a network address, normalizing host bits to zero.
fn apply_mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(ip) => {
            if prefix == 0 {
                return IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
            }
            let mask: u32 = !((1u32 << (32 - prefix)) - 1);
            IpAddr::V4(Ipv4Addr::from(u32::from(ip) & mask))
        }
        IpAddr::V6(ip) => IpAddr::V6(ip),
    }
}

/// Returns true when `ip` is contained in the CIDR `net/prefix`.
fn cidr_contains(net: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            if prefix > 32 {
                return false;
            }
            let mask: u32 = if prefix == 0 { 0 } else { !((1u32 << (32 - prefix)) - 1) };
            (u32::from(net) & mask) == (u32::from(ip) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            if prefix > 128 {
                return false;
            }
            let net_octets = net.octets();
            let ip_octets = ip.octets();
            let full_bytes = prefix as usize / 8;
            let rem_bits = prefix as usize % 8;
            for i in 0..full_bytes {
                if net_octets[i] != ip_octets[i] {
                    return false;
                }
            }
            if rem_bits > 0 {
                let mask = 0xFFu8 << (8 - rem_bits);
                return (net_octets[full_bytes] & mask) == (ip_octets[full_bytes] & mask);
            }
            true
        }
        _ => false,
    }
}

/// Extract the client IP for auth decisions and per-IP rate limiting.
///
/// The direct peer (from `ConnectInfo`) is always the baseline. `X-Forwarded-For`
/// / `X-Real-IP` are honored **only** when the direct peer is a configured trusted
/// proxy — a forged header from an untrusted peer cannot spoof the observed IP.
/// (red-team F5)
pub(crate) fn extract_client_ip(req: &Request, trusted: &TrustedProxies) -> IpAddr {
    // Direct peer address, present when the server is started with
    // `into_make_service_with_connect_info` (pairing-v2 Phase 0).
    let direct_ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());

    // Honor forwarded headers only from a trusted direct peer.
    if let Some(peer) = direct_ip {
        if trusted.contains(&peer) {
            if let Some(ip) = forwarded_ip(req) {
                return ip;
            }
        }
        return peer;
    }

    // Fallback to loopback (dashboard served from same host, no connect info).
    "127.0.0.1".parse().unwrap()
}

/// Read the client IP from `X-Forwarded-For` (leftmost entry) or `X-Real-IP`.
/// Caller is responsible for gating on a trusted direct peer.
fn forwarded_ip(req: &Request) -> Option<IpAddr> {
    if let Some(forwarded) = req.headers().get("x-forwarded-for") {
        if let Ok(value) = forwarded.to_str() {
            if let Some(first) = value.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }
    if let Some(real_ip) = req.headers().get("x-real-ip") {
        if let Ok(value) = real_ip.to_str() {
            if let Ok(ip) = value.trim().parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }
    None
}

// ─── Auth Endpoints ───────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthStatusResponse {
    pub dashboard_enabled: bool,
    pub auth_required: bool,
    pub auth_method: AuthMethod,
    pub tailscale: TailscaleState,
    pub client_is_tailnet: bool,
    pub credentials_configured: bool,
}

/// GET /api/v1/auth/status — check auth requirements for the current client.
pub async fn handle_auth_status(
    State(state): State<AdminState>,
    req: Request,
) -> impl IntoResponse {
    let client_ip = extract_client_ip(&req, &state.trusted_proxies);
    let ctx = evaluate_auth(&state, &client_ip).await;

    let credentials_configured =
        state.dashboard_config.username.is_some() && state.dashboard_config.password_hash.is_some();

    let auth_required = !ctx.authenticated;

    Json(AuthStatusResponse {
        dashboard_enabled: state.dashboard_config.enabled,
        auth_required,
        auth_method: ctx.method,
        tailscale: ctx.tailscale,
        client_is_tailnet: ctx.client_is_tailnet,
        credentials_configured,
    })
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub success: bool,
    pub message: Option<String>,
    pub session_token: Option<String>,
}

/// POST /api/v1/auth/login — authenticate with username/password.
///
/// Pairing-v2 Phase 0: (username, IP) lockout — 5 failures within the window
/// (default 15 min) locks the pair; sessions are created in the shared
/// `SessionStore` so the auth middleware can actually validate them.
pub async fn handle_login(
    State(state): State<AdminState>,
    cookies: Cookies,
    req: Request,
) -> Response {
    let config = &state.dashboard_config;

    // Client IP uses the same view as the auth middleware (XFF → X-Real-IP →
    // ConnectInfo → loopback) so lockout keys match what the rate limiter sees.
    let client_ip = extract_client_ip(&req, &state.trusted_proxies);

    // Parse the JSON body manually: the Request extractor must be last-ish and
    // Json cannot follow it, but we need the Request for the client IP.
    let body =
        match <axum::extract::Json<LoginRequest> as axum::extract::FromRequest<()>>::from_request(
            req,
            &(),
        )
        .await
        {
            Ok(axum::extract::Json(r)) => r,
            Err(rej) => return rej.into_response(),
        };

    // Check credentials are configured
    let (stored_user, stored_hash) = match (&config.username, &config.password_hash) {
        (Some(u), Some(h)) => (u.clone(), h.clone()),
        _ => {
            return Json(LoginResponse {
                success: false,
                message: Some("Dashboard credentials not configured".to_string()),
                session_token: None,
            })
            .into_response();
        }
    };

    // Lockout gate: reject before touching the password hash.
    if let Some(remaining) = state.rate_limits.login.is_locked(&body.username, client_ip) {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", remaining.as_secs().max(1).to_string())
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(
                r#"{"success":false,"message":"Too many failed logins. Try again later.","session_token":null}"#,
            ))
            .unwrap();
    }

    // Verify username
    if body.username != stored_user {
        state
            .rate_limits
            .login
            .record_failure(&body.username, client_ip);
        return Json(LoginResponse {
            success: false,
            message: Some("Invalid credentials".to_string()),
            session_token: None,
        })
        .into_response();
    }

    // Verify password against bcrypt hash
    let valid = bcrypt::verify(&body.password, &stored_hash).unwrap_or(false);

    if !valid {
        state
            .rate_limits
            .login
            .record_failure(&body.username, client_ip);
        return Json(LoginResponse {
            success: false,
            message: Some("Invalid credentials".to_string()),
            session_token: None,
        })
        .into_response();
    }

    state
        .rate_limits
        .login
        .record_success(&body.username, client_ip);

    // Create session in the shared store (validated by the auth middleware)
    let token = state
        .sessions
        .create(&body.username, config.session_timeout_hours)
        .await;

    // Set session cookie
    let cookie = Cookie::build((SESSION_COOKIE_NAME, token.clone()))
        .path("/")
        .http_only(true)
        .same_site(tower_cookies::cookie::SameSite::Lax)
        .max_age(tower_cookies::cookie::time::Duration::hours(
            config.session_timeout_hours as i64,
        ))
        .build();

    cookies.add(cookie);

    Json(LoginResponse {
        success: true,
        message: None,
        session_token: Some(token),
    })
    .into_response()
}

/// POST /api/v1/auth/logout — destroy session.
pub async fn handle_logout(State(state): State<AdminState>, cookies: Cookies) -> impl IntoResponse {
    // Remove the session from the store so the token stops validating.
    if let Some(cookie) = cookies.get(SESSION_COOKIE_NAME) {
        let token = cookie.value();
        if !token.is_empty() {
            state.sessions.remove(token).await;
        }
    }
    // Overwrite the session cookie with an expired one
    let expired = Cookie::build((SESSION_COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .max_age(tower_cookies::cookie::time::Duration::seconds(0))
        .build();
    cookies.add(expired);
    Json(serde_json::json!({"success": true}))
}

// ─── CLI Helper ───────────────────────────────────────────────────────

/// Hash a password for the `[dashboard]` config section.
/// Used by `monopsony-server dashboard-set-password` CLI command.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Ok(bcrypt::hash(password, 12)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::sync::{Arc, RwLock};

    use axum::{
        body::Body,
        extract::connect_info::ConnectInfo,
        http::{Method, Request, StatusCode},
        routing::post,
        Router,
    };
    use crate::{
        AdminState, CookieManagerLayer, DashboardConfig, TlsManager,
    };
    use crate::pairing::PendingPairStore;
    use crate::rate_limit::AdminRateLimits;
    use tempfile::tempdir;
    use tower::ServiceExt;

    #[test]
    fn test_session_token_generation() {
        let t1 = generate_session_token();
        let t2 = generate_session_token();
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 64); // 32 bytes hex-encoded
    }

    #[test]
    fn test_password_hashing() {
        let hash = hash_password("testpass123").unwrap();
        assert!(bcrypt::verify("testpass123", &hash).unwrap());
        assert!(!bcrypt::verify("wrongpass", &hash).unwrap());
    }

    #[tokio::test]
    async fn test_session_store_create_validate() {
        let store = SessionStore::new();
        let token = store.create("admin", 1).await;
        assert!(store.validate(&token).await);
        assert!(!store.validate("invalid").await);
    }

    #[tokio::test]
    async fn test_session_store_remove() {
        let store = SessionStore::new();
        let token = store.create("admin", 1).await;
        assert!(store.validate(&token).await);
        store.remove(&token).await;
        assert!(!store.validate(&token).await);
    }

    #[test]
    fn test_tailscale_state_default() {
        let state = TailscaleState::default();
        assert!(!state.available);
        assert!(!state.serve_mode);
        assert!(!state.funnel_mode);
    }

    #[test]
    fn test_trusted_proxies_empty_allows_nothing() {
        let trusted = TrustedProxies::from_config(&Vec::new());
        assert!(!trusted.contains(&"10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(!trusted.contains(&"127.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_trusted_proxies_single_ip_match() {
        let trusted = TrustedProxies::from_config(&["10.0.0.1".to_string()]);
        assert!(trusted.contains(&"10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(!trusted.contains(&"10.0.0.2".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_trusted_proxies_cidr_match() {
        let trusted = TrustedProxies::from_config(&["10.0.0.0/8".to_string()]);
        assert!(trusted.contains(&"10.255.255.255".parse::<IpAddr>().unwrap()));
        assert!(!trusted.contains(&"11.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_trusted_proxies_malformed_skipped() {
        let trusted =
            TrustedProxies::from_config(&["not-an-ip".to_string(), "10.0.0.1".to_string()]);
        assert!(trusted.contains(&"10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(!trusted.contains(&"10.0.0.2".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_trusted_proxies_ipv6_cidr() {
        let trusted = TrustedProxies::from_config(&["2001:db8::/32".to_string()]);
        assert!(trusted.contains(&"2001:db8:1234::1".parse::<IpAddr>().unwrap()));
        assert!(!trusted.contains(&"2002:db8::1".parse::<IpAddr>().unwrap()));
    }

    /// Regression test: login must set the session cookie (Set-Cookie), not
    /// just return the token in the JSON body.
    #[tokio::test]
    async fn test_login_sets_session_cookie() {
        let dir = tempdir().unwrap();
        let ca_cert_path = dir.path().join("ca.crt");
        let ca_key_path = dir.path().join("ca.key");
        let tls_manager = Arc::new(TlsManager::load_or_create(&ca_cert_path, &ca_key_path).unwrap());
        let password_hash = hash_password("testpass123").unwrap();

        let state = AdminState {
            db: None,
            cache_dir: dir.path().to_path_buf(),
            shadow_manager: None,
            mirror_manager: None,
            tls_manager,
            config_path: dir.path().join("config.toml"),
            dashboard_config: Arc::new(DashboardConfig {
                username: Some("admin".to_string()),
                password_hash: Some(password_hash),
                ..DashboardConfig::default()
            }),
            trusted_proxies: TrustedProxies::from_config(&Vec::new()),
            tailscale: Arc::new(TailscaleDetector::new()),
            grpc_addr: "127.0.0.1:50051".to_string(),
            ws_url: None,
            pairing_manager: Arc::new(tokio::sync::Mutex::new(None)),
            pairing_task: Arc::new(tokio::sync::Mutex::new(None)),
            sessions: Arc::new(SessionStore::new()),
            rate_limits: Arc::new(AdminRateLimits::from_config(20.0, 40, 5, 15)),
            pending_pairs: Arc::new(tokio::sync::Mutex::new(PendingPairStore::new())),
            mirror_verify_key: [0u8; 32],
            oracle_last_update: Arc::new(RwLock::new(None)),
            oracle_delta: Arc::new(RwLock::new(None)),
        };

        let router = Router::new()
            .route("/api/v1/auth/login", post(handle_login))
            .layer(CookieManagerLayer::new())
            .with_state(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .header("content-type", "application/json")
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
            .body(Body::from(r#"{"username":"admin","password":"testpass123"}"#.to_string()))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let set_cookie: Vec<String> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .collect();
        assert!(
            set_cookie.iter().any(|c| c.starts_with("monopsony_session=")),
            "login response must set the monopsony_session cookie, got {set_cookie:?}"
        );
    }
}
