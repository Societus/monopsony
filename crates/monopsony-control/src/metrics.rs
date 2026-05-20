//! Metrics server implementation for Prometheus scraping
//!
//! Uses metrics-exporter-prometheus for proper Prometheus metric exposition.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{extract::State, Json};
use tracing::info;

use monopsony_db::Database;

/// Metrics state shared across the server
#[derive(Clone)]
pub struct MetricsState {
    pub db: Option<Arc<Database>>,
    pub cache_dir: PathBuf,
    pub start_time: std::time::Instant,
}

impl MetricsState {
    pub fn new(db: Option<Arc<Database>>, cache_dir: PathBuf) -> Self {
        Self {
            db,
            cache_dir,
            start_time: std::time::Instant::now(),
        }
    }
}

/// Health check endpoint
#[derive(serde::Serialize)]
struct HealthStatus {
    status: String,
    database: bool,
    uptime_seconds: u64,
}

/// Statistics endpoint
#[derive(serde::Serialize)]
struct ServerStats {
    cache_size_bytes: u64,
    cache_file_count: u64,
    client_count: i64,
    uptime_seconds: u64,
}

/// Health check handler
async fn get_health(State(state): State<MetricsState>) -> Json<HealthStatus> {
    let db_healthy = if let Some(db) = &state.db {
        matches!(db.ping().await, Ok(true))
    } else {
        true // No DB configured = healthy
    };

    let status = if db_healthy { "healthy" } else { "unhealthy" };

    Json(HealthStatus {
        status: status.to_string(),
        database: db_healthy,
        uptime_seconds: std::time::Instant::now()
            .duration_since(state.start_time)
            .as_secs(),
    })
}

/// Statistics handler
async fn get_stats(State(state): State<MetricsState>) -> Json<ServerStats> {
    let mut cache_size_bytes = 0u64;
    let mut cache_file_count = 0u64;

    // Calculate cache size and file count
    if state.cache_dir.exists() {
        if let Ok(mut entries) = tokio::fs::read_dir(&state.cache_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                    if let Ok(metadata) = entry.metadata().await {
                        cache_size_bytes += metadata.len();
                        cache_file_count += 1;
                    }
                }
            }
        }
    }

    // Get client count from database
    let client_count = if let Some(db) = &state.db {
        db.count_clients().await.unwrap_or(0)
    } else {
        0
    };

    Json(ServerStats {
        cache_size_bytes,
        cache_file_count,
        client_count,
        uptime_seconds: std::time::Instant::now()
            .duration_since(state.start_time)
            .as_secs(),
    })
}

/// Install the global Prometheus exporter and return the handle.
/// Call `handle.render()` to get the Prometheus text format.
pub fn install_prometheus_exporter() -> PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder")
}

/// Handle to the Prometheus exporter for rendering metrics
pub type PrometheusHandle = metrics_exporter_prometheus::PrometheusHandle;

/// Prometheus metrics endpoint — serves the real metrics-exporter-prometheus output
async fn get_metrics(State(_state): State<MetricsState>, handle: axum::extract::Extension<PrometheusHandle>) -> String {
    handle.render()
}

/// Start the metrics HTTP server
pub async fn run_metrics_server(
    addr: SocketAddr,
    db: Option<Arc<Database>>,
    cache_dir: PathBuf,
) -> anyhow::Result<()> {
    let state = MetricsState::new(db, cache_dir);
    let prometheus_handle = install_prometheus_exporter();

    let app = axum::Router::new()
        .route("/health", axum::routing::get(get_health))
        .route("/stats", axum::routing::get(get_stats))
        .route("/metrics", axum::routing::get(get_metrics))
        .with_state(state)
        .layer(axum::Extension(prometheus_handle));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Metrics server listening on {}", addr);

    axum::serve(
        listener,
        app.into_make_service(),
    )
    .await?;

    Ok(())
}
