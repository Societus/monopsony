//! Monopsony Admin API — Dashboard backend
//!
//! Provides a REST API at `/api/v1/*` and serves the embedded dashboard
//! SPA at `/dashboard/*`. Uses Tailscale-aware authentication.

pub mod auth;
pub mod cache;
pub mod clients;
pub mod features;
pub mod health;
pub mod help;
pub mod mirrors;
pub mod pairing;
pub mod recipe;
pub mod settings;
pub mod shadow;
pub mod tracker;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::from_fn_with_state,
    response::Response,
    routing::{get, post},
    Router,
};
use monopsony_control::tls::TlsManager;
use monopsony_db::Database;
use monopsony_mirror::mirror::MirrorManager;
use monopsony_shadow::ShadowPackageManager;
use serde::{Deserialize, Serialize};
use tower_cookies::CookieManagerLayer;

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
    pub tailscale: Arc<auth::TailscaleDetector>,
    /// The server's gRPC listen address (e.g. "0.0.0.0:50051") used in PairResponse.
    pub grpc_addr: String,
    /// Active pairing session (code, status, deadline) managed by the pairing module.
    pub pairing_manager: Arc<tokio::sync::Mutex<Option<pairing::PairingSession>>>,
    /// Handle to the spawned pairing listener task — aborted on cancel to free port 50052.
    pub pairing_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            username: None,
            password_hash: None,
            session_timeout_hours: 24,
            always_require_login: false,
        }
    }
}

fn default_session_timeout() -> u64 {
    24
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
        // Health
        .route("/api/v1/health", get(health::handle_health))
        .route("/api/v1/stats", get(health::handle_stats))
        // Cache
        .route("/api/v1/cache", get(cache::handle_list_cache))
        .route("/api/v1/cache/stats", get(cache::handle_cache_stats))
        .route("/api/v1/cache/:key", axum::routing::delete(cache::handle_purge_entry))
        .route("/api/v1/cache/purge", post(cache::handle_purge))
        .route("/api/v1/scan/results", get(cache::handle_scan_results))
        // Shadow
        .route("/api/v1/shadow/packages", get(shadow::handle_list_packages))
        .route("/api/v1/shadow/packages", post(shadow::handle_register_package))
        .route("/api/v1/shadow/packages/:id", axum::routing::delete(shadow::handle_unregister_package))
        .route("/api/v1/shadow/packages/:id/recipe", get(shadow::handle_get_recipe))
        .route("/api/v1/shadow/packages/:id/check", post(shadow::handle_check_updates))
        .route("/api/v1/shadow/packages/:id/apply", post(shadow::handle_apply_update))
        .route("/api/v1/shadow/packages/:id/rollback", post(shadow::handle_rollback_update))
        .route("/api/v1/shadow/history", get(shadow::handle_history))
        // Recipe
        .route("/api/v1/shadow/recipe/validate", post(recipe::handle_validate))
        .route("/api/v1/shadow/recipe/estimate", post(recipe::handle_estimate))
        // Settings
        .route("/api/v1/config", get(settings::handle_get_config))
        .route("/api/v1/config", post(settings::handle_update_config))
        .route("/api/v1/config/scan", get(settings::handle_get_scan_config))
        .route("/api/v1/config/node", get(settings::handle_get_node_config))
        // Clients
        .route("/api/v1/clients", get(clients::handle_list_clients))
        .route("/api/v1/clients/:id", get(clients::handle_get_client))
        .route("/api/v1/clients/:id/inventory", get(clients::handle_client_inventory))
        // Mirrors
        .route("/api/v1/mirrors", get(mirrors::handle_list_mirrors))
        .route("/api/v1/mirrors", post(mirrors::handle_upsert_mirror))
        .route("/api/v1/mirrors/:distro", axum::routing::delete(mirrors::handle_delete_mirror))
        .route("/api/v1/mirrors/:distro/sync", post(mirrors::handle_trigger_sync))
        .route("/api/v1/mirrors/:distro/status", get(mirrors::handle_mirror_status))
        // Tracker
        .route("/api/v1/tracker/status", get(tracker::handle_tracker_status))
        .route("/api/v1/tracker/mirrors", get(tracker::handle_tracker_mirrors))
        .route("/api/v1/tracker/reputation/:npub", get(tracker::handle_tracker_reputation))
        .route("/api/v1/tracker/reports", get(tracker::handle_tracker_reports))
        // Pairing
        .route("/api/v1/pair/init", post(pairing::handle_pair_init))
        .route("/api/v1/pair/status", get(pairing::handle_pair_status))
        .route("/api/v1/pair/cancel", post(pairing::handle_pair_cancel))
        // Features & Monetization
        .route("/api/v1/features", get(features::handle_get_features))
        .route("/api/v1/features", axum::routing::put(features::handle_update_features))
        .route("/api/v1/features/monetization", get(features::handle_get_monetization))
        .route("/api/v1/features/monetization", axum::routing::put(features::handle_update_monetization))
        // Help system
        .route("/api/v1/help/:key", get(help::handle_get_help))
        // Protected by auth middleware
        .layer(from_fn_with_state(state.clone(), auth::auth_middleware));

    // The SPA fallback serves static files from the dashboard build directory.
    // If the dashboard isn't built yet, it returns a simple JSON message.
    let spa = Router::new()
        .fallback(handle_spa_fallback);

    Router::new()
        .merge(api)
        .merge(spa)
        .layer(CookieManagerLayer::new())
        .with_state(state)
}

/// Serve the SvelteKit SPA from the dashboard build directory.
///
/// Tries to serve from `dashboard/build/` relative to the crate workspace,
/// or falls back to a JSON message if the dashboard hasn't been built.
async fn handle_spa_fallback(
    State(state): State<AdminState>,
    req: Request,
) -> Response {
    use axum::body::Body;
    use axum::http::header;

    let raw_path = req.uri().path();
    // SvelteKit is built with base: '/dashboard', so asset URLs start with /dashboard/
    // Strip that prefix when looking up files in the build directory
    let path = raw_path
        .trim_start_matches('/')
        .strip_prefix("dashboard/")
        .unwrap_or_else(|| raw_path.trim_start_matches('/'));

    // If path is empty or just "dashboard", serve index.html
    let path = if path.is_empty() || path == "dashboard" { "index.html" } else { path };

    // Try to find the dashboard build directory
    // Check several locations
    let candidates = [
        PathBuf::from("dashboard/build"),
        PathBuf::from("../dashboard/build"),
        state.cache_dir.join("../dashboard/build"),
        PathBuf::from("/opt/monopsony/dashboard/build"),
    ];

    let build_dir = candidates.iter().find(|d| d.join("index.html").exists());

    match build_dir {
        Some(dir) => {
            // Serve static files from the build directory
            let file_path = if path.is_empty() || path == "index.html" {
                dir.join("index.html")
            } else {
                dir.join(path)
            };

            // For SPA routing: if the file doesn't exist, serve index.html
            let file_path = if file_path.exists() {
                file_path
            } else {
                dir.join("index.html")
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
                Err(_) => {
                    Response::builder()
                        .status(404)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"error":"Dashboard build not found. Run 'npm run build' in dashboard/."}"#,
                        ))
                        .unwrap()
                }
            }
        }
        None => {
            // Dashboard not built — return a helpful message
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"error":"Dashboard not built. Run 'cd dashboard && npm install && npm run build' first."}"#,
                ))
                .unwrap()
        }
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
