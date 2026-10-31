//! Client inventory endpoints.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use monopsony_common::models::ClientCert;
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
                            "cert_serial": c.cert_serial,
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
                        "cert_serial": client.cert_serial,
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

/// Days before expiry at which a still-valid certificate starts showing the
/// "expiring" badge (P4: valid / expiring / revoked). Certificates are issued
/// with a 90-day lifetime, so the last month is the renewal warning window.
pub const EXPIRING_WINDOW_DAYS: i64 = 30;

/// Derived dashboard status for one certificate. Precedence: revoked >
/// expired > expiring > valid — revocation must win over age so an
/// already-revoked cert never presents as merely "expired".
fn cert_badge(status: &str, expires_at: DateTime<Utc>, now: DateTime<Utc>) -> &'static str {
    if status == "revoked" {
        "revoked"
    } else if expires_at <= now {
        "expired"
    } else if expires_at - now <= chrono::Duration::days(EXPIRING_WINDOW_DAYS) {
        "expiring"
    } else {
        "valid"
    }
}

/// JSON projection for one client certificate, shared by the per-client list
/// and the fleet-wide revocation audit view. `status` is the raw DB status
/// ("active" | "revoked" | "expired"); `badge` is the derived dashboard state.
fn cert_json(cert: &ClientCert, now: DateTime<Utc>) -> serde_json::Value {
    let days_to_expiry = if cert.expires_at > now {
        Some((cert.expires_at - now).num_days())
    } else {
        None
    };
    serde_json::json!({
        "serial": cert.serial,
        "client_id": cert.client_id.0,
        "status": cert.status,
        "badge": cert_badge(&cert.status, cert.expires_at, now),
        "issued_at": cert.issued_at,
        "expires_at": cert.expires_at,
        "revoked_at": cert.revoked_at,
        "days_to_expiry": days_to_expiry,
    })
}

/// GET /api/v1/clients/:id/certs — list the client's certificates (serial,
/// status, issued/expires/revoked timestamps) so the dashboard can render
/// valid/expiring/revoked badges and per-cert revoke actions without reading
/// the `client_certs` table by hand (P4: "Client list: cert status").
pub async fn handle_list_client_certs(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let cid = match ClientId::parse(&id) {
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

    // 404 on an unknown client so a typo'd ID doesn't read as "no certs".
    match db.get_client(&cid).await {
        Ok(Some(_)) => {}
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
    }

    match db.list_client_certs_for_client(&cid).await {
        Ok(certs) => {
            let now = Utc::now();
            let list: Vec<serde_json::Value> = certs.iter().map(|c| cert_json(c, now)).collect();
            let count = list.len();
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"certs": list, "count": count})),
            )
                .into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/v1/certs/revoked — fleet-wide revocation audit view (P4): every
/// revoked certificate with its client and revocation time, newest first.
pub async fn handle_list_revoked_certs(State(state): State<AdminState>) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    match db.list_revoked_client_certs().await {
        Ok(certs) => {
            let now = Utc::now();
            let list: Vec<serde_json::Value> = certs.iter().map(|c| cert_json(c, now)).collect();
            Json(serde_json::Value::Array(list)).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn cert(expires_in: chrono::Duration, status: &str) -> ClientCert {
        let now = Utc::now();
        ClientCert {
            serial: "1a2b3c4d".to_string(),
            client_id: ClientId(Uuid::new_v4()),
            issued_at: now - chrono::Duration::days(60),
            expires_at: now + expires_in,
            status: status.to_string(),
            revoked_at: (status == "revoked").then_some(now),
        }
    }

    #[test]
    fn test_badge_valid_beyond_window() {
        let c = cert(chrono::Duration::days(31), "active");
        assert_eq!(cert_badge(&c.status, c.expires_at, Utc::now()), "valid");
    }

    #[test]
    fn test_badge_expiring_at_window_edge() {
        // Exactly EXPIRING_WINDOW_DAYS left counts as expiring (inclusive).
        let now = Utc::now();
        let expires_at = now + chrono::Duration::days(EXPIRING_WINDOW_DAYS);
        assert_eq!(cert_badge("active", expires_at, now), "expiring");
    }

    #[test]
    fn test_badge_expiring_inside_window() {
        let c = cert(chrono::Duration::days(5), "active");
        assert_eq!(cert_badge(&c.status, c.expires_at, Utc::now()), "expiring");
    }

    #[test]
    fn test_badge_expired_past_expiry() {
        let c = cert(chrono::Duration::hours(-1), "active");
        assert_eq!(cert_badge(&c.status, c.expires_at, Utc::now()), "expired");
    }

    #[test]
    fn test_badge_revoked_wins_over_expired() {
        // A revoked cert that has also aged past expiry must show revoked,
        // not expired — the operator action (revocation) is the salient fact.
        let c = cert(chrono::Duration::days(-3), "revoked");
        assert_eq!(cert_badge(&c.status, c.expires_at, Utc::now()), "revoked");
    }

    #[test]
    fn test_badge_revoked_while_still_valid() {
        let c = cert(chrono::Duration::days(45), "revoked");
        assert_eq!(cert_badge(&c.status, c.expires_at, Utc::now()), "revoked");
    }

    #[test]
    fn test_cert_json_shape() {
        // Deterministic: one `now`, certs built relative to it exactly.
        let now = Utc::now();
        let mk = |expires_at: DateTime<Utc>, status: &str| ClientCert {
            serial: "feed0001".to_string(),
            client_id: ClientId(Uuid::new_v4()),
            issued_at: now - chrono::Duration::days(60),
            expires_at,
            status: status.to_string(),
            revoked_at: (status == "revoked").then_some(now),
        };

        let active = mk(now + chrono::Duration::days(45), "active");
        let j = cert_json(&active, now);
        assert_eq!(j["serial"], "feed0001");
        assert_eq!(j["status"], "active");
        assert_eq!(j["badge"], "valid");
        assert_eq!(j["days_to_expiry"], 45);
        assert!(j["issued_at"].is_string());
        assert!(j["expires_at"].is_string());
        assert!(j["revoked_at"].is_null());

        let expiring = mk(now + chrono::Duration::days(7), "active");
        let j = cert_json(&expiring, now);
        assert_eq!(j["badge"], "expiring");
        assert_eq!(j["days_to_expiry"], 7);

        let expired = mk(now - chrono::Duration::days(1), "active");
        let j = cert_json(&expired, now);
        assert_eq!(j["badge"], "expired");
        assert!(j["days_to_expiry"].is_null());

        let revoked = mk(now + chrono::Duration::days(45), "revoked");
        let j = cert_json(&revoked, now);
        assert_eq!(j["badge"], "revoked");
        assert!(j["revoked_at"].is_string());
    }
}
