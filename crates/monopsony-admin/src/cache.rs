//! Cache management endpoints.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};

use crate::AdminState;

/// GET /api/v1/cache — list cache entries.
pub async fn handle_list_cache(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.list_cache_entries().await {
            Ok(entries) => {
                let result: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "url": e.url,
                            "content_type": e.content_type,
                            "content_length": e.content_length,
                            "cached_at": e.cached_at,
                            "last_accessed": e.last_accessed,
                            "hit_count": e.hit_count,
                            "sha256": e.sha256,
                        })
                    })
                    .collect();
                Json(serde_json::Value::Array(result))
            }
            Err(_) => Json(serde_json::json!([])),
        }
    } else {
        Json(serde_json::json!([]))
    }
}

/// GET /api/v1/cache/stats — cache statistics.
pub async fn handle_cache_stats(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.cache_stats().await {
            Ok((total_entries, total_size_bytes)) => Json(serde_json::json!({
                "cache_dir": state.cache_dir.to_string_lossy(),
                "total_entries": total_entries,
                "total_size_bytes": total_size_bytes,
            })),
            Err(e) => {
                tracing::error!("Failed to query cache stats: {}", e);
                Json(serde_json::json!({
                    "cache_dir": state.cache_dir.to_string_lossy(),
                    "total_entries": 0,
                    "total_size_bytes": 0,
                    "error": e.to_string(),
                }))
            }
        }
    } else {
        Json(serde_json::json!({
            "cache_dir": state.cache_dir.to_string_lossy(),
            "total_entries": 0,
            "total_size_bytes": 0,
            "error": "Database not available",
        }))
    }
}

/// DELETE /api/v1/cache/:key — purge a specific cache entry.
pub async fn handle_purge_entry(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let key = urlencoding::decode(&key)
        .map(|s| s.to_string())
        .unwrap_or(key);

    if let Some(ref db) = state.db {
        match db.delete_cache_entry(&key).await {
            Ok(true) => Json(serde_json::json!({"success": true})),
            Ok(false) => Json(serde_json::json!({"success": false, "message": "Not found"})),
            Err(e) => Json(serde_json::json!({"success": false, "message": e.to_string()})),
        }
    } else {
        Json(serde_json::json!({"success": false, "message": "Database not available"}))
    }
}

/// POST /api/v1/cache/purge — bulk purge by age or pattern.
pub async fn handle_purge(State(_state): State<AdminState>, _body: String) -> impl IntoResponse {
    Json(serde_json::json!({"success": false, "message": "Not yet implemented"}))
}

/// GET /api/v1/scan/results — recent scan results.
pub async fn handle_scan_results(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(serde_json::json!({"results": []}))
}
