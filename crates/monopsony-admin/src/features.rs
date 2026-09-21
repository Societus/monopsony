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
    /// Whether the server may remotely assign shadow recipes to clients.
    /// Disabled (and refusing to enable) when the server is publicly exposed.
    #[serde(default)]
    pub remote_assignment_enabled: bool,
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
        Some(db) => match db.get_setting(FEATURE_FLAGS_KEY).await {
            Ok(Some(val)) => serde_json::from_value::<FeatureFlags>(val).unwrap_or_default(),
            Ok(None) => FeatureFlags::default(),
            Err(e) => {
                warn!("Failed to load feature flags: {e}");
                FeatureFlags::default()
            }
        },
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

// ─── Remote Assignment Gate (Sprint 22 Phase F) ─────────────────────

/// Gate for enabling `remote_assignment_enabled`.
///
/// Remote assignment lets the server push shadow recipes to clients, which is
/// dangerous on a publicly exposed server. Refuse to enable the flag when the
/// server appears publicly exposed (funnel mode); that requires proven
/// ownership + hardware-key 2FA for all admins per Sprint 22 Phase F.
pub fn check_remote_assignment_enable(permitted: bool, flag_wanted: bool) -> Result<(), String> {
    if flag_wanted && !permitted {
        Err("remote assignment refused: server appears publicly exposed (funnel mode); requires proven ownership + hardware-key 2FA for all admins (see Sprint 22 Phase F docs)".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_wanted_and_permitted_ok() {
        assert!(check_remote_assignment_enable(true, true).is_ok());
    }

    #[test]
    fn gate_wanted_not_permitted_errs() {
        let err = check_remote_assignment_enable(false, true).unwrap_err();
        assert!(err.contains("remote assignment refused"));
    }

    #[test]
    fn gate_not_wanted_always_ok() {
        assert!(check_remote_assignment_enable(true, false).is_ok());
        assert!(check_remote_assignment_enable(false, false).is_ok());
    }

    #[test]
    fn flags_empty_json_defaults_remote_assignment_false() {
        let flags: FeatureFlags = serde_json::from_str("{}").unwrap();
        assert!(!flags.remote_assignment_enabled);
    }

    #[test]
    fn flags_round_trip_remote_assignment_true() {
        let flags: FeatureFlags =
            serde_json::from_str("{\"remote_assignment_enabled\":true}").unwrap();
        assert!(flags.remote_assignment_enabled);
        let back = serde_json::to_value(&flags).unwrap();
        assert_eq!(back["remote_assignment_enabled"], serde_json::json!(true));
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
        Some(db) => match db.get_setting(MONETIZATION_KEY).await {
            Ok(Some(val)) => serde_json::from_value::<MonetizationConfig>(val).unwrap_or_default(),
            Ok(None) => MonetizationConfig::default(),
            Err(e) => {
                warn!("Failed to load monetization config: {e}");
                MonetizationConfig::default()
            }
        },
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
