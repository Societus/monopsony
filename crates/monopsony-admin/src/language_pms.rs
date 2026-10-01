//! Language-PM runtime enablement endpoints (Sprint 23 Phase D).
//!
//! Exposes the runtime enablement surface for language package managers
//! (npm, pip, cargo). Settings live in the `server_settings` table as a
//! JSONB map under `monopsony_db::LANGUAGE_PMS_KEY` — changes apply at
//! runtime, no restart required.
//!
//! API contract (pinned — dashboard codes against this verbatim):
//! ```text
//! GET   /api/v1/admin/language-pms
//!   → 200 {"npm":   {"enabled": bool, "source": "default"|"auto"|"manual"},
//!          "pip":   {...}, "cargo": {...}}
//! PATCH /api/v1/admin/language-pms   body: {"pm": "npm"|"pip"|"cargo", "enabled": bool}
//!   → 200 updated full map (same shape as GET) · 400 unknown pm
//! ```

use std::collections::HashMap;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use monopsony_db::db::LanguagePmState;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AdminState;

/// The language PMs exposed on the admin surface. Order is the pinned
/// dashboard display order.
pub const SUPPORTED_LANGUAGE_PMS: [&str; 3] = ["npm", "pip", "cargo"];

/// `source` value for a state explicitly set via this API.
const SOURCE_MANUAL: &str = "manual";

/// Enablement state of one language PM as served to the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LanguagePmStateDto {
    pub enabled: bool,
    pub source: String,
}

/// PATCH request body: `{"pm": "npm"|"pip"|"cargo", "enabled": bool}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchLanguagePmRequest {
    pub pm: String,
    pub enabled: bool,
}

/// The full enablement map in pinned dashboard key order.
pub type LanguagePmsMap = HashMap<String, LanguagePmStateDto>;

/// Build the default map: all three PMs `{"enabled": false, "source": "default"}`.
/// Used when the setting is missing/unset or the DB is unavailable.
pub fn default_language_pms_map() -> LanguagePmsMap {
    SUPPORTED_LANGUAGE_PMS
        .iter()
        .map(|pm| {
            (
                (*pm).to_string(),
                LanguagePmStateDto {
                    enabled: false,
                    source: "default".to_string(),
                },
            )
        })
        .collect()
}

/// Validate a PATCH body's `pm` and `enabled` fields (pure; unit-tested).
/// Returns `(pm, enabled)` or a client-facing error message.
pub fn parse_patch(req: &PatchLanguagePmRequest) -> Result<(String, bool), String> {
    if !SUPPORTED_LANGUAGE_PMS.contains(&req.pm.as_str()) {
        return Err(format!(
            "unknown pm {:?}; supported: {}",
            req.pm,
            SUPPORTED_LANGUAGE_PMS.join(", ")
        ));
    }
    Ok((req.pm.clone(), req.enabled))
}

/// Apply a manual toggle to a (possibly partial) stored map, filling missing
/// entries with defaults (pure; unit-tested). Returned map is always complete.
pub fn apply_patch(
    stored: HashMap<String, LanguagePmState>,
    pm: &str,
    enabled: bool,
) -> LanguagePmsMap {
    let mut out: LanguagePmsMap = SUPPORTED_LANGUAGE_PMS
        .iter()
        .map(|name| {
            let state = stored.get(*name);
            (
                (*name).to_string(),
                LanguagePmStateDto {
                    enabled: state.is_some_and(|s| s.enabled),
                    source: state
                        .map(|s| s.source.clone())
                        .unwrap_or_else(|| "default".to_string()),
                },
            )
        })
        .collect();
    if let Some(entry) = out.get_mut(pm) {
        entry.enabled = enabled;
        entry.source = SOURCE_MANUAL.to_string();
    }
    out
}

/// Convert a `LanguagePmState` row from the DB into the DTO shape.
// Used only by DTO round-trip tests; handlers serialize state directly.
#[cfg(test)]
fn to_dto(state: LanguagePmState) -> LanguagePmStateDto {
    LanguagePmStateDto {
        enabled: state.enabled,
        source: state.source,
    }
}

/// GET /api/v1/admin/language-pms — current enablement map.
pub async fn handle_get_language_pms(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.db {
        Some(db) => match db.get_language_pms().await {
            Ok(stored) => Json(apply_patch(stored, "", false)),
            Err(e) => {
                warn!("Failed to load language PM enablement: {e}");
                Json(default_language_pms_map())
            }
        },
        None => Json(default_language_pms_map()),
    }
}

/// PATCH /api/v1/admin/language-pms — read-modify-write one PM's enablement,
/// mark it `manual`, and record the transition in the audit trail.
pub async fn handle_patch_language_pms(
    State(state): State<AdminState>,
    Json(req): Json<PatchLanguagePmRequest>,
) -> impl IntoResponse {
    let (pm, enabled) = match parse_patch(&req) {
        Ok(parsed) => parsed,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": msg })),
            )
        }
    };

    let Some(db) = state.db.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Database not available" })),
        );
    };

    let stored = match db.get_language_pms().await {
        Ok(map) => map,
        Err(e) => {
            warn!("Failed to load language PM enablement: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Failed to read: {e}") })),
            );
        }
    };

    let updated_map = apply_patch(stored, &pm, enabled);

    let db_states: HashMap<String, LanguagePmState> = updated_map
        .iter()
        .map(|(name, dto)| {
            (
                name.clone(),
                LanguagePmState {
                    enabled: dto.enabled,
                    source: dto.source.clone(),
                },
            )
        })
        .collect();

    // Serialize once for the response body (unifies return types across branches).
    let updated = serde_json::to_value(&updated_map).unwrap_or(serde_json::Value::Null);

    if let Err(e) = db.set_language_pms(&db_states).await {
        warn!("Failed to save language PM enablement: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to save: {e}") })),
        );
    }

    // Audit the transition. Non-fatal: the setting is already saved.
    let outcome = if enabled { "enabled" } else { "disabled" };
    if let Err(e) = db
        .record_pm_enablement_audit(&pm, outcome, SOURCE_MANUAL, None)
        .await
    {
        warn!("Failed to record PM enablement audit for {pm}: {e}");
    }

    (StatusCode::OK, Json(updated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_has_all_three_pms_disabled_by_default() {
        let map = default_language_pms_map();
        assert_eq!(map.len(), 3);
        for pm in SUPPORTED_LANGUAGE_PMS {
            let state = map.get(pm).unwrap();
            assert!(!state.enabled, "{pm} should default to disabled");
            assert_eq!(state.source, "default");
        }
    }

    #[test]
    fn default_map_serializes_to_pinned_shape() {
        let val = serde_json::to_value(default_language_pms_map()).unwrap();
        assert_eq!(
            val,
            serde_json::json!({
                "npm":   {"enabled": false, "source": "default"},
                "pip":   {"enabled": false, "source": "default"},
                "cargo": {"enabled": false, "source": "default"},
            })
        );
    }

    #[test]
    fn parse_patch_accepts_each_supported_pm() {
        for pm in SUPPORTED_LANGUAGE_PMS {
            let req = PatchLanguagePmRequest {
                pm: (*pm).to_string(),
                enabled: true,
            };
            assert_eq!(parse_patch(&req).unwrap(), ((*pm).to_string(), true));
        }
    }

    #[test]
    fn parse_patch_rejects_unknown_pm() {
        let req = PatchLanguagePmRequest {
            pm: "apt".to_string(),
            enabled: true,
        };
        let err = parse_patch(&req).unwrap_err();
        assert!(err.contains("unknown pm"), "got: {err}");
        assert!(
            err.contains("npm"),
            "error should list supported pms: {err}"
        );
    }

    #[test]
    fn apply_patch_sets_manual_source_and_preserves_siblings() {
        let mut stored = HashMap::new();
        stored.insert(
            "pip".to_string(),
            LanguagePmState {
                enabled: true,
                source: "auto".to_string(),
            },
        );
        let map = apply_patch(stored, "npm", true);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.get("npm").unwrap(),
            &LanguagePmStateDto {
                enabled: true,
                source: "manual".to_string()
            }
        );
        // pip keeps its auto-enabled state
        assert_eq!(
            map.get("pip").unwrap(),
            &LanguagePmStateDto {
                enabled: true,
                source: "auto".to_string()
            }
        );
        // cargo fills with defaults
        assert_eq!(
            map.get("cargo").unwrap(),
            &LanguagePmStateDto {
                enabled: false,
                source: "default".to_string()
            }
        );
    }

    #[test]
    fn apply_patch_disable_marks_manual() {
        let mut stored = HashMap::new();
        stored.insert(
            "npm".to_string(),
            LanguagePmState {
                enabled: true,
                source: "auto".to_string(),
            },
        );
        let map = apply_patch(stored, "npm", false);
        assert_eq!(
            map.get("npm").unwrap(),
            &LanguagePmStateDto {
                enabled: false,
                source: "manual".to_string()
            }
        );
    }

    #[test]
    fn to_dto_preserves_fields() {
        let dto = to_dto(LanguagePmState {
            enabled: true,
            source: "auto".to_string(),
        });
        assert!(dto.enabled);
        assert_eq!(dto.source, "auto");
    }
}
