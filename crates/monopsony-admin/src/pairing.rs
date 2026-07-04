//! Pairing endpoints — daemon-managed client pairing via admin API.
//!
//! The dashboard or CLI calls `POST /api/v1/pair/init` to start a pairing
//! session. The daemon spawns a short-lived TCP listener on port 50052 that
//! accepts one pairing connection, validates the code, registers the client
//! in the DB, generates an mTLS certificate, and sends the PairResponse.
//!
//! This replaces the old separate-process `monopsony-server pair` command
//! which was fragile when run over SSH (process lifecycle issues).

use std::net::SocketAddr;

use axum::{extract::State, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AdminState;

/// How long a pairing session stays open waiting for a client to connect.
const PAIRING_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Port for the plaintext pairing listener (same as the old CLI command).
const PAIRING_PORT: u16 = 50052;

/// In-memory state for a single active pairing session.
#[derive(Debug, Clone, Serialize)]
pub struct PairingSession {
    /// 6-character alphanumeric code (no ambiguous chars).
    pub code: String,
    /// Current status of the session.
    pub status: PairingStatus,
    /// When the session expires (Unix timestamp millis).
    pub deadline_ms: i64,
    /// Set after pairing completes — the client_id that was paired.
    pub paired_client_id: Option<String>,
    /// Set after pairing completes — the hostname that paired.
    pub paired_hostname: Option<String>,
    /// Set if pairing failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    /// Listener is active, waiting for a client.
    Waiting,
    /// Client connected and pairing completed.
    Completed,
    /// Client connected but something went wrong.
    Failed,
    /// Session expired or was cancelled.
    Expired,
}

// --- Request/Response types ---

#[derive(Debug, Deserialize)]
pub struct PairInitRequest {
    /// Optional: override the gRPC address sent to the client.
    /// Defaults to deriving from the listener's local address.
    #[serde(default)]
    pub grpc_addr_override: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PairInitResponse {
    pub code: String,
    pub deadline_ms: i64,
    /// CLI command for terminal-based pairing.
    pub cli_command: String,
}

#[derive(Debug, Serialize)]
pub struct PairStatusResponse {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub status: Option<PairingStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// --- API Handlers ---

/// `POST /api/v1/pair/init` — start a new pairing session.
///
/// Generates a 6-char code, binds port 50052, and spawns a background task
/// that waits for a single client connection. Only one session can be active
/// at a time; calling init again while a session is waiting returns 409.
pub async fn handle_pair_init(
    State(state): State<AdminState>,
    Json(req): Json<PairInitRequest>,
) -> impl IntoResponse {
    let mut session_lock = state.pairing_manager.lock().await;

    // Check if a session is already active
    if let Some(ref existing) = *session_lock {
        if existing.status == PairingStatus::Waiting {
            return (
                axum::http::StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "A pairing session is already active",
                    "code": existing.code,
                    "deadline_ms": existing.deadline_ms,
                })),
            );
        }
    }

    // Generate 6-char alphanumeric code (no ambiguous chars like 0/O/1/I)
    let charset: Vec<u8> = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".to_vec();
    let mut random_bytes = [0u8; 6];
    if let Err(e) = getrandom::getrandom(&mut random_bytes) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to generate code: {}", e)})),
        );
    }
    let pairing_code: String = random_bytes
        .iter()
        .map(|&b| charset[b as usize % charset.len()] as char)
        .collect();

    let deadline = chrono::Utc::now() + chrono::Duration::seconds(PAIRING_TIMEOUT_SECS as i64);
    let deadline_ms = deadline.timestamp_millis();

    let session = PairingSession {
        code: pairing_code.clone(),
        status: PairingStatus::Waiting,
        deadline_ms,
        paired_client_id: None,
        paired_hostname: None,
        error: None,
    };

    info!("Pairing session started: code={}, expires at {}", pairing_code, deadline);

    // Clone what the background task needs
    let tls_manager = state.tls_manager.clone();
    let db = state.db.clone();
    let mirror_manager = state.mirror_manager.clone();
    let grpc_addr_config = state.grpc_addr.clone();
    let grpc_addr_override = req.grpc_addr_override.clone();
    let code_for_task = pairing_code.clone();

    *session_lock = Some(session.clone());
    drop(session_lock);

    // Spawn the background pairing listener task
    let pairing_manager = state.pairing_manager.clone();
    let pairing_manager_for_error = pairing_manager.clone();
    let join_handle = tokio::spawn(async move {
        if let Err(e) = run_pairing_listener(code_for_task, tls_manager, db, mirror_manager, grpc_addr_config, grpc_addr_override, pairing_manager).await {
            let mut lock = pairing_manager_for_error.lock().await;
            if let Some(ref mut session) = *lock {
                if session.status == PairingStatus::Waiting {
                    session.status = PairingStatus::Failed;
                    session.error = Some(e.to_string());
                    warn!("Pairing listener failed: {}", e);
                }
            }
        }
    });

    // Store the JoinHandle so cancel can abort it
    {
        let mut task_lock = state.pairing_task.lock().await;
        *task_lock = Some(join_handle);
    }

    // Build CLI command hint for the dashboard
    let cli_command = format!(
        "monopsony-client pair --server <SERVER_IP>:{} --code {}",
        PAIRING_PORT, pairing_code
    );

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "code": pairing_code,
            "deadline_ms": deadline_ms,
            "cli_command": cli_command,
        })),
    )
}

/// `GET /api/v1/pair/status` — check active pairing session status.
pub async fn handle_pair_status(State(state): State<AdminState>) -> impl IntoResponse {
    let lock = state.pairing_manager.lock().await;

    match &*lock {
        Some(session) => {
            // Check if session has expired
            let now_ms = chrono::Utc::now().timestamp_millis();
            let is_expired = now_ms > session.deadline_ms && session.status == PairingStatus::Waiting;

            let status = if is_expired {
                PairingStatus::Expired
            } else {
                session.status.clone()
            };

            Json(PairStatusResponse {
                active: status == PairingStatus::Waiting,
                code: Some(session.code.clone()),
                status: Some(status),
                paired_client_id: session.paired_client_id.clone(),
                paired_hostname: session.paired_hostname.clone(),
                error: session.error.clone(),
            })
        }
        None => Json(PairStatusResponse {
            active: false,
            code: None,
            status: None,
            paired_client_id: None,
            paired_hostname: None,
            error: None,
        }),
    }
}

/// `POST /api/v1/pair/cancel` — cancel an active pairing session.
///
/// Marks the session Expired AND aborts the listener task to free port 50052.
pub async fn handle_pair_cancel(State(state): State<AdminState>) -> impl IntoResponse {
    let mut lock = state.pairing_manager.lock().await;
    match &mut *lock {
        Some(session) if session.status == PairingStatus::Waiting => {
            session.status = PairingStatus::Expired;
            session.error = Some("Cancelled by administrator".to_string());
            info!("Pairing session cancelled: code={}", session.code);
            drop(lock);

            // Abort the listener task to release port 50052
            let mut task_lock = state.pairing_task.lock().await;
            if let Some(handle) = task_lock.take() {
                handle.abort();
                info!("Pairing listener task aborted");
            }

            Json(serde_json::json!({"status": "cancelled"}))
        }
        _ => Json(serde_json::json!({"status": "no_active_session"})),
    }
}

// --- Background pairing listener ---

/// Bind port 50052 and accept a single pairing connection.
///
/// Uses `tokio::time::timeout` (not `select!` + `sleep_until`) to avoid the
/// instant-timeout bug that affected the old CLI pairing command.
async fn run_pairing_listener(
    pairing_code: String,
    tls_manager: std::sync::Arc<monopsony_control::tls::TlsManager>,
    db: Option<std::sync::Arc<monopsony_db::Database>>,
    mirror_manager: Option<std::sync::Arc<monopsony_mirror::MirrorManager>>,
    grpc_addr_config: String,
    grpc_addr_override: Option<String>,
    pairing_manager: std::sync::Arc<tokio::sync::Mutex<Option<PairingSession>>>,
) -> anyhow::Result<()> {
    use prost::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let pair_addr: SocketAddr = format!("0.0.0.0:{}", PAIRING_PORT).parse()?;
    let pair_listener = tokio::net::TcpListener::bind(pair_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind pairing port {}: {}", pair_addr, e))?;

    info!("Pairing listener on {} ({}s timeout)", pair_addr, PAIRING_TIMEOUT_SECS);

    // Use tokio::time::timeout — this is the fix for the instant-timeout bug.
    // The old code used tokio::select! with sleep_until, which resolved the
    // timeout branch immediately due to a runtime scheduling issue.
    let accept_result = tokio::time::timeout(
        std::time::Duration::from_secs(PAIRING_TIMEOUT_SECS),
        pair_listener.accept(),
    )
    .await;

    match accept_result {
        Ok(Ok((mut socket, addr))) => {
            info!("Pairing client connected from {}", addr);

            // Read length-prefixed PairRequest
            let mut len_buf = [0u8; 4];
            socket
                .read_exact(&mut len_buf)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read pairing request length: {}", e))?;
            let req_len = u32::from_le_bytes(len_buf) as usize;

            // Sanity-check the length to prevent allocation bombs
            if req_len > 65536 {
                anyhow::bail!("Pairing request too large: {} bytes", req_len);
            }

            let mut req_buf = vec![0u8; req_len];
            socket
                .read_exact(&mut req_buf)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read pairing request body: {}", e))?;

            let req = monopsony_common::proto::monopsony::PairRequest::decode(req_buf.as_slice())
                .map_err(|e| anyhow::anyhow!("Failed to decode pair request: {}", e))?;

            if req.code != pairing_code {
                // Send nothing — just close. The invalid code is logged.
                warn!("Invalid pairing code from {}", addr);
                update_pairing_session(
                    &pairing_manager,
                    PairingStatus::Failed,
                    None,
                    None,
                    Some("Invalid pairing code".to_string()),
                )
                .await;
                anyhow::bail!("Invalid pairing code from {}", addr);
            }

            info!(
                "Pairing code validated from {} ({} / {})",
                req.hostname, req.os, req.os_version
            );

            // Register client in DB first — creates the row and returns the
            // authoritative ClientId. This UUID is used for cert SAN,
            // record_pairing, and PairResponse so all three match.
            let cid = if let Some(db) = db.as_ref() {
                let registered = db
                    .register_client(
                        &req.hostname,
                        &req.os,
                        &req.os_version,
                        &req.package_managers,
                    )
                    .await?;
                db.record_pairing(&registered, chrono::Utc::now()).await?;
                info!("Client registered and paired in DB: {}", registered.0);

                // Auto-create mirror configs for the client's package managers
                if let Some(ref mirror_mgr) = mirror_manager {
                    let created = mirror_mgr.ensure_mirrors_for_package_managers(&req.package_managers).await;
                    if created > 0 {
                        info!(client = %req.hostname, created = created, "Auto-created mirror configs from pairing");
                    }
                }

                registered
            } else {
                warn!("No database available — generating UUID without DB registration");
                let fallback_id = uuid::Uuid::new_v4().simple().to_string();
                monopsony_common::ClientId::parse(&fallback_id)
                    .map_err(|e| anyhow::anyhow!("Invalid client ID: {}", e))?
            };

            let new_client_id = cid.0.to_string();

            // Generate certificate with SAN matching the DB client_id
            let (client_cert, client_key) = tls_manager.generate_client_cert(&new_client_id)?;
            let (client_cert_pem, client_key_pem) =
                monopsony_control::tls::TlsManager::cert_and_key_to_pem(&client_cert, &client_key);
            let ca_cert_pem = tls_manager.ca_cert_pem_bytes();

            // Derive the gRPC address to send to the client.
            // Prefer the override, then the socket's local address, then the config.
            let grpc_addr = grpc_addr_override.unwrap_or_else(|| {
                let local = socket.local_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| {
                    // Fall back to the configured bind address, replacing 0.0.0.0
                    grpc_addr_config
                        .split(':')
                        .next()
                        .filter(|ip| ip != &"0.0.0.0")
                        .unwrap_or("127.0.0.1")
                        .to_string()
                });
                // Extract port from grpc_addr_config (e.g. "0.0.0.0:50051" → "50051")
                let port = grpc_addr_config.rsplit(':').next().unwrap_or("50051");
                format!("{}:{}", local, port)
            });

            // Build and send PairResponse
            let resp = monopsony_common::proto::monopsony::PairResponse {
                client_id: new_client_id.clone(),
                client_cert_pem,
                client_key_pem,
                ca_cert_pem,
                grpc_addr,
            };
            let resp_bytes = resp.encode_to_vec();
            let resp_len = resp_bytes.len() as u32;
            socket.write_all(&resp_len.to_le_bytes()).await?;
            socket.write_all(&resp_bytes).await?;

            info!("Pairing certificates sent to {} (client_id={})", addr, new_client_id);

            // Update session status
            update_pairing_session(
                &pairing_manager,
                PairingStatus::Completed,
                Some(new_client_id.clone()),
                Some(req.hostname.clone()),
                None,
            )
            .await;
        }
        Ok(Err(e)) => {
            warn!("Failed to accept pairing connection: {}", e);
            update_pairing_session(
                &pairing_manager,
                PairingStatus::Failed,
                None,
                None,
                Some(format!("Accept failed: {}", e)),
            )
            .await;
        }
        Err(_) => {
            info!("Pairing session timed out after {}s", PAIRING_TIMEOUT_SECS);
            update_pairing_session(
                &pairing_manager,
                PairingStatus::Expired,
                None,
                None,
                Some("Timed out waiting for client".to_string()),
            )
            .await;
        }
    }

    Ok(())
}

/// Helper: update the pairing session inside the mutex.
async fn update_pairing_session(
    pairing_manager: &std::sync::Arc<tokio::sync::Mutex<Option<PairingSession>>>,
    status: PairingStatus,
    paired_client_id: Option<String>,
    paired_hostname: Option<String>,
    error: Option<String>,
) {
    let mut lock = pairing_manager.lock().await;
    if let Some(ref mut session) = *lock {
        session.status = status;
        if paired_client_id.is_some() {
            session.paired_client_id = paired_client_id;
        }
        if paired_hostname.is_some() {
            session.paired_hostname = paired_hostname;
        }
        if error.is_some() {
            session.error = error;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pairing_code_generation() {
        let charset: Vec<u8> = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".to_vec();
        let mut random_bytes = [0u8; 6];
        getrandom::getrandom(&mut random_bytes).unwrap();
        let code: String = random_bytes
            .iter()
            .map(|&b| charset[b as usize % charset.len()] as char)
            .collect();
        assert_eq!(code.len(), 6);
        for c in code.chars() {
            assert!(c.is_ascii_alphanumeric());
        }
    }

    #[test]
    fn test_pairing_session_status_serialization() {
        let json = serde_json::to_string(&PairingStatus::Waiting).unwrap();
        assert_eq!(json, "\"waiting\"");
        let json = serde_json::to_string(&PairingStatus::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
        let json = serde_json::to_string(&PairingStatus::Failed).unwrap();
        assert_eq!(json, "\"failed\"");
    }

    #[test]
    fn test_pair_status_response_when_no_session() {
        let resp = PairStatusResponse {
            active: false,
            code: None,
            status: None,
            paired_client_id: None,
            paired_hostname: None,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"active\":false"));
        assert!(!json.contains("code"));
    }
}
