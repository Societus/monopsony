//! Feature toggle and monetization endpoints.
//!
//! Manages server-level feature flags: shadow package support on mirrors,
//! storage/bandwidth quotas, client trust modes, and public server
//! funding mechanisms (donations, bounties, V4V).
//!
//! Settings are persisted in the `server_settings` table as JSONB values
//! under keys `feature_flags` and `monetization`.

use axum::{extract::State, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AdminState;

const FEATURE_FLAGS_KEY: &str = "feature_flags";
const MONETIZATION_KEY: &str = "monetization";

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
pub async fn handle_get_features(State(state): State<AdminState>) -> impl IntoResponse {
    let flags = match &state.db {
        Some(db) => {
            match db.get_setting(FEATURE_FLAGS_KEY).await {
                Ok(Some(val)) => serde_json::from_value::<FeatureFlags>(val).unwrap_or_default(),
                Ok(None) => FeatureFlags::default(),
                Err(e) => {
                    warn!("Failed to load feature flags: {e}");
                    FeatureFlags::default()
                }
            }
        }
        None => FeatureFlags::default(),
    };
    Json(flags)
}

/// PUT /api/v1/features — update feature flags.
pub async fn handle_update_features(
    State(state): State<AdminState>,
    Json(req): Json<FeatureFlags>,
) -> impl IntoResponse {
    match &state.db {
        Some(db) => {
            let val = serde_json::to_value(&req).unwrap_or_default();
            match db.set_setting(FEATURE_FLAGS_KEY, &val).await {
                Ok(()) => Json(serde_json::json!({
                    "success": true,
                    "features": req,
                    "message": "Feature flags saved"
                })),
                Err(e) => Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to save: {e}")
                })),
            }
        }
        None => Json(serde_json::json!({
            "success": false,
            "error": "Database not available"
        })),
    }
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
pub async fn handle_get_monetization(State(state): State<AdminState>) -> impl IntoResponse {
    let config = match &state.db {
        Some(db) => {
            match db.get_setting(MONETIZATION_KEY).await {
                Ok(Some(val)) => serde_json::from_value::<MonetizationConfig>(val).unwrap_or_default(),
                Ok(None) => MonetizationConfig::default(),
                Err(e) => {
                    warn!("Failed to load monetization config: {e}");
                    MonetizationConfig::default()
                }
            }
        }
        None => MonetizationConfig::default(),
    };
    Json(config)
}

/// PUT /api/v1/features/monetization — update monetization config.
pub async fn handle_update_monetization(
    State(state): State<AdminState>,
    Json(req): Json<MonetizationConfig>,
) -> impl IntoResponse {
    match &state.db {
        Some(db) => {
            let val = serde_json::to_value(&req).unwrap_or_default();
            match db.set_setting(MONETIZATION_KEY, &val).await {
                Ok(()) => Json(serde_json::json!({
                    "success": true,
                    "config": req,
                    "message": "Monetization config saved"
                })),
                Err(e) => Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to save: {e}")
                })),
            }
        }
        None => Json(serde_json::json!({
            "success": false,
            "error": "Database not available"
        })),
    }
}
