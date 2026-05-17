//! Monopsony Tracker — Decentralized mirror discovery service
//!
//! Lightweight HTTP tracker (like BitTorrent's tracker protocol).
//! Mirrors announce themselves; clients discover nearby mirrors.
//! SQLite-backed, ~600 lines.

mod auth;
mod report;
mod reputation;
mod stun;
mod v4v;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
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

use auth::{AuthState, JwtConfig, TrackerAuthMode};

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
    transport: String,         // JSON array stored as string
    endpoints: String,         // JSON array stored as string
    capacity_bytes: Option<u64>,
    bandwidth_bps: Option<u64>,
    uptime_seconds: Option<u64>,
    packages: Option<u64>,
    last_sync: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    bytes_served_24h: Option<u64>,
    requests_24h: Option<u64>,
    announced_at: i64,         // epoch seconds
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
    let freshness_score = mirror.last_sync
        .as_ref()
        .and_then(|ls| ls.parse::<i64>().ok().or_else(|| {
            chrono::DateTime::parse_from_rfc3339(ls).ok().map(|dt| dt.timestamp())
        }))
        .map(|ls_epoch| {
            let age_secs = (now_epoch - ls_epoch).max(0);
            (100.0 * (1.0 - (age_secs as f64 / 86400.0).min(1.0))).max(0.0)
        })
        .unwrap_or(25.0); // No sync info → low-neutral

    // Reliability: uptime → score (30d=100, 0=0)
    let reliability_score = mirror.uptime_seconds
        .map(|up| 100.0 * (up as f64 / 2_592_000.0).min(1.0))
        .unwrap_or(50.0);

    // Capacity: bandwidth → score (100MB/s=100, 0=0)
    let capacity_score = mirror.bandwidth_bps
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
            expires_at  INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_blacklist_mirror ON blacklist(mirror_id);
        CREATE INDEX IF NOT EXISTS idx_blacklist_operator ON blacklist(operator_npub);
        ",
    )?;
    Ok(())
}

/// Apply migrations for existing databases that may lack newer columns.
fn migrate_schema(conn: &rusqlite::Connection) {
    // Add operator_npub column if missing (SQLite has no IF NOT EXISTS for ALTER)
    let _ = conn.execute_batch("ALTER TABLE mirrors ADD COLUMN operator_npub TEXT");
    stun::migrate_nat_columns(conn);
}

/// Ensure the operators table exists (idempotent).
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
pub fn is_blacklisted(db: &rusqlite::Connection, mirror_id: &str, operator_npub: Option<&str>) -> bool {
    let now = chrono::Utc::now().timestamp();
    let mirror_blocked: bool = db.query_row(
        "SELECT COUNT(*) FROM blacklist WHERE mirror_id = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
        params![mirror_id, now],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) > 0;

    if mirror_blocked {
        return true;
    }

    if let Some(npub) = operator_npub {
        let op_blocked: bool = db.query_row(
            "SELECT COUNT(*) FROM blacklist WHERE operator_npub = ?1 AND mirror_id IS NULL AND (expires_at IS NULL OR expires_at > ?2)",
            params![npub, now],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) > 0;
        return op_blocked;
    }

    false
}

/// Auto-blacklist a mirror if hash_mismatch reports exceed threshold.
pub fn check_auto_blacklist(db: &rusqlite::Connection, mirror_id: &str, threshold: i64, window_hours: i64) -> bool {
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
    reason: String,
    expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct BlacklistEntry {
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
    if req.mirror_id.is_none() && req.operator_npub.is_none() {
        return (StatusCode::BAD_REQUEST, "must provide mirror_id or operator_npub").into_response();
    }
    let db = state.db.lock().await;
    let now = chrono::Utc::now().timestamp();
    match db.execute(
        "INSERT INTO blacklist (mirror_id, operator_npub, reason, source, created_at, expires_at)
         VALUES (?1, ?2, ?3, 'manual', ?4, ?5)",
        params![req.mirror_id, req.operator_npub, req.reason, now, req.expires_at],
    ) {
        Ok(_) => (StatusCode::OK, "blacklisted").into_response(),
        Err(e) => {
            warn!("Blacklist add DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
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
async fn handle_blacklist_list(
    State(state): State<Arc<TrackerState>>,
) -> impl IntoResponse {
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
    let rows = stmt.query_map([], |row| {
        Ok(BlacklistEntry {
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

/// POST /announce — mirror registers itself
async fn handle_announce(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<AnnounceRequest>,
) -> impl IntoResponse {
    if req.mirror_id.is_empty() || req.distro.is_empty() {
        return (StatusCode::BAD_REQUEST, "mirror_id and distro required").into_response();
    }
    if req.endpoints.is_empty() {
        return (StatusCode::BAD_REQUEST, "at least one endpoint required").into_response();
    }

    let db = state.db.lock().await;
    let transport_json = serde_json::to_string(&req.transport).unwrap_or_default();
    let endpoints_json = serde_json::to_string(&req.endpoints).unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    let (lat, lon) = req.lat_lon.unwrap_or((0.0, 0.0));
    let (bytes_24h, reqs_24h) = req.stats.as_ref()
        .map(|s| (s.bytes_served_24h, s.requests_24h))
        .unwrap_or((None, None));
    let operator_npub = req.operator_npub.as_deref();

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

    let result = db.execute(
        "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, capacity_bytes,
            bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
            bytes_served_24h, requests_24h, announced_at, operator_npub)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
            req.mirror_id, req.distro, transport_json, endpoints_json,
            req.capacity_bytes, req.bandwidth_bps, req.uptime_seconds,
            req.packages, req.last_sync, lat, lon,
            bytes_24h, reqs_24h, now, operator_npub
        ],
    );

    match result {
        Ok(_) => {
            info!("Announce: {} (distro={}, operator={})", req.mirror_id, req.distro, operator_npub.unwrap_or("none"));
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
             ORDER BY announced_at DESC"
        ) {
            Ok(s) => s,
            Err(e) => { error!("Discover DB prepare: {}", e); return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(); }
        };
        stmt.query_map(params![query.distro, format!("%\"{}\"%", tf)], |row| {
            Ok(row_to_mirror(row))
        }).map(|r| r.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    } else {
        let mut stmt = match db.prepare(
            "SELECT mirror_id, distro, transport, endpoints, capacity_bytes,
                    bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
                    bytes_served_24h, requests_24h, announced_at, operator_npub, nat_type
             FROM mirrors WHERE distro = ?1
             ORDER BY announced_at DESC"
        ) {
            Ok(s) => s,
            Err(e) => { error!("Discover DB prepare: {}", e); return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(); }
        };
        stmt.query_map(params![query.distro], |row| {
            Ok(row_to_mirror(row))
        }).map(|r| r.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    // Filter out blacklisted mirrors
    let rows: Vec<MirrorRecord> = rows.into_iter()
        .filter(|m| !is_blacklisted(&db, &m.mirror_id, m.operator_npub.as_deref()))
        .collect();

    drop(db);

    let now_epoch = chrono::Utc::now().timestamp();

    // Score and rank mirrors, expand endpoints into individual results
    let mut scored: Vec<DiscoveredMirror> = rows.iter()
        .flat_map(|m| {
            let score = score_mirror(m, query.lat, query.lon, now_epoch);
            let endpoints: Vec<String> = serde_json::from_str(&m.endpoints)
                .unwrap_or_default();
            let transports: Vec<String> = serde_json::from_str(&m.transport)
                .unwrap_or_default();

            endpoints.into_iter().enumerate().map(move |(i, ep)| {
                DiscoveredMirror {
                    mirror_id: m.mirror_id.clone(),
                    endpoint: ep,
                    transport: transports.get(i).cloned().unwrap_or_else(|| "http".to_string()),
                    packages: m.packages,
                    last_sync: m.last_sync.clone(),
                    score,
                    latency_ms: None,
                }
            })
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    Json(DiscoverResponse {
        mirrors: scored,
        ttl: 3600,
    }).into_response()
}

/// GET /health — tracker health check
async fn handle_health() -> &'static str {
    "ok"
}

/// POST /report wrapper — delegates to report handler then checks auto-blacklist
async fn handle_report_with_auto_blacklist(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<report::ReportRequest>,
) -> impl IntoResponse {
    let is_hash_mismatch = matches!(req.report_type, report::ReportType::HashMismatch);
    let mirror_id = req.mirror_id.clone();

    let resp = report::handle_report(State(state.clone()), Json(req)).await;

    // If report was accepted and was a hash_mismatch, check auto-blacklist
    if is_hash_mismatch {
        let db = state.db.lock().await;
        check_auto_blacklist(&db, &mirror_id, 3, 24);
    }

    resp
}

// ---------------------------------------------------------------------------
// CLI + main
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "monopsony-tracker", about = "Monopsony mirror discovery tracker")]
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("monopsony_tracker=info".parse()?)
        )
        .init();

    // Parse auth mode
    let auth_mode = TrackerAuthMode::from_str(&cli.auth_mode)
        .map_err(|e| anyhow::anyhow!(e))?;

    let valid_keys = cli.api_keys
        .as_deref()
        .map(|s| s.split(',').map(|k| k.trim().to_string()).collect())
        .unwrap_or_default();

    let jwt_config = if auth_mode == TrackerAuthMode::Jwt {
        let issuers = cli.jwt_issuers
            .as_deref()
            .map(|s| s.split(',').map(|i| i.trim().to_string()).collect())
            .unwrap_or_default();
        Some(JwtConfig {
            trusted_issuers: issuers,
            min_ttl_secs: 60,
        })
    } else {
        None
    };

    let auth_state = Arc::new(AuthState {
        auth_mode,
        valid_keys,
        jwt_config,
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

    let state = Arc::new(TrackerState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        auth: auth_state,
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
                Ok(n) => { if n > 0 { info!("Purged {} stale mirrors", n); } }
                Err(e) => warn!("Purge error: {}", e),
            }
        }
    });

    // Protected routes (auth middleware applied)
    let protected = Router::new()
        .route("/announce", post(handle_announce))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    // Open routes (no auth)
    let open = Router::new()
        .route("/discover", get(handle_discover))
        .route("/health", get(handle_health))
        .route("/report", post(handle_report_with_auto_blacklist))
        .route("/reports", get(report::handle_list_reports))
        .route("/reputation/{npub}", get(reputation::handle_get_reputation))
        .route("/reputation/dispute", post(reputation::handle_open_dispute))
        .route("/reputation/attest", post(reputation::handle_attest))
        .route("/blacklist", post(handle_blacklist_add))
        .route("/blacklist", get(handle_blacklist_list))
        .route("/blacklist/{id}", axum::routing::delete(handle_blacklist_remove));

    let app = protected
        .merge(open)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cli.addr).await?;
    info!("Monopsony tracker listening on {} (auth={})", cli.addr, cli.auth_mode);

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
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        report::init_reports_table(&conn);
        reputation::init_reputation_tables(&conn);
        stun::migrate_nat_columns(&conn);

        let jwt_config = if auth_mode == TrackerAuthMode::Jwt {
            Some(JwtConfig {
                trusted_issuers: vec!["trustbroker".to_string()],
                min_ttl_secs: 0,
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
            }),
        })
    }

    /// Build the test app (same router as production)
    fn test_app(state: Arc<TrackerState>) -> Router {
        let protected = Router::new()
            .route("/announce", post(handle_announce))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth::auth_middleware,
            ));

        let open = Router::new()
            .route("/discover", get(handle_discover))
            .route("/health", get(handle_health))
            .route("/report", post(handle_report_with_auto_blacklist))
            .route("/reports", get(report::handle_list_reports))
            .route("/reputation/{npub}", get(reputation::handle_get_reputation))
            .route("/reputation/dispute", post(reputation::handle_open_dispute))
            .route("/reputation/attest", post(reputation::handle_attest))
            .route("/blacklist", post(handle_blacklist_add))
            .route("/blacklist", get(handle_blacklist_list))
            .route("/blacklist/{id}", axum::routing::delete(handle_blacklist_remove));

        protected.merge(open).with_state(state)
    }

    fn make_announce_body(mirror_id: &str, distro: &str) -> String {
        serde_json::json!({
            "mirror_id": mirror_id,
            "distro": distro,
            "transport": ["http"],
            "endpoints": ["http://127.0.0.1:3129"],
            "lat_lon": [40.7128, -74.0060],
            "operator_npub": "deadbeef01"
        }).to_string()
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
        assert_eq!(resp.status(), AxumStatusCode::OK, "open mode should accept announce without key");
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
        assert_eq!(resp.status(), AxumStatusCode::UNAUTHORIZED, "token mode should reject without key");
    }

    #[tokio::test]
    async fn test_auth_token_mode_valid_key() {
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
        assert_eq!(resp.status(), AxumStatusCode::OK, "token mode should accept with valid key");
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
        assert_eq!(resp.status(), AxumStatusCode::UNAUTHORIZED, "token mode should reject wrong key");
    }

    #[tokio::test]
    async fn test_auth_jwt_mode_valid_token() {
        let state = test_state(TrackerAuthMode::Jwt, vec![]);
        let app = test_app(state);

        let claims = auth::JwtClaims {
            sub: "operator123".to_string(),
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
            .body(Body::from(make_announce_body("m1", "archlinux")))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK, "jwt mode should accept valid token");
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
        assert_eq!(resp.status(), AxumStatusCode::UNAUTHORIZED, "jwt mode should reject expired token");
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
        assert_eq!(resp.status(), AxumStatusCode::UNAUTHORIZED, "jwt mode should reject without token");
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
        assert_eq!(resp.status(), AxumStatusCode::OK, "discover should be open in any auth mode");
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
        assert_eq!(resp.status(), AxumStatusCode::OK, "health should be open in any auth mode");
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
        }).to_string();

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
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM operators WHERE npub = 'abc123'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1, "operator should be auto-created");

        let display_name: String = db.query_row(
            "SELECT display_name FROM operators WHERE npub = 'abc123'",
            [],
            |row| row.get(0),
        ).unwrap();
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
        let score = score_mirror(&mirror, Some(33.5), Some(-112.0), chrono::Utc::now().timestamp());
        assert!(score > 80.0, "Same-location score should be high, got {}", score);
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
            lat: Some(-33.8688),  // Sydney
            lon: Some(151.2093),
            bytes_served_24h: None,
            requests_24h: None,
            announced_at: chrono::Utc::now().timestamp(),
            operator_npub: None,
            nat_type: None,
        };
        // Client in NYC
        let score = score_mirror(&mirror, Some(40.7128), Some(-74.006), chrono::Utc::now().timestamp());
        // Very far → proximity near 0, no last_sync → freshness 25, no uptime → 50, no bw → 50
        // = 0.4*~0 + 0.3*25 + 0.2*50 + 0.1*50 = 22.5, minus NAT unknown penalty = ~14.5
        assert!(score < 35.0, "Far-away stale mirror should score low, got {}", score);
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
                req.mirror_id, req.distro, transport_json, endpoints_json,
                req.capacity_bytes, req.bandwidth_bps, req.uptime_seconds,
                req.packages, req.last_sync, 33.5, -112.0,
                10_000_000, 500, now, req.operator_npub
            ],
        ).unwrap();

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mirrors WHERE distro = 'archlinux'",
            [],
            |row| row.get(0),
        ).unwrap();
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
        }).to_string();

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
        }).to_string();

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
}
