//! Tracker recipe storage — publish, search, vote, report, and quarantine.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::TrackerState;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Create recipe-related tables (idempotent).
pub fn init_recipe_tables(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS shadow_recipes (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            description     TEXT,
            author_key_id   TEXT NOT NULL,
            operator_key_id TEXT,
            recipe_toml     TEXT NOT NULL,
            author_sig      TEXT NOT NULL,
            operator_sig    TEXT,
            source_type     TEXT NOT NULL,
            source_url      TEXT NOT NULL,
            architectures   TEXT NOT NULL DEFAULT '[]',
            install_dir     TEXT,
            config_tracked  INTEGER DEFAULT 0,
            version         TEXT,
            created_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            updated_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );
        CREATE INDEX IF NOT EXISTS idx_recipes_name ON shadow_recipes(name);
        CREATE INDEX IF NOT EXISTS idx_recipes_author ON shadow_recipes(author_key_id);

        CREATE TABLE IF NOT EXISTS recipe_votes (
            recipe_id       TEXT NOT NULL,
            voter_key_id    TEXT NOT NULL,
            direction       TEXT NOT NULL,
            weight          REAL NOT NULL DEFAULT 1.0,
            install_count   INTEGER DEFAULT 0,
            created_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (recipe_id, voter_key_id)
        );

        CREATE TABLE IF NOT EXISTS recipe_reports (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            recipe_id       TEXT NOT NULL,
            reporter_key_id TEXT NOT NULL,
            reason          TEXT NOT NULL,
            reporter_sig    TEXT NOT NULL,
            created_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );

        CREATE TABLE IF NOT EXISTS recipe_quarantine (
            recipe_id       TEXT PRIMARY KEY,
            reason          TEXT NOT NULL,
            quarantined_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            report_count    INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS annotations (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            package_name    TEXT NOT NULL,
            from_version    TEXT NOT NULL,
            to_version      TEXT NOT NULL,
            severity        TEXT NOT NULL,
            message         TEXT NOT NULL,
            url             TEXT,
            author_key_id   TEXT NOT NULL,
            signature       TEXT NOT NULL,
            source_server   TEXT,
            verified_count  INTEGER DEFAULT 0,
            created_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );

        CREATE INDEX IF NOT EXISTS idx_annotations_package
            ON annotations(package_name, from_version, to_version);
        ",
    );
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRecipeRequest {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub recipe_toml: String,
    pub author_sig: String,
    pub operator_sig: Option<String>,
    pub source_type: String,
    pub source_url: String,
    #[serde(default)]
    pub architectures: Vec<String>,
    pub install_dir: Option<String>,
    #[serde(default)]
    pub config_tracked: bool,
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeSearchResult {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub source_type: String,
    pub source_url: String,
    pub architectures: Vec<String>,
    pub version: Option<String>,
    pub score: f64,
    pub install_count: i64,
    pub report_count: i64,
    pub quarantined: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeDetail {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub recipe_toml: String,
    pub author_sig: String,
    pub operator_sig: Option<String>,
    pub source_type: String,
    pub source_url: String,
    pub architectures: Vec<String>,
    pub install_dir: Option<String>,
    pub version: Option<String>,
    pub score: f64,
    pub install_count: i64,
    pub report_count: i64,
    pub quarantined: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRequest {
    pub direction: String, // "up" or "down"
    pub voter_key_id: String,
    pub install_count: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRecipeRequest {
    pub reporter_key_id: String,
    pub reason: String,
    pub reporter_sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineEntry {
    pub recipe_id: String,
    pub reason: String,
    pub quarantined_at: i64,
    pub report_count: i64,
}

// ---------------------------------------------------------------------------
// Vote weight
// ---------------------------------------------------------------------------

/// Calculate vote weight based on install count.
/// weight = 1.0 + min(log2(install_count + 1), 5.0) / 5.0
fn vote_weight(install_count: i64) -> f64 {
    1.0 + (install_count as f64 + 1.0).log2().min(5.0) / 5.0
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a recipe is quarantined.
fn is_quarantined(db: &rusqlite::Connection, recipe_id: &str) -> bool {
    db.query_row(
        "SELECT COUNT(*) FROM recipe_quarantine WHERE recipe_id = ?1",
        params![recipe_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Get the report count for a recipe.
fn report_count(db: &rusqlite::Connection, recipe_id: &str) -> i64 {
    db.query_row(
        "SELECT COUNT(*) FROM recipe_reports WHERE recipe_id = ?1",
        params![recipe_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// Get the weighted vote score for a recipe.
fn recipe_score(db: &rusqlite::Connection, recipe_id: &str) -> f64 {
    let up: f64 = db.query_row(
        "SELECT COALESCE(SUM(weight), 0.0) FROM recipe_votes WHERE recipe_id = ?1 AND direction = 'up'",
        params![recipe_id],
        |row| row.get(0),
    ).unwrap_or(0.0);

    let down: f64 = db.query_row(
        "SELECT COALESCE(SUM(weight), 0.0) FROM recipe_votes WHERE recipe_id = ?1 AND direction = 'down'",
        params![recipe_id],
        |row| row.get(0),
    ).unwrap_or(0.0);

    up - down
}

/// Get total install count (sum of all voters' install_counts).
fn total_installs(db: &rusqlite::Connection, recipe_id: &str) -> i64 {
    db.query_row(
        "SELECT COALESCE(SUM(install_count), 0) FROM recipe_votes WHERE recipe_id = ?1",
        params![recipe_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// POST /recipes/publish — publish a signed recipe
pub async fn handle_publish(
    State(state): State<Arc<TrackerState>>,
    Json(req): Json<PublishRecipeRequest>,
) -> impl IntoResponse {
    if req.id.is_empty() || req.name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "id and name required",
        )
            .into_response();
    }
    if req.author_sig.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "author signature required",
        )
            .into_response();
    }

    let db = state.db.lock().await;
    let now = chrono::Utc::now().timestamp();
    let arch_json = serde_json::to_string(&req.architectures).unwrap_or_else(|_| "[]".into());

    let result = db.execute(
        "INSERT INTO shadow_recipes (id, name, description, author_key_id, operator_key_id,
            recipe_toml, author_sig, operator_sig, source_type, source_url,
            architectures, install_dir, config_tracked, version, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            description = excluded.description,
            operator_key_id = excluded.operator_key_id,
            recipe_toml = excluded.recipe_toml,
            author_sig = excluded.author_sig,
            operator_sig = excluded.operator_sig,
            source_type = excluded.source_type,
            source_url = excluded.source_url,
            architectures = excluded.architectures,
            install_dir = excluded.install_dir,
            config_tracked = excluded.config_tracked,
            version = excluded.version,
            updated_at = excluded.updated_at",
        params![
            req.id,
            req.name,
            req.description,
            req.author_key_id,
            req.operator_key_id,
            req.recipe_toml,
            req.author_sig,
            req.operator_sig,
            req.source_type,
            req.source_url,
            arch_json,
            req.install_dir,
            req.config_tracked as i32,
            req.version,
            now,
        ],
    );

    match result {
        Ok(_) => {
            info!("Recipe published: {} ({})", req.name, req.id);
            (StatusCode::OK, "published").into_response()
        }
        Err(e) => {
            warn!("Recipe publish DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// GET /recipes/search?q=&limit=
pub async fn handle_search(
    State(state): State<Arc<TrackerState>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let limit = query.limit.min(100) as usize;
    let pattern = format!("%{}%", query.q);

    let db = state.db.lock().await;
    let mut stmt = match db.prepare(
        "SELECT id, name, description, author_key_id, operator_key_id,
                source_type, source_url, architectures, version
         FROM shadow_recipes
         WHERE name LIKE ?1
         ORDER BY updated_at DESC
         LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!("Recipe search prepare: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let rows: Vec<RecipeSearchResult> = stmt
        .query_map(params![pattern, limit as i64], |row| {
            let id: String = row.get(0)?;
            let name: String = row.get(1)?;
            let desc: Option<String> = row.get(2)?;
            let author_key: String = row.get(3)?;
            let op_key: Option<String> = row.get(4)?;
            let src_type: String = row.get(5)?;
            let src_url: String = row.get(6)?;
            let arch_json: String = row.get::<_, String>(7)?;
            let version: Option<String> = row.get(8)?;

            let archs: Vec<String> = serde_json::from_str(&arch_json).unwrap_or_default();
            let quarantined = is_quarantined(&db, &id);
            let score = recipe_score(&db, &id);
            let installs = total_installs(&db, &id);
            let reports = report_count(&db, &id);

            Ok(RecipeSearchResult {
                id,
                name,
                description: desc,
                author_key_id: author_key,
                operator_key_id: op_key,
                source_type: src_type,
                source_url: src_url,
                architectures: archs,
                version,
                score,
                install_count: installs,
                report_count: reports,
                quarantined,
            })
        })
        .map(|r| r.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    // Filter out quarantined recipes from normal search
    let rows: Vec<_> = rows.into_iter().filter(|r| !r.quarantined).collect();

    Json(rows).into_response()
}

/// GET /recipes/:id — get recipe details
pub async fn handle_get_recipe(
    State(state): State<Arc<TrackerState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let mut stmt = match db.prepare(
        "SELECT id, name, description, author_key_id, operator_key_id,
                recipe_toml, author_sig, operator_sig, source_type, source_url,
                architectures, install_dir, version, created_at, updated_at
         FROM shadow_recipes WHERE id = ?1",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!("Recipe get prepare: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let result = stmt.query_row(params![id], |row| {
        let id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let desc: Option<String> = row.get(2)?;
        let author_key: String = row.get(3)?;
        let op_key: Option<String> = row.get(4)?;
        let toml: String = row.get(5)?;
        let author_sig: String = row.get(6)?;
        let op_sig: Option<String> = row.get(7)?;
        let src_type: String = row.get(8)?;
        let src_url: String = row.get(9)?;
        let arch_json: String = row.get::<_, String>(10)?;
        let install_dir: Option<String> = row.get(11)?;
        let version: Option<String> = row.get(12)?;
        let created: i64 = row.get(13)?;
        let updated: i64 = row.get(14)?;

        let archs: Vec<String> = serde_json::from_str(&arch_json).unwrap_or_default();
        let quarantined = is_quarantined(&db, &id);
        let score = recipe_score(&db, &id);
        let installs = total_installs(&db, &id);
        let reports = report_count(&db, &id);

        Ok(RecipeDetail {
            id,
            name,
            description: desc,
            author_key_id: author_key,
            operator_key_id: op_key,
            recipe_toml: toml,
            author_sig,
            operator_sig: op_sig,
            source_type: src_type,
            source_url: src_url,
            architectures: archs,
            install_dir,
            version,
            score,
            install_count: installs,
            report_count: reports,
            quarantined,
            created_at: created,
            updated_at: updated,
        })
    });

    match result {
        Ok(detail) => Json(detail).into_response(),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            (StatusCode::NOT_FOUND, "recipe not found").into_response()
        }
        Err(e) => {
            warn!("Recipe get error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// POST /recipes/:id/vote — upvote or downvote
pub async fn handle_vote(
    State(state): State<Arc<TrackerState>>,
    Path(recipe_id): Path<String>,
    Json(req): Json<VoteRequest>,
) -> impl IntoResponse {
    if req.direction != "up" && req.direction != "down" {
        return (StatusCode::BAD_REQUEST, "direction must be 'up' or 'down'").into_response();
    }
    if req.voter_key_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "voter_key_id required").into_response();
    }

    // Check recipe exists
    let db = state.db.lock().await;
    let exists: bool = db
        .query_row(
            "SELECT COUNT(*) FROM shadow_recipes WHERE id = ?1",
            params![recipe_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (StatusCode::NOT_FOUND, "recipe not found").into_response();
    }

    let install_count = req.install_count.unwrap_or(0);
    let weight = vote_weight(install_count);
    let now = chrono::Utc::now().timestamp();

    let result = db.execute(
        "INSERT INTO recipe_votes (recipe_id, voter_key_id, direction, weight, install_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(recipe_id, voter_key_id) DO UPDATE SET
            direction = excluded.direction,
            weight = excluded.weight,
            install_count = excluded.install_count,
            created_at = excluded.created_at",
        params![recipe_id, req.voter_key_id, req.direction, weight, install_count, now],
    );

    match result {
        Ok(_) => (StatusCode::OK, "voted").into_response(),
        Err(e) => {
            warn!("Vote DB error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// POST /recipes/:id/report — report a recipe as malicious
pub async fn handle_report_recipe(
    State(state): State<Arc<TrackerState>>,
    Path(recipe_id): Path<String>,
    Json(req): Json<ReportRecipeRequest>,
) -> impl IntoResponse {
    if req.reason.is_empty() || req.reporter_key_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "reason and reporter_key_id required").into_response();
    }

    let db = state.db.lock().await;

    // Check recipe exists
    let exists: bool = db
        .query_row(
            "SELECT COUNT(*) FROM shadow_recipes WHERE id = ?1",
            params![recipe_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (StatusCode::NOT_FOUND, "recipe not found").into_response();
    }

    let now = chrono::Utc::now().timestamp();

    // Insert report
    if let Err(e) = db.execute(
        "INSERT INTO recipe_reports (recipe_id, reporter_key_id, reason, reporter_sig, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![recipe_id, req.reporter_key_id, req.reason, req.reporter_sig, now],
    ) {
        warn!("Recipe report DB error: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
    }

    // Count reports from distinct reporters
    let distinct_reporters: i64 = db
        .query_row(
            "SELECT COUNT(DISTINCT reporter_key_id) FROM recipe_reports WHERE recipe_id = ?1",
            params![recipe_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Quarantine at threshold 3
    const QUARANTINE_THRESHOLD: i64 = 3;
    if distinct_reporters >= QUARANTINE_THRESHOLD && !is_quarantined(&db, &recipe_id) {
        if let Err(e) = db.execute(
            "INSERT INTO recipe_quarantine (recipe_id, reason, quarantined_at, report_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                recipe_id,
                format!("auto-quarantined: {} reports from distinct operators", distinct_reporters),
                now,
                distinct_reporters,
            ],
        ) {
            warn!("Quarantine DB error: {}", e);
        } else {
            info!("Recipe {} quarantined ({} reports)", recipe_id, distinct_reporters);
        }
    }

    (StatusCode::OK, "reported").into_response()
}

/// GET /recipes/quarantined — list quarantined recipes (admin)
pub async fn handle_list_quarantined(
    State(state): State<Arc<TrackerState>>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let mut stmt = match db.prepare(
        "SELECT recipe_id, reason, quarantined_at, report_count
         FROM recipe_quarantine
         ORDER BY quarantined_at DESC",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!("Quarantine list prepare: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let rows: Vec<QuarantineEntry> = stmt
        .query_map([], |row| {
            Ok(QuarantineEntry {
                recipe_id: row.get(0)?,
                reason: row.get(1)?,
                quarantined_at: row.get(2)?,
                report_count: row.get(3)?,
            })
        })
        .map(|r| r.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    Json(rows).into_response()
}

/// DELETE /recipes/quarantined/:id — permanently remove a quarantined recipe
pub async fn handle_quarantine_remove(
    State(state): State<Arc<TrackerState>>,
    Path(recipe_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;

    // Remove from quarantine table
    if let Err(e) = db.execute(
        "DELETE FROM recipe_quarantine WHERE recipe_id = ?1",
        params![recipe_id],
    ) {
        warn!("Quarantine remove failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
    }

    // Also remove the recipe itself and its votes/reports
    let _ = db.execute("DELETE FROM recipe_votes WHERE recipe_id = ?1", params![recipe_id]);
    let _ = db.execute("DELETE FROM recipe_reports WHERE recipe_id = ?1", params![recipe_id]);
    let _ = db.execute("DELETE FROM shadow_recipes WHERE id = ?1", params![recipe_id]);

    info!("permanently removed quarantined recipe {recipe_id}");
    (StatusCode::OK, "removed").into_response()
}

/// POST /recipes/quarantined/:id/reinstate — restore a quarantined recipe
pub async fn handle_quarantine_reinstate(
    State(state): State<Arc<TrackerState>>,
    Path(recipe_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;

    // Remove from quarantine table (recipe stays in shadow_recipes)
    let removed = db.execute(
        "DELETE FROM recipe_quarantine WHERE recipe_id = ?1",
        params![recipe_id],
    );

    match removed {
        Ok(0) => (StatusCode::NOT_FOUND, "not quarantined").into_response(),
        Ok(_) => {
            // Also clear reports so it doesn't immediately re-quarantine
            let _ = db.execute(
                "DELETE FROM recipe_reports WHERE recipe_id = ?1",
                params![recipe_id],
            );
            info!("reinstated recipe {recipe_id}");
            (StatusCode::OK, "reinstated").into_response()
        }
        Err(e) => {
            warn!("Quarantine reinstate failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the recipe sub-router.
// ---------------------------------------------------------------------------
// Annotation handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishAnnotationRequest {
    pub package_name: String,
    pub from_version: String,
    pub to_version: String,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    pub author_key_id: String,
    pub signature: String,
    #[serde(default)]
    pub source_server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AnnotationQuery {
    pub package: String,
    #[allow(dead_code)]
    pub from: Option<String>,
    #[allow(dead_code)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationResponse {
    pub id: i64,
    pub package_name: String,
    pub from_version: String,
    pub to_version: String,
    pub severity: String,
    pub message: String,
    pub url: Option<String>,
    pub author_key_id: String,
    pub signature: String,
    pub source_server: Option<String>,
    pub verified_count: i64,
    pub created_at: i64,
}

pub async fn handle_publish_annotation(
    State(state): State<Arc<TrackerState>>,
    Json(body): Json<PublishAnnotationRequest>,
) -> impl IntoResponse {
    if body.signature.is_empty() {
        return (StatusCode::BAD_REQUEST, "signature is required").into_response();
    }

    let conn = state.db.lock().await;
    let now = chrono::Utc::now().timestamp();
    let res = conn.execute(
        "INSERT INTO annotations (package_name, from_version, to_version, severity, message, url, author_key_id, signature, source_server, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            body.package_name,
            body.from_version,
            body.to_version,
            body.severity,
            body.message,
            body.url,
            body.author_key_id,
            body.signature,
            body.source_server,
            now,
        ],
    );

    match res {
        Ok(_) => {
            info!("published annotation for {} {}->{}", body.package_name, body.from_version, body.to_version);
            (StatusCode::OK, "annotation published").into_response()
        }
        Err(e) => {
            warn!("annotation publish failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

pub async fn handle_fetch_annotations(
    State(state): State<Arc<TrackerState>>,
    Query(query): Query<AnnotationQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT id, package_name, from_version, to_version, severity, message, url,
                    author_key_id, signature, source_server, verified_count, created_at
             FROM annotations
             WHERE package_name = ?1
             ORDER BY created_at DESC",
        )
        .unwrap();

    let rows = stmt
        .query_map(params![query.package], |row| {
            Ok(AnnotationResponse {
                id: row.get(0)?,
                package_name: row.get(1)?,
                from_version: row.get(2)?,
                to_version: row.get(3)?,
                severity: row.get(4)?,
                message: row.get(5)?,
                url: row.get(6)?,
                author_key_id: row.get(7)?,
                signature: row.get(8)?,
                source_server: row.get(9)?,
                verified_count: row.get(10)?,
                created_at: row.get(11)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    Json(rows).into_response()
}

pub fn recipe_router() -> Router<Arc<TrackerState>> {
    Router::new()
        .route("/recipes/publish", post(handle_publish))
        .route("/recipes/search", get(handle_search))
        .route("/recipes/quarantined", get(handle_list_quarantined))
        .route("/recipes/:id", get(handle_get_recipe))
        .route("/recipes/:id/vote", post(handle_vote))
        .route("/recipes/:id/report", post(handle_report_recipe))
        .route("/recipes/quarantined/:id", delete(handle_quarantine_remove))
        .route("/recipes/quarantined/:id/reinstate", post(handle_quarantine_reinstate))
        .route("/annotations/publish", post(handle_publish_annotation))
        .route("/annotations/fetch", get(handle_fetch_annotations))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{init_schema, TrackerState};
    use crate::auth::{AuthState, TrackerAuthMode};
    use axum::body::Body;
    use axum::http::{Request, StatusCode as AxumStatusCode};
    use tower::ServiceExt;

    fn test_state_with_recipes() -> Arc<TrackerState> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_recipe_tables(&conn);

        Arc::new(TrackerState {
            db: Arc::new(tokio::sync::Mutex::new(conn)),
            auth: Arc::new(AuthState {
                auth_mode: TrackerAuthMode::Open,
                valid_keys: vec![],
                jwt_config: None,
            }),
        })
    }

    fn recipe_app(state: Arc<TrackerState>) -> Router {
        recipe_router().with_state(state)
    }

    fn sample_publish(id: &str, name: &str) -> PublishRecipeRequest {
        PublishRecipeRequest {
            id: id.to_string(),
            name: name.to_string(),
            description: Some("A test recipe".into()),
            author_key_id: "author_key_123".into(),
            operator_key_id: Some("operator_key_456".into()),
            recipe_toml: "[package]\nname = \"test\"".into(),
            author_sig: "base64sig".into(),
            operator_sig: Some("base64opsig".into()),
            source_type: "github_release".into(),
            source_url: "https://github.com/example/tool".into(),
            architectures: vec!["linux/x86_64/glibc".into()],
            install_dir: Some("/usr/local/bin".into()),
            config_tracked: false,
            version: Some("1.0.0".into()),
        }
    }

    #[tokio::test]
    async fn test_publish_and_get() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        let body = serde_json::to_string(&sample_publish("hash1", "sonarr")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Get it back
        let app = recipe_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/recipes/hash1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        if status != AxumStatusCode::OK {
            eprintln!("GET /recipes/hash1 returned {}: {}", status, String::from_utf8_lossy(&body_bytes));
        }
        assert_eq!(status, AxumStatusCode::OK, "GET should return 200");

        let detail: RecipeDetail = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(detail.name, "sonarr");
        assert_eq!(detail.author_key_id, "author_key_123");
    }

    #[tokio::test]
    async fn test_search() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish two recipes
        for (id, name) in [("hash_a", "sonarr"), ("hash_b", "radarr")] {
            let body = serde_json::to_string(&sample_publish(id, name)).unwrap();
            let req = Request::builder()
                .method("POST")
                .uri("/recipes/publish")
                .header("Content-Type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), AxumStatusCode::OK);
        }

        // Search for "arr"
        let app = recipe_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/recipes/search?q=arr&limit=10")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let results: Vec<RecipeSearchResult> = serde_json::from_slice(&body).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_publish_unsigned_rejected() {
        let state = test_state_with_recipes();
        let app = recipe_app(state);

        let mut req_body = sample_publish("hash_x", "test");
        req_body.author_sig = String::new();
        let body = serde_json::to_string(&req_body).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_duplicate_publish_idempotent() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        let body = serde_json::to_string(&sample_publish("hash_dup", "test")).unwrap();
        for _ in 0..2 {
            let req = Request::builder()
                .method("POST")
                .uri("/recipes/publish")
                .header("Content-Type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), AxumStatusCode::OK);
        }

        // Should have exactly 1 recipe
        let db = state.db.lock().await;
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM shadow_recipes WHERE id = 'hash_dup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_vote_up_down() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish first
        let body = serde_json::to_string(&sample_publish("hash_v", "voter-test")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // Upvote
        let vote = VoteRequest {
            direction: "up".into(),
            voter_key_id: "voter1".into(),
            install_count: Some(5),
        };
        let body = serde_json::to_string(&vote).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/hash_v/vote")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Check score
        let db = state.db.lock().await;
        let score = recipe_score(&db, "hash_v");
        assert!(score > 0.0, "score after upvote should be positive, got {}", score);

        // Downvote (replaces upvote)
        drop(db);
        let vote = VoteRequest {
            direction: "down".into(),
            voter_key_id: "voter1".into(),
            install_count: Some(5),
        };
        let body = serde_json::to_string(&vote).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/hash_v/vote")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        let db = state.db.lock().await;
        let score = recipe_score(&db, "hash_v");
        assert!(score < 0.0, "score after downvote should be negative, got {}", score);
    }

    #[tokio::test]
    async fn test_vote_weight_increases_with_installs() {
        assert!(vote_weight(0) < vote_weight(5));
        assert!(vote_weight(5) < vote_weight(30));
        assert!(vote_weight(30) < vote_weight(100));
        // Cap at ~2.0
        assert!(vote_weight(1000) <= 2.0);
    }

    #[tokio::test]
    async fn test_report_and_quarantine() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish
        let body = serde_json::to_string(&sample_publish("hash_r", "report-test")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // Report 3 times from different reporters
        for i in 0..3 {
            let report = ReportRecipeRequest {
                reporter_key_id: format!("reporter_{}", i),
                reason: "suspicious".into(),
                reporter_sig: format!("sig_{}", i),
            };
            let body = serde_json::to_string(&report).unwrap();
            let req = Request::builder()
                .method("POST")
                .uri("/recipes/hash_r/report")
                .header("Content-Type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), AxumStatusCode::OK);
        }

        // Verify quarantined
        let db = state.db.lock().await;
        assert!(is_quarantined(&db, "hash_r"));
    }

    #[tokio::test]
    async fn test_quarantined_hidden_from_search() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish and quarantine
        let body = serde_json::to_string(&sample_publish("hash_q", "quarantine-test")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // Manually quarantine
        let db = state.db.lock().await;
        db.execute(
            "INSERT INTO recipe_quarantine (recipe_id, reason, quarantined_at, report_count) VALUES (?1, 'test', strftime('%s','now'), 3)",
            params!["hash_q"],
        ).unwrap();
        drop(db);

        // Search should not return it
        let app = recipe_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/recipes/search?q=quarantine&limit=10")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let results: Vec<RecipeSearchResult> = serde_json::from_slice(&body).unwrap();
        assert!(results.is_empty(), "quarantined recipe should be hidden from search");
    }

    #[tokio::test]
    async fn test_list_quarantined() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish and quarantine
        let body = serde_json::to_string(&sample_publish("hash_lq", "list-q-test")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        let db = state.db.lock().await;
        db.execute(
            "INSERT INTO recipe_quarantine (recipe_id, reason, quarantined_at, report_count) VALUES (?1, 'test quarantine', strftime('%s','now'), 3)",
            params!["hash_lq"],
        ).unwrap();
        drop(db);

        let app = recipe_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/recipes/quarantined")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let entries: Vec<QuarantineEntry> = serde_json::from_slice(&body).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].recipe_id, "hash_lq");
    }

    #[test]
    fn test_vote_weight_formula() {
        // 0 installs: 1.0 + log2(1)/5 = 1.0
        let w0 = vote_weight(0);
        assert!((w0 - 1.0).abs() < 0.001, "0 installs: {}", w0);

        // 1 install: 1.0 + log2(2)/5 = 1.0 + 0.2 = 1.2
        let w1 = vote_weight(1);
        assert!((w1 - 1.2).abs() < 0.01, "1 install: {}", w1);

        // 5 installs: 1.0 + log2(6)/5 = 1.0 + 0.517 = 1.517
        let w5 = vote_weight(5);
        assert!((w5 - 1.517).abs() < 0.01, "5 installs: {}", w5);

        // 30 installs: 1.0 + log2(31)/5 = 1.0 + 0.991 = 1.991
        let w30 = vote_weight(30);
        assert!((w30 - 1.99).abs() < 0.02, "30 installs: {}", w30);

        // 1000 installs: capped at 2.0
        let w1000 = vote_weight(1000);
        assert!(w1000 <= 2.0, "1000 installs should be capped: {}", w1000);
    }

    #[tokio::test]
    async fn test_publish_and_fetch_annotation() {
        let state = test_state_with_recipes();

        // Publish first
        let app = recipe_app(state.clone());
        let ann = PublishAnnotationRequest {
            package_name: "sonarr".to_string(),
            from_version: "4.0.6".to_string(),
            to_version: "4.0.7".to_string(),
            severity: "warning".to_string(),
            message: "Database migration".to_string(),
            url: Some("https://example.com".to_string()),
            author_key_id: "op123".to_string(),
            signature: "base64sig".to_string(),
            source_server: None,
        };

        let req = Request::builder()
            .method("POST")
            .uri("/annotations/publish")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&ann).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Fetch from same state
        let app2 = recipe_app(state);
        let req_fetch = Request::builder()
            .method("GET")
            .uri("/annotations/fetch?package=sonarr")
            .body(Body::empty())
            .unwrap();
        let resp = app2.oneshot(req_fetch).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let anns: Vec<AnnotationResponse> = serde_json::from_slice(&body).unwrap();
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].package_name, "sonarr");
        assert_eq!(anns[0].severity, "warning");
    }

    #[tokio::test]
    async fn test_annotation_requires_signature() {
        let state = test_state_with_recipes();
        let app = recipe_app(state);

        let ann = PublishAnnotationRequest {
            package_name: "mytool".to_string(),
            from_version: "1.0".to_string(),
            to_version: "2.0".to_string(),
            severity: "info".to_string(),
            message: "minor".to_string(),
            url: None,
            author_key_id: "op1".to_string(),
            signature: "".to_string(),
            source_server: None,
        };

        let req = Request::builder()
            .method("POST")
            .uri("/annotations/publish")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&ann).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_reinstate_quarantined_recipe() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish
        let body = serde_json::to_string(&sample_publish("hash_rein", "reinstate-test")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // Manually quarantine
        let db = state.db.lock().await;
        db.execute(
            "INSERT INTO recipe_quarantine (recipe_id, reason, quarantined_at, report_count) VALUES (?1, 'test', strftime('%s','now'), 3)",
            params!["hash_rein"],
        ).unwrap();
        drop(db);

        // Reinstate
        let app2 = recipe_app(state.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/quarantined/hash_rein/reinstate")
            .body(Body::empty())
            .unwrap();
        let resp = app2.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Verify no longer quarantined
        let db = state.db.lock().await;
        assert!(!is_quarantined(&db, "hash_rein"));
    }

    #[tokio::test]
    async fn test_remove_quarantined_recipe() {
        let state = test_state_with_recipes();
        let app = recipe_app(state.clone());

        // Publish
        let body = serde_json::to_string(&sample_publish("hash_rem", "remove-test")).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/recipes/publish")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // Manually quarantine
        let db = state.db.lock().await;
        db.execute(
            "INSERT INTO recipe_quarantine (recipe_id, reason, quarantined_at, report_count) VALUES (?1, 'test', strftime('%s','now'), 3)",
            params!["hash_rem"],
        ).unwrap();
        drop(db);

        // Permanently remove
        let app2 = recipe_app(state.clone());
        let req = Request::builder()
            .method("DELETE")
            .uri("/recipes/quarantined/hash_rem")
            .body(Body::empty())
            .unwrap();
        let resp = app2.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::OK);

        // Verify recipe gone completely
        let db = state.db.lock().await;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM shadow_recipes WHERE id = 'hash_rem'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "recipe should be permanently removed");
        assert!(!is_quarantined(&db, "hash_rem"));
    }

    #[tokio::test]
    async fn test_reinstate_nonexistent_returns_not_found() {
        let state = test_state_with_recipes();
        let app = recipe_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/recipes/quarantined/nonexistent/reinstate")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatusCode::NOT_FOUND);
    }
}
