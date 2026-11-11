//! Update-distribution endpoints (Sprint 34 P1) + rollout/self-apply
//! surfaces (Sprint 34 P4a).
//!
//! Read-only view over the `update_releases` / `update_artifacts` tables the
//! server's distributor maintains, an out-of-band "check now" trigger, the
//! rollout creator + ack-state view over `pending_update_assignments`, and
//! the guarded server self-apply (staged two-phase via the client crate's
//! self-update machinery — the admin crate consumes it as a library, the
//! same way the server binary's thin wrapper does).

use axum::{extract::State, response::IntoResponse, Json};
use monopsony_client::self_update;
use monopsony_control::grpc::contract_version_cmp;
use serde::Deserialize;
use serde_json::json;
use std::cmp::Ordering;
use tracing::{info, warn};

use crate::AdminState;

/// GET /api/v1/updates — stored releases (newest fetch first) with
/// per-release artifact completeness.
pub async fn handle_list_updates(State(state): State<AdminState>) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return Json(json!({"error": "Database not available"}));
    };
    let releases = match db.list_update_releases(20).await {
        Ok(releases) => releases,
        Err(e) => return Json(json!({"error": e.to_string()})),
    };
    let mut out = Vec::new();
    for release in &releases {
        let artifacts = db
            .list_update_artifacts(&release.id)
            .await
            .unwrap_or_default();
        // Per-binary-kind platform completeness (contract: 4 platforms each).
        let server_platforms: Vec<&str> = artifacts
            .iter()
            .filter(|a| a.package == "monopsony-server")
            .map(|a| a.platform.as_str())
            .collect();
        let client_platforms: Vec<&str> = artifacts
            .iter()
            .filter(|a| a.package == "monopsony-client")
            .map(|a| a.platform.as_str())
            .collect();
        out.push(json!({
            "version": release.version,
            "prerelease": release.prerelease,
            "fetched_at": release.fetched_at.to_rfc3339(),
            "changelog_url": release.changelog_url,
            "assets_complete": release.assets_complete,
            "server_platforms": server_platforms,
            "client_platforms": client_platforms,
            "artifact_count": artifacts.len(),
        }));
    }
    Json(json!({"releases": out}))
}

/// POST /api/v1/updates/check — trigger an out-of-band distributor poll.
/// The poller picks the signal up between interval ticks; results appear
/// under GET /api/v1/updates once fetched.
pub async fn handle_trigger_update_check(State(state): State<AdminState>) -> impl IntoResponse {
    match &state.update_check_signal {
        Some(signal) => {
            signal.notify_one();
            info!("update check triggered via admin API");
            Json(json!({"success": true, "message": "update check triggered"}))
        }
        None => Json(json!(
            {"success": false, "message": "update distributor not running (requires database)"}
        )),
    }
}

// ── Rollout + self-apply (Sprint 34 P4a) ───────────────────────────────────

/// One fleet row for guard evaluation (decoupled from the DB tuple so tests
/// need no database).
#[derive(Debug, Clone)]
struct FleetClientVersion {
    client_id: String,
    hostname: String,
    /// None = client has never reported a monopsony version (pre-P2 client
    /// or no heartbeat yet).
    version: Option<String>,
}

/// Classification of the fleet against a self-apply target (plan P4:
/// "warns while any active paired client reports a version < target").
#[derive(Debug, Default)]
struct SelfApplyGuard {
    /// Reported version below the target — blocks without override.
    /// Unparseable non-empty reports land here too: an unreadable version
    /// must not read as converged (fail-safe; override available).
    lagging: Vec<FleetClientVersion>,
    /// No reported version — listed for the operator, never blocking.
    unknown: Vec<FleetClientVersion>,
    /// Reported version >= target.
    converged: Vec<FleetClientVersion>,
}

impl SelfApplyGuard {
    fn blocked(&self) -> bool {
        !self.lagging.is_empty()
    }
}

/// Pure guard classifier (P4a): compares each client's REPORTED version
/// against the target with the same release-contract comparator the
/// distribution gate uses (`contract_version_cmp`), so "what counts as
/// converged" can never disagree with "what gets offered".
fn evaluate_self_apply_guard(target: &str, fleet: &[FleetClientVersion]) -> SelfApplyGuard {
    let mut guard = SelfApplyGuard::default();
    for client in fleet {
        match client.version.as_deref() {
            None | Some("") => guard.unknown.push(client.clone()),
            Some(reported) => match contract_version_cmp(reported, target) {
                Some(ord) if ord != Ordering::Less => guard.converged.push(client.clone()),
                _ => guard.lagging.push(client.clone()),
            },
        }
    }
    guard
}

fn fleet_client_json(client: &FleetClientVersion) -> serde_json::Value {
    json!({
        "client_id": client.client_id,
        "hostname": client.hostname,
        "monopsony_version": client.version.clone().unwrap_or_default(),
    })
}

/// POST /api/v1/updates/self-apply body.
#[derive(Debug, Deserialize)]
pub struct SelfApplyRequest {
    /// Target release version (must be a stored, complete release with a
    /// servable `monopsony-server` asset for this host's platform).
    pub version: String,
    /// Operator override for the fleet-convergence guard (plan P4: warn and
    /// refuse without an explicit override while any client lags). Accepts
    /// the plan's literal flag name (`override`) too.
    #[serde(default, alias = "override")]
    pub override_guard: bool,
}

/// POST /api/v1/updates/rollout body.
#[derive(Debug, Deserialize)]
pub struct RolloutRequest {
    /// Target release version (must be a stored COMPLETE release — clients
    /// can only download complete releases).
    pub version: String,
    /// Explicit target client ids; empty when `all` is true.
    #[serde(default)]
    pub client_ids: Vec<String>,
    /// Roll out to every registered client.
    #[serde(default)]
    pub all: bool,
}

/// POST /api/v1/updates/rollout — create (or reset) per-client assignment
/// rows for `version`. Re-approving an existing (client, version) pair
/// resets the row to 'pending' (upsert), so a re-rollout re-pushes to
/// clients that already resolved the previous round.
pub async fn handle_create_rollout(
    State(state): State<AdminState>,
    Json(req): Json<RolloutRequest>,
) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Database not available"})),
        )
            .into_response();
    };
    if req.version.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "version is required"})),
        )
            .into_response();
    }

    let release = match db.get_update_release(&req.version).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": format!("no stored release {}", req.version)})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    if !release.assets_complete {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!(
                {"error": format!("release {} is not complete — not distributable", req.version)}
            )),
        )
            .into_response();
    }

    // Resolve targets. `all` = every registered client row (presence of a
    // row IS paired/registered); otherwise the explicit id list.
    let mut targets: Vec<monopsony_common::models::ClientId> = Vec::new();
    if req.all {
        match db.list_clients().await {
            Ok(clients) => targets.extend(clients.into_iter().map(|c| c.id)),
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    } else {
        for id in &req.client_ids {
            match monopsony_common::ClientId::parse(id) {
                Ok(cid) => targets.push(cid),
                Err(e) => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("Invalid client ID {id}: {e}")})),
                    )
                        .into_response();
                }
            }
        }
    }
    if targets.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "no target clients (pass client_ids or all=true)"})),
        )
            .into_response();
    }

    for client_id in &targets {
        if let Err(e) = db.create_update_assignment(client_id.0, &req.version).await {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }
    info!(
        version = %req.version,
        assigned = targets.len(),
        "Rollout created via admin API"
    );
    (
        axum::http::StatusCode::OK,
        Json(json!({
            "version": req.version,
            "assigned": targets.len(),
            "client_ids": targets.iter().map(|c| c.0.to_string()).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

/// GET /api/v1/updates/rollouts — the rollout/ack state view: every
/// assignment row (with hostname resolved) plus the fleet's recorded
/// monopsony versions (client-page version column + skew badge input).
pub async fn handle_list_rollouts(State(state): State<AdminState>) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Database not available"})),
        )
            .into_response();
    };

    let assignments = match db.list_update_assignments(500).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let fleet = db
        .list_client_monopsony_versions()
        .await
        .unwrap_or_default();
    let hostname_of: std::collections::HashMap<String, String> = fleet
        .iter()
        .map(|(id, hostname, _)| (id.to_string(), hostname.clone()))
        .collect();

    let rollouts: Vec<serde_json::Value> = assignments
        .iter()
        .map(|a| {
            json!({
                "client_id": a.client_id.to_string(),
                "hostname": hostname_of.get(&a.client_id.to_string()).cloned().unwrap_or_default(),
                "version": a.version,
                "status": a.status,
                "created_at": a.created_at,
                "delivered_at": a.delivered_at,
                "resolved_at": a.resolved_at,
            })
        })
        .collect();
    let fleet_json: Vec<serde_json::Value> = fleet
        .iter()
        .map(|(id, hostname, version)| {
            json!({
                "client_id": id.to_string(),
                "hostname": hostname,
                "monopsony_version": version.clone().unwrap_or_default(),
            })
        })
        .collect();

    (
        axum::http::StatusCode::OK,
        Json(json!({
            // Same workspace version the server binary reports.
            "server_version": env!("CARGO_PKG_VERSION"),
            "assignments": rollouts,
            "fleet": fleet_json,
        })),
    )
        .into_response()
}

/// POST /api/v1/updates/self-apply — guarded staged self-update of THIS
/// server from its own LOCAL blob store (plan P4). Order semantics (D4):
/// distribute → clients converge → self-apply LAST; the guard enforces the
/// convergence precondition, the override flag releases it. The server
/// keeps serving while running N against stored N+1 blobs (serving is
/// data, not code) until the spawned finalize swaps + restarts it.
pub async fn handle_self_apply(
    State(state): State<AdminState>,
    Json(req): Json<SelfApplyRequest>,
) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Database not available"})),
        )
            .into_response();
    };
    let Some(ref shadow_manager) = state.shadow_manager else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Shadow manager not available (requires database)"})),
        )
            .into_response();
    };
    if req.version.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "version is required"})),
        )
            .into_response();
    }
    // The target must parse under the release-contract comparator — every
    // later comparison (guard, no-op detection) fails closed otherwise.
    if contract_version_cmp(&req.version, &req.version).is_none() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unparseable target version {:?}", req.version)})),
        )
            .into_response();
    }

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    // Already at/above the target (dev build ahead of the stored release):
    // idempotent no-op, not an error — a double-click must not restage.
    if contract_version_cmp(&current_version, &req.version).is_some_and(|ord| ord != Ordering::Less)
    {
        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "staged": false,
                "reason": "already_running_target_or_newer",
                "from_version": current_version,
                "to_version": req.version,
            })),
        )
            .into_response();
    }

    // Data gates: complete release + servable server asset for this platform.
    let release = match db.get_update_release(&req.version).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": format!("no stored release {}", req.version)})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    if !release.assets_complete {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!(
                {"error": format!("release {} is not complete — not distributable", req.version)}
            )),
        )
            .into_response();
    }
    let platform = match self_update::release_platform() {
        Some(p) => p,
        None => {
            return (
                axum::http::StatusCode::CONFLICT,
                Json(json!({"error": format!(
                    "no release asset contract for this platform ({}/{})",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )})),
            )
                .into_response();
        }
    };
    let artifact = match db.list_update_artifacts(&release.id).await {
        Ok(rows) => rows
            .into_iter()
            .find(|a| a.package == "monopsony-server" && a.platform == platform && a.blob_indexed),
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let Some(artifact) = artifact else {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({"error": format!(
                "no stored asset for monopsony-server {} on {}",
                req.version, platform
            )})),
        )
            .into_response();
    };

    // Confirm-guard (plan P4): refuse without explicit override while any
    // client still reports below the target. With override, proceed loudly.
    let fleet = db
        .list_client_monopsony_versions()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, hostname, version)| FleetClientVersion {
            client_id: id.to_string(),
            hostname,
            version,
        })
        .collect::<Vec<_>>();
    let guard = evaluate_self_apply_guard(&req.version, &fleet);
    if guard.blocked() {
        if !req.override_guard {
            warn!(
                target = %req.version,
                lagging = guard.lagging.len(),
                "Self-apply refused by the fleet-convergence guard (override not set)"
            );
            return (
                axum::http::StatusCode::CONFLICT,
                Json(json!({
                    "guard_triggered": true,
                    "override_required": true,
                    "error": "active clients still report versions below the target — pass override=true to self-apply anyway",
                    "target_version": req.version,
                    "lagging_clients": guard.lagging.iter().map(fleet_client_json).collect::<Vec<_>>(),
                    "unknown_clients": guard.unknown.iter().map(fleet_client_json).collect::<Vec<_>>(),
                    "converged_clients": guard.converged.iter().map(fleet_client_json).collect::<Vec<_>>(),
                })),
            )
                .into_response();
        }
        warn!(
            target = %req.version,
            lagging = guard.lagging.len(),
            "Self-apply proceeding with OPERATOR OVERRIDE of the fleet-convergence guard"
        );
    }

    // Local blob consumption: cache index (blob_indexed already proved the
    // index row) → bytes → sha256 re-verification against the DB lineage
    // digest (same defense in depth as the streaming serve path).
    let cache = shadow_manager.cache_ref();
    let entry = match cache
        .lookup_asset("monopsony-server", &req.version, &platform)
        .await
    {
        Some(entry) => entry,
        None => {
            return (
                axum::http::StatusCode::CONFLICT,
                Json(json!({"error": format!(
                    "asset monopsony-server/{} on {} missing from the cache index",
                    req.version, platform
                )})),
            )
                .into_response();
        }
    };
    let blob = match cache.read_blob(&entry.blob_hash).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("cache read error: {e}")})),
            )
                .into_response();
        }
    };
    let actual_sha = self_update_sha(&blob);
    if !actual_sha.eq_ignore_ascii_case(&artifact.sha256) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!(
                "stored update blob no longer matches its lineage digest — refusing to self-apply (expected {}, got {})",
                artifact.sha256, actual_sha
            )})),
        )
            .into_response();
    }

    // Stage + spawn the detached finalize (P3 machinery; the server binary
    // carries the matching hidden finalize/rollback CLI arms). The finalize
    // owns swap → restart via detected init → health gate → commit/rollback;
    // this process then exits so the old binary is not left running the
    // fleet's control plane during the swap.
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("failed to resolve current executable: {e}")})),
            )
                .into_response();
        }
    };
    let target_dir = match current_exe.parent() {
        Some(p) => p.to_path_buf(),
        None => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "current binary has no parent directory"})),
            )
                .into_response();
        }
    };
    let staged = match self_update::stage_update_asset(&blob, &target_dir, &current_exe) {
        Ok(pair) => pair,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("failed to stage update asset: {e:#}")})),
            )
                .into_response();
        }
    };
    let cmd = self_update::FinalizeCommand {
        staged_path: staged.0,
        staged_sha256: staged.1,
        from_version: current_version.clone(),
        to_version: req.version.clone(),
        binary: "server".to_string(),
        created_at: chrono::Utc::now().timestamp(),
    };
    let command_file = match self_update::write_finalize_command(&target_dir, &cmd) {
        Ok(f) => f,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("failed to write finalize command: {e:#}")})),
            )
                .into_response();
        }
    };
    if let Err(e) = self_update::spawn_detached_finalize(&current_exe, &command_file, &target_dir) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to spawn detached finalize: {e:#}")})),
        )
            .into_response();
    }

    info!(
        from = %current_version,
        to = %req.version,
        guard_overridden = guard.blocked(),
        "Self-apply staged; detached finalize spawned — daemon exiting shortly"
    );

    // Exit AFTER the response flushes (client daemon exits immediately on
    // the control stream; a REST response must land first). The finalize's
    // pre_update hook / restart machinery owns the service lifecycle from
    // here; without an init system the operator restarts manually.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        info!("MONOPSONY SELF-UPDATE: exiting for finalize (swap, restart, health gate)");
        std::process::exit(0);
    });

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "staged": true,
            "from_version": current_version,
            "to_version": req.version,
            "guard_overridden": guard.blocked(),
            "note": "staged; detached finalize will swap, restart and health-gate the server",
        })),
    )
        .into_response()
}

/// sha256 hex of a blob (thin wrapper so the test module can reuse it).
fn self_update_sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet(id: &str, hostname: &str, version: Option<&str>) -> FleetClientVersion {
        FleetClientVersion {
            client_id: id.to_string(),
            hostname: hostname.to_string(),
            version: version.map(str::to_string),
        }
    }

    #[test]
    fn guard_classifies_converged_lagging_unknown() {
        let fleet = [
            fleet("a", "converged-a", Some("1.1.0")),
            fleet("b", "lagging-b", Some("1.0.0")),
            fleet("c", "unknown-c", None),
            fleet("d", "silent-d", Some("")),
        ];
        let guard = evaluate_self_apply_guard("1.1.0", &fleet);
        assert_eq!(guard.converged.len(), 1);
        assert_eq!(guard.lagging.len(), 1);
        assert_eq!(guard.unknown.len(), 2);
        assert!(guard.blocked());
    }

    #[test]
    fn guard_prerelease_ordering_matches_release_contract() {
        // rc.2 >= rc.1 converges; rc.1 vs rc.2 lags; release beats prerelease.
        let guard = evaluate_self_apply_guard(
            "1.0.0-rc.2",
            &[
                fleet("a", "on-rc2", Some("1.0.0-rc.2")),
                fleet("b", "on-rc1", Some("1.0.0-rc.1")),
                fleet("c", "on-release", Some("1.0.0")),
            ],
        );
        assert_eq!(guard.converged.len(), 2);
        assert_eq!(guard.lagging.len(), 1);
        assert!(guard.blocked());
    }

    #[test]
    fn guard_treats_unparseable_report_as_lagging() {
        // A garbage version must never read as converged (fail-safe).
        let guard =
            evaluate_self_apply_guard("1.1.0", &[fleet("a", "garbage", Some("not-a-version"))]);
        assert_eq!(guard.lagging.len(), 1);
        assert!(guard.blocked());
    }

    #[test]
    fn guard_clear_fleet_is_not_blocked() {
        let guard = evaluate_self_apply_guard("1.1.0", &[fleet("a", "ok", Some("1.1.0"))]);
        assert!(!guard.blocked());
        // Empty fleet (no clients yet): nothing can lag.
        assert!(!evaluate_self_apply_guard("1.1.0", &[]).blocked());
    }

    #[test]
    fn self_apply_request_deserializes_override_alias() {
        let plain: SelfApplyRequest =
            serde_json::from_str(r#"{"version": "1.2.3"}"#).expect("plain body");
        assert_eq!(plain.version, "1.2.3");
        assert!(!plain.override_guard);
        let aliased: SelfApplyRequest =
            serde_json::from_str(r#"{"version": "1.2.3", "override": true}"#).expect("alias");
        assert!(aliased.override_guard);
        let explicit: SelfApplyRequest =
            serde_json::from_str(r#"{"version": "1.2.3", "override_guard": true}"#)
                .expect("explicit");
        assert!(explicit.override_guard);
    }

    #[test]
    fn rollout_request_defaults_match_contract() {
        let req: RolloutRequest = serde_json::from_str(r#"{"version": "1.2.3"}"#).expect("body");
        assert!(!req.all);
        assert!(req.client_ids.is_empty());
        let req: RolloutRequest =
            serde_json::from_str(r#"{"version": "1.2.3", "all": true}"#).expect("all body");
        assert!(req.all);
    }
}
