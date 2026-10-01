//! Remote shadow-recipe assignment endpoints (Sprint 22 slice 22-E2a).
//!
//! - GET  /api/v1/admin/clients/:id/shadow-context
//! - POST /api/v1/admin/clients/:id/assign-recipe
//!
//! Assignments land in `pending_shadow_assignments` and are pushed to the
//! client as a `ServerMessage::Command` (UPDATE_CONFIG / shadow_assign) the
//! next time the client's control stream yields a message.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

use monopsony_common::models::ClientId;

use crate::{features::FeatureFlags, AdminState};

/// Pure decision gate for assign-recipe. Factored out for unit testing.
///
/// Returns `Ok(())` when the assignment may proceed, otherwise the HTTP
/// status code + reason string to return to the dashboard.
pub fn gate_decision(
    flag_enabled: bool,
    publicly_exposed: bool,
    in_catalog: bool,
) -> Result<(), (StatusCode, String)> {
    if !flag_enabled {
        return Err((
            StatusCode::FORBIDDEN,
            "remote assignment disabled on server".to_string(),
        ));
    }
    if publicly_exposed {
        return Err((
            StatusCode::FORBIDDEN,
            "refused: server appears publicly exposed (funnel mode); requires proven ownership + hardware-key 2FA for all admins".to_string(),
        ));
    }
    if !in_catalog {
        return Err((
            StatusCode::NOT_FOUND,
            "recipe not found in catalog".to_string(),
        ));
    }
    Ok(())
}

/// Inventory entry: one installed package on the client.
#[derive(Debug, Serialize)]
pub struct InventoryEntry {
    pub name: String,
    pub version: String,
    pub manager: String,
}

/// One shadow scan finding for the client.
#[derive(Debug, Serialize)]
pub struct ScanFinding {
    pub recipe_name: String,
    pub installed: bool,
    pub detected_version: Option<String>,
    pub method: Option<String>,
}

/// Response shape for GET /admin/clients/:id/shadow-context (pinned contract).
#[derive(Debug, Serialize)]
pub struct ShadowContextResponse {
    pub inventory: Vec<InventoryEntry>,
    pub scan_findings: Vec<ScanFinding>,
    pub catalog: Vec<String>,
}

/// GET /api/v1/admin/clients/:id/shadow-context
pub async fn handle_shadow_context(
    State(state): State<AdminState>,
    Path(client_id): Path<String>,
) -> impl IntoResponse {
    let Some(db) = state.db.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "database not available"})),
        );
    };
    let uuid = match Uuid::parse_str(&client_id) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid client id"})),
            )
        }
    };
    let cid = ClientId(uuid);

    let inventory = match db.get_packages_for_client(&cid).await {
        Ok(pkgs) => pkgs
            .into_iter()
            .map(|p| InventoryEntry {
                name: p.name,
                version: p.version,
                manager: p.manager,
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, "shadow-context: inventory query failed");
            Vec::new()
        }
    };

    let scan_findings = match db.list_shadow_scan_results().await {
        Ok(rows) => rows
            .into_iter()
            .filter(|r| r.client_id == Some(uuid))
            .map(|r| ScanFinding {
                recipe_name: r.recipe_name,
                installed: r.installed,
                detected_version: r.detected_version,
                method: r.method,
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, "shadow-context: scan results query failed");
            Vec::new()
        }
    };

    let catalog = match db.list_shadow_catalog().await {
        Ok(rows) => rows.into_iter().map(|r| r.name).collect(),
        Err(e) => {
            warn!(error = %e, "shadow-context: catalog query failed");
            Vec::new()
        }
    };

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(ShadowContextResponse {
                inventory,
                scan_findings,
                catalog,
            })
            .unwrap_or_else(
                |_| serde_json::json!({"inventory":[],"scan_findings":[],"catalog":[]}),
            ),
        ),
    )
}

/// POST /api/v1/admin/clients/:id/assign-recipe
pub async fn handle_assign_recipe(
    State(state): State<AdminState>,
    Path(client_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let recipe_name = body
        .get("recipe_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if recipe_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"accepted": false, "reason": "recipe_name is required"})),
        );
    }

    let Some(db) = state.db.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"accepted": false, "reason": "database not available"})),
        );
    };
    let uuid = match Uuid::parse_str(&client_id) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"accepted": false, "reason": "invalid client id"})),
            )
        }
    };
    let client_uuid = Uuid::parse_str(&client_id).ok();

    // 1. Feature flag gate.
    let flag_enabled = match db.get_setting("feature_flags").await {
        Ok(Some(val)) => serde_json::from_value::<FeatureFlags>(val)
            .map(|f| f.remote_assignment_enabled)
            .unwrap_or(false),
        Ok(None) => false,
        Err(e) => {
            warn!(error = %e, "assign-recipe: failed to load feature flags");
            false
        }
    };

    // 2. Public exposure gate (funnel mode).
    let exposed = state.tailscale.detect_public_exposure().await;

    // 3. Catalog membership.
    let in_catalog = match db.list_shadow_catalog().await {
        Ok(rows) => rows.iter().any(|r| r.name == recipe_name),
        Err(e) => {
            warn!(error = %e, "assign-recipe: catalog query failed");
            false
        }
    };

    if let Err((status, reason)) = gate_decision(flag_enabled, exposed, in_catalog) {
        let outcome = if status == StatusCode::NOT_FOUND {
            "refused"
        } else {
            "refused"
        };
        if let Err(e) = db
            .record_shadow_assignment_audit(client_uuid, &recipe_name, outcome, Some(&reason))
            .await
        {
            warn!(error = %e, "assign-recipe: audit write failed");
        }
        return (
            status,
            Json(serde_json::json!({"accepted": false, "reason": reason})),
        );
    }

    // Final assertion — the enable gate from 22-F must agree with the flag.
    if let Err(gate_err) = crate::features::check_remote_assignment_enable(flag_enabled, true) {
        warn!(gate_err = %gate_err, "assign-recipe: final gate assertion failed");
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"accepted": false, "reason": gate_err})),
        );
    }

    match db.create_pending_assignment(uuid, &recipe_name).await {
        Ok(()) => {
            if let Err(e) = db
                .record_shadow_assignment_audit(Some(uuid), &recipe_name, "assigned", None)
                .await
            {
                warn!(error = %e, "assign-recipe: audit write failed");
            }
            (StatusCode::OK, Json(serde_json::json!({"accepted": true})))
        }
        Err(e) => {
            warn!(error = %e, "assign-recipe: pending assignment insert failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "accepted": false,
                    "reason": format!("failed to record pending assignment: {e}")
                })),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn reason_of(res: Result<(), (StatusCode, String)>) -> String {
        res.unwrap_err().1
    }

    #[test]
    fn gate_all_ok_accepts() {
        assert!(gate_decision(true, false, true).is_ok());
    }

    #[test]
    fn gate_flag_disabled_refuses() {
        let res = gate_decision(false, false, true);
        let (status, reason) = res.unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(reason, "remote assignment disabled on server");
    }

    #[test]
    fn gate_publicly_exposed_refuses() {
        let res = gate_decision(true, true, true);
        let (status, reason) = res.unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(reason.contains("publicly exposed"));
    }

    #[test]
    fn gate_not_in_catalog_404s() {
        let res = gate_decision(true, false, false);
        let (status, reason) = res.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(reason.contains("not found"));
    }

    #[test]
    fn gate_golden_disabled_beats_exposure() {
        // Flag check comes first: even if exposed, the disabled reason wins.
        assert_eq!(
            reason_of(gate_decision(false, true, true)),
            "remote assignment disabled on server"
        );
    }
}
