//! Monopsony Tracker — Decentralized mirror discovery service
//!
//! Lightweight HTTP tracker (like BitTorrent's tracker protocol).
//! Mirrors announce themselves; clients discover nearby mirrors.
//! SQLite-backed, ~500 lines.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

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

    W_PROXIMITY * proximity_score
        + W_FRESHNESS * freshness_score
        + W_RELIABILITY * reliability_score
        + W_CAPACITY * capacity_score
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
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TrackerState {
    db: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// POST /announce — mirror registers itself
async fn handle_announce(
    State(state): State<TrackerState>,
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

    let result = db.execute(
        "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, capacity_bytes,
            bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
            bytes_served_24h, requests_24h, announced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
            announced_at = excluded.announced_at",
        params![
            req.mirror_id, req.distro, transport_json, endpoints_json,
            req.capacity_bytes, req.bandwidth_bps, req.uptime_seconds,
            req.packages, req.last_sync, lat, lon,
            bytes_24h, reqs_24h, now
        ],
    );

    match result {
        Ok(_) => {
            info!("Announce: {} (distro={})", req.mirror_id, req.distro);
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
    State(state): State<TrackerState>,
    Query(query): Query<DiscoverQuery>,
) -> impl IntoResponse {
    let limit = query.limit.unwrap_or(10).min(50) as usize;
    let transport_filter = query.transport.as_deref();

    let db = state.db.lock().await;

    let rows: Vec<MirrorRecord> = if let Some(tf) = transport_filter {
        let mut stmt = match db.prepare(
            "SELECT mirror_id, distro, transport, endpoints, capacity_bytes,
                    bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
                    bytes_served_24h, requests_24h, announced_at
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
                    bytes_served_24h, requests_24h, announced_at
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

    // Open SQLite DB
    let conn = if cli.db.to_string_lossy() == ":memory:" {
        rusqlite::Connection::open_in_memory()?
    } else {
        rusqlite::Connection::open(&cli.db)?
    };

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
            PRIMARY KEY (mirror_id, distro)
        );
        CREATE INDEX IF NOT EXISTS idx_mirrors_distro ON mirrors(distro);
        CREATE INDEX IF NOT EXISTS idx_mirrors_announced ON mirrors(announced_at);
        "
    )?;

    let state = TrackerState { db: Arc::new(tokio::sync::Mutex::new(conn)) };

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

    let app = Router::new()
        .route("/announce", post(handle_announce))
        .route("/discover", get(handle_discover))
        .route("/health", get(handle_health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cli.addr).await?;
    info!("Monopsony tracker listening on {}", cli.addr);

    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let score = score_mirror(&mirror, None, None, chrono::Utc::now().timestamp());
        // With no client geo: proximity=50, freshness~100, reliability=100, capacity=100
        // = 0.4*50 + 0.3*100 + 0.2*100 + 0.1*100 = 20+30+20+10 = 80
        assert!(score > 75.0, "Score should be ~80, got {}", score);
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
        };
        // Client at same location
        let score = score_mirror(&mirror, Some(33.5), Some(-112.0), chrono::Utc::now().timestamp());
        assert!(score > 90.0, "Same-location score should be high, got {}", score);
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
        };
        // Client in NYC
        let score = score_mirror(&mirror, Some(40.7128), Some(-74.006), chrono::Utc::now().timestamp());
        // Very far → proximity near 0, no last_sync → freshness 25, no uptime → 50, no bw → 50
        // = 0.4*~0 + 0.3*25 + 0.2*50 + 0.1*50 = 0+7.5+10+5 = 22.5
        assert!(score < 35.0, "Far-away stale mirror should score low, got {}", score);
    }

    #[test]
    fn test_db_roundtrip() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE mirrors (
                mirror_id TEXT NOT NULL, distro TEXT NOT NULL,
                transport TEXT NOT NULL DEFAULT '[]', endpoints TEXT NOT NULL DEFAULT '[]',
                capacity_bytes INTEGER, bandwidth_bps INTEGER, uptime_seconds INTEGER,
                packages INTEGER, last_sync TEXT, lat REAL, lon REAL,
                bytes_served_24h INTEGER, requests_24h INTEGER, announced_at INTEGER NOT NULL,
                PRIMARY KEY (mirror_id, distro)
            )"
        ).unwrap();

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
        };

        let transport_json = serde_json::to_string(&req.transport).unwrap();
        let endpoints_json = serde_json::to_string(&req.endpoints).unwrap();
        let now = chrono::Utc::now().timestamp();

        conn.execute(
            "INSERT INTO mirrors (mirror_id, distro, transport, endpoints, capacity_bytes,
                bandwidth_bps, uptime_seconds, packages, last_sync, lat, lon,
                bytes_served_24h, requests_24h, announced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                req.mirror_id, req.distro, transport_json, endpoints_json,
                req.capacity_bytes, req.bandwidth_bps, req.uptime_seconds,
                req.packages, req.last_sync, 33.5, -112.0,
                10_000_000, 500, now
            ],
        ).unwrap();

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mirrors WHERE distro = 'archlinux'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1);
    }
}
