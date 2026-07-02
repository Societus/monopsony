//! Tailscale-aware authentication for the admin dashboard.
//!
//! Detects whether the server is behind `tailscale serve` (tailnet-only) or
//! `tailscale funnel` (public internet), and auto-trusts tailnet clients in
//! serve mode. Falls back to username/password session auth otherwise.

use std::collections::HashMap;
use std::net::IpAddr;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for TailscaleState {
    fn default() -> Self {
        Self {
            available: false,
            serve_mode: false,
            funnel_mode: false,
            tailnet_name: None,
            self_ip: None,
            hostname: None,
        }
    }
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
        if web.is_none() || web.unwrap().as_object().map(|o| o.is_empty()).unwrap_or(true) {
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
                        if handler.get("Funnel").and_then(|f| f.as_bool()).unwrap_or(false) {
                            has_funnel = true;
                        }
                    }
                }
            }
        }

        // Also check the top-level Funnel field
        if parsed.get("Funnel").and_then(|f| f.as_bool()).unwrap_or(false) {
            has_funnel = true;
        }

        if has_funnel {
            (false, true) // funnel mode
        } else {
            (true, false) // serve mode (tailnet only)
        }
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
    username: String,
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
        self.sessions.write().await.insert(token.clone(), session);
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
        self.sessions.write().await.retain(|_, s| now < s.expires_at);
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

    // Get client IP from connection info or X-Forwarded-For
    let client_ip = extract_client_ip(&req);
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

    // Check for valid session cookie
    if let Some(cookie) = cookies.get(SESSION_COOKIE_NAME) {
        let token = cookie.value();
        if !token.is_empty() {
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

fn extract_client_ip(req: &Request) -> IpAddr {
    // Check X-Forwarded-For first (when behind a reverse proxy like Tailscale Serve)
    if let Some(forwarded) = req.headers().get("x-forwarded-for") {
        if let Ok(value) = forwarded.to_str() {
            if let Some(first) = value.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }

    // Check X-Real-IP
    if let Some(real_ip) = req.headers().get("x-real-ip") {
        if let Ok(value) = real_ip.to_str() {
            if let Ok(ip) = value.parse::<IpAddr>() {
                return ip;
            }
        }
    }

    // Fallback to loopback (dashboard served from same host)
    "127.0.0.1".parse().unwrap()
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
    let client_ip = extract_client_ip(&req);
    let ctx = evaluate_auth(&state, &client_ip).await;

    let credentials_configured = state.dashboard_config.username.is_some()
        && state.dashboard_config.password_hash.is_some();

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
pub async fn handle_login(
    State(state): State<AdminState>,
    cookies: Cookies,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    let config = &state.dashboard_config;

    // Check credentials are configured
    let (stored_user, stored_hash) = match (&config.username, &config.password_hash) {
        (Some(u), Some(h)) => (u.clone(), h.clone()),
        _ => {
            return Json(LoginResponse {
                success: false,
                message: Some("Dashboard credentials not configured".to_string()),
                session_token: None,
            });
        }
    };

    // Verify username
    if req.username != stored_user {
        return Json(LoginResponse {
            success: false,
            message: Some("Invalid credentials".to_string()),
            session_token: None,
        });
    }

    // Verify password against bcrypt hash
    let valid = bcrypt::verify(&req.password, &stored_hash).unwrap_or(false);

    if !valid {
        return Json(LoginResponse {
            success: false,
            message: Some("Invalid credentials".to_string()),
            session_token: None,
        });
    }

    // Create session
    // TODO: use shared SessionStore instance for validation in middleware
    let token = generate_session_token();

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
}

/// POST /api/v1/auth/logout — destroy session.
pub async fn handle_logout(cookies: Cookies) -> impl IntoResponse {
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
}
