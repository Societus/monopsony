//! Tracker proxy endpoints.
//!
//! These endpoints proxy queries to the tracker's HTTP API when the tracker
//! is running on the same host, or return status indicating it's not available.

use axum::{extract::State, response::IntoResponse, Json};

use crate::AdminState;

/// GET /api/v1/tracker/status — tracker running state and auth mode.
pub async fn handle_tracker_status(State(_state): State<AdminState>) -> impl IntoResponse {
    // The tracker is a separate binary. Check if it's running by probing
    // its health endpoint. We don't have a direct handle to it.
    // For now, return a basic status — the frontend can detect availability.
    Json(serde_json::json!({
        "running": false,
        "auth_mode": "open",
        "message": "Tracker is a standalone binary. Start it with: monopsony-tracker"
    }))
}

/// GET /api/v1/tracker/mirrors — announced mirrors from the tracker.
pub async fn handle_tracker_mirrors(State(_state): State<AdminState>) -> impl IntoResponse {
    // The tracker runs its own HTTP server. The dashboard could proxy
    // requests to it, but for now we return an empty list with a note.
    Json(serde_json::json!({
        "mirrors": [],
        "message": "Tracker API not connected"
    }))
}

/// GET /api/v1/tracker/reputation/:npub — reputation for an operator.
pub async fn handle_tracker_reputation(
    axum::extract::Path(npub): axum::extract::Path<String>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "npub": npub,
        "score": null,
        "events": [],
        "message": "Tracker API not connected"
    }))
}

/// GET /api/v1/tracker/reports — recent abuse/security reports.
pub async fn handle_tracker_reports(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "reports": [],
        "message": "Tracker API not connected"
    }))
}
