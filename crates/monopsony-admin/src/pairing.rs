//! Pairing endpoints — daemon-managed client pairing via admin API.
//!
//! Two modes:
//! - Legacy (pairing_mode = `legacy-code`): `POST /api/v1/pair/init` mints a
//!   6-char code, starts a TCP listener on port 50052, the client connects
//!   with the code, and the server issues an mTLS cert in one exchange.
//! - v2 / Sunshine (pairing_mode = `pin`, default): client initiates with
//!   `POST /api/v1/pair/request` (ephemeral pubkey + PIN hash), admin
//!   approves via `POST /api/v1/pair/approve` (request_id + PIN), client
//!   collects via `POST /api/v1/pair/collect` (proof of possession).
//!
//! Registration-at-collection: the DB row is created only when the client
//! collects the cert, fixing the register-before-complete asymmetry.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{ConnectInfo, State},
    response::IntoResponse,
    Json,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tracing::{info, warn};
use uuid::Uuid;

use crate::AdminState;
use anyhow::Context;

/// How long a pairing session stays open waiting for a client to connect.
const PAIRING_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Port for the plaintext pairing listener (same as the old CLI command).
const PAIRING_PORT: u16 = 50052;

/// How long the pairing exchange waits for socket reads/writes before giving
/// up. A silent or wedged peer must fail the session, not hold the
/// single-accept listener open forever.
const PAIRING_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Upper bound for pairing's best-effort mirror auto-creation. Pairing is
/// control-plane critical; mirror config creation is not — if the mirror
/// subsystem's config lock is contended or stuck, pairing proceeds without it
/// (2026-10-17: a wedged sync task froze pairing mid-handshake exactly here).
const MIRROR_ENSURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ─── Pending pair store (pairing-v2 Phase 2) ─────────────────────────────
/// In-memory store for v2 pairing requests. Cap 50, TTL 15 min, dedup-replace
/// on same claimed identity + source IP, 3-strike PIN invalidation.
/// No DB writes happen here — registration occurs only at collect time.
#[derive(Debug, Clone)]
pub struct PendingPairEntry {
    pub request_id: String,
    pub claimed_hostname: String,
    pub claimed_os: String,
    pub claimed_os_version: String,
    pub package_managers: Vec<String>,
    pub ephemeral_pubkey: Vec<u8>,   // client Ed25519 pubkey (32 bytes)
    pub pin_hash: String,             // hex SHA-256(pin_salt + pin)
    pub pin_salt: String,             // client-generated random salt (hex)
    pub pin_attempts: u32,            // 0-3
    pub source_ip: String,            // observed peer IP
    pub created_at: i64,              // epoch seconds
    pub expires_at: i64,              // epoch seconds, pending TTL (15 min)
    pub status: PendingPairStatus,
    pub client_id: Option<String>,    // set at collect time
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingPairStatus {
    Pending,      // awaiting admin approval
    Approved,     // admin approved, awaiting client collection
    Collected,    // client collected the cert (PIN single-use)
    Expired,      // TTL passed without collection
    Invalidated,  // 3 wrong PINs or admin deny
}

/// In-memory store with cap, TTL, dedup-replace, and expiry.
#[derive(Debug, Default)]
pub struct PendingPairStore {
    /// request_id -> entry
    entries: HashMap<String, PendingPairEntry>,
    /// claimed_hostname + source_ip -> request_id (for dedup-replace)
    by_identity: HashMap<(String, String), String>,
}

const PENDING_CAP: usize = 50;
const PENDING_TTL_SECS: i64 = 900;   // 15 minutes
const APPROVED_UNCOLLECTED_TTL_SECS: i64 = 300; // 5 minutes after approval
const MAX_PIN_ATTEMPTS: u32 = 3;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl PendingPairStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a new pairing request. Dedup-replaces an existing entry from the
    /// same claimed identity + source IP. Returns the request_id.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        claimed_hostname: String,
        claimed_os: String,
        claimed_os_version: String,
        package_managers: Vec<String>,
        ephemeral_pubkey: Vec<u8>,
        pin_hash: String,
        pin_salt: String,
        source_ip: String,
    ) -> String {
        let now = now_secs();
        let request_id = Uuid::new_v4().to_string();
        let expires_at = now + PENDING_TTL_SECS;

        // Dedup-replace: if same identity + source already has a pending
        // request, invalidate the old one and reuse its request_id slot.
        let dedup_key = (claimed_hostname.clone(), source_ip.clone());
        if let Some(existing_id) = self.by_identity.remove(&dedup_key) {
            if let Some(existing) = self.entries.get_mut(&existing_id) {
                existing.status = PendingPairStatus::Invalidated;
            }
        }

        let entry = PendingPairEntry {
            request_id: request_id.clone(),
            claimed_hostname,
            claimed_os,
            claimed_os_version,
            package_managers,
            ephemeral_pubkey,
            pin_hash,
            pin_salt,
            pin_attempts: 0,
            source_ip,
            created_at: now,
            expires_at,
            status: PendingPairStatus::Pending,
            client_id: None,
        };

        // Enforce cap: evict oldest expired/collected/invalidated first.
        if self.entries.len() >= PENDING_CAP {
            self.evict_stale();
        }
        // If still at cap, evict oldest pending.
        if self.entries.len() >= PENDING_CAP {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.created_at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                if let Some(e) = self.entries.remove(&id) {
                    self.by_identity.remove(&(e.claimed_hostname, e.source_ip));
                }
            }
        }

        self.entries.insert(request_id.clone(), entry);
        self.by_identity.insert(dedup_key, request_id.clone());
        request_id
    }

    fn evict_stale(&mut self) {
        let now = now_secs();
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                e.status == PendingPairStatus::Collected
                    || e.status == PendingPairStatus::Invalidated
                    || now > e.expires_at
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(e) = self.entries.remove(&id) {
                self.by_identity.remove(&(e.claimed_hostname, e.source_ip));
            }
        }
    }

    /// List all pending (not-yet-collected) entries for the admin dashboard.
    pub fn list_pending(&self) -> Vec<PendingPairEntry> {
        let now = now_secs();
        self.entries
            .values()
            .filter(|e| {
                // Expired entries that haven't been marked yet
                if now > e.expires_at && e.status == PendingPairStatus::Pending {
                    return false;
                }
                // Approved-but-uncollected past its shorter clock
                if e.status == PendingPairStatus::Approved
                    && now > e.created_at + APPROVED_UNCOLLECTED_TTL_SECS
                {
                    return false;
                }
                e.status == PendingPairStatus::Pending
                    || e.status == PendingPairStatus::Approved
            })
            .cloned()
            .collect()
    }

    /// Get an entry by request_id.
    pub fn get(&self, request_id: &str) -> Option<PendingPairEntry> {
        self.entries.get(request_id).cloned()
    }

    /// Validate a PIN attempt. Returns Ok(true) if correct, Ok(false) if wrong
    /// (incrementing attempts), Err if already invalidated/collected.
    pub fn attempt_pin(&mut self, request_id: &str, pin: &str) -> Result<bool, String> {
        let entry = self
            .entries
            .get_mut(request_id)
            .ok_or_else(|| "Request not found".to_string())?;

        // A pending request past its TTL can no longer be approved (D8).
        let now = now_secs();
        if entry.status == PendingPairStatus::Pending && now > entry.expires_at {
            entry.status = PendingPairStatus::Expired;
            warn!("Pairing request {} expired before approval", request_id);
            return Err("Request expired".to_string());
        }

        if entry.status == PendingPairStatus::Approved {
            // Idempotent: repeat approve of the same request is a no-ok, not an error (D8).
            return Ok(true);
        }

        if entry.status != PendingPairStatus::Pending {
            return Err(format!("Request is {:?}, not pending", entry.status));
        }

        // Compute expected hash: SHA-256(pin_salt + pin)
        let mut hasher = sha2::Sha256::new();
        hasher.update(entry.pin_salt.as_bytes());
        hasher.update(pin.as_bytes());
        let expected = hex::encode(hasher.finalize());

        if expected == entry.pin_hash {
            entry.status = PendingPairStatus::Approved;
            info!("Pairing request {} approved by PIN", request_id);
            Ok(true)
        } else {
            entry.pin_attempts += 1;
            if entry.pin_attempts >= MAX_PIN_ATTEMPTS {
                entry.status = PendingPairStatus::Invalidated;
                warn!(
                    "Pairing request {} invalidated after {} failed PIN attempts",
                    request_id, entry.pin_attempts
                );
            }
            Ok(false)
        }
    }

    /// Mark a request as collected (PIN single-use). Returns the entry if
    /// found and collectable. Approved-but-uncollected requests expire on a
    /// shorter clock than the pending TTL (D8).
    pub fn mark_collected(&mut self, request_id: &str) -> Option<PendingPairEntry> {
        let entry = self.entries.get_mut(request_id)?;
        if entry.status != PendingPairStatus::Approved {
            return None;
        }
        let now = now_secs();
        if now > entry.created_at + APPROVED_UNCOLLECTED_TTL_SECS {
            entry.status = PendingPairStatus::Expired;
            warn!("Pairing request {} expired before collection", request_id);
            return None;
        }
        entry.status = PendingPairStatus::Collected;
        entry.client_id = entry.client_id.clone();
        Some(entry.clone())
    }

    /// Deny a request (admin action).
    pub fn deny(&mut self, request_id: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(request_id) {
            if entry.status == PendingPairStatus::Pending
                || entry.status == PendingPairStatus::Approved
            {
                entry.status = PendingPairStatus::Invalidated;
                true
            } else {
                false
            }
        } else {
            false
        }
    }
}

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

    info!(
        "Pairing session started: code={}, expires at {}",
        pairing_code, deadline
    );

    // Clone what the background task needs
    let tls_manager = state.tls_manager.clone();
    let db = state.db.clone();
    let mirror_manager = state.mirror_manager.clone();
    let grpc_addr_config = state.grpc_addr.clone();
    let grpc_addr_override = req.grpc_addr_override.clone();
    let ws_url = state.ws_url.clone();
    let mirror_verify_key = state.mirror_verify_key;
    let code_for_task = pairing_code.clone();

    *session_lock = Some(session.clone());
    drop(session_lock);

    // Spawn the background pairing listener task
    let pairing_manager = state.pairing_manager.clone();
    let pairing_manager_for_error = pairing_manager.clone();
    let join_handle = tokio::spawn(async move {
        if let Err(e) = run_pairing_listener(
            code_for_task,
            tls_manager,
            db,
            mirror_manager,
            grpc_addr_config,
            grpc_addr_override,
            ws_url,
            mirror_verify_key,
            pairing_manager,
        )
        .await
        {
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
            let is_expired =
                now_ms > session.deadline_ms && session.status == PairingStatus::Waiting;

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
// Listener wiring mirrors the subsystem surface it hands off to; a params
// struct would just relocate the arity.
#[allow(clippy::too_many_arguments)]
async fn run_pairing_listener(
    pairing_code: String,
    tls_manager: std::sync::Arc<monopsony_control::tls::TlsManager>,
    db: Option<std::sync::Arc<monopsony_db::Database>>,
    mirror_manager: Option<std::sync::Arc<monopsony_mirror::MirrorManager>>,
    grpc_addr_config: String,
    grpc_addr_override: Option<String>,
    ws_url: Option<String>,
    mirror_verify_key: [u8; 32],
    pairing_manager: std::sync::Arc<tokio::sync::Mutex<Option<PairingSession>>>,
) -> anyhow::Result<()> {
    use prost::Message;
    use tokio::io::AsyncWriteExt;

    let pair_addr: SocketAddr = format!("0.0.0.0:{}", PAIRING_PORT).parse()?;
    let pair_listener = tokio::net::TcpListener::bind(pair_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind pairing port {}: {}", pair_addr, e))?;

    info!(
        "Pairing listener on {} ({}s timeout)",
        pair_addr, PAIRING_TIMEOUT_SECS
    );

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

            // Read length-prefixed PairRequest, bounded so a silent
            // connection (port probe, wedged client) can't hold the
            // single-accept session open forever.
            let req_buf = tokio::time::timeout(PAIRING_IO_TIMEOUT, read_pair_request(&mut socket))
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Timed out after {:?} reading pairing request from {} — connection silent",
                        PAIRING_IO_TIMEOUT,
                        addr
                    )
                })??;

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

                // Auto-create mirror configs for the client's package
                // managers. Bounded: the mirror manager's config lock can be
                // held indefinitely by a stuck sync task (2026-10-17 tailnet
                // pairing hang), and pairing must never wedge on it.
                if let Some(ref mirror_mgr) = mirror_manager {
                    match tokio::time::timeout(
                        MIRROR_ENSURE_TIMEOUT,
                        mirror_mgr.ensure_mirrors_for_package_managers(&req.package_managers),
                    )
                    .await
                    {
                        Ok(created) if created > 0 => {
                            info!(client = %req.hostname, created = created, "Auto-created mirror configs from pairing");
                        }
                        Ok(_) => {}
                        Err(_) => {
                            warn!(
                                client = %req.hostname,
                                "Mirror auto-creation timed out after {:?} (mirror subsystem busy) — continuing pairing without it",
                                MIRROR_ENSURE_TIMEOUT
                            );
                        }
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
            // (red-team F3: 90-day validity + serial tracked for revocation)
            let (cert_serial, client_cert, client_key) =
                tls_manager.generate_client_cert(&new_client_id)?;
            let (client_cert_pem, client_key_pem) =
                monopsony_control::tls::TlsManager::cert_and_key_to_pem(&client_cert, &client_key);
            let ca_cert_pem = tls_manager.ca_cert_pem_bytes();

            // Record the cert serial on the client row for revocation tracking.
            if let Some(db) = db.as_ref() {
                db.update_client_cert_serial(&cid, &cert_serial)
                    .await
                    .context("Failed to record client cert serial")?;
            }

            // Derive the gRPC address to send to the client.
            // Prefer the override, then the socket's local address, then the config.
            let grpc_addr = grpc_addr_override.unwrap_or_else(|| {
                let local = socket
                    .local_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_else(|_| {
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
                ws_url: ws_url.clone().unwrap_or_default(),
                mirror_verify_key: mirror_verify_key.to_vec(),
            };
            let resp_bytes = resp.encode_to_vec();
            let resp_len = resp_bytes.len() as u32;
            tokio::time::timeout(PAIRING_IO_TIMEOUT, async {
                socket.write_all(&resp_len.to_le_bytes()).await?;
                socket.write_all(&resp_bytes).await
            })
            .await
            .map_err(|_| anyhow::anyhow!("Timed out sending pairing response to {}", addr))??;

            info!(
                "Pairing certificates sent to {} (client_id={})",
                addr, new_client_id
            );

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

/// Read one `[u32 LE length][protobuf bytes]` frame from `stream`, enforcing
/// the 64 KiB cap shared with the client's response reader.
async fn read_pair_request<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read pairing request length: {}", e))?;
    let req_len = u32::from_le_bytes(len_buf) as usize;

    // Sanity-check the length to prevent allocation bombs
    if req_len > 65536 {
        anyhow::bail!("Pairing request too large: {} bytes", req_len);
    }

    let mut req_buf = vec![0u8; req_len];
    stream
        .read_exact(&mut req_buf)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read pairing request body: {}", e))?;
    Ok(req_buf)
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

// ─── Pairing v2 / Sunshine request/response types ────────────────────────

/// POST /api/v1/pair/request — client submits a v2 pairing request.
#[derive(Debug, Deserialize)]
pub struct PairRequestV2 {
    /// Client-generated Ed25519 public key (32 bytes) for key-bound issuance.
    pub ephemeral_pubkey: String, // hex-encoded
    /// hex SHA-256(pin_salt + pin)
    pub pin_hash: String,
    /// Client-generated random salt (hex)
    pub pin_salt: String,
    pub claimed_hostname: String,
    pub claimed_os: String,
    pub claimed_os_version: String,
    #[serde(default)]
    pub package_managers: Vec<String>,
}

/// Response to POST /api/v1/pair/request.
#[derive(Debug, Serialize)]
pub struct PairRequestV2Response {
    pub request_id: String,
    pub expires_at: i64,
}

/// GET /api/v1/pair/request/:request_id/status — client polls for approval status.
#[derive(Debug, Serialize)]
pub struct PairRequestStatusResponse {
    pub request_id: String,
    pub status: String, // "pending" | "approved" | "collected" | "expired" | "invalidated"
    pub pin_attempts: u32,
    pub expires_at: i64,
}

/// POST /api/v1/pair/approve — admin approves a pending request.
#[derive(Debug, Deserialize)]
pub struct ApprovePairRequest {
    pub request_id: String,
    pub pin: String,
}

#[derive(Debug, Serialize)]
pub struct ApprovePairResponse {
    pub approved: bool,
    pub message: String,
}

/// POST /api/v1/pair/collect — client collects cert after approval.
#[derive(Debug, Deserialize)]
pub struct CollectPairRequest {
    pub request_id: String,
    /// hex-encoded Ed25519 public key (must match the one in the original request)
    pub ephemeral_pubkey: String,
    /// Ed25519 signature over request_id bytes (hex-encoded)
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct CollectPairResponse {
    pub collected: bool,
    pub client_id: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
    pub ca_cert_pem: String,
    pub grpc_addr: String,
    pub ws_url: String,
    pub mirror_verify_key: Vec<u8>,
}

/// POST /api/v1/pair/enroll — fleet-scale pre-approved pairing (P5).
#[derive(Debug, Deserialize)]
pub struct EnrollPairRequest {
    /// Plaintext enrollment key (mopk-...); validated by hashed lookup.
    pub enrollment_key: String,
    /// hex-encoded Ed25519 ephemeral public key (32 bytes).
    pub ephemeral_pubkey: String,
    /// Ed25519 signature over `monopsony-enroll-v1` (hex, 64 bytes) —
    /// proof of possession of the ephemeral private key.
    pub signature: String,
    pub claimed_hostname: String,
    pub claimed_os: String,
    pub claimed_os_version: String,
    #[serde(default)]
    pub package_managers: Vec<String>,
}

// ─── Pairing v2 handlers ─────────────────────────────────────────────────

/// POST /api/v1/pair/request — v2 pairing intake (unauth, rate-limited).
/// Client submits ephemeral pubkey + PIN hash; server stores as pending.
pub async fn handle_pair_request_v2(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AdminState>,
    Json(req): Json<PairRequestV2>,
) -> impl IntoResponse {
    // Check pairing mode
    if state.dashboard_config.pairing_mode == crate::PairingMode::Off {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "pairing is disabled"})),
        );
    }

    // Decode ephemeral pubkey
    let ephemeral_pubkey = match hex::decode(&req.ephemeral_pubkey) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "ephemeral_pubkey must be 32 hex bytes"})),
            );
        }
    };

    // Validate pin_hash and pin_salt are non-empty
    if req.pin_hash.is_empty() || req.pin_salt.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "pin_hash and pin_salt are required"})),
        );
    }

    let source_ip = addr.ip().to_string();

    let mut store = state.pending_pairs.lock().await;
    let request_id = store.submit(
        req.claimed_hostname.clone(),
        req.claimed_os.clone(),
        req.claimed_os_version.clone(),
        req.package_managers,
        ephemeral_pubkey,
        req.pin_hash.clone(),
        req.pin_salt.clone(),
        source_ip.clone(),
    );
    let entry = store.get(&request_id).unwrap();

    drop(store);

    info!(
        "Pairing v2 request submitted: request_id={}, hostname={}, source_ip={}",
        request_id, req.claimed_hostname, source_ip
    );

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "request_id": request_id,
            "expires_at": entry.expires_at,
        })),
    )
}

/// GET /api/v1/pair/request/:request_id/status — client polls for status.
pub async fn handle_pair_request_status(
    State(state): State<AdminState>,
    axum::extract::Path(request_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let store = state.pending_pairs.lock().await;
    let entry = match store.get(&request_id) {
        Some(e) => e,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "request not found"})),
            );
        }
    };
    drop(store);

    let status_str = match entry.status {
        PendingPairStatus::Pending => "pending",
        PendingPairStatus::Approved => "approved",
        PendingPairStatus::Collected => "collected",
        PendingPairStatus::Expired => "expired",
        PendingPairStatus::Invalidated => "invalidated",
    };

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "request_id": request_id,
            "status": status_str,
            "pin_attempts": entry.pin_attempts,
            "expires_at": entry.expires_at,
        })),
    )
}

/// GET /api/v1/pair/pending — list pending requests (auth, admin dashboard).
pub async fn handle_pair_pending(State(state): State<AdminState>) -> impl IntoResponse {
    let store = state.pending_pairs.lock().await;
    let pending = store.list_pending();
    drop(store);

    let result: Vec<serde_json::Value> = pending
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "request_id": e.request_id,
                "claimed_hostname": e.claimed_hostname,
                "claimed_os": e.claimed_os,
                "claimed_os_version": e.claimed_os_version,
                "source_ip": e.source_ip,
                "created_at": e.created_at,
                "expires_at": e.expires_at,
                "pin_attempts": e.pin_attempts,
                "status": match e.status {
                    PendingPairStatus::Pending => "pending",
                    PendingPairStatus::Approved => "approved",
                    PendingPairStatus::Collected => "collected",
                    PendingPairStatus::Expired => "expired",
                    PendingPairStatus::Invalidated => "invalidated",
                },
            })
        })
        .collect();

    Json(serde_json::Value::Array(result))
}

/// POST /api/v1/pair/approve — admin approves a pending request (row-bound,
/// 3-strike PIN, idempotent).
pub async fn handle_pair_approve(
    State(state): State<AdminState>,
    Json(req): Json<ApprovePairRequest>,
) -> impl IntoResponse {
    let mut store = state.pending_pairs.lock().await;
    let result = store.attempt_pin(&req.request_id, &req.pin);
    drop(store);

    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    match result {
        Ok(true) => {
            let _ = db
                .record_pair_approval_audit(&req.request_id, "approved", "system", None)
                .await;
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "approved": true,
                    "message": "Request approved. Client may now collect the certificate."
                })),
            )
                .into_response()
        }
        Ok(false) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "approved": false,
                "message": "Invalid PIN."
            })),
        )
            .into_response(),
        Err(msg) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "approved": false,
                "message": msg
            })),
        )
            .into_response(),
    }
}

/// POST /api/v1/pair/deny — admin denies a pending request (auth).
pub async fn handle_pair_deny(
    State(state): State<AdminState>,
    axum::extract::Path(request_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let mut store = state.pending_pairs.lock().await;
    let denied = store.deny(&request_id);
    drop(store);

    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    if denied {
        let _ = db
            .record_pair_approval_audit(&request_id, "denied", "system", None)
            .await;
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({"status": "denied"})),
        )
            .into_response()
    } else {
        (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "request not found or already resolved"})),
        )
            .into_response()
    }
}

/// POST /api/v1/pair/collect — client collects cert after approval.
/// Proof of possession: Ed25519 signature over request_id bytes using the
/// ephemeral privkey. Registration-at-collection: DB row created here.
pub async fn handle_pair_collect(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AdminState>,
    Json(req): Json<CollectPairRequest>,
) -> impl IntoResponse {
    let source_ip = addr.ip().to_string();

    // Decode ephemeral pubkey
    let ephemeral_pubkey_bytes = match hex::decode(&req.ephemeral_pubkey) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "ephemeral_pubkey must be 32 hex bytes"})),
            );
        }
    };

    // Decode signature (Ed25519 = 64 bytes)
    let signature_bytes = match hex::decode(&req.signature) {
        Ok(bytes) if bytes.len() == 64 => bytes,
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "signature must be 64 hex bytes"})),
            );
        }
    };

    // Look up the pending request
    let mut store = state.pending_pairs.lock().await;
    let entry = match store.get(&req.request_id) {
        Some(e) => e,
        None => {
            drop(store);
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "request not found"})),
            );
        }
    };

    // Verify status is Approved
    if entry.status != PendingPairStatus::Approved {
        drop(store);
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Request is {:?}, not approved", entry.status)
            })),
        );
    }

    // Verify ephemeral_pubkey matches
    if entry.ephemeral_pubkey != ephemeral_pubkey_bytes {
        drop(store);
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "ephemeral_pubkey does not match the request"})),
        );
    }

    // Verify Ed25519 signature over request_id bytes
    let verifying_key = match VerifyingKey::from_bytes(&ephemeral_pubkey_bytes.try_into().unwrap()) {
        Ok(vk) => vk,
        Err(_) => {
            drop(store);
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid ephemeral public key"})),
            );
        }
    };
    let sig = match Signature::from_slice(&signature_bytes) {
        Ok(s) => s,
        Err(_) => {
            drop(store);
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid signature"})),
            );
        }
    };

    if verifying_key.verify(req.request_id.as_bytes(), &sig).is_err() {
        drop(store);
        warn!(
            "Collect attempt with invalid signature: request_id={}, source_ip={}",
            req.request_id, source_ip
        );
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "signature verification failed"})),
        );
    }

    // All checks passed — mark as collected
    let collected_entry = match store.mark_collected(&req.request_id) {
        Some(e) => e,
        None => {
            drop(store);
            return (
                axum::http::StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "request already collected or invalid"})),
            );
        }
    };
    drop(store);

    info!(
        "Pairing v2 collect: request_id={}, client claimed hostname={}, source_ip={}",
        req.request_id, collected_entry.claimed_hostname, source_ip
    );

    // Generate cert and register client (registration-at-collection)
    // Use the TlsManager to generate a client cert bound to the client_id
    let client_id = uuid::Uuid::new_v4().to_string();

    let (cert_serial, client_cert, client_key) = match state
        .tls_manager
        .generate_client_cert(&client_id)
    {
        Ok(result) => result,
        Err(e) => {
            warn!("Failed to generate client cert for collect: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to generate certificate"})),
            );
        }
    };

    let (client_cert_pem, client_key_pem) =
        monopsony_control::tls::TlsManager::cert_and_key_to_pem(&client_cert, &client_key);
    let ca_cert_pem = state.tls_manager.ca_cert_pem_bytes();

    // Register client in DB (registration-at-collection)
    if let Some(ref db) = state.db {
        match db
            .register_client(
                &collected_entry.claimed_hostname,
                &collected_entry.claimed_os,
                &collected_entry.claimed_os_version,
                &collected_entry.package_managers,
            )
            .await
        {
            Ok(cid) => {
                let issued_at = chrono::Utc::now();
                let expires_at = issued_at
                    + chrono::Duration::seconds(
                        state.tls_manager.client_cert_validity_days() as i64 * 86_400,
                    );
                let _ = db.record_pairing(&cid, issued_at).await;
                let _ = db.update_client_cert_serial(&cid, &cert_serial).await;
                let _ = db
                    .insert_client_cert(&cert_serial, &cid, issued_at, expires_at)
                    .await;
            }
            Err(e) => {
                warn!("Failed to register client during collect: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "failed to register client"})),
                );
            }
        }
    }

    // Derive gRPC address
    let grpc_addr = state.grpc_addr.clone();

    // Convert PEM bytes to UTF-8 strings (they are valid ASCII/UTF-8 PEM)
    let client_cert_pem = String::from_utf8_lossy(&client_cert_pem).into_owned();
    let client_key_pem = String::from_utf8_lossy(&client_key_pem).into_owned();
    let ca_cert_pem = String::from_utf8_lossy(&ca_cert_pem).into_owned();

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "collected": true,
            "client_id": client_id,
            "client_cert_pem": client_cert_pem,
            "client_key_pem": client_key_pem,
            "ca_cert_pem": ca_cert_pem,
            "grpc_addr": grpc_addr,
            "ws_url": state.ws_url.clone().unwrap_or_default(),
            "mirror_verify_key": state.mirror_verify_key.to_vec(),
        })),
    )
}
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

    #[tokio::test]
    async fn read_pair_request_decodes_valid_frame() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        let payload = b"pair-frame".to_vec();
        client
            .write_all(&(payload.len() as u32).to_le_bytes())
            .await
            .unwrap();
        client.write_all(&payload).await.unwrap();

        let buf = read_pair_request(&mut server).await.unwrap();
        assert_eq!(buf, payload);
    }

    #[tokio::test]
    async fn read_pair_request_rejects_oversized_length() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&1_000_000u32.to_le_bytes()).await.unwrap();

        let err = read_pair_request(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn read_pair_request_errors_on_truncated_frame() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&16u32.to_le_bytes()).await.unwrap();
        client.write_all(b"short").await.unwrap();
        drop(client); // EOF mid-frame

        assert!(read_pair_request(&mut server).await.is_err());
    }

    // ─── PendingPairStore tests ──────────────────────────────────────

    #[test]
    fn pending_store_submit_and_list() {
        let mut store = PendingPairStore::new();
        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec!["apt".to_string()],
            vec![1u8; 32],
            "hash1".to_string(),
            "salt1".to_string(),
            "10.0.0.1".to_string(),
        );
        assert!(!id.is_empty());

        let pending = store.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, id);
        assert_eq!(pending[0].claimed_hostname, "client1");
        assert_eq!(pending[0].status, PendingPairStatus::Pending);
    }

    #[test]
    fn pending_store_approve_and_collect() {
        let mut store = PendingPairStore::new();

        // Pre-compute the correct PIN hash: SHA-256("salt" + "correct_pin")
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"salt");
        hasher.update(b"correct_pin");
        let correct_pin_hash = hex::encode(hasher.finalize());

        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            correct_pin_hash,   // pin_hash = SHA-256("salt" + "correct_pin")
            "salt".to_string(),
            "10.0.0.1".to_string(),
        );

        // Wrong PIN → attempt incremented, still pending
        let result = store.attempt_pin(&id, "wrong_pin").unwrap();
        assert!(!result);
        let entry = store.get(&id).unwrap();
        assert_eq!(entry.pin_attempts, 1);
        assert_eq!(entry.status, PendingPairStatus::Pending);

        // Correct PIN → approved
        let result = store.attempt_pin(&id, "correct_pin").unwrap();
        assert!(result);
        let entry = store.get(&id).unwrap();
        assert_eq!(entry.status, PendingPairStatus::Approved);

        // Collect → marked collected, returns entry
        let collected = store.mark_collected(&id).unwrap();
        assert_eq!(collected.status, PendingPairStatus::Collected);

        // Collect again → already collected, returns None
        assert!(store.mark_collected(&id).is_none());
    }

    #[test]
    fn pending_store_three_wrong_pins_invalidate() {
        let mut store = PendingPairStore::new();
        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            "hash".to_string(),
            "salt".to_string(),
            "10.0.0.1".to_string(),
        );

        // Three wrong PINs
        for _ in 0..3 {
            let result = store.attempt_pin(&id, "wrong").unwrap();
            assert!(!result);
        }

        let entry = store.get(&id).unwrap();
        assert_eq!(entry.status, PendingPairStatus::Invalidated);
        assert_eq!(entry.pin_attempts, 3);

        // Fourth attempt on invalidated request → error
        let result = store.attempt_pin(&id, "whatever");
        assert!(result.is_err());
    }

    #[test]
    fn pending_store_dedup_replace_same_identity() {
        let mut store = PendingPairStore::new();
        let id1 = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            "hash1".to_string(),
            "salt1".to_string(),
            "10.0.0.1".to_string(),
        );
        let id2 = store.submit(
            "client1".to_string(), // same identity
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![2u8; 32],
            "hash2".to_string(),
            "salt2".to_string(),
            "10.0.0.1".to_string(), // same source IP
        );

        // First request should be invalidated
        let e1 = store.get(&id1).unwrap();
        assert_eq!(e1.status, PendingPairStatus::Invalidated);

        // Second request is pending
        let e2 = store.get(&id2).unwrap();
        assert_eq!(e2.status, PendingPairStatus::Pending);

        // Only one entry in list_pending
        let pending = store.list_pending();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn pending_store_cap_evicts_stale() {
        let mut store = PendingPairStore::new();
        // Fill up to cap
        for i in 0..PENDING_CAP {
            store.submit(
                format!("client{}", i),
                "Linux".to_string(),
                "6.0".to_string(),
                vec![],
                vec![i as u8; 32],
                "hash".to_string(),
                "salt".to_string(),
                format!("10.0.0.{}", i),
            );
        }
        assert_eq!(store.entries.len(), PENDING_CAP);

        // One more submission should trigger eviction, keeping count at cap
        store.submit(
            "client_new".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![99u8; 32],
            "hash".to_string(),
            "salt".to_string(),
            "10.0.0.99".to_string(),
        );
        assert_eq!(store.entries.len(), PENDING_CAP);
        // At least one of the original entries should be gone
        assert!(store.entries.values().any(|e| e.claimed_hostname == "client_new"));
    }

    #[test]
    fn pending_store_expired_not_listed() {
        let mut store = PendingPairStore::new();
        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            "hash".to_string(),
            "salt".to_string(),
            "10.0.0.1".to_string(),
        );

        // Manually expire the entry
        {
            let entry = store.entries.get_mut(&id).unwrap();
            entry.expires_at = now_secs() - 1; // already expired
        }

        let pending = store.list_pending();
        assert!(pending.is_empty());
    }

    /// P2 gate: a pending request past its TTL cannot be approved.
    #[test]
    fn pending_store_expired_cannot_be_approved() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"salt");
        hasher.update(b"correct_pin");
        let correct_pin_hash = hex::encode(hasher.finalize());

        let mut store = PendingPairStore::new();
        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            correct_pin_hash,
            "salt".to_string(),
            "10.0.0.1".to_string(),
        );

        // Expire the entry
        {
            let entry = store.entries.get_mut(&id).unwrap();
            entry.expires_at = now_secs() - 1;
        }

        // Approve must fail with "expired", not succeed
        let result = store.attempt_pin(&id, "correct_pin");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Request expired");

        let entry = store.get(&id).unwrap();
        assert_eq!(entry.status, PendingPairStatus::Expired);
    }

    /// P2 gate: an approved-but-uncollected request expires on the shorter
    /// collection clock (APPROVED_UNCOLLECTED_TTL_SECS), so mark_collected
    /// rejects it and marks it Expired.
    #[test]
    fn pending_store_approved_expires_before_collection() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"salt");
        hasher.update(b"correct_pin");
        let correct_pin_hash = hex::encode(hasher.finalize());

        let mut store = PendingPairStore::new();
        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            correct_pin_hash,
            "salt".to_string(),
            "10.0.0.1".to_string(),
        );

        // Approve it
        assert!(store.attempt_pin(&id, "correct_pin").unwrap());

        // Push it past the approved-uncollected TTL
        {
            let entry = store.entries.get_mut(&id).unwrap();
            entry.created_at = now_secs() - APPROVED_UNCOLLECTED_TTL_SECS - 1;
        }

        // Collection must fail (request expired before collection)
        assert!(store.mark_collected(&id).is_none());
        let entry = store.get(&id).unwrap();
        assert_eq!(entry.status, PendingPairStatus::Expired);
    }

    /// P2 gate: repeating an approve on an already-approved request is a
    /// no-op returning Ok(true), not an error (idempotent).
    #[test]
    fn pending_store_approve_is_idempotent() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"salt");
        hasher.update(b"correct_pin");
        let correct_pin_hash = hex::encode(hasher.finalize());

        let mut store = PendingPairStore::new();
        let id = store.submit(
            "client1".to_string(),
            "Linux".to_string(),
            "6.0".to_string(),
            vec![],
            vec![1u8; 32],
            correct_pin_hash,
            "salt".to_string(),
            "10.0.0.1".to_string(),
        );

        // First approve succeeds
        assert!(store.attempt_pin(&id, "correct_pin").unwrap());

        // Repeat approve is a no-op, not an error
        assert!(store.attempt_pin(&id, "correct_pin").unwrap());

        let entry = store.get(&id).unwrap();
        assert_eq!(entry.status, PendingPairStatus::Approved);
    }
}

/// GET /api/v1/pair/audit — list pairing approval/deny audit history (auth,
/// admin dashboard approval audit view).
pub async fn handle_pair_audit(
    State(state): State<AdminState>,
) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    match db.list_pair_approval_audit(100).await {
        Ok(entries) => {
            let result: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "request_id": e.request_id,
                        "action": e.action,
                        "approver": e.approver,
                        "detail": e.detail,
                        "created_at": e.created_at,
                    })
                })
                .collect();
            Json(serde_json::Value::Array(result)).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/v1/pair/enroll — fleet-scale pre-approved pairing (P5).
///
/// The client presents a valid enrollment key plus its ephemeral pubkey and a
/// possession signature (Ed25519 over the literal bytes
/// `monopsony-enroll-v1`). On success the server registers the client,
/// issues a cert (same path as collect: registration-at-issuance), bumps the
/// key's use counter, and returns the cert bundle. Unauthenticated but
/// rate-limited; the key itself is the pre-approval (D6), and issuance is
/// key-bound — the signature proves possession of the presented pubkey.
pub async fn handle_pair_enroll(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AdminState>,
    Json(req): Json<EnrollPairRequest>,
) -> impl IntoResponse {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    const ENROLL_POSSESSION_MSG: &[u8] = b"monopsony-enroll-v1";

    if state.dashboard_config.pairing_mode == crate::PairingMode::Off {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "pairing is disabled"})),
        )
            .into_response();
    }

    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    // 1. Validate the enrollment key (hashed lookup, revoked/expired/uses).
    let mut hasher = sha2::Sha256::new();
    hasher.update(req.enrollment_key.as_bytes());
    let key_hash = hex::encode(hasher.finalize());
    let key = match db.validate_enrollment_key(&key_hash).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            warn!(
                source_ip = %addr.ip(),
                "Enroll rejected: invalid, expired, exhausted, or revoked key"
            );
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid or exhausted enrollment key"})),
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

    // 2. Decode and verify the ephemeral pubkey + possession signature.
    let ephemeral_pubkey_bytes = match hex::decode(&req.ephemeral_pubkey) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "ephemeral_pubkey must be 32 hex bytes"})),
            )
                .into_response();
        }
    };
    let signature_bytes = match hex::decode(&req.signature) {
        Ok(bytes) if bytes.len() == 64 => bytes,
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "signature must be 64 hex bytes"})),
            )
                .into_response();
        }
    };

    let verifying_key = match VerifyingKey::from_bytes(
        &ephemeral_pubkey_bytes.clone().try_into().unwrap(),
    ) {
        Ok(vk) => vk,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid ephemeral public key"})),
            )
                .into_response();
        }
    };
    let sig = match Signature::from_slice(&signature_bytes) {
        Ok(s) => s,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid signature"})),
            )
                .into_response();
        }
    };
    if verifying_key.verify(ENROLL_POSSESSION_MSG, &sig).is_err() {
        warn!(
            source_ip = %addr.ip(),
            "Enroll rejected: possession signature verification failed"
        );
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "signature verification failed"})),
        )
            .into_response();
    }

    // 3. Issue the cert (key-bound: same generate path as collect) and
    //    register the client.
    let client_id = uuid::Uuid::new_v4().to_string();
    let (cert_serial, client_cert, client_key) = match state.tls_manager.generate_client_cert(&client_id)
    {
        Ok(result) => result,
        Err(e) => {
            warn!("Failed to generate client cert for enroll: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to generate certificate"})),
            )
                .into_response();
        }
    };
    let (client_cert_pem, client_key_pem) =
        monopsony_control::tls::TlsManager::cert_and_key_to_pem(&client_cert, &client_key);
    let ca_cert_pem = state.tls_manager.ca_cert_pem_bytes();

    let issued_at = chrono::Utc::now();
    let expires_at = issued_at
        + chrono::Duration::seconds(
            state.tls_manager.client_cert_validity_days() as i64 * 86_400,
        );

    match db
        .register_client(
            &req.claimed_hostname,
            &req.claimed_os,
            &req.claimed_os_version,
            &req.package_managers,
        )
        .await
    {
        Ok(cid) => {
            let _ = db.record_pairing(&cid, issued_at).await;
            let _ = db.update_client_cert_serial(&cid, &cert_serial).await;
            let _ = db
                .insert_client_cert(&cert_serial, &cid, issued_at, expires_at)
                .await;
        }
        Err(e) => {
            warn!("Failed to register client during enroll: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to register client"})),
            )
                .into_response();
        }
    }

    // 4. Bump the use counter.
    if let Err(e) = db.increment_enrollment_key_use(&key.id).await {
        warn!(
            key_id = %key.id,
            "Failed to increment enrollment key use count: {}", e
        );
    }

    // 5. Audit the enrollment.
    let _ = db
        .record_pair_approval_audit(
            &format!("enroll:{}", key.id),
            "approved",
            "enrollment-key",
            Some(&req.claimed_hostname),
        )
        .await;

    info!(
        client_id = %client_id,
        key_id = %key.id,
        label = %key.label,
        source_ip = %addr.ip(),
        "Client enrolled via enrollment key"
    );

    (
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "enrolled": true,
            "client_id": client_id,
            "client_cert_pem": String::from_utf8_lossy(&client_cert_pem),
            "client_key_pem": String::from_utf8_lossy(&client_key_pem),
            "ca_cert_pem": String::from_utf8_lossy(&ca_cert_pem),
            "grpc_addr": state.grpc_addr,
            "ws_url": state.ws_url.clone().unwrap_or_default(),
            "mirror_verify_key": state.mirror_verify_key.to_vec(),
        })),
    )
        .into_response()
}
