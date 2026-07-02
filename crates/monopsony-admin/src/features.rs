//! Feature toggle and monetization endpoints.
//!
//! Manages server-level feature flags: shadow package support on mirrors,
//! storage/bandwidth quotas, client trust modes, and public server
//! funding mechanisms (donations, bounties, V4V).

use axum::{extract::State, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::AdminState;

// ─── Feature Flags ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeatureFlags {
    /// Whether mirrors support shadow packages
    #[serde(default)]
    pub shadow_enabled: bool,
    /// Whether storage quota is enforced
    #[serde(default)]
    pub storage_quota_enabled: bool,
    /// Storage quota in GB (0 = unlimited)
    #[serde(default)]
    pub storage_quota_gb: u64,
    /// Whether bandwidth quota is enforced
    #[serde(default)]
    pub bandwidth_quota_enabled: bool,
    /// Bandwidth quota in Mbps (0 = unlimited)
    #[serde(default)]
    pub bandwidth_quota_mbps: u64,
    /// Client trust mode: "open", "token", "jwt", "mtls"
    #[serde(default = "default_trust_mode")]
    pub trust_mode: String,
    /// Auto-blacklist threshold (number of reports before auto-blacklist)
    #[serde(default = "default_auto_blacklist_threshold")]
    pub auto_blacklist_threshold: i64,
}

fn default_trust_mode() -> String {
    "open".to_string()
}

fn default_auto_blacklist_threshold() -> i64 {
    5
}

/// GET /api/v1/features — get current feature flags.
pub async fn handle_get_features(State(_state): State<AdminState>) -> impl IntoResponse {
    // TODO: load from server.toml or DB. For now, return defaults.
    Json(FeatureFlags::default())
}

/// PUT /api/v1/features — update feature flags.
pub async fn handle_update_features(
    State(_state): State<AdminState>,
    Json(req): Json<FeatureFlags>,
) -> impl IntoResponse {
    // TODO: persist to config and apply at runtime
    Json(serde_json::json!({
        "success": true,
        "features": req,
        "message": "Feature flags updated (requires restart for some changes)"
    }))
}

// ─── Monetization ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonetizationConfig {
    /// Whether monetization features are visible to clients
    #[serde(default)]
    pub public_metrics_enabled: bool,
    /// Lightning payment pointer (e.g., Lightning address)
    #[serde(default)]
    pub lightning_address: Option<String>,
    /// Bitcoin donation address
    #[serde(default)]
    pub bitcoin_address: Option<String>,
    /// OpenAlias payment pointer
    #[serde(default)]
    pub openalias: Option<String>,
    /// Bug bounty fund address
    #[serde(default)]
    pub bounty_address: Option<String>,
    /// Bounty reward tiers (in sats)
    #[serde(default)]
    pub bounty_critical_sats: Option<u64>,
    #[serde(default)]
    pub bounty_high_sats: Option<u64>,
    #[serde(default)]
    pub bounty_medium_sats: Option<u64>,
    /// V4V (Value for Value) boost configuration
    #[serde(default)]
    pub v4v_enabled: bool,
    #[serde(default)]
    pub v4v_default_boost_sats: Option<u64>,
}

/// GET /api/v1/features/monetization — get monetization config.
pub async fn handle_get_monetization(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(MonetizationConfig::default())
}

/// PUT /api/v1/features/monetization — update monetization config.
pub async fn handle_update_monetization(
    State(_state): State<AdminState>,
    Json(req): Json<MonetizationConfig>,
) -> impl IntoResponse {
    // TODO: persist to config
    Json(serde_json::json!({
        "success": true,
        "config": req,
        "message": "Monetization config updated"
    }))
}
