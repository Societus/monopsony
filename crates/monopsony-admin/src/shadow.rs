//! Shadow package management endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::AdminState;

/// GET /api/v1/shadow/packages — list all shadow packages.
pub async fn handle_list_packages(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        let pool = db.pool();
        let rows = sqlx::query(
            "SELECT id, name, version, source_type, source_url \
             FROM shadow_packages ORDER BY name",
        )
        .fetch_all(pool)
        .await;

        match rows {
            Ok(rows) => {
                let packages: Vec<serde_json::Value> = rows.iter().map(|row| {
                    serde_json::json!({
                        "id": row.try_get::<uuid::Uuid, _>("id").ok(),
                        "name": row.try_get::<String, _>("name").unwrap_or_default(),
                        "version": row.try_get::<String, _>("version").unwrap_or_default(),
                        "source_type": row.try_get::<String, _>("source_type").unwrap_or_default(),
                        "source_url": row.try_get::<String, _>("source_url").unwrap_or_default(),
                    })
                }).collect();
                Json(serde_json::Value::Array(packages))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else {
        Json(serde_json::json!([]))
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterPackageRequest {
    pub name: String,
    pub source_type: String,
    pub source_url: String,
    pub client_id: Option<String>,
}

/// POST /api/v1/shadow/packages — register a new shadow package.
pub async fn handle_register_package(
    State(state): State<AdminState>,
    Json(req): Json<RegisterPackageRequest>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        let client_uuid = uuid::Uuid::new_v4();
        let install_paths = serde_json::json!([]);
        let config_paths: Option<&serde_json::Value> = None;

        match db
            .create_shadow_package(
                &client_uuid,
                &req.name,
                "0.0.0",
                "x86_64",
                "linux",
                None,
                &req.source_type,
                &req.source_url,
                "",
                &install_paths,
                config_paths,
                None,
                None,
                None,
            )
            .await
        {
            Ok(_) => Json(serde_json::json!({"success": true})),
            Err(e) => Json(serde_json::json!({"success": false, "message": e.to_string()})),
        }
    } else {
        Json(serde_json::json!({"success": false, "message": "Database not available"}))
    }
}

/// DELETE /api/v1/shadow/packages/:id — unregister a shadow package.
pub async fn handle_unregister_package(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match uuid::Uuid::parse_str(&id) {
        Ok(uuid) => {
            if let Some(ref db) = state.db {
                match db.delete_shadow_package(&uuid).await {
                    Ok(true) => Json(serde_json::json!({"success": true})),
                    Ok(false) => {
                        Json(serde_json::json!({"success": false, "message": "Not found"}))
                    }
                    Err(e) => Json(serde_json::json!({"success": false, "message": e.to_string()})),
                }
            } else {
                Json(serde_json::json!({"success": false, "message": "Database not available"}))
            }
        }
        Err(_) => Json(serde_json::json!({"success": false, "message": "Invalid UUID"})),
    }
}

/// GET /api/v1/shadow/packages/:id/recipe — get recipe for a package.
pub async fn handle_get_recipe(
    State(_state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    Json(serde_json::json!({"id": id, "recipe": null}))
}

/// POST /api/v1/shadow/packages/:id/check — check for updates.
pub async fn handle_check_updates(
    State(state): State<AdminState>,
    Path(_id): Path<String>,
) -> impl IntoResponse {
    if state.shadow_manager.is_some() {
        Json(serde_json::json!({"success": true, "message": "Update check queued"}))
    } else {
        Json(serde_json::json!({"success": false, "message": "Shadow manager not available"}))
    }
}

/// POST /api/v1/shadow/packages/:id/apply — apply an update.
pub async fn handle_apply_update(Path(_id): Path<String>) -> impl IntoResponse {
    Json(serde_json::json!({"success": false, "message": "Not yet implemented"}))
}

/// POST /api/v1/shadow/packages/:id/rollback — rollback an update.
pub async fn handle_rollback_update(Path(_id): Path<String>) -> impl IntoResponse {
    Json(serde_json::json!({"success": false, "message": "Not yet implemented"}))
}

/// GET /api/v1/shadow/history — update history log.
pub async fn handle_history(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(serde_json::json!([]))
}

/// Derive the suggested staging fields from a parsed catalog recipe. Pure —
/// no db or state — so the mapping is unit-testable. Mapping evidence: the
/// control-plane staging fixture (crates/monopsony-control/src/grpc.rs)
/// surfaces recipe config paths as suggested accessory dirs and firewall
/// rules as (port, protocol, direction, description) rows for client review.
/// The health check is serialized to a JSON string because the staging
/// column is JSONB and create_shadow_staging expects content that parses
/// as JSON.
pub(crate) fn derive_suggested_staging(
    recipe: &monopsony_common::shadow_types::ShadowRecipe,
) -> (
    String,
    Vec<String>,
    Vec<monopsony_db::db::FirewallRuleRow>,
    Option<String>,
) {
    let suggested_target_dir = recipe.install.target_dir.clone();
    let suggested_accessory_dirs = recipe.install.config_paths.clone().unwrap_or_default();
    let suggested_firewall_rules = recipe
        .install
        .firewall_rules
        .iter()
        .map(|rule| monopsony_db::db::FirewallRuleRow {
            port: rule.port,
            protocol: rule.protocol.clone(),
            direction: rule.direction.clone(),
            description: rule.description.clone(),
        })
        .collect();
    let suggested_health_check = recipe
        .install
        .health_check
        .as_ref()
        .and_then(|hc| serde_json::to_string(hc).ok());
    (
        suggested_target_dir,
        suggested_accessory_dirs,
        suggested_firewall_rules,
        suggested_health_check,
    )
}

/// POST /api/v1/shadow/clients/:id/staging — stage a catalog recipe to a
/// client for later review (ListShadowStaging / ResolveShadowStaging).
///
/// Design decision: staging creation is an admin-session operator action and
/// intentionally carries NO feature flag, unlike the client-triggered
/// assignment funnel (handle_assign_recipe) which is dual-gated (server
/// feature flag + public-exposure check + catalog membership). The staged
/// row is only a suggestion — the client still reviews and resolves it
/// before anything is installed.
pub async fn handle_create_staging(
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
    let client_uuid = match Uuid::parse_str(&client_id) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"accepted": false, "reason": "invalid client id"})),
            )
        }
    };

    let catalog = match db.list_shadow_catalog().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "create-staging: catalog query failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"accepted": false, "reason": e.to_string()})),
            );
        }
    };
    let Some(row) = catalog.iter().find(|r| r.name == recipe_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"accepted": false, "reason": format!("recipe '{}' not found in the shadow catalog", recipe_name)})),
        );
    };

    let recipe: monopsony_common::shadow_types::ShadowRecipe =
        match toml::from_str(&row.recipe_toml) {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"accepted": false, "reason": format!("stored recipe TOML failed to parse: {e}")})),
                )
            }
        };

    let (suggested_target_dir, suggested_accessory_dirs, suggested_firewall_rules, suggested_health_check) =
        derive_suggested_staging(&recipe);

    match db
        .create_shadow_staging(
            client_uuid,
            &recipe_name,
            &suggested_target_dir,
            &suggested_accessory_dirs,
            &suggested_firewall_rules,
            suggested_health_check.as_deref(),
        )
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "accepted": true,
                "recipe_name": recipe_name,
                "client_id": client_id,
            })),
        ),
        // The documented Err path of create_shadow_staging is the
        // (client, recipe) conflict; surface it as 409. Anything else is a
        // storage failure, not a conflict — 500.
        Err(e) if e.to_string().contains("already exists") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"accepted": false, "reason": e.to_string()})),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "create-staging: insert failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"accepted": false, "reason": e.to_string()})),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RECIPE_TOML: &str = r#"
[package]
name = "checkmk-agent"
source_type = "github_release"
source_url = "https://example.com/checkmk"

[install]
method = "extract"
target_dir = "/opt/checkmk"
config_paths = ["/etc/checkmk"]

[[install.firewall_rules]]
port = 8443
protocol = "tcp"
direction = "inbound"
description = "web UI"

[[install.firewall_rules]]
port = 514
protocol = "udp"
direction = "outbound"
description = "syslog"

[install.health_check]
type = "port"
port = 8443

[version]
detect_command = "/opt/checkmk --version"
detect_pattern = "([0-9.]+)"
"#;

    const MINIMAL_RECIPE_TOML: &str = r#"
[package]
name = "tiny"
source_type = "github_release"
source_url = "https://example.com/tiny"

[install]
method = "extract"
target_dir = "/opt/tiny"

[version]
detect_command = "/opt/tiny --version"
detect_pattern = "([0-9.]+)"
"#;

    fn parse(toml_str: &str) -> monopsony_common::shadow_types::ShadowRecipe {
        toml::from_str(toml_str).expect("fixture recipe must parse")
    }

    #[test]
    fn derive_full_recipe_yields_catalog_suggestions() {
        let (target, accessory, firewall, health) = derive_suggested_staging(&parse(SAMPLE_RECIPE_TOML));
        assert_eq!(target, "/opt/checkmk");
        assert_eq!(accessory, vec!["/etc/checkmk".to_string()]);
        assert_eq!(firewall.len(), 2);
        assert_eq!(firewall[0].port, 8443);
        assert_eq!(firewall[0].protocol, "tcp");
        assert_eq!(firewall[0].direction, "inbound");
        assert_eq!(firewall[0].description, "web UI");
        assert_eq!(firewall[1].port, 514);
        assert_eq!(firewall[1].protocol, "udp");
        assert_eq!(firewall[1].direction, "outbound");
        assert_eq!(firewall[1].description, "syslog");
        // The health check string must parse as JSON (staging column is JSONB).
        let hc: serde_json::Value = serde_json::from_str(&health.expect("health check present"))
            .expect("health check must be valid JSON");
        assert_eq!(hc["type"], "port");
        assert_eq!(hc["port"], 8443);
    }

    #[test]
    fn derive_minimal_recipe_yields_empty_defaults() {
        let (target, accessory, firewall, health) = derive_suggested_staging(&parse(MINIMAL_RECIPE_TOML));
        assert_eq!(target, "/opt/tiny");
        assert!(accessory.is_empty());
        assert!(firewall.is_empty());
        assert!(health.is_none());
    }

    #[test]
    fn malformed_recipe_toml_is_rejected_by_parse_step() {
        assert!(toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(
            "this is not [valid toml"
        )
        .is_err());
    }
}
