//! Operator reputation system.
//!
//! Tracks operator reliability with a composite reputation score that follows
//! the operator (npub), not individual mirrors. Negative reputation events are
//! disputable via attestation from other operators.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
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
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ReputationEventType {
    Report,
    Attestation,
    UpvoteCluster,
    DownvoteCluster,
    UptimeBonus,
    StalePenalty,
}

impl std::fmt::Display for ReputationEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Report => write!(f, "report"),
            Self::Attestation => write!(f, "attestation"),
            Self::UpvoteCluster => write!(f, "upvote_cluster"),
            Self::DownvoteCluster => write!(f, "downvote_cluster"),
            Self::UptimeBonus => write!(f, "uptime_bonus"),
            Self::StalePenalty => write!(f, "stale_penalty"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DisputeRequest {
    pub event_id: i64,
    pub respondent_npub: String,
    pub evidence: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AttestRequest {
    pub dispute_id: i64,
    pub attester_npub: String,
}

#[derive(Debug, Serialize)]
pub struct ReputationResponse {
    pub npub: String,
    pub reputation: f64,
    pub total_uptime: i64,
    pub event_count: i64,
    pub open_disputes: i64,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Create reputation event and dispute tables.
pub fn init_reputation_tables(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reputation_events (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            operator_npub  TEXT NOT NULL,
            event_type     TEXT NOT NULL,
            source_npub    TEXT NOT NULL,
            weight         REAL NOT NULL,
            mirror_id      TEXT,
            details        TEXT,
            disputed       INTEGER DEFAULT 0,
            expires_at     INTEGER,
            created_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );
        CREATE INDEX IF NOT EXISTS idx_rep_events_operator ON reputation_events(operator_npub);

        CREATE TABLE IF NOT EXISTS reputation_disputes (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id       INTEGER NOT NULL,
            respondent_npub TEXT NOT NULL,
            evidence       TEXT NOT NULL,
            attestation_npubs TEXT NOT NULL DEFAULT '[]',
            attestation_weight REAL DEFAULT 0.0,
            dismissal_threshold REAL NOT NULL DEFAULT 2.0,
            status         TEXT NOT NULL DEFAULT 'open',
            resolved_at    INTEGER,
            created_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );
        ",
    );
}

// ---------------------------------------------------------------------------
// Reputation computation (sync — rusqlite is synchronous)
// ---------------------------------------------------------------------------

/// Default starting reputation for new operators.
pub const BASE_REPUTATION: f64 = 50.0;

/// Get current reputation for an operator (computed from events).
pub fn get_reputation(
    db: &rusqlite::Connection,
    npub: &str,
) -> f64 {
    let event_sum: f64 = db.query_row(
        "SELECT COALESCE(SUM(weight), 0.0) FROM reputation_events
         WHERE operator_npub = ?1 AND disputed = 0",
        params![npub],
        |row| row.get(0),
    )
    .unwrap_or(0.0);

    (BASE_REPUTATION + event_sum).clamp(0.0, 100.0)
}

/// Record a reputation event.
#[allow(clippy::too_many_arguments, dead_code)]
pub async fn record_event(
    db: &rusqlite::Connection,
    operator_npub: &str,
    event_type: ReputationEventType,
    source_npub: &str,
    weight: f64,
    mirror_id: Option<&str>,
    details: Option<&str>,
    disputable: bool,
) -> Result<i64, String> {
    let now = chrono::Utc::now().timestamp();
    let expires_at = if disputable { Some(now + 14 * 86400) } else { None };

    db.execute(
        "INSERT INTO reputation_events (operator_npub, event_type, source_npub, weight,
            mirror_id, details, disputed, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
        params![operator_npub, event_type.to_string(), source_npub, weight,
            mirror_id, details, expires_at],
    )
    .map_err(|e| format!("DB error: {}", e))?;

    let id: i64 = db.query_row(
        "SELECT last_insert_rowid()",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0);

    let reputation = get_reputation(db, operator_npub);
    let _ = db.execute(
        "UPDATE operators SET reputation = ?1 WHERE npub = ?2",
        params![reputation, operator_npub],
    );

    Ok(id)
}

// ---------------------------------------------------------------------------
// Dispute flow (sync)
// ---------------------------------------------------------------------------

/// Open a dispute against a negative reputation event.
pub fn open_dispute(
    db: &rusqlite::Connection,
    event_id: i64,
    respondent_npub: &str,
    evidence: &str,
) -> Result<i64, String> {
    let disputed: i64 = db.query_row(
        "SELECT disputed FROM reputation_events WHERE id = ?1",
        params![event_id],
        |row| row.get(0),
    )
    .map_err(|e| format!("Event not found: {}", e))?;

    if disputed != 0 {
        return Err("Event already disputed".to_string());
    }

    let now = chrono::Utc::now().timestamp();

    db.execute(
        "INSERT INTO reputation_disputes (event_id, respondent_npub, evidence, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![event_id, respondent_npub, evidence, now],
    )
    .map_err(|e| format!("DB error: {}", e))?;

    let dispute_id: i64 = db.query_row(
        "SELECT last_insert_rowid()",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0);

    db.execute(
        "UPDATE reputation_events SET disputed = 1 WHERE id = ?1",
        params![event_id],
    )
    .map_err(|e| format!("DB error: {}", e))?;

    Ok(dispute_id)
}

/// Add an attestation to a dispute. If total attestation weight exceeds
/// dismissal_threshold, dismiss the dispute and reverse the negative event.
pub fn attest_dispute(
    db: &rusqlite::Connection,
    dispute_id: i64,
    attester_npub: &str,
) -> Result<String, String> {
    let (event_id, mut npubs_json, mut weight, threshold, status): (i64, String, f64, f64, String) = db.query_row(
        "SELECT event_id, attestation_npubs, attestation_weight, dismissal_threshold, status
         FROM reputation_disputes WHERE id = ?1",
        params![dispute_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )
    .map_err(|e| format!("Dispute not found: {}", e))?;

    if status != "open" {
        return Err(format!("Dispute is {}", status));
    }

    let mut npubs: Vec<String> = serde_json::from_str(&npubs_json).unwrap_or_default();
    if npubs.contains(&attester_npub.to_string()) {
        return Err("Already attested".to_string());
    }
    npubs.push(attester_npub.to_string());
    npubs_json = serde_json::to_string(&npubs).unwrap_or_default();

    weight += 1.0;

    let now = chrono::Utc::now().timestamp();

    if weight >= threshold {
        db.execute(
            "UPDATE reputation_events SET disputed = 2 WHERE id = ?1",
            params![event_id],
        )
        .map_err(|e| format!("DB error: {}", e))?;

        let operator_npub: String = db.query_row(
            "SELECT operator_npub FROM reputation_events WHERE id = ?1",
            params![event_id],
            |row| row.get(0),
        )
        .unwrap_or_default();

        let reputation = get_reputation(db, &operator_npub);
        let _ = db.execute(
            "UPDATE operators SET reputation = ?1 WHERE npub = ?2",
            params![reputation, operator_npub],
        );

        db.execute(
            "UPDATE reputation_disputes SET attestation_npubs = ?1, attestation_weight = ?2,
             status = 'dismissed', resolved_at = ?3 WHERE id = ?4",
            params![npubs_json, weight, now, dispute_id],
        )
        .map_err(|e| format!("DB error: {}", e))?;

        Ok("dismissed".to_string())
    } else {
        db.execute(
            "UPDATE reputation_disputes SET attestation_npubs = ?1, attestation_weight = ?2
             WHERE id = ?3",
            params![npubs_json, weight, dispute_id],
        )
        .map_err(|e| format!("DB error: {}", e))?;

        Ok("attested".to_string())
    }
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// GET /reputation/:npub — get operator reputation
pub async fn handle_get_reputation(
    State(state): State<Arc<TrackerState>>,
    axum::extract::Path(npub): axum::extract::Path<String>,
) -> Response {
    let db = state.db.lock().await;

    let reputation = get_reputation(&db, &npub);

    let total_uptime: i64 = db.query_row(
        "SELECT COALESCE(total_uptime, 0) FROM operators WHERE npub = ?1",
        params![npub],
        |row| row.get(0),
    )
    .unwrap_or(0);

    let event_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM reputation_events WHERE operator_npub = ?1",
        params![npub],
        |row| row.get(0),
    )
    .unwrap_or(0);

    let open_disputes: i64 = db.query_row(
        "SELECT COUNT(*) FROM reputation_disputes d
         JOIN reputation_events e ON d.event_id = e.id
         WHERE e.operator_npub = ?1 AND d.status = 'open'",
        params![npub],
        |row| row.get(0),
    )
    .unwrap_or(0);

    Json(ReputationResponse {
        npub,
        reputation,
        total_uptime,
        event_count,
        open_disputes,
    }).into_response()
}

/// POST /reputation/dispute — open a dispute
pub async fn handle_open_dispute(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<DisputeRequest>,
) -> Response {
    let db = state.db.lock().await;

    match open_dispute(&db, req.event_id, &req.respondent_npub, &req.evidence) {
        Ok(id) => {
            info!("Dispute opened: id={} event={}", id, req.event_id);
            (StatusCode::OK, format!("{{\"dispute_id\":{}}}", id)).into_response()
        }
        Err(e) => {
            warn!("Dispute open failed: {}", e);
            (StatusCode::BAD_REQUEST, e).into_response()
        }
    }
}

/// POST /reputation/attest — attest to a dispute
pub async fn handle_attest(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<AttestRequest>,
) -> Response {
    let db = state.db.lock().await;

    match attest_dispute(&db, req.dispute_id, &req.attester_npub) {
        Ok(status) => {
            info!("Attestation: dispute={} status={}", req.dispute_id, status);
            (StatusCode::OK, format!("{{\"status\":\"{}\"}}", status)).into_response()
        }
        Err(e) => {
            warn!("Attestation failed: {}", e);
            (StatusCode::BAD_REQUEST, e).into_response()
        }
    }
}
