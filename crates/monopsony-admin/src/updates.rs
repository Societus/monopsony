//! Update-distribution endpoints (Sprint 34 P1).
//!
//! Read-only view over the `update_releases` / `update_artifacts` tables the
//! server's distributor maintains, plus an out-of-band "check now" trigger.
//! The trigger is a tokio `Notify` owned by the server binary — the admin
//! crate never touches monopsony-server types.

use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;
use tracing::info;

use crate::AdminState;

/// GET /api/v1/updates — stored releases (newest fetch first) with
/// per-release artifact completeness.
pub async fn handle_list_updates(State(state): State<AdminState>) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return Json(json!({"error": "Database not available"}));
    };
    let releases = match db.list_update_releases(20).await {
        Ok(releases) => releases,
        Err(e) => return Json(json!({"error": e.to_string()})),
    };
    let mut out = Vec::new();
    for release in &releases {
        let artifacts = db.list_update_artifacts(&release.id).await.unwrap_or_default();
        // Per-binary-kind platform completeness (contract: 4 platforms each).
        let server_platforms: Vec<&str> = artifacts
            .iter()
            .filter(|a| a.package == "monopsony-server")
            .map(|a| a.platform.as_str())
            .collect();
        let client_platforms: Vec<&str> = artifacts
            .iter()
            .filter(|a| a.package == "monopsony-client")
            .map(|a| a.platform.as_str())
            .collect();
        out.push(json!({
            "version": release.version,
            "prerelease": release.prerelease,
            "fetched_at": release.fetched_at.to_rfc3339(),
            "changelog_url": release.changelog_url,
            "assets_complete": release.assets_complete,
            "server_platforms": server_platforms,
            "client_platforms": client_platforms,
            "artifact_count": artifacts.len(),
        }));
    }
    Json(json!({"releases": out}))
}

/// POST /api/v1/updates/check — trigger an out-of-band distributor poll.
/// The poller picks the signal up between interval ticks; results appear
/// under GET /api/v1/updates once fetched.
pub async fn handle_trigger_update_check(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.update_check_signal {
        Some(signal) => {
            signal.notify_one();
            info!("update check triggered via admin API");
            Json(json!({"success": true, "message": "update check triggered"}))
        }
        None => Json(json!(
            {"success": false, "message": "update distributor not running (requires database)"}
        )),
    }
}
