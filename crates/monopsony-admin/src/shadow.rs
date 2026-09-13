//! Shadow package management endpoints.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use sqlx::Row;

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
