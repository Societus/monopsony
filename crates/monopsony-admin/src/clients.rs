//! Client inventory endpoints.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use monopsony_common::os_release::{default_catalog, merge_release_rows};
use monopsony_common::ClientId;
use tracing::info;

use crate::guided;
use crate::AdminState;
use monopsony_common::os_release::OsReleaseUpgradeRow;

/// server_settings key holding the curated release catalog (grounding D2).
/// JSONB shape: `{ "debian": ["13"], "ubuntu": ["24.04"], ... }`.
const OS_RELEASE_CATALOG_KEY: &str = "os_release_catalog";

/// Load the release catalog: the operator-set server_settings value when
/// present, else the code-default seed. The default is never written back —
/// an operator edit must not be clobbered at boot.
async fn load_release_catalog(db: &monopsony_db::Database) -> serde_json::Value {
    match db.get_setting(OS_RELEASE_CATALOG_KEY).await {
        Ok(Some(v)) => v,
        _ => default_catalog(),
    }
}

/// Merged release rows + verdict notes for one client: the server-curated
/// catalog rows stamped with the client's persisted Phase 3 verdict. Shared
/// by the list/detail JSON and the release-guide endpoint so the guide always
/// matches the rows the dashboard shows.
fn merged_release_rows(
    client: &monopsony_common::models::Client,
    catalog: &serde_json::Value,
) -> (Vec<OsReleaseUpgradeRow>, Vec<String>) {
    let (Some(distro), Some(distro_version)) = (
        client.reported_distro.as_ref(),
        client.reported_distro_version.as_ref(),
    ) else {
        return (Vec::new(), Vec::new());
    };

    // Server-curated merge (grounding D2): unknown majors never adopt.
    let mut releases = merge_release_rows(catalog, distro, distro_version);

    // Sprint 29 Phase 3 (grounding D5): stamp the client-reported vendor
    // preflight verdict onto its release rows. All rows carry this client's
    // distro, so the host-level verdict applies to each.
    let notes = client.update_readiness_notes.clone().unwrap_or_default();
    if let Some(ready) = client.update_ready {
        for row in &mut releases {
            row.ready = ready;
            row.readiness_notes = notes.clone();
        }
    }
    (releases, notes)
}

/// Per-client updates + release-readiness JSON for the REST surface, or
/// `null` when the client has never reported an UpdateReport (absent data
/// renders nothing on the dashboard).
fn client_updates_json(
    client: &monopsony_common::models::Client,
    catalog: &serde_json::Value,
) -> serde_json::Value {
    let (Some(distro), Some(distro_version)) = (
        client.reported_distro.as_ref(),
        client.reported_distro_version.as_ref(),
    ) else {
        return serde_json::Value::Null;
    };

    let (releases, notes) = merged_release_rows(client, catalog);

    // Sprint 29 Phase 4 (grounding D6): PVE hosts report os-release
    // ID=debian — the verdict notes lift the presentation to Proxmox VE.
    let distro_label = guided::classify_distro_label(distro, &notes);

    serde_json::json!({
        "security": client.update_security,
        "other": client.update_other,
        "total": client.update_total,
        "reported_at": client.update_reported_at,
        "distro": distro,
        "distro_version": distro_version,
        "distro_label": distro_label,
        "releases": releases,
    })
}

/// GET /api/v1/clients — list all registered clients.
pub async fn handle_list_clients(State(state): State<AdminState>) -> impl IntoResponse {
    if let Some(ref db) = state.db {
        match db.list_clients().await {
            Ok(clients) => {
                let catalog = load_release_catalog(db).await;
                let result: Vec<serde_json::Value> = clients
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id.0,
                            "hostname": c.hostname,
                            "os": c.os,
                            "registered_at": c.registered_at,
                            "last_seen": c.last_seen,
                            "updates": client_updates_json(c, &catalog),
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
                Ok(Some(client)) => {
                    let catalog = load_release_catalog(db).await;
                    Json(serde_json::json!({
                        "id": client.id.0,
                        "hostname": client.hostname,
                        "os": client.os,
                        "os_version": client.os_version,
                        "registered_at": client.registered_at,
                        "last_seen": client.last_seen,
                        "updates": client_updates_json(&client, &catalog),
                    }))
                }
                Ok(None) => Json(serde_json::json!({"error": "Client not found"})),
                Err(e) => Json(serde_json::json!({"error": e.to_string()})),
            },
            Err(e) => Json(serde_json::json!({"error": format!("Invalid client ID: {}", e)})),
        }
    } else {
        Json(serde_json::json!({"error": "Database not available"}))
    }
}

/// GET /api/v1/clients/:id/release-guide — Sprint 29 Phase 4 (grounding D6):
/// the rendered, DISPLAY-ONLY guided upgrade checklist for this client's
/// selected release. The server renders text from authored recipe data +
/// the client's persisted verdict; nothing here executes anything.
/// `ready = false` clients get their verdict notes back so the dashboard can
/// show the not-ready badge instead of the guide.
pub async fn handle_client_release_guide(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    let client_id = match monopsony_common::ClientId::parse(&id) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid client ID: {}", e)})),
            )
                .into_response();
        }
    };

    let client = match db.get_client(&client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Client not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let Some(distro) = client.reported_distro.clone() else {
        return (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({"guide": null, "reason": "client has not reported a distro"})),
        )
            .into_response();
    };

    let catalog = load_release_catalog(db).await;
    let (releases, notes) = merged_release_rows(&client, &catalog);
    let distro_label = guided::classify_distro_label(&distro, &notes);

    // No server-curated target (unknown major, already current, absent
    // verdict): nothing to guide — the dashboard shows the plain row.
    let Some(selected) = releases.first().cloned() else {
        return (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "guide": null,
                "reason": "no available release for this client",
                "distro_label": distro_label,
                "ready": false,
                "readiness_notes": notes,
            })),
        )
            .into_response();
    };

    let Some((recipe_id, recipe_toml)) = guided::recipe_for(&distro, &notes) else {
        return (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "guide": null,
                "reason": "no authored guide for this distro",
                "distro_label": distro_label,
                "ready": selected.ready,
                "readiness_notes": notes,
            })),
        )
            .into_response();
    };

    let inputs = guided::RenderInputs {
        from_version: client.reported_distro_version.clone(),
        to_version: Some(selected.to_version.clone()),
    };
    match guided::render(recipe_toml, &inputs) {
        Ok(mut guide) => {
            guide.distro_label = distro_label.clone();
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "recipe_id": recipe_id,
                    "guide": guide,
                    "ready": selected.ready,
                    "readiness_notes": notes,
                    "distro_label": distro_label,
                })),
            )
                .into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
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
