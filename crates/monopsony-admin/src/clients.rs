//! Client inventory endpoints.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use monopsony_common::ClientId;
use tracing::{info};

use crate::AdminState;

/// GET /api/v1/clients — list all registered clients.
pub async fn handle_list_clients(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.list_clients().await {
            Ok(clients) => {
                let result: Vec<serde_json::Value> = clients
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id.0,
                            "hostname": c.hostname,
                            "os": c.os,
                            "registered_at": c.registered_at,
                            "last_seen": c.last_seen,
                        })
                    })
                    .collect();
                Json(serde_json::Value::Array(result))
            }
            Err(e) => Json(serde_json::json!({"error": e.to_string()})),
        }
    } else {
        Json(serde_json::json!([]))
    }
}

/// GET /api/v1/clients/:id — get client details.
pub async fn handle_get_client(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match monopsony_common::ClientId::parse(&id) {
            Ok(client_id) => match db.get_client(&client_id).await {
                Ok(Some(client)) => Json(serde_json::json!({
                    "id": client.id.0,
                    "hostname": client.hostname,
                    "os": client.os,
                    "os_version": client.os_version,
                    "registered_at": client.registered_at,
                    "last_seen": client.last_seen,
                })),
                Ok(None) => Json(serde_json::json!({"error": "Client not found"})),
                Err(e) => Json(serde_json::json!({"error": e.to_string()})),
            },
            Err(e) => Json(serde_json::json!({"error": format!("Invalid client ID: {}", e)})),
        }
    } else {
        Json(serde_json::json!({"error": "Database not available"}))
    }
}

/// GET /api/v1/clients/:id/inventory — get client's installed packages.
pub async fn handle_client_inventory(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match monopsony_common::ClientId::parse(&id) {
            Ok(client_id) => {
                // get_packages_for_client returns Vec<Package> with name/version
                match db.get_packages_for_client(&client_id).await {
                    Ok(packages) => {
                        let result: Vec<serde_json::Value> = packages
                            .iter()
                            .map(|p| {
                                serde_json::json!({
                                    "name": p.name,
                                    "version": p.version,
                                })
                            })
                            .collect();
                        Json(serde_json::json!({"packages": result}))
                    }
                    Err(e) => Json(serde_json::json!({"error": e.to_string()})),
                }
            }
            Err(e) => Json(serde_json::json!({"error": format!("Invalid client ID: {}", e)})),
        }
    } else {
        Json(serde_json::json!({"error": "Database not available"}))
    }
}

/// POST /api/v1/clients/:id/certs/:serial/revoke — revoke a client
/// certificate by serial (red-team F3). Revokes the DB row and the in-memory
/// TLS accept-path cache so the cert is rejected immediately at TLS accept,
/// covering both direct gRPC and /ws/tunnel-relayed connections.
pub async fn handle_revoke_client_cert(
    State(state): State<AdminState>,
    Path((client_id, serial)): Path<(String, String)>,
) -> impl IntoResponse {
    let cid = match ClientId::parse(&client_id) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid client ID: {e}")})),
            )
                .into_response();
        }
    };

    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    // Only certificates issued to this client may be revoked through this endpoint.
    let certs = match db.list_client_certs_for_client(&cid).await {
        Ok(certs) => certs,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    if !certs.iter().any(|c| c.serial == serial) {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Certificate not found for this client"})),
        )
            .into_response();
    }

    match db.revoke_client_cert(&serial).await {
        Ok(true) => {
            state.tls_manager.note_revoked_serial(&serial);
            info!(serial = %serial, client = %cid, "Revoked client certificate");
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"revoked": true, "serial": serial})),
            )
                .into_response()
        }
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Certificate not found or already revoked"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
