//! Monopsony Tracker — Decentralized mirror discovery service
//!
//! Lightweight HTTP tracker (like BitTorrent's tracker protocol).
//! Mirrors announce themselves; clients discover nearby mirrors.
//! SQLite-backed, ~600 lines.

mod auth;
mod recipes;
mod report;
mod reputation;
mod stun;
mod v4v;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use auth::{AuthState, AuthenticatedIdentity, JwtConfig, TrackerAuthMode, DEV_JWT_SECRET};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use monopsony_common::peer_blacklist::{
    canonical_json, canonicalize_mirror_id, BlacklistAction, BlacklistEntry, BlacklistSubject,
};

/// Tracker-side cap on signed entries exported via GET /blacklist/signed.
const SIGNED_EXPORT_LIMIT: i64 = 256;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Mirror announcement payload (POST /announce)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnounceRequest {
    pub mirror_id: String,
    pub distro: String,
    pub transport: Vec<String>,
    pub endpoints: Vec<String>,
    pub capacity_bytes: Option<u64>,
    pub bandwidth_bps: Option<u64>,
    pub uptime_seconds: Option<u64>,
    pub packages: Option<u64>,
    pub last_sync: Option<String>,
    /// Optional [lat, lon] for geo scoring
    pub lat_lon: Option<(f64, f64)>,
    pub stats: Option<AnnounceStats>,
    /// Operator identity: Ed25519 public key (hex). Required in token/jwt modes.
    pub operator_npub: Option<String>,
    /// Ed25519 signature over announce payload
    pub operator_signature: Option<String>,
    /// Human-readable operator name
    pub display_name: Option<String>,
    /// Payment pointer (e.g. lightning:)
    pub payment_pointer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnounceStats {
    pub bytes_served_24h: Option<u64>,
    pub requests_24h: Option<u64>,
}

/// Discovery query (GET /discover?distro=archlinux&lat=33.5&lon=-112.0)
#[derive(Debug, Deserialize)]
pub struct DiscoverQuery {
    pub distro: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Max mirrors to return (default 10)
    pub limit: Option<u32>,
    /// Filter by transport type
    pub transport: Option<String>,
}

/// Discovery response
#[derive(Debug, Serialize)]
pub struct DiscoverResponse {
    pub mirrors: Vec<DiscoveredMirror>,
    pub ttl: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredMirror {
    pub mirror_id: String,
    pub endpoint: String,
    pub transport: String,
    pub packages: Option<u64>,
    pub last_sync: Option<String>,
    pub score: f64,
    pub latency_ms: Option<u64>,
}

/// Stored mirror record (DB row)
#[derive(Debug, Clone)]
#[allow(dead_code)] // DB row type; fields used when serving detailed mirror queries
struct MirrorRecord {
    mirror_id: String,
    distro: String,
    transport: String, // JSON array stored as string
    endpoints: String, // JSON array stored as string
    capacity_bytes: Option<u64>,
    bandwidth_bps: Option<u64>,
    uptime_seconds: Option<u64>,
    packages: Option<u64>,
    last_sync: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    bytes_served_24h: Option<u64>,
    requests_24h: Option<u64>,
    announced_at: i64, // epoch seconds
    operator_npub: Option<String>,
    nat_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Score a mirror based on proximity, freshness, reliability, capacity.
/// Returns 0.0–100.0.
fn score_mirror(
    mirror: &MirrorRecord,
    client_lat: Option<f64>,
    client_lon: Option<f64>,
    now_epoch: i64,
) -> f64 {
    // Weights
    const W_PROXIMITY: f64 = 0.4;
    const W_FRESHNESS: f64 = 0.3;
    const W_RELIABILITY: f64 = 0.2;
    const W_CAPACITY: f64 = 0.1;

    // Proximity: haversine distance → score (0–100, 0km=100, >10000km=0)
    let proximity_score = match (client_lat, client_lon, mirror.lat, mirror.lon) {
        (Some(clat), Some(clon), Some(mlat), Some(mlon)) => {
            let dist_km = haversine_km(clat, clon, mlat, mlon);
            (100.0 * (1.0 - (dist_km / 10000.0).min(1.0))).max(0.0)
        }
        _ => 50.0, // No geo info → neutral score
    };

    // Freshness: time since last sync → score (0s=100, >24h=0)
    let freshness_score = mirror
        .last_sync
        .as_ref()
        .and_then(|ls| {
            ls.parse::<i64>().ok().or_else(|| {
                chrono::DateTime::parse_from_rfc3339(ls)
                    .ok()
                    .map(|dt| dt.timestamp())
            })
        })
        .map(|ls_epoch| {
            let age_secs = (now_epoch - ls_epoch).max(0);
            (100.0 * (1.0 - (age_secs as f64 / 86400.0).min(1.0))).max(0.0)
        })
        .unwrap_or(25.0); // No sync info → low-neutral

    // Reliability: uptime → score (30d=100, 0=0)
    let reliability_score = mirror
        .uptime_seconds
        .map(|up| 100.0 * (up as f64 / 2_592_000.0).min(1.0))
        .unwrap_or(50.0);

    // Capacity: bandwidth → score (100MB/s=100, 0=0)
    let capacity_score = mirror
        .bandwidth_bps
        .map(|bw| 100.0 * (bw as f64 / 104_857_600.0).min(1.0))
        .unwrap_or(50.0);

    let base = W_PROXIMITY * proximity_score
        + W_FRESHNESS * freshness_score
        + W_RELIABILITY * reliability_score
        + W_CAPACITY * capacity_score;

    // Apply NAT penalty (Symmetric NAT = harder to reach)
    stun::apply_nat_penalty(base, mirror.nat_type.as_deref())
}

/// Haversine distance between two lat/lon points in km
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0; // Earth radius in km
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    r * 2.0 * a.sqrt().asin()
}

fn row_to_mirror(row: &rusqlite::Row) -> MirrorRecord {
    MirrorRecord {
        mirror_id: row.get(0).unwrap_or_default(),
        distro: row.get(1).unwrap_or_default(),
        transport: row.get(2).unwrap_or_else(|_| "[]".to_string()),
        endpoints: row.get(3).unwrap_or_else(|_| "[]".to_string()),
        capacity_bytes: row.get(4).ok(),
        bandwidth_bps: row.get(5).ok(),
        uptime_seconds: row.get(6).ok(),
        packages: row.get(7).ok(),
        last_sync: row.get(8).ok(),
        lat: row.get(9).ok(),
        lon: row.get(10).ok(),
        bytes_served_24h: row.get(11).ok(),
        requests_24h: row.get(12).ok(),
        announced_at: row.get(13).unwrap_or(0),
        operator_npub: row.get(14).ok(),
        nat_type: row.get(15).ok(),
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TrackerState {
    pub db: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    pub auth: Arc<AuthState>,
    /// Open-mode gate for the LEGACY unsigned POST /blacklist path (red-team
    /// A2). Open mode's auth middleware passes everyone, so the legacy insert
    /// would be unauthenticated permanent /discover censorship (expires_at
    /// NULL, no retraction short of an operator DELETE). Default false = the
    /// legacy path 403s in open mode; signed entries are unaffected (they
    /// carry their own pinned-originator + signature authorization). Set via
    /// `--allow-open-blacklist`.
    pub allow_open_blacklist: bool,
}

// ---------------------------------------------------------------------------
// DB schema
// ---------------------------------------------------------------------------

/// Create all tables for a fresh database.
fn init_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mirrors (
            mirror_id       TEXT NOT NULL,
            distro          TEXT NOT NULL,
            transport       TEXT NOT NULL DEFAULT '[]',
            endpoints       TEXT NOT NULL DEFAULT '[]',
            capacity_bytes  INTEGER,
            bandwidth_bps   INTEGER,
            uptime_seconds  INTEGER,
            packages        INTEGER,
            last_sync       TEXT,
            lat             REAL,
            lon             REAL,
            bytes_served_24h INTEGER,
            requests_24h    INTEGER,
            announced_at    INTEGER NOT NULL,
            operator_npub   TEXT,
            owner_npub      TEXT,
            PRIMARY KEY (mirror_id, distro)
        );
        CREATE INDEX IF NOT EXISTS idx_mirrors_distro ON mirrors(distro);
        CREATE INDEX IF NOT EXISTS idx_mirrors_announced ON mirrors(announced_at);

        CREATE TABLE IF NOT EXISTS operators (
            npub            TEXT PRIMARY KEY,
            first_seen      INTEGER NOT NULL,
            display_name    TEXT,
            payment_pointer TEXT,
            reputation      REAL NOT NULL DEFAULT 50.0,
            total_uptime    INTEGER DEFAULT 0,
            created_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );

        CREATE TABLE IF NOT EXISTS blacklist (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            mirror_id   TEXT,
            operator_npub TEXT,
            reason      TEXT NOT NULL,
            source      TEXT NOT NULL DEFAULT 'manual',
            auto_threshold INTEGER,
            created_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            expires_at  INTEGER,
            subject     TEXT,
            action      TEXT,
            originator  TEXT,
            seq         INTEGER,
            signature   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_blacklist_mirror ON blacklist(mirror_id);
        CREATE INDEX IF NOT EXISTS idx_blacklist_operator ON blacklist(operator_npub);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_blacklist_entry
            ON blacklist(originator, subject, seq) WHERE originator IS NOT NULL;
        ",
    )?;
    Ok(())
}

/// Apply migrations for existing databases that may lack newer columns.
fn migrate_schema(conn: &rusqlite::Connection) {
    // Add operator_npub column if missing (SQLite has no IF NOT EXISTS for ALTER)
    let _ = conn.execute_batch("ALTER TABLE mirrors ADD COLUMN operator_npub TEXT");
    // Mirror-id ownership binding (red-team A3/A4 fix #1): the operator that
    // first PROVABLY claimed a mirror_id. NULL on legacy rows — deliberately
    // not backfilled from operator_npub (that column was attacker-writable
    // pre-fix; backfill would launder a hijack into permanent ownership).
    // Binds lazily on the attributed operator's first signed re-announce.
    let _ = conn.execute_batch("ALTER TABLE mirrors ADD COLUMN owner_npub TEXT");
    stun::migrate_nat_columns(conn);
    // Signed blacklist entry columns (2026-10-31 peers-gossip propagation).
    // SQLite UNIQUE treats NULLs as distinct, so legacy unsigned rows never
    // collide in idx_blacklist_entry; the index is partial anyway.
    let _ = conn.execute_batch("ALTER TABLE blacklist ADD COLUMN subject TEXT");
    let _ = conn.execute_batch("ALTER TABLE blacklist ADD COLUMN action TEXT");
    let _ = conn.execute_batch("ALTER TABLE blacklist ADD COLUMN originator TEXT");
    let _ = conn.execute_batch("ALTER TABLE blacklist ADD COLUMN seq INTEGER");
    let _ = conn.execute_batch("ALTER TABLE blacklist ADD COLUMN signature TEXT");
    let _ = conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_blacklist_entry
         ON blacklist(originator, subject, seq) WHERE originator IS NOT NULL",
    );
}

/// Ensure the operators table exists (idempotent).
#[allow(dead_code)]
fn ensure_operators_table(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS operators (
            npub            TEXT PRIMARY KEY,
            first_seen      INTEGER NOT NULL,
            display_name    TEXT,
            payment_pointer TEXT,
            reputation      REAL NOT NULL DEFAULT 50.0,
            total_uptime    INTEGER DEFAULT 0,
            created_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );",
    );
}

// ---------------------------------------------------------------------------
// Blacklist helpers (sync — rusqlite is synchronous)
// ---------------------------------------------------------------------------

/// Check if a mirror_id or its operator is blacklisted.
///
/// Retract-aware: a signed entry is suppressed when a HIGHER-seq entry for
/// the same (originator, subject) exists with action 'retract'. Legacy
/// unsigned rows (originator NULL) can never be suppressed — the NOT EXISTS
/// predicate is vacuously true for them, preserving today's behavior.
pub fn is_blacklisted(
    db: &rusqlite::Connection,
    mirror_id: &str,
    operator_npub: Option<&str>,
) -> bool {
    let now = chrono::Utc::now().timestamp();
    let mirror_blocked: bool = db
        .query_row(
            "SELECT COUNT(*) FROM blacklist b
         WHERE b.mirror_id = ?1
           AND (b.expires_at IS NULL OR b.expires_at > ?2)
           AND b.action IS NOT 'retract'
           AND NOT EXISTS (
               SELECT 1 FROM blacklist r
               WHERE r.originator = b.originator AND r.subject = b.subject
                 AND r.action = 'retract' AND r.seq > b.seq
           )",
            params![mirror_id, now],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if mirror_blocked {
        return true;
    }

    if let Some(npub) = operator_npub {
        let op_blocked: bool = db
            .query_row(
                "SELECT COUNT(*) FROM blacklist b
             WHERE b.operator_npub = ?1 AND b.mirror_id IS NULL
               AND (b.expires_at IS NULL OR b.expires_at > ?2)
               AND b.action IS NOT 'retract'
               AND NOT EXISTS (
                   SELECT 1 FROM blacklist r
                   WHERE r.originator = b.originator AND r.subject = b.subject
                     AND r.action = 'retract' AND r.seq > b.seq
               )",
                params![npub, now],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        return op_blocked;
    }

    false
}

/// Auto-blacklist a mirror if hash_mismatch reports exceed threshold.
pub fn check_auto_blacklist(
    db: &rusqlite::Connection,
    mirror_id: &str,
    threshold: i64,
    window_hours: i64,
) -> bool {
    let cutoff = chrono::Utc::now().timestamp() - (window_hours * 3600);
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM reports WHERE mirror_id = ?1 AND report_type = 'hash_mismatch' AND created_at > ?2",
        params![mirror_id, cutoff],
        |row| row.get(0),
    )
    .unwrap_or(0);

    if count >= threshold {
        let _ = db.execute(
            "INSERT INTO blacklist (mirror_id, reason, source, auto_threshold) VALUES (?1, ?2, 'auto', ?3)",
            params![mirror_id, format!("auto-blacklisted: {} hash_mismatch reports in {}h", count, window_hours), threshold],
        );
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Blacklist HTTP handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct BlacklistRequest {
    mirror_id: Option<String>,
    operator_npub: Option<String>,
    /// Legacy-path reason. Defaulted so signed-entry bodies (which carry the
    /// reason inside the entry) don't need a duplicate field.
    #[serde(default)]
    reason: String,
    expires_at: Option<i64>,
    /// Signed blacklist entry (peers-gossip propagation). When present, the
    /// legacy fields are ignored: the entry is signature-verified against its
    /// own originator, seq-checked for replay, and stored for export on
    /// GET /blacklist/signed.
    #[serde(default)]
    entry: Option<BlacklistEntry>,
}

#[derive(Debug, Serialize)]
struct BlacklistRow {
    id: i64,
    mirror_id: Option<String>,
    operator_npub: Option<String>,
    reason: String,
    source: String,
    auto_threshold: Option<i64>,
    created_at: i64,
    expires_at: Option<i64>,
}

/// POST /blacklist — manually add to blacklist
async fn handle_blacklist_add(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<BlacklistRequest>,
) -> impl IntoResponse {
    if let Some(entry) = &req.entry {
        return handle_signed_blacklist_add(&state, entry).await;
    }
    // A2 gate (red-team §9 fix #2): open mode's auth middleware passes
    // everyone, so this legacy unsigned insert would be unauthenticated
    // PERMANENT /discover censorship (expires_at NULL, no retraction short
    // of an operator DELETE). Fail closed unless the operator explicitly
    // opted in via --allow-open-blacklist. Signed entries were dispatched
    // above and carry their own pinned-originator + signature authorization.
    if state.auth.auth_mode == TrackerAuthMode::Open && !state.allow_open_blacklist {
        warn!(
            "Rejected legacy unsigned blacklist add (mirror_id={:?}, operator={:?}) \
             in open mode — pass --allow-open-blacklist to permit it",
            req.mirror_id, req.operator_npub
        );
        return (
            StatusCode::FORBIDDEN,
            "legacy blacklist adds are disabled in open mode (unauthenticated \
             permanent censorship risk); use signed entries or pass \
             --allow-open-blacklist",
        )
            .into_response();
    }
    if req.mirror_id.is_none() && req.operator_npub.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "must provide mirror_id or operator_npub",
        )
            .into_response();
    }
    // A6: legacy mirror_id bans land canonical too — a ban stored under a
    // mutated id (case/whitespace) would never match the canonical rows
    // announces are stored under, which is the exact evasion A6 closes.
    // Ids outside the printable-ASCII allowlist are rejected, not rewritten.
    let mirror_id = match req.mirror_id.as_deref() {
        Some(raw) => match canonicalize_mirror_id(raw) {
            Ok(id) => Some(id),
            Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
        },
        None => None,
    };
    // Legacy strictness preserved: `reason` was a required JSON field before
    // the signed path existed; the serde default exists only for that path.
    if req.reason.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "reason required").into_response();
    }
    let db = state.db.lock().await;
    let now = chrono::Utc::now().timestamp();
    match db.execute(
        "INSERT INTO blacklist (mirror_id, operator_npub, reason, source, created_at, expires_at)
         VALUES (?1, ?2, ?3, 'manual', ?4, ?5)",
        params![
            mirror_id,
            req.operator_npub,
            req.reason,
            now,
            req.expires_at
        ],
    ) {
        Ok(_) => (StatusCode::OK, "blacklisted").into_response(),
        Err(e) => {
            warn!("Blacklist add DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// Signed-entry POST path: trust-FIRST — the entry's originator must be in
/// the tracker's pinned operator set (auth middleware authenticates the
/// transport; the pin set is the AUTHORIZATION for minting bans into the
/// tracker DB — without it any open-auth peer could flood the DB/export with
/// self-signed entries), then the Ed25519 signature is verified against the
/// entry's own originator key (proves the entry was not tampered and is owned
/// by the claimed originator), then per-(originator, subject) seq
/// monotonicity — a replayed or stale entry is rejected with 409.
async fn handle_signed_blacklist_add(
    state: &Arc<TrackerState>,
    entry: &BlacklistEntry,
) -> axum::response::Response {
    let pinned = &state.auth.pinned_originators;
    let trusted_originator = !pinned.is_empty()
        && pinned
            .iter()
            .any(|k| k.eq_ignore_ascii_case(&entry.originator));
    if !trusted_originator {
        warn!(
            "Rejected signed blacklist entry from unpinned originator {} (subject={}; \
             {} pins configured)",
            entry.originator,
            entry.subject,
            pinned.len()
        );
        return (
            StatusCode::FORBIDDEN,
            "originator not in tracker pinned operator set \
             (configure --pinned-originators)",
        )
            .into_response();
    }

    if let Err(e) = entry.verify_signature() {
        warn!("Rejected signed blacklist entry: {}", e);
        return (StatusCode::BAD_REQUEST, format!("invalid entry: {}", e)).into_response();
    }

    let db = state.db.lock().await;
    let max_seq: Option<i64> = db
        .query_row(
            "SELECT MAX(seq) FROM blacklist WHERE originator = ?1 AND subject = ?2",
            params![entry.originator, entry.subject],
            |row| row.get(0),
        )
        .unwrap_or(None);
    if let Some(max) = max_seq {
        if entry.seq as i64 <= max {
            warn!(
                "Rejected stale signed blacklist entry (seq {} <= {}): originator={}, subject={}",
                entry.seq, max, entry.originator, entry.subject
            );
            return (
                StatusCode::CONFLICT,
                "stale seq: a higher or equal seq already exists for this (originator, subject)",
            )
                .into_response();
        }
    }

    // Mirror the subject into the legacy columns so is_blacklisted's
    // mirror_id/operator_npub queries keep working unchanged.
    let (mirror_id, operator_npub) = match BlacklistSubject::parse(&entry.subject) {
        Some(BlacklistSubject::Mirror(id)) => (Some(id), None),
        Some(BlacklistSubject::Operator(npub)) => (None, Some(npub)),
        None => (None, None),
    };
    let now = chrono::Utc::now().timestamp();
    match db.execute(
        "INSERT INTO blacklist (mirror_id, operator_npub, reason, source, created_at, expires_at,
                                subject, action, originator, seq, signature)
         VALUES (?1, ?2, ?3, 'manual', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            mirror_id,
            operator_npub,
            entry.reason,
            now,
            entry.expires_at,
            entry.subject,
            entry.action.as_str(),
            entry.originator,
            entry.seq as i64,
            entry.signature,
        ],
    ) {
        Ok(_) => {
            info!(
                "Signed blacklist entry stored: subject={} seq={} originator={}",
                entry.subject, entry.seq, entry.originator
            );
            (StatusCode::OK, "blacklisted").into_response()
        }
        Err(e) => {
            warn!("Signed blacklist add DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// GET /blacklist/signed — export signed entries for mirror/client ingestion
/// (peers-gossip propagation source of truth). Legacy unsigned rows —
/// including auto-blacklist rows — are NEVER exported: only signature-verified
/// entries may propagate.
async fn handle_blacklist_signed(State(state): State<Arc<TrackerState>>) -> impl IntoResponse {
    let db = state.db.lock().await;
    let mut stmt = match db.prepare(
        "SELECT subject, action, reason, originator, seq, created_at, expires_at, signature
         FROM blacklist
         WHERE originator IS NOT NULL AND subject IS NOT NULL
         ORDER BY seq DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!("Blacklist signed export prepare: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    // Defense-in-depth (red-team §9 fix #5): re-verify every row's signature
    // against its claimed originator at export so out-of-band SQLite
    // tampering cannot launder a forged row into the propagation stream.
    // Failing rows are skipped and logged (log-only, no metric: the tracker
    // carries no metrics exporter; the warn names the tampered row).
    let rows = stmt
        .query_map(params![SIGNED_EXPORT_LIMIT], |row| {
            let action: String = row.get(1)?;
            Ok(BlacklistEntry {
                subject: row.get(0)?,
                action: if action == "retract" {
                    BlacklistAction::Retract
                } else {
                    BlacklistAction::Ban
                },
                reason: row.get(2)?,
                originator: row.get(3)?,
                seq: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                signature: row.get(7).unwrap_or_default(),
            })
        })
        .map(|r| {
            r.filter_map(|r| r.ok())
                .filter_map(|entry| match entry.verify_signature() {
                    Ok(()) => Some(entry),
                    Err(e) => {
                        warn!(
                            "Signed export re-verify FAILED — skipping row: subject={} \
                             originator={} seq={} err={}",
                            entry.subject, entry.originator, entry.seq, e
                        );
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Json(rows).into_response()
}

/// DELETE /blacklist/:id — remove from blacklist
async fn handle_blacklist_remove(
    State(state): State<Arc<TrackerState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match db.execute("DELETE FROM blacklist WHERE id = ?1", params![id]) {
        Ok(rows) => {
            if rows > 0 {
                (StatusCode::OK, "removed").into_response()
            } else {
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        }
        Err(e) => {
            warn!("Blacklist remove DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// GET /blacklist — list blacklist entries
async fn handle_blacklist_list(State(state): State<Arc<TrackerState>>) -> impl IntoResponse {
    let db = state.db.lock().await;
    let mut stmt = match db.prepare(
        "SELECT id, mirror_id, operator_npub, reason, source, auto_threshold, created_at, expires_at
         FROM blacklist ORDER BY created_at DESC LIMIT 100"
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!("Blacklist list DB prepare: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(BlacklistRow {
                id: row.get(0)?,
                mirror_id: row.get(1).ok(),
                operator_npub: row.get(2).ok(),
                reason: row.get(3)?,
                source: row.get(4)?,
                auto_threshold: row.get(5).ok(),
                created_at: row.get(6)?,
                expires_at: row.get(7).ok(),
            })
        })
        .map(|r| r.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default();

    Json(rows).into_response()
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// Verify an announce payload's operator signature (design prerequisite 2,
/// strictness per open question 7: require+verify in token/jwt mode,
/// verify-when-present in open mode). Signed bytes = canonical JSON of the
/// request with `operator_signature` removed, verified against the
/// `operator_npub` key.
fn verify_announce_signature(
    req: &AnnounceRequest,
    mode: &TrackerAuthMode,
    jwt_npub: Option<&str>,
) -> Result<(), String> {
    let npub = req.operator_npub.as_deref().unwrap_or("");
    let sig = req.operator_signature.as_deref().unwrap_or("");

    match mode {
        TrackerAuthMode::Open => {
            // Open mode: verify when present. A lone npub (no signature)
            // keeps today's behavior — the operator row is best-effort.
            if sig.is_empty() {
                return Ok(());
            }
            if npub.is_empty() {
                return Err(
                    "operator_npub is required when operator_signature is present".to_string(),
                );
            }
            verify_announce_sig_bytes(req, npub, sig)
        }
        TrackerAuthMode::Token | TrackerAuthMode::Jwt => {
            // Authenticated modes: the announce MUST be signed by the
            // claimed operator — accepted-but-unsigned identity claims are
            // how mirror-id spoofing stayed trivial.
            if npub.is_empty() || sig.is_empty() {
                return Err(format!(
                    "operator_npub and operator_signature are required in {} mode",
                    mode
                ));
            }
            if let Some(sub) = jwt_npub {
                if !npub.eq_ignore_ascii_case(sub) {
                    return Err(
                        "operator_npub does not match the authenticated JWT subject".to_string()
                    );
                }
            }
            verify_announce_sig_bytes(req, npub, sig)
        }
    }
}

/// Check the Ed25519 signature over the canonical announce payload.
fn verify_announce_sig_bytes(
    req: &AnnounceRequest,
    npub: &str,
    sig_hex: &str,
) -> Result<(), String> {
    let key_bytes = hex::decode(npub)
        .ok()
        .filter(|b| b.len() == 32)
        .ok_or_else(|| "operator_npub must be 64-char hex Ed25519 key".to_string())?;
    let verifying_key = VerifyingKey::from_bytes(
        key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| "operator_npub must be 64-char hex Ed25519 key".to_string())?,
    )
    .map_err(|_| "operator_npub is not a valid Ed25519 key".to_string())?;
    let sig_bytes = hex::decode(sig_hex)
        .ok()
        .filter(|b| b.len() == 64)
        .ok_or_else(|| "operator_signature must be 128-char hex".to_string())?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|_| "operator_signature malformed".to_string())?;

    let mut value = serde_json::to_value(req).map_err(|e| e.to_string())?;
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("operator_signature");
    }
    verifying_key
        .verify(canonical_json(&value).as_bytes(), &signature)
        .map_err(|_| "operator signature invalid".to_string())
}

/// Ownership decision for an announce against an existing mirror row
/// (red-team A3/A4 fix #1 — bind a mirror_id to the operator that first
/// claimed it; the pre-fix UPSERT let any announce-capable peer overwrite
/// an honest mirror's endpoints AND operator attribution).
///
/// Rule table (full rationale: context/kanban-progress-f1-ownership-binding.md):
/// - **Owner-bound row** (`row_owner` set): only a signature-verified announce
///   by that exact key may update it. An unsigned claim of the owner npub is
///   the A3 spoof; a different npub — even validly signed by its own key — is
///   the A4 hijack.
/// - **Unowned, attributed row** (legacy rows, `row_npub` set): an announce
///   claiming a different npub is rejected; the same npub passes (open-mode
///   best-effort re-announces keep working) and binds ownership once
///   signature-verified (lazy migration of pre-fix rows).
/// - **Unattributed row**: first claim wins — open mode's documented
///   demo-mode residual. Ownership binds only on a verified signature; an
///   unsigned npub never becomes an owner.
///
/// Note on the spec's "unless signed by the original operator key" clause:
/// announce signatures verify against the announce's own `operator_npub`
/// (see [`verify_announce_sig_bytes`]), so a different-npub announce signed
/// by the original key cannot verify — that escape hatch is structurally
/// the npub-match path. There is no key-rotation/transfer path in v1.
///
/// Returns `Ok(Some(npub))` when this announce binds ownership, `Ok(None)`
/// when the write is allowed with no ownership change, `Err(reason)` when
/// the announce must be rejected (HTTP 409).
fn announce_ownership_check(
    row_owner: Option<&str>,
    row_npub: Option<&str>,
    incoming_npub: Option<&str>,
    sig_verified: bool,
) -> Result<Option<String>, String> {
    match row_owner {
        Some(owner) => match incoming_npub {
            Some(npub) if npub.eq_ignore_ascii_case(owner) => {
                if sig_verified {
                    Ok(None)
                } else {
                    Err(format!(
                        "mirror_id is owner-bound to {owner}; unsigned re-announce rejected"
                    ))
                }
            }
            _ => Err(format!(
                "mirror_id is owned by operator {owner}; announce from a different operator rejected"
            )),
        },
        None => match row_npub {
            Some(attributed) => match incoming_npub {
                Some(npub) if npub.eq_ignore_ascii_case(attributed) => {
                    if sig_verified {
                        Ok(Some(npub.to_string()))
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(format!(
                    "mirror_id is already claimed by operator {attributed}; \
                     announce from a different operator rejected"
                )),
            },
            None => {
                if sig_verified {
                    Ok(incoming_npub.map(|npub| npub.to_string()))
                } else {
                    Ok(None)
                }
            }
        },
    }
}

/// POST /announce — mirror registers itself
async fn handle_announce(
    State(state): State<Arc<TrackerState>>,
    operator: Option<axum::extract::Extension<auth::OperatorNpub>>,
    Json(req): Json<AnnounceRequest>,
) -> impl IntoResponse {
    // A6 canonicalization gate (§9 fix #4): every stored mirror_id lands
    // canonical (trim + ASCII-lowercase) or the announce is rejected — never
    // rewritten silently, so the announcer sees the error. Case/edge-space
    // mutations collapse onto the canonical identity that bans and F1
    // ownership binding key on; anything outside the printable-ASCII
    // allowlist (interior whitespace, control chars, unicode lookalikes) is
    // rejected. The announce SIGNATURE still verifies over the raw payload
    // (operator authentication); canonicalization applies to the stored id.
    let mirror_id = match canonicalize_mirror_id(&req.mirror_id) {
        Ok(id) => id,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    if req.distro.is_empty() {
        return (StatusCode::BAD_REQUEST, "distro required").into_response();
    }
    if req.endpoints.is_empty() {
        return (StatusCode::BAD_REQUEST, "at least one endpoint required").into_response();
    }

    if let Err(e) = verify_announce_signature(
        &req,
        &state.auth.auth_mode,
        operator.as_ref().map(|o| o.0 .0.as_str()),
    ) {
        warn!("Announce rejected: {}", e);
        return (StatusCode::UNAUTHORIZED, e).into_response();
    }

    let db = state.db.lock().await;
    let transport_json = serde_json::to_string(&req.transport).unwrap_or_default();
    let endpoints_json = serde_json::to_string(&req.endpoints).unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    let (lat, lon) = req.lat_lon.unwrap_or((0.0, 0.0));
    let (bytes_24h, reqs_24h) = req
        .stats
        .as_ref()
        .map(|s| (s.bytes_served_24h, s.requests_24h))
        .unwrap_or((None, None));
    let operator_npub = req.operator_npub.as_deref().filter(|s| !s.is_empty());
    // True iff the announce carried a signature that passed the gate above
    // (mandatory in token/jwt modes, verify-when-present in open mode) —
    // i.e. the claimed npub is cryptographically proven.
    let sig_verified = req
        .operator_signature
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    // Mirror-id ownership binding (red-team A3/A4, fix #1): the UPSERT
    // below must never let a second operator take over a mirror_id another
    // operator already claimed. Rule table: announce_ownership_check.
    let existing_row: Option<(Option<String>, Option<String>)> = db
        .query_row(
            "SELECT operator_npub, owner_npub FROM mirrors
             WHERE mirror_id = ?1 AND distro = ?2",
            params![mirror_id, req.distro],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let owner_to_bind: Option<String> = match existing_row.as_ref() {
        Some((row_npub, row_owner)) => {
            match announce_ownership_check(
                row_owner.as_deref().filter(|s| !s.is_empty()),
                row_npub.as_deref().filter(|s| !s.is_empty()),
                operator_npub,
                sig_verified,
            ) {
                Ok(bind) => bind,
                Err(reason) => {
                    warn!(
                        "Announce rejected (ownership): mirror_id={} distro={}: {}",
                        mirror_id, req.distro, reason
                    );
                    return (StatusCode::CONFLICT, reason).into_response();
                }
            }
        }
        // Fresh row: ownership binds only on a verified signature.
        None => {
            if sig_verified {
                operator_npub.map(|s| s.to_string())
            } else {
                None
            }
        }
    };

    // Auto-create operator row if npub provided
    if let Some(npub) = operator_npub {
        if !npub.is_empty() {
            let display_name = req.display_name.as_deref().unwrap_or("");
            let payment_pointer = req.payment_pointer.as_deref().unwrap_or("");
            let _ = db.execute(
                "INSERT INTO operators (npub, first_seen, display_name, payment_pointer)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(npub) DO UPDATE SET
                    display_name = CASE WHEN ?3 != '' THEN ?3 ELSE operators.display_name END,
                    payment_pointer = CASE WHEN ?4 != '' THEN ?4 ELSE operators.payment_pointer END",
                params![npub, now, display_name, payment_pointer],
            );
        }
    }

    // Ownership is written on the INSERT path only; the conflict path
    // preserves the stored owner (binding for an existing row happens in
    // the Ok arm below, after the row update lands).
    let result = db.execute(
        "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, capacity_bytes,
            bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
            bytes_served_24h, requests_24h, announced_at, operator_npub, owner_npub)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(mirror_id, distro) DO UPDATE SET
            transport = excluded.transport,
            endpoints = excluded.endpoints,
            capacity_bytes = excluded.capacity_bytes,
            bandwidth_bps = excluded.bandwidth_bps,
            uptime_seconds = excluded.uptime_seconds,
            packages = excluded.packages,
            last_sync = excluded.last_sync,
            lat = excluded.lat,
            lon = excluded.lon,
            bytes_served_24h = excluded.bytes_served_24h,
            requests_24h = excluded.requests_24h,
            announced_at = excluded.announced_at,
            operator_npub = excluded.operator_npub",
        params![
            mirror_id,
            req.distro,
            transport_json,
            endpoints_json,
            req.capacity_bytes,
            req.bandwidth_bps,
            req.uptime_seconds,
            req.packages,
            req.last_sync,
            lat,
            lon,
            bytes_24h,
            reqs_24h,
            now,
            operator_npub,
            owner_to_bind
        ],
    );

    match result {
        Ok(_) => {
            // First proven claim on an existing unowned row: bind ownership
            // now (the UPSERT above deliberately kept the stored owner).
            if let (Some(owner), true) = (&owner_to_bind, existing_row.is_some()) {
                let _ = db.execute(
                    "UPDATE mirrors SET owner_npub = ?1
                     WHERE mirror_id = ?2 AND distro = ?3",
                    params![owner, mirror_id, req.distro],
                );
                info!(
                    "Mirror ownership bound: {} ({}) -> {}",
                    mirror_id, req.distro, owner
                );
            }
            info!(
                "Announce: {} (distro={}, operator={})",
                mirror_id,
                req.distro,
                operator_npub.unwrap_or("none")
            );
            (StatusCode::OK, "announced").into_response()
        }
        Err(e) => {
            error!("Announce DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// GET /discover — client finds mirrors
async fn handle_discover(
    State(state): State<Arc<TrackerState>>,
    Query(query): Query<DiscoverQuery>,
) -> impl IntoResponse {
    let limit = query.limit.unwrap_or(10).min(50) as usize;
    let transport_filter = query.transport.as_deref();

    let db = state.db.lock().await;

    let rows: Vec<MirrorRecord> = if let Some(tf) = transport_filter {
        let mut stmt = match db.prepare(
            "SELECT mirror_id, distro, transport, endpoints, capacity_bytes,
                    bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
                    bytes_served_24h, requests_24h, announced_at, operator_npub, nat_type
             FROM mirrors WHERE distro = ?1 AND transport LIKE ?2
             ORDER BY announced_at DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("Discover DB prepare: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        };
        stmt.query_map(params![query.distro, format!("%\"{}\"%", tf)], |row| {
            Ok(row_to_mirror(row))
        })
        .map(|r| r.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    } else {
        let mut stmt = match db.prepare(
            "SELECT mirror_id, distro, transport, endpoints, capacity_bytes,
                    bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
                    bytes_served_24h, requests_24h, announced_at, operator_npub, nat_type
             FROM mirrors WHERE distro = ?1
             ORDER BY announced_at DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("Discover DB prepare: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        };
        stmt.query_map(params![query.distro], |row| Ok(row_to_mirror(row)))
            .map(|r| r.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    // Filter out blacklisted mirrors
    let rows: Vec<MirrorRecord> = rows
        .into_iter()
        .filter(|m| !is_blacklisted(&db, &m.mirror_id, m.operator_npub.as_deref()))
        .collect();

    drop(db);

    let now_epoch = chrono::Utc::now().timestamp();

    // Score and rank mirrors, expand endpoints into individual results
    let mut scored: Vec<DiscoveredMirror> = rows
        .iter()
        .flat_map(|m| {
            let score = score_mirror(m, query.lat, query.lon, now_epoch);
            let endpoints: Vec<String> = serde_json::from_str(&m.endpoints).unwrap_or_default();
            let transports: Vec<String> = serde_json::from_str(&m.transport).unwrap_or_default();

            endpoints
                .into_iter()
                .enumerate()
                .map(move |(i, ep)| DiscoveredMirror {
                    mirror_id: m.mirror_id.clone(),
                    endpoint: ep,
                    transport: transports
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| "http".to_string()),
                    packages: m.packages,
                    last_sync: m.last_sync.clone(),
                    score,
                    latency_ms: None,
                })
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    Json(DiscoverResponse {
        mirrors: scored,
        ttl: 3600,
    })
    .into_response()
}

/// GET /health — tracker health check
async fn handle_health() -> &'static str {
    "ok"
}

/// POST /report wrapper — delegates to the report handler, then feeds
/// auto-blacklist ONLY for authenticated reporters. In open mode the
/// middleware inserts no OperatorNpub extension, so a forged hash_mismatch
/// flood can never trip the ≥3/24h permanent ban (design honesty: unauthenticated
/// reports must not be able to censor tracker-local /discover). Accepted
/// reports still land in the table in open mode; only the automatic
/// enforcement is suppressed.
async fn handle_report_with_auto_blacklist(
    State(state): State<Arc<TrackerState>>,
    reporter: Option<Extension<AuthenticatedIdentity>>,
    Json(req): Json<report::ReportRequest>,
) -> impl IntoResponse {
    let is_hash_mismatch = matches!(req.report_type, report::ReportType::HashMismatch);
    let mirror_id = req.mirror_id.clone();
    let authenticated = reporter.is_some();

    let resp = report::handle_report(State(state.clone()), Json(req)).await;

    // If the report was accepted, was a hash_mismatch, and the reporter is
    // authenticated, check auto-blacklist.
    if is_hash_mismatch {
        if authenticated {
            let db = state.db.lock().await;
            check_auto_blacklist(&db, &mirror_id, 3, 24);
        } else {
            warn!(
                "Suppressed auto-blacklist check for mirror={} — hash_mismatch report \
                 arrived without an authenticated reporter (open mode); configure token/jwt \
                 auth to enable automatic enforcement",
                mirror_id
            );
        }
    }

    resp
}

// ---------------------------------------------------------------------------
// CLI + main
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "monopsony-tracker",
    about = "Monopsony mirror discovery tracker"
)]
struct Cli {
    /// Address to bind (default 0.0.0.0:3130)
    #[arg(long, default_value = "0.0.0.0:3130")]
    addr: SocketAddr,
    /// SQLite database path (default: tracker.db in current directory)
    #[arg(long, default_value = "tracker.db")]
    db: PathBuf,
    /// Purge mirrors older than this many seconds (default: 7200 = 2h)
    #[arg(long, default_value = "7200")]
    stale_timeout: i64,
    /// Enable V4V platform mode (bounty/governance tables)
    #[arg(long, default_value = "false")]
    v4v_enabled: bool,
    /// Auth mode: open (default), token, or jwt
    #[arg(long, default_value = "open")]
    auth_mode: String,
    /// Valid API keys (comma-separated, for token mode)
    #[arg(long)]
    api_keys: Option<String>,
    /// Trusted trust broker issuer npubs (comma-separated, for jwt mode)
    #[arg(long)]
    jwt_issuers: Option<String>,
    /// HS256 secret verifying JWTs (jwt mode). Falls back to
    /// MONOPSONY_TRACKER_JWT_SECRET; when neither is set, jwt mode runs on
    /// the PUBLIC dev secret with a startup warning — never a production
    /// authority.
    #[arg(long)]
    jwt_secret: Option<String>,
    /// Operator Ed25519 public keys (comma-separated 64-char hex) allowed to
    /// originate signed blacklist entries. Falls back to
    /// MONOPSONY_TRACKER_PINNED_ORIGINATORS. Empty/absent = signed blacklist
    /// adds are REJECTED (fail closed).
    #[arg(long)]
    pinned_originators: Option<String>,
    /// OPT-IN (open mode only): permit the legacy unsigned POST /blacklist
    /// path despite open auth. Without it, open mode rejects legacy adds with
    /// 403 — they are unauthenticated permanent /discover censorship
    /// (red-team A2). Signed blacklist entries are never gated by this flag.
    #[arg(long, default_value = "false")]
    allow_open_blacklist: bool,
}

/// Open-mode startup warning (red-team §9 fix #6): open auth means NO
/// authentication on announces or legacy blacklist writes — any peer can
/// hijack a mirror_id's announce row and censor discovery. One loud warn at
/// startup; pure function so the message itself is testable. Returns None in
/// authenticated modes (token/jwt), where both surfaces are auth-gated.
fn open_mode_startup_warning(auth_mode: &TrackerAuthMode) -> Option<String> {
    if *auth_mode != TrackerAuthMode::Open {
        return None;
    }
    Some(
        "auth_mode=open: DEMO MODE — announces and legacy blacklist writes are \
         UNAUTHENTICATED. Any peer can hijack a mirror_id's announce row \
         (redirect discovery to attacker endpoints) and add permanent \
         /discover censorship entries. Use --auth-mode token (or jwt) with \
         --api-keys for anything beyond local testing."
            .to_string(),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("monopsony_tracker=info".parse()?),
        )
        .init();

    // Parse auth mode
    let auth_mode = TrackerAuthMode::from_str(&cli.auth_mode).map_err(|e| anyhow::anyhow!(e))?;

    // Red-team §9 fix #6: one loud startup warning in open mode.
    if let Some(warning) = open_mode_startup_warning(&auth_mode) {
        warn!("{}", warning);
    }

    let valid_keys = cli
        .api_keys
        .as_deref()
        .map(|s| s.split(',').map(|k| k.trim().to_string()).collect())
        .unwrap_or_default();

    let jwt_config = if auth_mode == TrackerAuthMode::Jwt {
        let issuers = cli
            .jwt_issuers
            .as_deref()
            .map(|s| s.split(',').map(|i| i.trim().to_string()).collect())
            .unwrap_or_default();
        // Configurable HS256 secret (design caveat carried since 2026-10-31:
        // the hardcoded dev secret made jwt mode forgeable). CLI flag first,
        // then env, then the public dev secret with a loud warning.
        let jwt_secret = cli
            .jwt_secret
            .or_else(|| std::env::var("MONOPSONY_TRACKER_JWT_SECRET").ok());
        match &jwt_secret {
            Some(_) => info!(
                "JWT mode: HS256 secret configured (--jwt-secret or MONOPSONY_TRACKER_JWT_SECRET)"
            ),
            None => warn!(
                "JWT mode: NO secret configured — falling back to the PUBLIC dev secret \
                 ('{}'). Tokens are forgeable by anyone with the source; JWT mode is NOT \
                 a production authority until --jwt-secret or MONOPSONY_TRACKER_JWT_SECRET \
                 is set.",
                DEV_JWT_SECRET
            ),
        }
        Some(JwtConfig {
            trusted_issuers: issuers,
            min_ttl_secs: 60,
            jwt_secret: jwt_secret.unwrap_or_else(|| DEV_JWT_SECRET.to_string()),
        })
    } else {
        None
    };

    // Signed blacklist add policy: only pinned originators may mint entries
    // into the tracker DB. Empty set = fail closed (open-auth peers can no
    // longer pollute the tracker DB/export with self-signed bans).
    let mut pinned_originators = cli
        .pinned_originators
        .or_else(|| std::env::var("MONOPSONY_TRACKER_PINNED_ORIGINATORS").ok())
        .map(|s| {
            s.split(',')
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if pinned_originators.is_empty() {
        warn!(
            "No pinned originators configured (--pinned-originators or \
             MONOPSONY_TRACKER_PINNED_ORIGINATORS): signed blacklist adds will be REJECTED. \
             Configure the operator keys allowed to mint bans."
        );
    } else {
        for k in &pinned_originators {
            if !(k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit())) {
                warn!("Pinned originator '{}' is not a 64-char hex key; it will never match", k);
            }
        }
        info!(
            "Signed blacklist adds restricted to {} pinned originator key(s)",
            pinned_originators.len()
        );
    }
    pinned_originators.sort_unstable();
    pinned_originators.dedup();

    let auth_state = Arc::new(AuthState {
        auth_mode,
        valid_keys,
        jwt_config,
        pinned_originators,
    });

    // Open SQLite DB
    let conn = if cli.db.to_string_lossy() == ":memory:" {
        rusqlite::Connection::open_in_memory()?
    } else {
        rusqlite::Connection::open(&cli.db)?
    };

    init_schema(&conn)?;
    migrate_schema(&conn);

    if cli.v4v_enabled {
        v4v::init_v4v_schema(&conn);
    }

    reputation::init_reputation_tables(&conn);
    recipes::init_recipe_tables(&conn);

    let state = Arc::new(TrackerState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        auth: auth_state,
        allow_open_blacklist: cli.allow_open_blacklist,
    });

    // Periodic stale mirror purge
    let purge_db = state.db.clone();
    let stale_timeout = cli.stale_timeout;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let db = purge_db.lock().await;
            let cutoff = chrono::Utc::now().timestamp() - stale_timeout;
            match db.execute(
                "DELETE FROM mirrors WHERE announced_at < ?1",
                params![cutoff],
            ) {
                Ok(n) => {
                    if n > 0 {
                        info!("Purged {} stale mirrors", n);
                    }
                }
                Err(e) => warn!("Purge error: {}", e),
            }
        }
    });

    // Protected routes (auth middleware applied). Blacklist WRITES are here:
    // an unauthenticated ban write is a censorship DoS locally and would be
    // remote poison-a-fleet once entries propagate (design prerequisite 1).
    // POST /report is here too: reports feed auto-blacklist, so an
    // unauthenticated forged-report flood would be a censorship DoS against
    // tracker-local /discover (permanent ≥3/24h auto-ban). In open mode the
    // middleware passes everyone but inserts no identity, and the report
    // wrapper suppresses auto-blacklist for identity-less reporters — the
    // route placement plus the identity gate together close the flood path.
    let protected = Router::new()
        .route("/announce", post(handle_announce))
        .route("/blacklist", post(handle_blacklist_add))
        .route("/report", post(handle_report_with_auto_blacklist))
        .route(
            "/blacklist/:id",
            axum::routing::delete(handle_blacklist_remove),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    // Open routes (no auth). GET /blacklist stays open (design Q6 default —
    // reason strings are mildly sensitive; flip to auth if the operator wants
    // reasons private). GET /blacklist/signed is the propagation export.
    let open = Router::new()
        .route("/discover", get(handle_discover))
        .route("/health", get(handle_health))
        .route("/reports", get(report::handle_list_reports))
        .route("/reputation/:npub", get(reputation::handle_get_reputation))
        .route("/reputation/dispute", post(reputation::handle_open_dispute))
        .route("/reputation/attest", post(reputation::handle_attest))
        .route("/blacklist", get(handle_blacklist_list))
        .route("/blacklist/signed", get(handle_blacklist_signed))
        .merge(recipes::recipe_router());

    let app = protected.merge(open).with_state(state);

    let listener = tokio::net::TcpListener::bind(cli.addr).await?;
    info!(
        "Monopsony tracker listening on {} (auth={})",
        cli.addr, cli.auth_mode
    );

    let shutdown = async {
        tokio::signal::ctrl_c().await.unwrap();
        info!("Shutdown signal received, draining in-flight requests...");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    info!("Tracker shut down gracefully");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as AxumStatusCode};
    use tower::ServiceExt;

    /// Build a test TrackerState with in-memory DB and given auth mode
    fn test_state(auth_mode: TrackerAuthMode, keys: Vec<String>) -> Arc<TrackerState> {
        test_state_with_pins(auth_mode, keys, vec![])
    }

    /// Same as [`test_state`] but with an explicit pinned-originator set
    /// (the signed-blacklist add authorization).
    fn test_state_with_pins(
        auth_mode: TrackerAuthMode,
        keys: Vec<String>,
        pinned_originators: Vec<String>,
    ) -> Arc<TrackerState> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        report::init_reports_table(&conn);
        reputation::init_reputation_tables(&conn);
        stun::migrate_nat_columns(&conn);
        recipes::init_recipe_tables(&conn);

        let jwt_config = if auth_mode == TrackerAuthMode::Jwt {
            Some(JwtConfig {
                trusted_issuers: vec!["trustbroker".to_string()],
                min_ttl_secs: 0,
                jwt_secret: auth::DEV_JWT_SECRET.to_string(),
            })
        } else {
            None
        };

        Arc::new(TrackerState {
            db: Arc::new(tokio::sync::Mutex::new(conn)),
            auth: Arc::new(AuthState {
                auth_mode,
                valid_keys: keys,
                jwt_config,
                pinned_originators,
            }),
            allow_open_blacklist: false,
        })
    }

    /// Same as [`test_state_with_pins`] but with the open-mode legacy
    /// blacklist gate disabled (`--allow-open-blacklist`).
    fn test_state_with_open_blacklist_allowed(
        auth_mode: TrackerAuthMode,
        keys: Vec<String>,
        pinned_originators: Vec<String>,
    ) -> Arc<TrackerState> {
        let s = test_state_with_pins(auth_mode, keys, pinned_originators);
        Arc::new(TrackerState {
            db: s.db.clone(),
            auth: s.auth.clone(),
            allow_open_blacklist: true,
        })
    }

    /// The pinned-originator set for a test signing key.
    fn pinned_of(key: &ed25519_dalek::SigningKey) -> Vec<String> {
        vec![hex::encode(key.verifying_key().as_bytes())]
    }

    /// Build the test app (same router as production)
    fn test_app(state: Arc<TrackerState>) -> Router {
        let protected = Router::new()
            .route("/announce", post(handle_announce))
            .route("/blacklist", post(handle_blacklist_add))
            .route("/report", post(handle_report_with_auto_blacklist))
            .route(
                "/blacklist/:id",
                axum::routing::delete(handle_blacklist_remove),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth::auth_middleware,
            ));

        let open = Router::new()
            .route("/discover", get(handle_discover))
            .route("/health", get(handle_health))
            .route("/reports", get(report::handle_list_reports))
            .route("/reputation/:npub", get(reputation::handle_get_reputation))
            .route("/reputation/dispute", post(reputation::handle_open_dispute))
            .route("/reputation/attest", post(reputation::handle_attest))
            .route("/blacklist", get(handle_blacklist_list))
            .route("/blacklist/signed", get(handle_blacklist_signed))
            .merge(recipes::recipe_router());

        protected.merge(open).with_state(state)
    }

    fn make_announce_body(mirror_id: &str, distro: &str) -> String {
        serde_json::json!({
            "mirror_id": mirror_id,
            "distro": distro,
            "transport": ["http"],
            "endpoints": ["http://127.0.0.1:3129"],
            "lat_lon": [40.7128, -74.0060]
        })
        .to_string()
    }

    /// Announce body signed by `key`: signs the canonical JSON of the FULL
    /// AnnounceRequest (as the tracker reconstructs it) minus the
    /// `operator_signature` field — the documented announce signing contract.
    fn make_signed_announce_body(
        mirror_id: &str,
        distro: &str,
        key: &ed25519_dalek::SigningKey,
    ) -> String {
        use ed25519_dalek::Signer;
        let req = AnnounceRequest {
            mirror_id: mirror_id.to_string(),
            distro: distro.to_string(),
            transport: vec!["http".to_string()],
            endpoints: vec!["http://127.0.0.1:3129".to_string()],
            capacity_bytes: None,
            bandwidth_bps: None,
            uptime_seconds: None,
            packages: None,
            last_sync: None,
            lat_lon: Some((40.7128, -74.0060)),
            stats: None,
            operator_npub: Some(hex::encode(key.verifying_key().as_bytes())),
            operator_signature: None,
            display_name: None,
            payment_pointer: None,
        };
        let mut value = serde_json::to_value(&req).unwrap();
        // The tracker signs over the request minus the signature field —
        // mirror that exactly (a null field would change the bytes).
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("operator_signature");
        }
        let sig = key.sign(canonical_json(&value).as_bytes());
        value["operator_signature"] = serde_json::Value::String(hex::encode(sig.to_bytes()));
        value.to_string()
    }

    /// A signed blacklist entry (ban) from `key`.
    fn signed_ban_entry(
        subject: &str,
        seq: u64,
        key: &ed25519_dalek::SigningKey,
    ) -> monopsony_common::peer_blacklist::BlacklistEntry {
        use monopsony_common::peer_blacklist::{BlacklistAction, BlacklistEntry};
        let mut entry = BlacklistEntry {
            subject: subject.to_string(),
            action: BlacklistAction::Ban,
            reason: "test ban".to_string(),
            originator: String::new(),
            seq,
            created_at: chrono::Utc::now().timestamp(),
            expires_at: None,
            signature: String::new(),
        };
        entry.sign(key);
        entry
    }

    #[tokio::test]
    async fn test_auth_open_mode_announce_no_key() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::OK,
            "open mode should accept announce without key"
        );
    }

    #[tokio::test]
    async fn test_auth_token_mode_no_key() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "token mode should reject without key"
        );
    }

    #[tokio::test]
    async fn test_auth_token_mode_valid_key() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);

        // Token mode requires a signed announce (prerequisite 2).
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("X-Monopsony-Key", "secret-key")
            .body(Body::from(make_signed_announce_body(
                "m1",
                "archlinux",
                &key,
            )))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::OK,
            "token mode should accept a valid key + signed announce"
        );
    }

    #[tokio::test]
    async fn test_auth_token_mode_invalid_key() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("X-Monopsony-Key", "wrong-key")
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "token mode should reject wrong key"
        );
    }

    #[tokio::test]
    async fn test_auth_jwt_mode_valid_token() {
        let state = test_state(TrackerAuthMode::Jwt, vec![]);
        let app = test_app(state);

        // JWT sub IS the operator npub; the body must carry the same npub
        // plus a valid signature over the payload (prerequisite 2).
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let npub = hex::encode(key.verifying_key().as_bytes());
        let claims = auth::JwtClaims {
            sub: npub,
            iss: "trustbroker".to_string(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: None,
            tier: None,
            entitlements: None,
        };
        let jwt = auth::encode_test_jwt(&claims).unwrap();

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", jwt))
            .body(Body::from(make_signed_announce_body(
                "m1",
                "archlinux",
                &key,
            )))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::OK,
            "jwt mode should accept a valid token + matching signed announce"
        );
    }

    #[tokio::test]
    async fn test_auth_jwt_mode_expired_token() {
        let state = test_state(TrackerAuthMode::Jwt, vec![]);
        let app = test_app(state);

        let claims = auth::JwtClaims {
            sub: "operator123".to_string(),
            iss: "trustbroker".to_string(),
            exp: chrono::Utc::now().timestamp() - 120,
            iat: None,
            tier: None,
            entitlements: None,
        };
        let jwt = auth::encode_test_jwt(&claims).unwrap();

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", jwt))
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "jwt mode should reject expired token"
        );
    }

    #[tokio::test]
    async fn test_auth_jwt_mode_no_token() {
        let state = test_state(TrackerAuthMode::Jwt, vec![]);
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "jwt mode should reject without token"
        );
    }

    #[tokio::test]
    async fn test_discover_always_open() {
        // Even in token mode, discover should be accessible without auth
        let state = test_state(TrackerAuthMode::Token, vec!["key".to_string()]);
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/discover?distro=archlinux")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::OK,
            "discover should be open in any auth mode"
        );
    }

    #[tokio::test]
    async fn test_health_always_open() {
        let state = test_state(TrackerAuthMode::Token, vec!["key".to_string()]);
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::OK,
            "health should be open in any auth mode"
        );
    }

    #[tokio::test]
    async fn test_operator_auto_created_on_announce() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state.clone());

        let body = serde_json::json!({
            "mirror_id": "m-op",
            "distro": "archlinux",
            "transport": ["http"],
            "endpoints": ["http://127.0.0.1:3129"],
            "operator_npub": "abc123",
            "display_name": "Test Operator"
        })
        .to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Verify operator was created in DB
        let db = state.db.lock().await;
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM operators WHERE npub = 'abc123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "operator should be auto-created");

        let display_name: String = db
            .query_row(
                "SELECT display_name FROM operators WHERE npub = 'abc123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(display_name, "Test Operator");
    }

    // --- Original tests ---

    #[test]
    fn test_haversine() {
        // NYC to LA: ~3944 km
        let dist = haversine_km(40.7128, -74.0060, 34.0522, -118.2437);
        assert!((dist - 3944.0).abs() < 100.0, "NYC-LA distance: {}", dist);
    }

    #[test]
    fn test_scoring_no_geo() {
        let mirror = MirrorRecord {
            mirror_id: "test".to_string(),
            distro: "archlinux".to_string(),
            transport: r#"["http"]"#.to_string(),
            endpoints: r#"["http://test:3129"]"#.to_string(),
            capacity_bytes: Some(500_000_000_000),
            bandwidth_bps: Some(104_857_600),
            uptime_seconds: Some(2_592_000),
            packages: Some(847),
            last_sync: Some(chrono::Utc::now().timestamp().to_string()),
            lat: None,
            lon: None,
            bytes_served_24h: None,
            requests_24h: None,
            announced_at: chrono::Utc::now().timestamp(),
            operator_npub: None,
            nat_type: None,
        };
        let score = score_mirror(&mirror, None, None, chrono::Utc::now().timestamp());
        // With no client geo: proximity=50, freshness~100, reliability=100, capacity=100
        // = 0.4*50 + 0.3*100 + 0.2*100 + 0.1*100 = 80
        // minus NAT penalty (Unknown = 8.0) = ~72
        assert!(score > 67.0, "Score should be ~72, got {}", score);
    }

    #[test]
    fn test_scoring_nearby() {
        let mirror = MirrorRecord {
            mirror_id: "test".to_string(),
            distro: "archlinux".to_string(),
            transport: r#"["http"]"#.to_string(),
            endpoints: r#"["http://test:3129"]"#.to_string(),
            capacity_bytes: Some(500_000_000_000),
            bandwidth_bps: Some(50_000_000),
            uptime_seconds: Some(2_592_000),
            packages: None,
            last_sync: Some(chrono::Utc::now().timestamp().to_string()),
            lat: Some(33.5),
            lon: Some(-112.0),
            bytes_served_24h: None,
            requests_24h: None,
            announced_at: chrono::Utc::now().timestamp(),
            operator_npub: None,
            nat_type: None,
        };
        // Client at same location
        let score = score_mirror(
            &mirror,
            Some(33.5),
            Some(-112.0),
            chrono::Utc::now().timestamp(),
        );
        assert!(
            score > 80.0,
            "Same-location score should be high, got {}",
            score
        );
    }

    #[test]
    fn test_scoring_far_away() {
        let mirror = MirrorRecord {
            mirror_id: "test".to_string(),
            distro: "archlinux".to_string(),
            transport: r#"["http"]"#.to_string(),
            endpoints: r#"["http://test:3129"]"#.to_string(),
            capacity_bytes: None,
            bandwidth_bps: None,
            uptime_seconds: None,
            packages: None,
            last_sync: None,
            lat: Some(-33.8688), // Sydney
            lon: Some(151.2093),
            bytes_served_24h: None,
            requests_24h: None,
            announced_at: chrono::Utc::now().timestamp(),
            operator_npub: None,
            nat_type: None,
        };
        // Client in NYC
        let score = score_mirror(
            &mirror,
            Some(40.7128),
            Some(-74.006),
            chrono::Utc::now().timestamp(),
        );
        // Very far → proximity near 0, no last_sync → freshness 25, no uptime → 50, no bw → 50
        // = 0.4*~0 + 0.3*25 + 0.2*50 + 0.1*50 = 22.5, minus NAT unknown penalty = ~14.5
        assert!(
            score < 35.0,
            "Far-away stale mirror should score low, got {}",
            score
        );
    }

    #[test]
    fn test_db_roundtrip() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let req = AnnounceRequest {
            mirror_id: "test-mirror".to_string(),
            distro: "archlinux".to_string(),
            transport: vec!["http".to_string(), "https".to_string()],
            endpoints: vec!["http://test:3129".to_string()],
            capacity_bytes: Some(500_000_000_000),
            bandwidth_bps: Some(50_000_000),
            uptime_seconds: Some(86400),
            packages: Some(847),
            last_sync: Some("2026-04-19T00:00:00Z".to_string()),
            lat_lon: Some((33.5, -112.0)),
            stats: Some(AnnounceStats {
                bytes_served_24h: Some(10_000_000),
                requests_24h: Some(500),
            }),
            operator_npub: Some("test-npub".to_string()),
            operator_signature: None,
            display_name: None,
            payment_pointer: None,
        };

        let transport_json = serde_json::to_string(&req.transport).unwrap();
        let endpoints_json = serde_json::to_string(&req.endpoints).unwrap();
        let now = chrono::Utc::now().timestamp();

        conn.execute(
            "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, capacity_bytes,
                bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
                bytes_served_24h, requests_24h, announced_at, operator_npub)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                req.mirror_id,
                req.distro,
                transport_json,
                endpoints_json,
                req.capacity_bytes,
                req.bandwidth_bps,
                req.uptime_seconds,
                req.packages,
                req.last_sync,
                33.5,
                -112.0,
                10_000_000,
                500,
                now,
                req.operator_npub
            ],
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mirrors WHERE distro = 'archlinux'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // --- Report tests ---

    #[tokio::test]
    async fn test_report_hash_mismatch() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state.clone());

        let body = serde_json::json!({
            "mirror_id": "m1",
            "report_type": "hash_mismatch",
            "package_name": "nginx-1.24.0",
            "expected_hash": "abc123",
            "actual_hash": "def456",
            "timestamp": chrono::Utc::now().timestamp()
        })
        .to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/report")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Verify stored in DB
        let db = state.db.lock().await;
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM reports WHERE mirror_id = 'm1' AND report_type = 'hash_mismatch'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_report_missing_mirror_id() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state);

        let body = serde_json::json!({
            "mirror_id": "",
            "report_type": "slow_response",
            "timestamp": chrono::Utc::now().timestamp()
        })
        .to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/report")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_list_reports() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state.clone());

        // Insert a report directly
        let db = state.db.lock().await;
        let now = chrono::Utc::now().timestamp();
        db.execute(
            "INSERT INTO reports (mirror_id, report_type, timestamp, created_at) VALUES (?1, ?2, ?3, ?4)",
            params!["m1", "hash_mismatch", now, now],
        ).unwrap();
        drop(db);

        let req = Request::builder()
            .method("GET")
            .uri("/reports?mirror_id=m1")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let reports = parsed.get("reports").unwrap().as_array().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0]["mirror_id"], "m1");
        assert_eq!(reports[0]["report_type"], "hash_mismatch");
    }

    // -----------------------------------------------------------------------
    // Blacklist write auth + signed-entry propagation (prerequisite 1)
    // -----------------------------------------------------------------------

    async fn post_blacklist(app: Router, key_header: Option<&str>, body: String) -> AxumStatusCode {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/blacklist")
            .header("Content-Type", "application/json");
        if let Some(k) = key_header {
            builder = builder.header("X-Monopsony-Key", k);
        }
        let resp = app
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        resp.status()
    }

    fn blacklist_body(mirror_id: &str) -> String {
        serde_json::json!({ "mirror_id": mirror_id, "reason": "test" }).to_string()
    }

    fn signed_entry_body(entry: &monopsony_common::peer_blacklist::BlacklistEntry) -> String {
        serde_json::json!({ "entry": entry }).to_string()
    }

    #[tokio::test]
    async fn blacklist_post_unauthenticated_rejected_in_token_mode() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);
        let status = post_blacklist(app, None, blacklist_body("m1")).await;
        assert_eq!(
            status,
            AxumStatusCode::UNAUTHORIZED,
            "blacklist writes must NOT be on the open router"
        );
    }

    #[tokio::test]
    async fn blacklist_delete_unauthenticated_rejected_in_token_mode() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/blacklist/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxumStatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn blacklist_post_with_key_accepted_in_token_mode() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);
        let status = post_blacklist(app, Some("secret-key"), blacklist_body("m1")).await;
        assert_eq!(status, AxumStatusCode::OK);
    }

    // -----------------------------------------------------------------------
    // A2: open-mode legacy blacklist gate (red-team §9 fix #2)
    // -----------------------------------------------------------------------

    /// Open mode without the operator flag: the legacy unsigned insert is
    /// unauthenticated PERMANENT /discover censorship (expires_at NULL, no
    /// retraction) — must 403, and must not persist a row.
    #[tokio::test]
    async fn open_mode_legacy_blacklist_add_rejected_without_flag() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state.clone());
        let status = post_blacklist(app, None, blacklist_body("honest-mirror")).await;
        assert_eq!(
            status,
            AxumStatusCode::FORBIDDEN,
            "open-mode legacy blacklist add must fail closed without \
             --allow-open-blacklist (red-team A2)"
        );
        let db = state.db.lock().await;
        assert!(
            !is_blacklisted(&db, "honest-mirror", None),
            "rejected legacy add must not persist a censorship row"
        );
    }

    /// Explicit operator opt-in restores the legacy path (with a loud log),
    /// and signed entries were never gated by the flag.
    #[tokio::test]
    async fn open_mode_legacy_blacklist_add_allowed_with_flag() {
        let state = test_state_with_open_blacklist_allowed(TrackerAuthMode::Open, vec![], vec![]);
        let app = test_app(state.clone());
        let status = post_blacklist(app, None, blacklist_body("m1")).await;
        assert_eq!(status, AxumStatusCode::OK);
        let db = state.db.lock().await;
        assert!(is_blacklisted(&db, "m1", None));
    }

    /// The warning is a pure startup seam: Some (with the hijack/censorship
    /// rationale) only in open mode, None in authenticated modes.
    #[test]
    fn open_mode_startup_warning_fires_only_in_open_mode() {
        let open = open_mode_startup_warning(&TrackerAuthMode::Open)
            .expect("open mode must produce a startup warning");
        assert!(open.contains("hijack"), "warning must name announce-id hijack");
        assert!(
            open.contains("censorship"),
            "warning must name discovery censorship"
        );
        assert!(
            open.contains("DEMO MODE"),
            "warning must mark open mode as demo"
        );
        assert_eq!(
            open_mode_startup_warning(&TrackerAuthMode::Token),
            None,
            "token mode is authenticated — no warning"
        );
        assert_eq!(
            open_mode_startup_warning(&TrackerAuthMode::Jwt),
            None,
            "jwt mode is authenticated — no warning"
        );
    }

    #[tokio::test]
    async fn signed_entry_roundtrip_stored_and_exported() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], pinned_of(&key));
        let app = test_app(state.clone());
        let entry = signed_ban_entry("mirror:badhost-archlinux", 1, &key);

        let status = post_blacklist(app, None, signed_entry_body(&entry)).await;
        assert_eq!(status, AxumStatusCode::OK);

        // Export carries the signed entry verbatim.
        let app2 = test_app(state.clone());
        let resp = app2
            .oneshot(
                Request::builder()
                    .uri("/blacklist/signed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let entries: Vec<monopsony_common::peer_blacklist::BlacklistEntry> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], entry);
        assert!(entries[0].verify_signature().is_ok());

        // The stored row feeds the legacy enforcement path too.
        let db = state.db.lock().await;
        assert!(is_blacklisted(&db, "badhost-archlinux", None));
    }

    /// Defense-in-depth (red-team §9 fix #5): the signed export re-verifies
    /// each row's signature against its claimed originator, so out-of-band
    /// SQLite tampering cannot launder a forged row into the propagation
    /// stream. A tampered row is skipped (logged), valid rows still export.
    #[tokio::test]
    async fn export_skips_signature_failing_rows() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], pinned_of(&key));

        let app = test_app(state.clone());
        let status = post_blacklist(
            app,
            None,
            signed_entry_body(&signed_ban_entry("mirror:tampered-target", 1, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::OK);

        let app = test_app(state.clone());
        let status = post_blacklist(
            app,
            None,
            signed_entry_body(&signed_ban_entry("mirror:export-survivor", 1, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::OK);

        // Out-of-band tampering of ONE row's signature in the tracker DB
        // (simulated local compromise) must not reach the export.
        {
            let db = state.db.lock().await;
            db.execute(
                "UPDATE blacklist SET signature = 'deadbeef' WHERE mirror_id = 'tampered-target'",
                [],
            )
            .unwrap();
        }

        let resp = test_app(state)
            .oneshot(
                Request::builder()
                    .uri("/blacklist/signed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let entries: Vec<monopsony_common::peer_blacklist::BlacklistEntry> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "exactly the untampered row survives export re-verification"
        );
        assert_eq!(entries[0].subject, "mirror:export-survivor");
        assert!(entries[0].verify_signature().is_ok());
    }

    #[tokio::test]
    async fn signed_entry_bad_signature_rejected() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        // The originator IS pinned; the entry fails on the signature check
        // (the trust-first pin gate would 403 an unpinned key regardless).
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], pinned_of(&key));
        let app = test_app(state);
        let mut entry = signed_ban_entry("mirror:badhost-archlinux", 1, &key);
        entry.reason = "tampered after signing".to_string();

        let status = post_blacklist(app, None, signed_entry_body(&entry)).await;
        assert_eq!(status, AxumStatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn signed_entry_seq_replay_rejected_with_conflict() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]);
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], pinned_of(&key));
        let app = test_app(state);

        let status = post_blacklist(
            app.clone(),
            None,
            signed_entry_body(&signed_ban_entry("mirror:m-arch", 5, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::OK);

        // Same seq replayed → conflict.
        let status = post_blacklist(
            app.clone(),
            None,
            signed_entry_body(&signed_ban_entry("mirror:m-arch", 5, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::CONFLICT);

        // Stale (lower) seq also rejected.
        let status = post_blacklist(
            app.clone(),
            None,
            signed_entry_body(&signed_ban_entry("mirror:m-arch", 4, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::CONFLICT);

        // Higher seq accepted.
        let status = post_blacklist(
            app,
            None,
            signed_entry_body(&signed_ban_entry("mirror:m-arch", 6, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::OK);
    }

    #[tokio::test]
    async fn retract_at_higher_seq_lifts_tracker_enforcement() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], pinned_of(&key));
        let app = test_app(state.clone());

        let status = post_blacklist(
            app,
            None,
            signed_entry_body(&signed_ban_entry("mirror:m-arch", 1, &key)),
        )
        .await;
        assert_eq!(status, AxumStatusCode::OK);

        {
            let db = state.db.lock().await;
            assert!(is_blacklisted(&db, "m-arch", None));
        }

        // Signed retract from the same originator at seq 2.
        let mut retract = signed_ban_entry("mirror:m-arch", 2, &key);
        retract.action = monopsony_common::peer_blacklist::BlacklistAction::Retract;
        retract.reason = "false positive".to_string();
        retract.sign(&key);
        let app = test_app(state.clone());
        let status = post_blacklist(app, None, signed_entry_body(&retract)).await;
        assert_eq!(status, AxumStatusCode::OK);

        let db = state.db.lock().await;
        assert!(
            !is_blacklisted(&db, "m-arch", None),
            "higher-seq retract must lift the ban (no clock trust)"
        );
    }

    #[tokio::test]
    async fn signed_entry_from_unpinned_originator_rejected() {
        // Validly signed by `outsider`, but the tracker only pins `authority`:
        // open-auth peers can no longer mint self-signed bans into the DB.
        let authority = ed25519_dalek::SigningKey::from_bytes(&[6u8; 32]);
        let outsider = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], pinned_of(&authority));
        let app = test_app(state.clone());
        let entry = signed_ban_entry("mirror:victim-archlinux", 1, &outsider);

        let status = post_blacklist(app, None, signed_entry_body(&entry)).await;
        assert_eq!(
            status,
            AxumStatusCode::FORBIDDEN,
            "unpinned originator must be rejected before any DB write"
        );

        let db = state.db.lock().await;
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM blacklist WHERE originator IS NOT NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "no signed row may exist for an unpinned originator");
    }

    #[tokio::test]
    async fn signed_add_rejected_when_no_pins_configured() {
        // Fail closed: with no pinned set, even a cryptographically valid
        // entry is rejected — the tracker never trusts by default.
        let key = ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]);
        let state = test_state_with_pins(TrackerAuthMode::Token, vec!["secret-key".to_string()], vec![]);
        let app = test_app(state);
        let entry = signed_ban_entry("mirror:m-arch", 1, &key);

        let status = post_blacklist(app, Some("secret-key"), signed_entry_body(&entry)).await;
        assert_eq!(
            status,
            AxumStatusCode::FORBIDDEN,
            "empty pinned set must reject signed adds (fail closed)"
        );
    }

    // --- /report auth + auto-blacklist suppression ---

    async fn post_report(app: Router, key_header: Option<&str>, mirror_id: &str) -> AxumStatusCode {
        let body = serde_json::json!({
            "mirror_id": mirror_id,
            "report_type": "hash_mismatch",
            "timestamp": chrono::Utc::now().timestamp()
        })
        .to_string();
        let mut builder = Request::builder()
            .method("POST")
            .uri("/report")
            .header("Content-Type", "application/json");
        if let Some(k) = key_header {
            builder = builder.header("X-Monopsony-Key", k);
        }
        let resp = app
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        resp.status()
    }

    async fn count_auto_blacklist_rows(state: &Arc<TrackerState>, mirror_id: &str) -> i64 {
        let db = state.db.lock().await;
        db.query_row(
            "SELECT COUNT(*) FROM blacklist WHERE mirror_id = ?1 AND source = 'auto'",
            params![mirror_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn report_flood_cannot_auto_blacklist_in_open_mode() {
        // The forged-report censorship vector: 3 hash_mismatch reports used
        // to permanently auto-ban the mirror. Unauthenticated reporters no
        // longer feed auto-blacklist — reports are stored, enforcement is not.
        let state = test_state(TrackerAuthMode::Open, vec![]);
        for _ in 0..3 {
            let app = test_app(state.clone());
            let status = post_report(app, None, "flooded-mirror").await;
            assert_eq!(status, AxumStatusCode::OK, "open mode still accepts reports");
        }
        assert_eq!(
            count_auto_blacklist_rows(&state, "flooded-mirror").await,
            0,
            "unauthenticated reports must never trigger the auto-ban"
        );
    }

    #[tokio::test]
    async fn authenticated_reports_trigger_auto_blacklist_in_token_mode() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        for _ in 0..3 {
            let app = test_app(state.clone());
            let status = post_report(app, Some("secret-key"), "flaky-mirror").await;
            assert_eq!(status, AxumStatusCode::OK);
        }
        assert_eq!(
            count_auto_blacklist_rows(&state, "flaky-mirror").await,
            1,
            "3 authenticated hash_mismatch reports trip the ≥3/24h auto-ban"
        );

        // And without the key the route is protected outright.
        let app = test_app(state.clone());
        let status = post_report(app, None, "unauth-mirror").await;
        assert_eq!(status, AxumStatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auto_blacklist_rows_never_exported_on_signed_endpoint() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        // Seed reports and drive the auto-blacklist path.
        {
            let db = state.db.lock().await;
            let now = chrono::Utc::now().timestamp();
            for _ in 0..3 {
                db.execute(
                    "INSERT INTO reports (mirror_id, report_type, timestamp, created_at) VALUES (?1, 'hash_mismatch', ?2, ?2)",
                    params!["flaky-mirror", now],
                )
                .unwrap();
            }
            drop(db);
        }
        {
            let db = state.db.lock().await;
            assert!(check_auto_blacklist(&db, "flaky-mirror", 3, 24));
        }

        let app = test_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/blacklist/signed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let entries: Vec<monopsony_common::peer_blacklist::BlacklistEntry> =
            serde_json::from_slice(&body).unwrap();
        assert!(
            entries.is_empty(),
            "unsigned auto rows must stay tracker-local (blast-radius containment)"
        );
    }

    // -----------------------------------------------------------------------
    // Announce signature verification (prerequisite 2)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn announce_token_mode_requires_signature() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("X-Monopsony-Key", "secret-key")
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "token mode must reject unsigned announces"
        );
    }

    #[tokio::test]
    async fn announce_token_mode_rejects_tampered_signature() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let app = test_app(state);

        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut body = make_signed_announce_body("m1", "archlinux", &key);
        // Tamper with a signed field after signing.
        let mut value: serde_json::Value = serde_json::from_str(&body).unwrap();
        value["distro"] = serde_json::Value::String("evil".to_string());
        body = value.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("X-Monopsony-Key", "secret-key")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn announce_open_mode_rejects_bad_signature_when_present() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state);

        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut body = make_signed_announce_body("m1", "archlinux", &key);
        let mut value: serde_json::Value = serde_json::from_str(&body).unwrap();
        value["mirror_id"] = serde_json::Value::String("spoofed".to_string());
        body = value.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "open mode verifies signatures when present (Q7 default)"
        );
    }

    #[tokio::test]
    async fn announce_open_mode_accepts_unsigned() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);
    }

    #[tokio::test]
    async fn announce_jwt_mode_rejects_npub_mismatch() {
        let state = test_state(TrackerAuthMode::Jwt, vec![]);
        let app = test_app(state);

        // Body signed by one key, JWT authenticated as a different npub.
        let key = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let claims = auth::JwtClaims {
            sub: hex::encode(
                ed25519_dalek::SigningKey::from_bytes(&[12u8; 32])
                    .verifying_key()
                    .as_bytes(),
            ),
            iss: "trustbroker".to_string(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: None,
            tier: None,
            entitlements: None,
        };
        let jwt = auth::encode_test_jwt(&claims).unwrap();

        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", jwt))
            .body(Body::from(make_signed_announce_body(
                "m1",
                "archlinux",
                &key,
            )))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            AxumStatusCode::UNAUTHORIZED,
            "body npub must match the authenticated JWT subject"
        );
    }

    // -----------------------------------------------------------------------
    // Mirror-id ownership binding (red-team A3/A4, fix #1)
    // -----------------------------------------------------------------------

    /// Signed announce with explicit endpoints — the hijack tests must be
    /// able to tell honest and attacker endpoints apart in the stored row.
    fn make_signed_announce_body_with_endpoints(
        mirror_id: &str,
        distro: &str,
        key: &ed25519_dalek::SigningKey,
        endpoints: &[&str],
    ) -> String {
        use ed25519_dalek::Signer;
        let req = AnnounceRequest {
            mirror_id: mirror_id.to_string(),
            distro: distro.to_string(),
            transport: vec!["http".to_string()],
            endpoints: endpoints.iter().map(|e| e.to_string()).collect(),
            capacity_bytes: None,
            bandwidth_bps: None,
            uptime_seconds: None,
            packages: None,
            last_sync: None,
            lat_lon: Some((40.7128, -74.0060)),
            stats: None,
            operator_npub: Some(hex::encode(key.verifying_key().as_bytes())),
            operator_signature: None,
            display_name: None,
            payment_pointer: None,
        };
        let mut value = serde_json::to_value(&req).unwrap();
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("operator_signature");
        }
        let sig = key.sign(canonical_json(&value).as_bytes());
        value["operator_signature"] = serde_json::Value::String(hex::encode(sig.to_bytes()));
        value.to_string()
    }

    /// Unsigned open-mode announce body with an optional best-effort npub.
    fn make_unsigned_announce_body(
        mirror_id: &str,
        distro: &str,
        npub: Option<&str>,
        endpoint: &str,
    ) -> String {
        let mut value = serde_json::json!({
            "mirror_id": mirror_id,
            "distro": distro,
            "transport": ["http"],
            "endpoints": [endpoint],
            "lat_lon": [40.7128, -74.0060]
        });
        if let Some(n) = npub {
            value["operator_npub"] = serde_json::Value::String(n.to_string());
        }
        value.to_string()
    }

    /// (endpoints, operator_npub, owner_npub) of a stored mirror row.
    async fn mirror_row(
        state: &Arc<TrackerState>,
        mirror_id: &str,
        distro: &str,
    ) -> Option<(String, Option<String>, Option<String>)> {
        let db = state.db.lock().await;
        db.query_row(
            "SELECT endpoints, operator_npub, owner_npub FROM mirrors
             WHERE mirror_id = ?1 AND distro = ?2",
            params![mirror_id, distro],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    }

    async fn post_announce(
        state: &Arc<TrackerState>,
        key_header: Option<&str>,
        body: String,
    ) -> AxumStatusCode {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json");
        if let Some(k) = key_header {
            builder = builder.header("X-Monopsony-Key", k);
        }
        let req = builder.body(Body::from(body)).unwrap();
        let resp = test_app(state.clone()).oneshot(req).await.unwrap();
        resp.status()
    }

    /// A4 flip test: an authenticated attacker announcing with their OWN
    /// valid signature must not take over a mirror_id another operator
    /// already claimed. Fails on the pre-fix unconditional UPSERT.
    #[tokio::test]
    async fn announce_hijack_of_existing_mirror_id_rejected() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let honest = ed25519_dalek::SigningKey::from_bytes(&[21u8; 32]);
        let attacker = ed25519_dalek::SigningKey::from_bytes(&[22u8; 32]);

        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints(
                "m-hijack",
                "archlinux",
                &honest,
                &["http://honest:3129"],
            ),
        )
        .await;
        assert_eq!(s, AxumStatusCode::OK, "honest first claim must be accepted");

        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints(
                "m-hijack",
                "archlinux",
                &attacker,
                &["http://attacker:3129"],
            ),
        )
        .await;
        assert_eq!(
            s,
            AxumStatusCode::CONFLICT,
            "hijack of an owned mirror_id must be rejected (red-team A4)"
        );

        let (endpoints, operator, owner) =
            mirror_row(&state, "m-hijack", "archlinux").await.unwrap();
        assert!(
            endpoints.contains("honest:3129"),
            "attacker endpoints must not be stored, got: {}",
            endpoints
        );
        let honest_npub = hex::encode(honest.verifying_key().as_bytes());
        assert_eq!(operator.as_deref(), Some(honest_npub.as_str()));
        assert_eq!(
            owner.as_deref(),
            Some(honest_npub.as_str()),
            "signed first claim binds ownership"
        );

        // The binding is per-row: another distro is a separate row…
        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints(
                "m-hijack",
                "debian",
                &attacker,
                &["http://attacker-deb:3129"],
            ),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::OK,
            "same mirror_id under another distro is a separate row"
        );

        // …and a fresh mirror_id is claimable by any authenticated operator.
        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints(
                "m-other",
                "archlinux",
                &attacker,
                &["http://attacker2:3129"],
            ),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::OK,
            "a fresh mirror_id is claimable by anyone authenticated"
        );
    }

    /// Legitimate updates keep working: same operator re-announce updates
    /// endpoints and preserves the owner binding.
    #[tokio::test]
    async fn announce_same_operator_reannounce_updates_endpoints() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let op = ed25519_dalek::SigningKey::from_bytes(&[23u8; 32]);

        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints("m-re", "archlinux", &op, &["http://old:3129"]),
        )
        .await;
        assert_eq!(s, AxumStatusCode::OK);

        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints("m-re", "archlinux", &op, &["http://new:3129"]),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::OK,
            "same-operator re-announce must stay accepted"
        );

        let (endpoints, _, owner) = mirror_row(&state, "m-re", "archlinux").await.unwrap();
        assert!(
            endpoints.contains("new:3129"),
            "legitimate endpoint updates must persist, got: {}",
            endpoints
        );
        assert!(owner.is_some(), "owner binding survives re-announces");
    }

    /// A3 flip test: even on an open (demo) tracker, an unsigned announce
    /// cannot swap the operator attribution of an existing row.
    #[tokio::test]
    async fn announce_open_mode_hijack_of_attributed_row_rejected() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let honest_npub = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[24u8; 32])
                .verifying_key()
                .as_bytes(),
        );
        let attacker_npub = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[25u8; 32])
                .verifying_key()
                .as_bytes(),
        );

        let s = post_announce(
            &state,
            None,
            make_unsigned_announce_body("m-open", "archlinux", Some(&honest_npub), "http://honest:3129"),
        )
        .await;
        assert_eq!(s, AxumStatusCode::OK);

        let s = post_announce(
            &state,
            None,
            make_unsigned_announce_body(
                "m-open",
                "archlinux",
                Some(&attacker_npub),
                "http://attacker:3129",
            ),
        )
        .await;
        assert_eq!(
            s,
            AxumStatusCode::CONFLICT,
            "open mode must not let a second peer swap row attribution (A3)"
        );

        let (endpoints, operator, owner) =
            mirror_row(&state, "m-open", "archlinux").await.unwrap();
        assert!(
            endpoints.contains("honest:3129"),
            "attacker endpoints must not overwrite, got: {}",
            endpoints
        );
        assert_eq!(operator.as_deref(), Some(honest_npub.as_str()));
        assert!(owner.is_none(), "unsigned claims never bind ownership");
    }

    /// A row claimed WITH a signature is crypto-bound even on an open
    /// tracker: later unsigned announces claiming the same npub are spoofing.
    #[tokio::test]
    async fn announce_open_mode_owner_bound_row_rejects_unsigned_claim() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let op = ed25519_dalek::SigningKey::from_bytes(&[26u8; 32]);
        let op_npub = hex::encode(op.verifying_key().as_bytes());

        let s = post_announce(
            &state,
            None,
            make_signed_announce_body_with_endpoints("m-own", "archlinux", &op, &["http://op:3129"]),
        )
        .await;
        assert_eq!(s, AxumStatusCode::OK);
        let (_, _, owner) = mirror_row(&state, "m-own", "archlinux").await.unwrap();
        assert_eq!(
            owner.as_deref(),
            Some(op_npub.as_str()),
            "signed open-mode claim binds ownership"
        );

        // Unsigned spoof of the SAME npub → reject (identity claim, no proof).
        let s = post_announce(
            &state,
            None,
            make_unsigned_announce_body("m-own", "archlinux", Some(&op_npub), "http://attacker:3129"),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::CONFLICT,
            "owner-bound rows reject unsigned npub claims"
        );

        // The owner's signed re-announce still works in open mode.
        let s = post_announce(
            &state,
            None,
            make_signed_announce_body_with_endpoints("m-own", "archlinux", &op, &["http://op2:3129"]),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::OK,
            "signed owner path keeps working in open mode"
        );
        let (endpoints, _, _) = mirror_row(&state, "m-own", "archlinux").await.unwrap();
        assert!(endpoints.contains("op2:3129"), "signed update persisted");
    }

    /// Documented demo-mode residual: a row that never carried an npub stays
    /// freely claimable in open mode (no identity ever existed to defend).
    #[tokio::test]
    async fn announce_open_mode_unattributed_row_first_claim_wins() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let npub_b = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[27u8; 32])
                .verifying_key()
                .as_bytes(),
        );

        let s = post_announce(
            &state,
            None,
            make_unsigned_announce_body("m-anon", "archlinux", None, "http://first:3129"),
        )
        .await;
        assert_eq!(s, AxumStatusCode::OK);

        let s = post_announce(
            &state,
            None,
            make_unsigned_announce_body("m-anon", "archlinux", Some(&npub_b), "http://second:3129"),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::OK,
            "unattributed rows remain claimable (documented demo-mode residual)"
        );
        let (endpoints, _, owner) = mirror_row(&state, "m-anon", "archlinux").await.unwrap();
        assert!(endpoints.contains("second:3129"));
        assert!(owner.is_none(), "unsigned attribution never binds ownership");
    }

    /// Migration path: rows predating the owner column (owner NULL,
    /// attribution set) bind ownership on the attributed operator's first
    /// SIGNED re-announce (lazy adoption), then behave owner-bound.
    #[tokio::test]
    async fn announce_legacy_row_binds_owner_on_first_signed_reannounce() {
        let state = test_state(TrackerAuthMode::Token, vec!["secret-key".to_string()]);
        let op = ed25519_dalek::SigningKey::from_bytes(&[28u8; 32]);
        let op_npub = hex::encode(op.verifying_key().as_bytes());

        {
            let db = state.db.lock().await;
            db.execute(
                "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, announced_at, operator_npub)
                 VALUES ('m-legacy', 'archlinux', '[\"http\"]', '[\"http://legacy:3129\"]', ?1, ?2)",
                params![chrono::Utc::now().timestamp(), &op_npub],
            )
            .unwrap();
        }

        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints(
                "m-legacy",
                "archlinux",
                &op,
                &["http://legacy2:3129"],
            ),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::OK,
            "attributed operator's signed re-announce is the migration path"
        );
        let (_, _, owner) = mirror_row(&state, "m-legacy", "archlinux").await.unwrap();
        assert_eq!(
            owner.as_deref(),
            Some(op_npub.as_str()),
            "first signed re-announce binds owner (lazy adoption)"
        );

        // Once bound, a different authenticated operator is locked out (A4).
        let attacker = ed25519_dalek::SigningKey::from_bytes(&[29u8; 32]);
        let s = post_announce(
            &state,
            Some("secret-key"),
            make_signed_announce_body_with_endpoints(
                "m-legacy",
                "archlinux",
                &attacker,
                &["http://attacker:3129"],
            ),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::CONFLICT,
            "bound legacy rows reject other operators"
        );
    }

    /// Migration protection: legacy rows must not be hijackable by a
    /// DIFFERENT npub while still unowned — signed or unsigned.
    #[tokio::test]
    async fn announce_legacy_row_rejects_npub_swap() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let honest_npub = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[30u8; 32])
                .verifying_key()
                .as_bytes(),
        );
        let attacker = ed25519_dalek::SigningKey::from_bytes(&[31u8; 32]);

        {
            let db = state.db.lock().await;
            db.execute(
                "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, announced_at, operator_npub)
                 VALUES ('m-leg2', 'archlinux', '[\"http\"]', '[\"http://legacy:3129\"]', ?1, ?2)",
                params![chrono::Utc::now().timestamp(), &honest_npub],
            )
            .unwrap();
        }

        let attacker_npub = hex::encode(attacker.verifying_key().as_bytes());
        let s = post_announce(
            &state,
            None,
            make_unsigned_announce_body(
                "m-leg2",
                "archlinux",
                Some(&attacker_npub),
                "http://attacker:3129",
            ),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::CONFLICT,
            "unsigned npub swap on a legacy row is a hijack (A3)"
        );

        let s = post_announce(
            &state,
            None,
            make_signed_announce_body_with_endpoints(
                "m-leg2",
                "archlinux",
                &attacker,
                &["http://attacker:3129"],
            ),
        )
        .await;
        assert_eq!(
            s, AxumStatusCode::CONFLICT,
            "a different operator cannot adopt an attributed legacy row"
        );
    }

    /// House-rule-5 guard: the ownership fix must not weaken the signed
    /// export — rows without a cryptographic originator never propagate.
    #[tokio::test]
    async fn signed_export_still_excludes_originator_null_rows() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        {
            let db = state.db.lock().await;
            db.execute(
                "INSERT INTO blacklist (mirror_id, reason, source, originator)
                 VALUES ('m-x', 'legacy manual ban', 'manual', NULL)",
                [],
            )
            .unwrap();
        }

        let req = Request::builder()
            .uri("/blacklist/signed")
            .body(Body::empty())
            .unwrap();
        let resp = test_app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("m-x"),
            "originator-NULL rows must stay excluded from the signed export"
        );
    }

    /// Direct matrix test of the ownership decision function.
    #[test]
    fn ownership_check_matrix() {
        let owner = "aa";
        let other = "bb";
        // Owner-bound rows: only the owner's verified signature passes.
        assert_eq!(
            announce_ownership_check(Some(owner), None, Some(owner), true),
            Ok(None)
        );
        assert!(announce_ownership_check(Some(owner), None, Some(owner), false).is_err());
        assert!(announce_ownership_check(Some(owner), None, Some(other), true).is_err());
        assert!(announce_ownership_check(Some(owner), Some(owner), None, true).is_err());
        // Unowned attributed (legacy) row: same npub passes; verified match
        // binds ownership; any different npub (or none) rejects.
        assert_eq!(
            announce_ownership_check(None, Some(owner), Some(owner), false),
            Ok(None)
        );
        assert_eq!(
            announce_ownership_check(None, Some(owner), Some(owner), true),
            Ok(Some(owner.to_string()))
        );
        assert!(announce_ownership_check(None, Some(owner), Some(other), true).is_err());
        assert!(announce_ownership_check(None, Some(owner), None, true).is_err());
        // Unattributed row: first claim wins; only verified claims bind.
        assert_eq!(
            announce_ownership_check(None, None, Some(other), true),
            Ok(Some(other.to_string()))
        );
        assert_eq!(
            announce_ownership_check(None, None, Some(other), false),
            Ok(None)
        );
        assert_eq!(announce_ownership_check(None, None, None, false), Ok(None));
        // npub comparison is case-insensitive (hex keys).
        assert_eq!(announce_ownership_check(Some("AA"), None, Some("aa"), true), Ok(None));
        assert_eq!(
            announce_ownership_check(None, Some("AA"), Some("aa"), true),
            Ok(Some("aa".to_string()))
        );
    }

    // -----------------------------------------------------------------------
    // A6 mirror_id canonicalization (§9 fix #4)
    // -----------------------------------------------------------------------

    async fn post_announce_raw(app: Router, body: String) -> AxumStatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/announce")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    /// Case/edge-space mutations of the same id collapse onto ONE canonical
    /// row — the identity bans and ownership binding key on.
    #[tokio::test]
    async fn announce_with_mutated_id_lands_canonical() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state.clone());

        assert_eq!(
            post_announce_raw(
                app.clone(),
                make_announce_body("  Honest-Host ", "archlinux")
            )
            .await,
            AxumStatusCode::OK
        );
        assert_eq!(
            post_announce_raw(app, make_announce_body("HONEST-HOST", "archlinux")).await,
            AxumStatusCode::OK
        );

        let db = state.db.lock().await;
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM mirrors WHERE distro = 'archlinux'",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "mutated variants must collapse onto one canonical row");
        let stored: String = db
            .query_row(
                "SELECT mirror_id FROM mirrors WHERE distro = 'archlinux'",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "honest-host", "stored id must be the canonical form");
    }

    /// Ids outside the printable-ASCII allowlist are REJECTED at ingestion
    /// (400), never rewritten — and rejected announces create no rows.
    #[tokio::test]
    async fn announce_rejects_ids_outside_canonical_allowlist() {
        let state = test_state(TrackerAuthMode::Open, vec![]);
        let app = test_app(state.clone());

        for bad in [
            "honest host",
            "honest\u{00a0}host",
            "honest\u{043e}st",
            "honest\u{0007}host",
            "   ",
            "",
        ] {
            assert_eq!(
                post_announce_raw(app.clone(), make_announce_body(bad, "archlinux")).await,
                AxumStatusCode::BAD_REQUEST,
                "mirror_id {bad:?} must be rejected at ingestion"
            );
        }
        let db = state.db.lock().await;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM mirrors", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "rejected announces must not create rows");
    }

    /// The full A6 closure on the tracker: ban the canonical id, re-announce
    /// a mutated variant (lands canonical on the SAME row), /discover must
    /// not serve the banned mirror.
    #[tokio::test]
    async fn banned_mirror_hidden_from_discover_despite_mutated_announce() {
        let state = test_state_with_open_blacklist_allowed(TrackerAuthMode::Open, vec![], vec![]);
        let app = test_app(state.clone());

        // Seed the canonical row.
        assert_eq!(
            post_announce_raw(app.clone(), make_announce_body("honest-host", "archlinux")).await,
            AxumStatusCode::OK
        );
        // Ban the canonical id (legacy path, canonicalized on add).
        let status = post_blacklist(app.clone(), None, blacklist_body("honest-host")).await;
        assert_eq!(status, AxumStatusCode::OK);
        // Attacker re-announces a mutated variant — the ROW updates (fresh
        // announce wins the UPSERT), the IDENTITY does not change.
        assert_eq!(
            post_announce_raw(app.clone(), make_announce_body(" HONEST-host", "archlinux")).await,
            AxumStatusCode::OK
        );

        let req = Request::builder()
            .method("GET")
            .uri("/discover?distro=archlinux")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("honest-host"),
            "banned mirror must stay filtered from /discover after a mutated re-announce: {text}"
        );
    }

    /// Legacy bans land canonical: a mutated ban id is stored canonical so
    /// `is_blacklisted` (exact match on canonical rows) hits; invalid ids are
    /// rejected without storing a row.
    #[tokio::test]
    async fn legacy_blacklist_add_lands_canonical_and_rejects_invalid() {
        let state = test_state_with_open_blacklist_allowed(TrackerAuthMode::Open, vec![], vec![]);
        let app = test_app(state.clone());

        let status = post_blacklist(app.clone(), None, blacklist_body("  Honest-Host ")).await;
        assert_eq!(status, AxumStatusCode::OK);
        {
            let db = state.db.lock().await;
            assert!(
                is_blacklisted(&db, "honest-host", None),
                "canonical query must hit the canonicalized ban row"
            );
        }

        let status = post_blacklist(app, None, blacklist_body("honest host")).await;
        assert_eq!(status, AxumStatusCode::BAD_REQUEST);
        {
            let db = state.db.lock().await;
            let count: i64 = db
                .query_row("SELECT COUNT(*) FROM blacklist", params![], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "only the canonical ban row may exist");
        }
    }

    /// A6 choke point at tracker intake: a correctly signed entry over a
    /// MUTATED mirror subject fails verification (BadSubject via
    /// BlacklistSubject::parse) and stores nothing — mutated ids cannot enter
    /// signed blacklist state even with a valid signature.
    #[tokio::test]
    async fn signed_entry_with_mutated_subject_rejected() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let npub = hex::encode(key.verifying_key().as_bytes());
        let state = test_state_with_pins(TrackerAuthMode::Open, vec![], vec![npub]);
        let app = test_app(state.clone());

        let entry = signed_ban_entry("mirror:Honest-Host", 1, &key);
        assert_eq!(
            entry.verify_signature(),
            Err(monopsony_common::peer_blacklist::BlacklistError::BadSubject)
        );
        let status = post_blacklist(app, None, signed_entry_body(&entry)).await;
        assert_eq!(
            status,
            AxumStatusCode::BAD_REQUEST,
            "mutated subject must fail signature verification"
        );
        let db = state.db.lock().await;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM blacklist", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "rejected entries must not persist");
    }
}
