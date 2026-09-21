//! Health and status endpoints.

use axum::{extract::State, response::IntoResponse, Json};
use serde::Serialize;
use std::collections::HashMap;

use crate::AdminState;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub services: HashMap<String, ServiceHealth>,
    pub uptime_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct ServiceHealth {
    pub healthy: bool,
    pub status: String,
    pub message: Option<String>,
}

/// GET /api/v1/health — aggregate health of all subsystems.
pub async fn handle_health(State(state): State<AdminState>) -> impl IntoResponse {
    let mut services = HashMap::new();

    // Database
    let db_healthy = if let Some(ref db) = state.db {
        match db.ping().await {
            Ok(true) => true,
            _ => false,
        }
    } else {
        false
    };
    services.insert(
        "database".to_string(),
        ServiceHealth {
            healthy: db_healthy || state.db.is_none(),
            status: if state.db.is_some() {
                if db_healthy {
                    "connected".to_string()
                } else {
                    "unreachable".to_string()
                }
            } else {
                "disabled".to_string()
            },
            message: None,
        },
    );

    // Shadow manager
    services.insert(
        "shadow".to_string(),
        ServiceHealth {
            healthy: true,
            status: if state.shadow_manager.is_some() {
                "active".into()
            } else {
                "disabled".into()
            },
            message: None,
        },
    );

    // Mirror manager
    services.insert(
        "mirror".to_string(),
        ServiceHealth {
            healthy: true,
            status: if state.mirror_manager.is_some() {
                "active".into()
            } else {
                "disabled".into()
            },
            message: None,
        },
    );

    // TLS
    services.insert(
        "tls".to_string(),
        ServiceHealth {
            healthy: true,
            status: "active".into(),
            message: Some(state.tls_manager.ca_fingerprint()),
        },
    );

    let all_healthy = services.values().all(|s| s.healthy);

    Json(HealthResponse {
        status: if all_healthy {
            "healthy".into()
        } else {
            "degraded".into()
        },
        services,
        uptime_seconds: 0,
    })
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub cache_dir: String,
    pub cache_exists: bool,
    pub total_clients: i64,
    pub total_packages: i64,
    pub shadow_package_count: usize,
    /// MAX(resolved_at) from package_map — freshest oracle resolution (unix ms).
    pub oracle_last_update: Option<i64>,
}

/// GET /api/v1/stats — quick statistics for the dashboard overview.
pub async fn handle_stats(State(state): State<AdminState>) -> impl IntoResponse {
    let cache_exists = state.cache_dir.exists();

    let total_clients = if let Some(ref db) = state.db {
        db.count_clients().await.unwrap_or(0)
    } else {
        0
    };

    let total_packages = if let Some(ref db) = state.db {
        db.list_packages()
            .await
            .map(|p| p.len() as i64)
            .unwrap_or(0)
    } else {
        0
    };

    Json(StatsResponse {
        cache_dir: state.cache_dir.display().to_string(),
        cache_exists,
        total_clients,
        total_packages,
        shadow_package_count: 0,
        oracle_last_update: *state
            .oracle_last_update
            .read()
            .unwrap_or_else(|e| e.into_inner()),
    })
}
