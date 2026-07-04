//! Mirror management endpoints.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use sqlx::Row;
use tracing::{info, warn};

use crate::AdminState;

/// GET /api/v1/mirrors — list all mirror configurations.
pub async fn handle_list_mirrors(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.list_mirror_configs().await {
            Ok(configs) => {
                let result: Vec<serde_json::Value> = configs
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "distro": c.distro,
                            "upstream_url": c.upstream_url,
                            "local_path": c.local_path,
                            "sync_enabled": c.sync_enabled,
                            "sync_interval_secs": c.sync_interval_secs,
                            "bandwidth_limit_bps": c.bandwidth_limit_bps,
                            "public": c.public,
                            "last_sync": c.last_sync,
                            "sync_status": c.sync_status,
                            "total_bytes": c.total_bytes,
                        })
                    })
                    .collect();
                Json(serde_json::Value::Array(result))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else {
        Json(serde_json::json!([]))
    }
}

#[derive(Debug, Deserialize)]
pub struct UpsertMirrorRequest {
    pub distro: String,
    pub upstream_url: String,
    pub sync_enabled: Option<bool>,
    pub sync_interval_secs: Option<i64>,
    pub bandwidth_limit_bps: Option<i64>,
    pub include_patterns: Option<Vec<String>>,
    pub exclude_patterns: Option<Vec<String>>,
    pub public: Option<bool>,
}

/// POST /api/v1/mirrors — add or update a mirror configuration.
pub async fn handle_upsert_mirror(
    State(state): State<AdminState>,
    Json(req): Json<UpsertMirrorRequest>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        let config = monopsony_db::db::MirrorUpsertConfig {
            distro: req.distro.clone(),
            upstream_url: req.upstream_url.clone(),
            local_path: format!("mirrors/{}", req.distro),
            sync_enabled: req.sync_enabled.unwrap_or(true),
            sync_interval_secs: req.sync_interval_secs.unwrap_or(3600),
            bandwidth_limit_bps: req.bandwidth_limit_bps,
            include_patterns: serde_json::to_value(
                req.include_patterns.unwrap_or_default(),
            )
            .unwrap_or_default(),
            exclude_patterns: serde_json::to_value(
                req.exclude_patterns.unwrap_or_default(),
            )
            .unwrap_or_default(),
            public: req.public.unwrap_or(false),
        };

        match db.upsert_mirror_config(&config).await {
            Ok(_) => {
                // Also update the in-memory mirror manager if available
                if let Some(ref mgr) = state.mirror_manager {
                    if let Some(mc) = mirror_config_from_upsert(&config) {
                        mgr.upsert_config(mc).await;
                    }
                }
                Json(serde_json::json!({"success": true}))
            }
            Err(e) => Json(serde_json::json!({"success": false, "message": e.to_string()})),
        }
    } else {
        Json(serde_json::json!({"success": false, "message": "Database not available"}))
    }
}

fn mirror_config_from_upsert(config: &monopsony_db::db::MirrorUpsertConfig) -> Option<monopsony_mirror::mirror::MirrorConfig> {
    use monopsony_mirror::mirror::MirrorConfig;
    use std::path::PathBuf;

    Some(MirrorConfig {
        distro: config.distro.clone(),
        upstream_url: config.upstream_url.clone(),
        local_path: PathBuf::from(&config.local_path),
        sync_enabled: config.sync_enabled,
        sync_interval_secs: config.sync_interval_secs as u64,
        bandwidth_limit_bps: config.bandwidth_limit_bps.map(|v| v as u64),
        include_patterns: serde_json::from_value(config.include_patterns.clone()).unwrap_or_default(),
        exclude_patterns: serde_json::from_value(config.exclude_patterns.clone()).unwrap_or_default(),
        public: config.public,
        ..Default::default()
    })
}

/// DELETE /api/v1/mirrors/:distro — remove a mirror.
pub async fn handle_delete_mirror(
    State(state): State<AdminState>,
    Path(distro): Path<String>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.delete_mirror_config(&distro).await {
            Ok(true) => {
                if let Some(ref mgr) = state.mirror_manager {
                    mgr.remove_config(&distro).await;
                }
                Json(serde_json::json!({"success": true}))
            }
            Ok(false) => Json(serde_json::json!({"success": false, "message": "Not found"})),
            Err(e) => Json(serde_json::json!({"success": false, "message": e.to_string()})),
        }
    } else {
        Json(serde_json::json!({"success": false, "message": "Database not available"}))
    }
}

/// POST /api/v1/mirrors/:distro/sync — trigger immediate sync.
///
/// Spawns the sync as a background task so the HTTP response returns
/// immediately. The sync status is visible via GET /mirrors/:distro/status.
pub async fn handle_trigger_sync(
    State(state): State<AdminState>,
    Path(distro): Path<String>,
) -> impl IntoResponse {
    let mgr = match &state.mirror_manager {
        Some(m) => m.clone(),
        None => return Json(serde_json::json!({"success": false, "message": "Mirror manager not available"})),
    };

    // Verify the mirror exists before spawning
    match mgr.get_config(&distro).await {
        Some(config) => {
            if config.sync_status == "syncing" {
                return Json(serde_json::json!({"success": false, "message": "Sync already in progress"}));
            }

            // Mark as syncing immediately so the UI reflects it
            if let Some(ref db) = state.db {
                let _ = db.update_mirror_sync_status(&distro, "syncing").await;
            }

            // Spawn the sync as a detached background task
            let distro_cloned = distro.clone();
            let db = state.db.clone();
            tokio::spawn(async move {
                let result = mgr.trigger_sync(&distro_cloned).await;
                match &result {
                    Ok(()) => info!("Background sync completed for {}", distro_cloned),
                    Err(e) => {
                        warn!("Background sync failed for {}: {}", distro_cloned, e);
                        if let Some(db) = db {
                            let _ = db.update_mirror_sync_status(&distro_cloned, &format!("error: {}", e)).await;
                        }
                    }
                }
            });

            Json(serde_json::json!({"success": true, "message": "Sync started in background"}))
        }
        None => Json(serde_json::json!({"success": false, "message": format!("Mirror not found: {}", distro)})),
    }
}

/// GET /api/v1/mirrors/:distro/status — detailed sync status for a mirror.
pub async fn handle_mirror_status(
    State(state): State<AdminState>,
    Path(distro): Path<String>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.get_mirror_config(&distro).await {
            Ok(Some(c)) => {
                Json(serde_json::json!({
                    "distro": c.distro,
                    "sync_status": c.sync_status,
                    "last_sync": c.last_sync,
                    "total_bytes": c.total_bytes,
                    "sync_enabled": c.sync_enabled,
                    "sync_interval_secs": c.sync_interval_secs,
                }))
            }
            Ok(None) => Json(serde_json::json!({"error": "Mirror not found"})),
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else {
        Json(serde_json::json!({"error": "Database not available"}))
    }
}
