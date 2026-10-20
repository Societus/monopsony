//! Monopsony Admin API — Dashboard backend
//!
//! Provides a REST API at `/api/v1/*` and serves the embedded dashboard
//! SPA at `/dashboard/*`. Uses Tailscale-aware authentication.

pub mod assign;
pub mod auth;
pub mod cache;
pub mod clients;
pub mod enrollment;
pub mod features;
pub mod health;
pub mod help;
pub mod language_pms;
pub mod mirrors;
pub mod oracle;
pub mod pairing;
pub mod rate_limit;
pub mod recipe;
pub mod settings;
pub mod shadow;
pub mod tracker;
pub mod ws_tunnel;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::from_fn_with_state,
    response::Response,
    routing::{get, patch, post},
    Router,
};
use monopsony_control::tls::TlsManager;
use monopsony_db::Database;
use monopsony_mirror::mirror::MirrorManager;
use monopsony_shadow::ShadowPackageManager;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tower_cookies::CookieManagerLayer;

/// Embedded dashboard build output. At compile time, rust-embed reads
/// the `dashboard/build/` directory relative to the workspace root and
/// bakes every file into the binary. This eliminates the need for a
/// separate dashboard install step on production servers.
#[derive(RustEmbed)]
#[folder = "../../dashboard/build/"]
struct DashboardAsset;

/// Shared state passed to all admin API handlers.
#[derive(Clone)]
pub struct AdminState {
    pub db: Option<Arc<Database>>,
    pub cache_dir: PathBuf,
    pub shadow_manager: Option<Arc<ShadowPackageManager>>,
    pub mirror_manager: Option<Arc<MirrorManager>>,
    pub tls_manager: Arc<TlsManager>,
    pub config_path: PathBuf,
    pub dashboard_config: Arc<DashboardConfig>,
    /// Trusted reverse proxies whose X-Forwarded-For / X-Real-IP are honored
    /// for observed identity and per-IP rate limiting (red-team F5).
    pub trusted_proxies: auth::TrustedProxies,
    pub tailscale: Arc<auth::TailscaleDetector>,
    /// The server's gRPC listen address (e.g. "0.0.0.0:50051") used in PairResponse.
    pub grpc_addr: String,
    /// Optional WebSocket tunnel URL (e.g. "wss://monopsony.example.com/ws/tunnel").
    /// When set, included in PairResponse so clients know to use WS transport.
    pub ws_url: Option<String>,
    /// Active pairing session (code, status, deadline) managed by the pairing module.
    pub pairing_manager: Arc<tokio::sync::Mutex<Option<pairing::PairingSession>>>,
    /// Handle to the spawned pairing listener task — aborted on cancel to free port 50052.
    pub pairing_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Login sessions (pairing-v2 Phase 0): created at login, validated by the
    /// auth middleware. Before Phase 0 the middleware accepted any non-empty
    /// cookie — the store was never consulted.
    pub sessions: Arc<auth::SessionStore>,
    /// Per-IP + global rate limiting and login lockout (pairing-v2 Phase 0).
    pub rate_limits: Arc<rate_limit::AdminRateLimits>,
    /// Pending v2 pairing requests (pairing-v2 Phase 2): in-memory store,
    /// cap 50, TTL 15 min, dedup-replace, 3-strike PIN. Shared between the
    /// intake handler, admin approve/pending endpoints, and the collect flow.
    pub pending_pairs: Arc<tokio::sync::Mutex<pairing::PendingPairStore>>,
    /// Pin-store verifying key (Sprint 20 Phase 3) — 32 raw Ed25519 bytes,
    /// delivered to clients in PairResponse so they can verify pin attestations.
    pub mirror_verify_key: [u8; 32],
    /// MAX(resolved_at) from package_map (Sprint 21 slice 21-5a) — freshest
    /// oracle source resolution timestamp for the dashboard. None in DB-less
    /// mode or when the table is empty. RwLock so the oracle refresh loop
    /// (sourcedb) can keep it live as deltas are generated.
    pub oracle_last_update: std::sync::Arc<std::sync::RwLock<Option<i64>>>,
    /// Serialized signed oracle delta + verify-key pin for clients, refreshed
    /// periodically from the observations table. None = endpoint serves 404.
    pub oracle_delta: std::sync::Arc<std::sync::RwLock<Option<String>>>,
}

/// Dashboard configuration from `[dashboard]` in server.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_hash: Option<String>,
    #[serde(default = "default_session_timeout")]
    pub session_timeout_hours: u64,
    #[serde(default)]
    pub always_require_login: bool,
    /// Sustained per-IP request rate for the admin API (requests/sec).
    /// Pairing-v2 Phase 0 rate limiting.
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: f64,
    /// Per-IP burst capacity (max instantaneous requests).
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u64,
    /// Failed logins per (username, IP) before lockout.
    #[serde(default = "default_login_max_attempts")]
    pub login_max_attempts: u32,
    /// Lockout duration in minutes.
    #[serde(default = "default_login_lockout_minutes")]
    pub login_lockout_minutes: u64,
    /// Pairing mode: `pin` (v2, admin approval + client PIN, default for new
    /// installs), `legacy-code` (v1, 6-char code minted by server), or `off`.
    #[serde(default = "default_pairing_mode")]
    pub pairing_mode: PairingMode,
    /// Trusted reverse proxies whose `X-Forwarded-For` / `X-Real-IP` headers are honored
    /// for observed-identity display and per-IP rate limiting. Entries are CIDR ranges
    /// (e.g. `"10.0.0.0/8"`) or single IPs (`"10.0.0.1"`).
    /// When empty, forwarded headers are never trusted and the direct peer IP is used.
    /// (red-team F5: forged X-Forwarded-For behind CF/Caddy must not spoof observed identity)
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

/// Pairing mode selection (pairing-v2 D5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairingMode {
    /// v2 Sunshine model: client-initiated request, admin approves with PIN.
    #[default]
    Pin,
    /// v1 legacy: server mints a 6-char code, client presents it. Trusted-LAN only.
    LegacyCode,
    /// Pairing disabled entirely.
    Off,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            username: None,
            password_hash: None,
            session_timeout_hours: 24,
            always_require_login: false,
            rate_limit_rps: default_rate_limit_rps(),
            rate_limit_burst: default_rate_limit_burst(),
            login_max_attempts: default_login_max_attempts(),
            login_lockout_minutes: default_login_lockout_minutes(),
            pairing_mode: default_pairing_mode(),
            trusted_proxies: Vec::new(),
        }
    }
}

fn default_session_timeout() -> u64 {
    24
}

fn default_rate_limit_rps() -> f64 {
    20.0
}

fn default_rate_limit_burst() -> u64 {
    40
}

fn default_login_max_attempts() -> u32 {
    5
}

fn default_login_lockout_minutes() -> u64 {
    15
}

fn default_pairing_mode() -> PairingMode {
    PairingMode::Pin
}

/// Build the admin router with all API routes and the dashboard SPA.
///
/// The router has two layers:
/// - `/api/v1/*` routes are protected by the auth middleware
/// - All other routes serve the static dashboard SPA (no auth — the SPA
///   itself calls `/api/v1/auth/status` to determine if login is needed)
pub fn admin_router(state: AdminState) -> Router {
    let api = Router::new()
        // Auth
        .route("/api/v1/auth/status", get(auth::handle_auth_status))
        .route("/api/v1/auth/login", post(auth::handle_login))
        .route("/api/v1/auth/logout", post(auth::handle_logout))
        // Pairing v2 intake — unauthenticated, rate-limited (client initiates).
        .route("/api/v1/pair/request", post(pairing::handle_pair_request_v2))
        .route(
            "/api/v1/pair/request/:request_id/status",
            get(pairing::handle_pair_request_status),
        )
        // Health
        .route("/api/v1/health", get(health::handle_health))
        .route("/api/v1/stats", get(health::handle_stats))
        // Cache
        .route("/api/v1/cache", get(cache::handle_list_cache))
        .route("/api/v1/cache/stats", get(cache::handle_cache_stats))
        .route(
            "/api/v1/cache/:key",
            axum::routing::delete(cache::handle_purge_entry),
        )
        .route("/api/v1/cache/purge", post(cache::handle_purge))
        .route("/api/v1/scan/results", get(cache::handle_scan_results))
        // Shadow
        .route("/api/v1/shadow/packages", get(shadow::handle_list_packages))
        .route(
            "/api/v1/shadow/packages",
            post(shadow::handle_register_package),
        )
        .route(
            "/api/v1/shadow/packages/:id",
            axum::routing::delete(shadow::handle_unregister_package),
        )
        .route(
            "/api/v1/shadow/packages/:id/recipe",
            get(shadow::handle_get_recipe),
        )
        .route(
            "/api/v1/shadow/packages/:id/check",
            post(shadow::handle_check_updates),
        )
        .route(
            "/api/v1/shadow/packages/:id/apply",
            post(shadow::handle_apply_update),
        )
        .route(
            "/api/v1/shadow/packages/:id/rollback",
            post(shadow::handle_rollback_update),
        )
        .route("/api/v1/shadow/history", get(shadow::handle_history))
        // Recipe
        .route(
            "/api/v1/shadow/recipe/validate",
            post(recipe::handle_validate),
        )
        .route(
            "/api/v1/shadow/recipe/estimate",
            post(recipe::handle_estimate),
        )
        // Settings
        .route("/api/v1/config", get(settings::handle_get_config))
        .route("/api/v1/config", post(settings::handle_update_config))
        .route("/api/v1/config/scan", get(settings::handle_get_scan_config))
        .route("/api/v1/config/node", get(settings::handle_get_node_config))
        // Language-PM enablement (Sprint 23 Phase D)
        .route(
            "/api/v1/admin/language-pms",
            get(language_pms::handle_get_language_pms),
        )
        .route(
            "/api/v1/admin/language-pms",
            patch(language_pms::handle_patch_language_pms),
        )
        // Clients
        .route("/api/v1/clients", get(clients::handle_list_clients))
        .route("/api/v1/clients/:id", get(clients::handle_get_client))
        .route(
            "/api/v1/clients/:id/inventory",
            get(clients::handle_client_inventory),
        )
        .route(
            "/api/v1/clients/:id/certs/:serial/revoke",
            post(clients::handle_revoke_client_cert),
        )
        // Mirrors
        .route("/api/v1/mirrors", get(mirrors::handle_list_mirrors))
        .route("/api/v1/mirrors", post(mirrors::handle_upsert_mirror))
        .route(
            "/api/v1/mirrors/:distro",
            axum::routing::delete(mirrors::handle_delete_mirror),
        )
        .route(
            "/api/v1/mirrors/:distro/sync",
            post(mirrors::handle_trigger_sync),
        )
        .route(
            "/api/v1/mirrors/:distro/status",
            get(mirrors::handle_mirror_status),
        )
        // Tracker
        .route(
            "/api/v1/tracker/status",
            get(tracker::handle_tracker_status),
        )
        .route(
            "/api/v1/tracker/mirrors",
            get(tracker::handle_tracker_mirrors),
        )
        .route(
            "/api/v1/tracker/reputation/:npub",
            get(tracker::handle_tracker_reputation),
        )
        .route(
            "/api/v1/tracker/reports",
            get(tracker::handle_tracker_reports),
        )
        // Pairing
        .route("/api/v1/pair/init", post(pairing::handle_pair_init))
        .route("/api/v1/pair/status", get(pairing::handle_pair_status))
        .route("/api/v1/pair/cancel", post(pairing::handle_pair_cancel))
        // Pairing v2 admin endpoints (auth required — dashboard approve/deny).
        .route("/api/v1/pair/pending", get(pairing::handle_pair_pending))
        .route("/api/v1/pair/approve", post(pairing::handle_pair_approve))
        .route("/api/v1/pair/deny/:request_id", post(pairing::handle_pair_deny))
        // Pairing v2 approval audit — auth-gated, lists approve/deny history (P4).
        .route("/api/v1/pair/audit", get(pairing::handle_pair_audit))
        // Enrollment keys (P5) — auth-gated dashboard key manager.
        .route(
            "/api/v1/enrollment-keys",
            post(enrollment::handle_create_enrollment_key),
        )
        .route(
            "/api/v1/enrollment-keys",
            get(enrollment::handle_list_enrollment_keys),
        )
        .route(
            "/api/v1/enrollment-keys/:id",
            axum::routing::delete(enrollment::handle_revoke_enrollment_key),
        )
        // Enrollment pairing — client presents a pre-approval key (P5);
        // unauthenticated but rate-limited, like the v2 intake.
        .route("/api/v1/pair/enroll", post(pairing::handle_pair_enroll))
        // Pairing v2 client collection — client presents proof of possession,
        // no session cookie (the client is not an admin); rate-limited by the
        // outer middleware instead (pairing-v2 D8).
        .route("/api/v1/pair/collect", post(pairing::handle_pair_collect))
        // Features & Monetization
        .route("/api/v1/features", get(features::handle_get_features))
        .route(
            "/api/v1/features",
            axum::routing::put(features::handle_update_features),
        )
        .route(
            "/api/v1/features/monetization",
            get(features::handle_get_monetization),
        )
        .route(
            "/api/v1/features/monetization",
            axum::routing::put(features::handle_update_monetization),
        )
        // Help system
        .route("/api/v1/help/:key", get(help::handle_get_help))
        // Sprint 22 slice 22-E2a: remote shadow-recipe assignment
        .route(
            "/api/v1/admin/clients/:id/shadow-context",
            get(assign::handle_shadow_context),
        )
        .route(
            "/api/v1/admin/clients/:id/assign-recipe",
            post(assign::handle_assign_recipe),
        )
        // Protected by auth middleware
        .layer(from_fn_with_state(state.clone(), auth::auth_middleware));

    // The SPA fallback serves static files from the dashboard build directory.
    // If the dashboard isn't built yet, it returns a simple JSON message.
    let spa = Router::new().fallback(handle_spa_fallback);

    // WebSocket tunnels — NOT behind auth middleware.
    // - /ws/tunnel: carries the gRPC mTLS handshake as opaque bytes; the gRPC
    //   server's TLS stack authenticates the client.
    // - /ws/pair: relays the plaintext pairing exchange; the pairing code is
    //   the credential.
    let ws = Router::new()
        .route("/ws/tunnel", get(ws_tunnel::handle_ws_tunnel))
        .route("/ws/pair", get(ws_tunnel::handle_ws_pair))
        // Oracle delta (Sprint 21 fleet wiring): unauthed like the WS tier —
        // integrity comes from the Ed25519 signature, not the channel.
        .route(
            "/.well-known/monopsony/oracle-delta.json",
            get(oracle::handle_oracle_delta),
        );

    Router::new()
        .merge(api)
        .merge(spa)
        .merge(ws)
        .layer(CookieManagerLayer::new())
        // Phase 0 rate limiting — outermost so limited requests are rejected
        // before any auth/cookie work. Covers the SPA, API, and /ws routes.
        .layer(from_fn_with_state(
            state.clone(),
            rate_limit::rate_limit_middleware,
        ))
        .with_state(state)
}

/// Serve the SvelteKit SPA from embedded assets compiled into the binary.
///
/// Uses rust-embed to serve files baked in at compile time from
/// `dashboard/build/`. Falls back to disk-based serving from the source
/// tree when running in dev mode (cargo run from workspace root).
async fn handle_spa_fallback(State(_state): State<AdminState>, req: Request) -> Response {
    use axum::body::Body;
    use axum::http::header;

    let raw_path = req.uri().path();
    // SvelteKit is built with base: '/dashboard', so asset URLs start with /dashboard/
    // Strip that prefix when looking up files in the build directory
    let path = raw_path
        .trim_start_matches('/')
        .strip_prefix("dashboard/")
        .unwrap_or_else(|| raw_path.trim_start_matches('/'));

    // Normalize: empty or "dashboard" → index.html (SPA entry point)
    let asset_path = if path.is_empty() || path == "dashboard" {
        "index.html"
    } else {
        path
    };

    // Try embedded assets first (production binary)
    if let Some(file) = DashboardAsset::get(asset_path) {
        let mime = mime_guess::from_path(asset_path)
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        return Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .body(Body::from(file.data.into_owned()))
            .unwrap();
    }

    // SPA fallback: unknown routes serve index.html (client-side routing)
    if let Some(file) = DashboardAsset::get("index.html") {
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(file.data.into_owned()))
            .unwrap();
    }

    // Dev mode fallback: try disk-based serving from source tree
    let disk_candidates = [
        PathBuf::from("dashboard/build"),
        PathBuf::from("../dashboard/build"),
        PathBuf::from("/opt/monopsony/dashboard/build"),
    ];

    let build_dir = disk_candidates
        .iter()
        .find(|d| d.join("index.html").exists());

    if let Some(dir) = build_dir {
        let file_path = if asset_path == "index.html" {
            dir.join("index.html")
        } else {
            let p = dir.join(asset_path);
            if p.exists() {
                p
            } else {
                dir.join("index.html")
            }
        };

        match tokio::fs::read(&file_path).await {
            Ok(contents) => {
                let mime = mime_guess::from_path(&file_path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string();
                Response::builder()
                    .header(header::CONTENT_TYPE, mime)
                    .body(Body::from(contents))
                    .unwrap()
            }
            Err(_) => Response::builder()
                .status(404)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"Dashboard file not found."}"#))
                .unwrap(),
        }
    } else {
        Response::builder()
            .status(404)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":"Dashboard not built. Run 'cd dashboard && npm install && npm run build' before compiling the server."}"#,
            ))
            .unwrap()
    }
}

/// Check whether an IP address is within the Tailscale CGNAT range (100.64.0.0/10).
pub fn is_tailscale_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 100.64.0.0/10 → first octet 100, second octet 64-127
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }
        IpAddr::V6(_) => {
            // Tailscale also assigns fd7a:115c:a1e0::/48 addresses
            // We check the first 6 hex groups for the Tailscale ULA prefix
            // Simplified: check fd7a:115c:a1e0 prefix
            false // IPv6 Tailscale check is uncommon; most clients use IPv4 CGNAT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tailscale_ip_detection() {
        assert!(is_tailscale_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_ip(&"100.125.219.67".parse().unwrap()));
        assert!(is_tailscale_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_tailscale_ip(&"100.63.255.255".parse().unwrap()));
        assert!(!is_tailscale_ip(&"100.128.0.0".parse().unwrap()));
        assert!(!is_tailscale_ip(&"192.168.1.1".parse().unwrap()));
        assert!(!is_tailscale_ip(&"10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_dashboard_config_defaults() {
        let config = DashboardConfig::default();
        assert!(!config.enabled);
        assert!(config.username.is_none());
        assert_eq!(config.session_timeout_hours, 24);
        assert!(!config.always_require_login);
    }
}
