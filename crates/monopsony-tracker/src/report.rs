//! Client report protocol.
//!
//! Clients can report hash mismatches, slow responses, stale mirrors, or
//! malicious content to the tracker. Reports are stored in the `reports`
//! table and used for reputation scoring and auto-blacklisting.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::TrackerState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRequest {
    pub mirror_id: String,
    pub report_type: ReportType,
    pub package_name: Option<String>,
    pub expected_hash: Option<String>,
    pub actual_hash: Option<String>,
    pub client_ip: Option<String>,
    pub timestamp: i64,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportType {
    HashMismatch,
    SlowResponse,
    StaleMirror,
    MaliciousContent,
    Other,
}

impl std::fmt::Display for ReportType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashMismatch => write!(f, "hash_mismatch"),
            Self::SlowResponse => write!(f, "slow_response"),
            Self::StaleMirror => write!(f, "stale_mirror"),
            Self::MaliciousContent => write!(f, "malicious_content"),
            Self::Other => write!(f, "other"),
        }
    }
}

/// GET /reports response
#[derive(Debug, Serialize)]
pub struct ReportsResponse {
    pub reports: Vec<ReportEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReportEntry {
    pub id: i64,
    pub mirror_id: String,
    pub report_type: String,
    pub package_name: Option<String>,
    pub expected_hash: Option<String>,
    pub actual_hash: Option<String>,
    pub client_ip: Option<String>,
    pub details: Option<String>,
    pub timestamp: i64,
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Create the reports table if it doesn't exist.
#[allow(dead_code)]
pub fn init_reports_table(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reports (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            mirror_id    TEXT NOT NULL,
            report_type  TEXT NOT NULL,
            package_name TEXT,
            expected_hash TEXT,
            actual_hash  TEXT,
            client_ip    TEXT,
            details      TEXT,
            timestamp    INTEGER NOT NULL,
            created_at   INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );
        CREATE INDEX IF NOT EXISTS idx_reports_mirror ON reports(mirror_id);
        CREATE INDEX IF NOT EXISTS idx_reports_type ON reports(report_type);
        ",
    );
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /report — client reports a problem with a mirror
pub async fn handle_report(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<ReportRequest>,
) -> impl IntoResponse {
    if req.mirror_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "mirror_id required").into_response();
    }

    let db = state.db.lock().await;
    let report_type_str = req.report_type.to_string();
    let now = chrono::Utc::now().timestamp();

    let result = db.execute(
        "INSERT INTO reports (mirror_id, report_type, package_name, expected_hash,
            actual_hash, client_ip, details, timestamp, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            req.mirror_id,
            report_type_str,
            req.package_name,
            req.expected_hash,
            req.actual_hash,
            req.client_ip,
            req.details,
            req.timestamp,
            now,
        ],
    );

    match result {
        Ok(_) => {
            info!(
                "Report: mirror={} type={}",
                req.mirror_id, report_type_str
            );
            (StatusCode::OK, "reported").into_response()
        }
        Err(e) => {
            warn!("Report DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// GET /reports?mirror_id=... — list reports for a mirror (optional filter)
pub async fn handle_list_reports(
    State(state): State<Arc<TrackerState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let db = state.db.lock().await;

    let rows = if let Some(mirror_id) = params.get("mirror_id") {
        let mut stmt = match db.prepare(
            "SELECT id, mirror_id, report_type, package_name, expected_hash,
                    actual_hash, client_ip, details, timestamp, created_at
             FROM reports WHERE mirror_id = ?1
             ORDER BY created_at DESC LIMIT 100"
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("List reports DB prepare: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        };
        stmt.query_map(params![mirror_id], |row| {
            Ok(ReportEntry {
                id: row.get(0)?,
                mirror_id: row.get(1)?,
                report_type: row.get(2)?,
                package_name: row.get(3).ok(),
                expected_hash: row.get(4).ok(),
                actual_hash: row.get(5).ok(),
                client_ip: row.get(6).ok(),
                details: row.get(7).ok(),
                timestamp: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .map(|r| r.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    } else {
        let mut stmt = match db.prepare(
            "SELECT id, mirror_id, report_type, package_name, expected_hash,
                    actual_hash, client_ip, details, timestamp, created_at
             FROM reports
             ORDER BY created_at DESC LIMIT 100"
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("List reports DB prepare: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        };
        stmt.query_map([], |row| {
            Ok(ReportEntry {
                id: row.get(0)?,
                mirror_id: row.get(1)?,
                report_type: row.get(2)?,
                package_name: row.get(3).ok(),
                expected_hash: row.get(4).ok(),
                actual_hash: row.get(5).ok(),
                client_ip: row.get(6).ok(),
                details: row.get(7).ok(),
                timestamp: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .map(|r| r.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    };

    Json(ReportsResponse { reports: rows }).into_response()
}

/// Count reports for a mirror in the last N hours. Used by blacklisting logic.
#[allow(dead_code)]
pub async fn count_recent_reports(
    db: &tokio::sync::MutexGuard<'_, rusqlite::Connection>,
    mirror_id: &str,
    report_type: &str,
    hours: i64,
) -> i64 {
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);
    db.query_row(
        "SELECT COUNT(*) FROM reports WHERE mirror_id = ?1 AND report_type = ?2 AND created_at > ?3",
        params![mirror_id, report_type, cutoff],
        |row| row.get(0),
    )
    .unwrap_or(0)
}
