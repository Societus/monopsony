//! gRPC service implementation for Monopsony control plane

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use monopsony_common::models::ClientId;
use monopsony_common::package_manager::{PackageManagerKind, PackageRef};
use monopsony_common::proto::monopsony::monopsony_control_server::{
    MonopsonyControl, MonopsonyControlServer,
};
use monopsony_common::proto::monopsony::{
    client_message, command, server_message, ApplyShadowUpdateRequest, CheckShadowUpdatesRequest,
    ClientMessage, Command, DownloadShadowAssetRequest, GetShadowRecipeRequest, HeartbeatRequest,
    HeartbeatResponse, InventoryDelta, InventoryReport, InventoryResponse,
    ListShadowCatalogRequest, ListShadowPackagesRequest, RegisterRequest, RegisterResponse,
    RegisterShadowPackageRequest, RollbackShadowUpdateRequest, ServerMessage, ShadowAssetChunk,
    ShadowCatalog, ShadowCatalogEntry, ShadowPackageInfo, ShadowPackageList, ShadowScanAck,
    ShadowScanReport, ShadowUpdateList, ShadowUpdateResult, SignedShadowRecipe, SyncRequest,
    UnregisterShadowPackageRequest, UnregisterShadowPackageResponse,
};

use crate::tls::TlsManager;
use monopsony_db::db::TamperAlertInsert;
use monopsony_db::Database;
use monopsony_mirror::MirrorManager;
use monopsony_shadow::catalog_hash::{catalog_hash, entry_hash, sign_entry};
use monopsony_shadow::ShadowPackageManager;

pub struct MonopsonyGrpc {
    db: Arc<Database>,
    tls_manager: Arc<TlsManager>,
    shadow_manager: Option<Arc<ShadowPackageManager>>,
    mirror_manager: Option<Arc<MirrorManager>>,
    telemetry_enabled: bool,
    /// Ed25519 verifying key for pin attestations, delivered to clients on
    /// register. Zeroed ([0u8; 32]) = pinning not provisioned (inert).
    mirror_verify_key: [u8; 32],
}

impl MonopsonyGrpc {
    pub fn new(
        db: Arc<Database>,
        tls_manager: Arc<TlsManager>,
        shadow_manager: Option<Arc<ShadowPackageManager>>,
    ) -> Self {
        Self {
            db,
            tls_manager,
            shadow_manager,
            mirror_manager: None,
            telemetry_enabled: true,
            mirror_verify_key: [0u8; 32],
        }
    }

    /// Set the pin-attestation verifying key handed to clients at register.
    pub fn with_mirror_verify_key(mut self, mirror_verify_key: [u8; 32]) -> Self {
        self.mirror_verify_key = mirror_verify_key;
        self
    }

    /// Set the mirror manager (called from start() where the manager is already available).
    pub fn with_mirror_manager(mut self, mirror_manager: Option<Arc<MirrorManager>>) -> Self {
        self.mirror_manager = mirror_manager;
        self
    }

    /// Set telemetry enabled flag from server config.
    pub fn with_telemetry(mut self, enabled: bool) -> Self {
        self.telemetry_enabled = enabled;
        self
    }

    async fn handle_register(&self, req: RegisterRequest) -> Result<RegisterResponse> {
        monopsony_common::metrics::grpc_request("register");
        let start = std::time::Instant::now();
        info!(
            client_hostname = %req.hostname,
            client_os = %req.os,
            "Handling register request"
        );

        let client_id = self
            .db
            .register_client(
                &req.hostname,
                &req.os,
                &req.os_version,
                &req.package_managers,
            )
            .await
            .context("Failed to register client")?;

        debug!(client_id = %client_id, "Client registered successfully");

        // Auto-create mirror configs for any package managers the client reports
        if let Some(ref mirror_mgr) = self.mirror_manager {
            let created = mirror_mgr
                .ensure_mirrors_for_package_managers(&req.package_managers)
                .await;
            if created > 0 {
                info!(client_id = %client_id, created = created, "Auto-created mirror configs from client registration");
            }
        }

        // Generate client certificate
        let (cert, key) = self
            .tls_manager
            .generate_client_cert(&client_id.to_string())?;
        let (cert_pem, key_pem) = TlsManager::cert_and_key_to_pem(&cert, &key);

        Ok(RegisterResponse {
            client_id: client_id.to_string(),
            enabled_managers: req.package_managers,
            client_cert_pem: cert_pem,
            client_key_pem: key_pem,
            ca_cert_pem: self.tls_manager.ca_cert_pem().to_string(),
            mirror_verify_key: self.mirror_verify_key.to_vec(),
        })
        .inspect(|_resp| {
            monopsony_common::metrics::grpc_request_duration(
                "register",
                start.elapsed().as_secs_f64(),
            );
            monopsony_common::metrics::client_registered();
        })
    }

    async fn handle_heartbeat(&self, req: HeartbeatRequest) -> Result<HeartbeatResponse> {
        monopsony_common::metrics::grpc_request("heartbeat");
        let start = std::time::Instant::now();
        debug!(client_id = %req.client_id, "Handling heartbeat");

        let client_id =
            ClientId::parse(&req.client_id).context("Invalid client ID in heartbeat request")?;

        self.db.heartbeat(&client_id).await?;

        monopsony_common::metrics::grpc_request_duration(
            "heartbeat",
            start.elapsed().as_secs_f64(),
        );

        Ok(HeartbeatResponse {
            server_time: chrono::Utc::now().timestamp(),
        })
    }

    async fn handle_inventory(&self, req: InventoryReport) -> Result<InventoryResponse> {
        monopsony_common::metrics::grpc_request("inventory");
        let start = std::time::Instant::now();
        info!(
            client_id = %req.client_id,
            package_count = req.packages.len(),
            full_sync = req.full_sync,
            "Handling inventory report"
        );

        let client_id =
            ClientId::parse(&req.client_id).context("Invalid client ID in inventory report")?;

        let packages: Vec<PackageRef> = req
            .packages
            .iter()
            .filter_map(|entry| {
                let manager = match PackageManagerKind::parse_kind(&entry.manager) {
                    Some(m) => m,
                    None => {
                        warn!(
                            manager = %entry.manager,
                            "Unknown package manager in inventory, skipping entry"
                        );
                        return None;
                    }
                };
                Some(PackageRef {
                    name: entry.name.clone(),
                    version: if entry.version.is_empty() {
                        None
                    } else {
                        Some(entry.version.clone())
                    },
                    architecture: if entry.arch.is_empty() {
                        None
                    } else {
                        Some(entry.arch.clone())
                    },
                    manager,
                    installed_at: if self.telemetry_enabled {
                        entry.installed_at
                    } else {
                        0
                    },
                })
            })
            .collect();

        debug!(
            client_id = %client_id,
            mapped_packages = packages.len(),
            "Recording inventory"
        );

        self.db.record_inventory(&client_id, &packages).await?;

        monopsony_common::metrics::grpc_request_duration(
            "inventory",
            start.elapsed().as_secs_f64(),
        );

        Ok(InventoryResponse {
            accepted: true,
            message: format!("Recorded {} packages", packages.len()),
        })
    }

    async fn handle_inventory_delta(&self, req: InventoryDelta) -> Result<InventoryResponse> {
        info!(
            client_id = %req.client_id,
            added_count = req.added.len(),
            removed_count = req.removed.len(),
            "Handling inventory delta"
        );

        let client_id =
            ClientId::parse(&req.client_id).context("Invalid client ID in inventory delta")?;

        // Process added packages
        let added: Vec<PackageRef> = req
            .added
            .iter()
            .filter_map(|entry| {
                let manager = match PackageManagerKind::parse_kind(&entry.manager) {
                    Some(m) => m,
                    None => {
                        warn!(
                            manager = %entry.manager,
                            "Unknown package manager in delta, skipping entry"
                        );
                        return None;
                    }
                };
                Some(PackageRef {
                    name: entry.name.clone(),
                    version: if entry.version.is_empty() {
                        None
                    } else {
                        Some(entry.version.clone())
                    },
                    architecture: if entry.arch.is_empty() {
                        None
                    } else {
                        Some(entry.arch.clone())
                    },
                    manager,
                    installed_at: if self.telemetry_enabled {
                        entry.installed_at
                    } else {
                        0
                    },
                })
            })
            .collect();

        // Process removed packages (these are package identifiers like "apt:nginx:1.18.0:amd64")
        let removed: Vec<(PackageManagerKind, String)> = req
            .removed
            .iter()
            .filter_map(|id| {
                // Parse format: "manager:name:version:arch" or "manager:name"
                let parts: Vec<&str> = id.split(':').collect();
                if parts.is_empty() {
                    warn!(package_id = %id, "Invalid package ID format in delta removal");
                    return None;
                }
                let manager = match PackageManagerKind::parse_kind(parts[0]) {
                    Some(m) => m,
                    None => {
                        warn!(manager = %parts[0], "Unknown package manager in delta removal");
                        return None;
                    }
                };
                // Use the full ID string as the package identifier
                Some((manager, id.clone()))
            })
            .collect();

        debug!(
            client_id = %client_id,
            mapped_added = added.len(),
            mapped_removed = removed.len(),
            "Applying inventory delta"
        );

        self.db
            .apply_inventory_delta(&client_id, &added, &removed)
            .await?;

        Ok(InventoryResponse {
            accepted: true,
            message: format!(
                "Added {} packages, removed {} packages",
                added.len(),
                removed.len()
            ),
        })
    }

    async fn dispatch_client_message(&self, msg: ClientMessage) -> Result<ServerMessage, Status> {
        let inner = msg
            .msg
            .ok_or_else(|| Status::invalid_argument("Empty client message"))?;

        match inner {
            client_message::Msg::Register(req) => {
                let resp = self.handle_register(req).await.map_err(|e| {
                    error!(error = %e, "Register handler failed");
                    Status::internal(format!("Register failed: {}", e))
                })?;
                Ok(ServerMessage {
                    msg: Some(server_message::Msg::Register(resp)),
                })
            }
            client_message::Msg::Heartbeat(req) => {
                let client_id_str = req.client_id.clone();
                let _resp = self.handle_heartbeat(req).await.map_err(|e| {
                    error!(error = %e, "Heartbeat handler failed");
                    Status::internal(format!("Heartbeat failed: {}", e))
                })?;
                // Sprint 22 slice 22-E2a: a pending shadow assignment is
                // pushed INSTEAD of the sync_request ack, one ServerMessage
                // per ClientMessage. Status stays 'pending' — the client
                // reports applied/refused in a later slice, which resolves
                // the row via complete_pending_assignment.
                if let Some(cmd_msg) = self.take_pending_assignment_command(&client_id_str).await {
                    return Ok(cmd_msg);
                }
                // Heartbeat response goes via the unary RPC; for streaming, send a sync_request as ack
                Ok(ServerMessage {
                    msg: Some(server_message::Msg::SyncRequest(SyncRequest {})),
                })
            }
            client_message::Msg::Inventory(req) => {
                self.handle_inventory(req).await.map_err(|e| {
                    error!(error = %e, "Inventory handler failed");
                    Status::internal(format!("Inventory report failed: {}", e))
                })?;
                Ok(ServerMessage {
                    msg: Some(server_message::Msg::SyncRequest(SyncRequest {})),
                })
            }
            client_message::Msg::Delta(delta) => {
                self.handle_inventory_delta(delta).await.map_err(|e| {
                    error!(error = %e, "Inventory delta handler failed");
                    Status::internal(format!("Inventory delta failed: {}", e))
                })?;
                Ok(ServerMessage {
                    msg: Some(server_message::Msg::SyncRequest(SyncRequest {})),
                })
            }
            client_message::Msg::TamperAlert(alert) => {
                // Alarm surface: a client verified served bytes against the
                // signed pin and they did not match.
                error!(
                    url = %alert.url,
                    expected_sha256 = %alert.expected_sha256,
                    actual_sha256 = %alert.actual_sha256,
                    detail = %alert.detail,
                    observed_at = alert.observed_at,
                    "TAMPER ALERT: client-reported cache integrity mismatch"
                );
                let insert = TamperAlertInsert {
                    url: alert.url.clone(),
                    expected_sha256: alert.expected_sha256.clone(),
                    actual_sha256: alert.actual_sha256.clone(),
                    detail: if alert.detail.is_empty() {
                        None
                    } else {
                        Some(alert.detail.clone())
                    },
                    // client identity: future (stream does not carry authenticated identity into dispatch)
                    client_id: None,
                    observed_at: alert.observed_at,
                };
                if let Err(e) = self.db.record_tamper_alert(&insert).await {
                    error!(error = %e, "Failed to record tamper alert in database");
                }
                Ok(ServerMessage {
                    msg: Some(server_message::Msg::SyncRequest(SyncRequest {})),
                })
            }
        }
    }

    /// Sprint 22 slice 22-E2a: build the pending-assignment push Command for
    /// a client, if one is queued. Marks the row delivered (status stays
    /// 'pending' until the client reports applied/refused) and returns
    /// ServerMessage{ Command(UPDATE_CONFIG, shadow_assign params) } per the
    /// pinned contract. Recipe fields come from the shadow_recipes catalog.
    async fn take_pending_assignment_command(&self, client_id_str: &str) -> Option<ServerMessage> {
        let client_id = match monopsony_common::models::ClientId::parse(client_id_str) {
            Ok(id) => id.0,
            Err(e) => {
                warn!(client_id = %client_id_str, error = %e, "Invalid client id for pending-assignment push");
                return None;
            }
        };

        let pending = match self.db.take_pending_assignments(client_id).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "Failed to query pending shadow assignments");
                return None;
            }
        };
        let first = pending.first()?;

        let catalog = match self.db.list_shadow_catalog().await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to load shadow catalog for assignment push");
                return None;
            }
        };
        let recipe = match catalog.iter().find(|r| r.name == first.recipe_name) {
            Some(r) => r,
            None => {
                // Recipe removed from catalog after assignment; leave the row
                // pending rather than resolving it server-side (v1 contract).
                warn!(recipe = %first.recipe_name, "Pending assignment recipe missing from catalog");
                return None;
            }
        };

        // Sign the raw recipe_toml with the server recipe key — the client's
        // trust check verifies a signature over the TOML bytes. The catalog's
        // stored signature covers canonical_entry_json instead and will NOT
        // verify client-side (same contract as get_shadow_recipe).
        let (sig_b64, key_id_str) = match self.shadow_manager.as_ref() {
            Some(mgr) => {
                use monopsony_shadow::shadow_signing::{key_id, sign_recipe};
                let sk = mgr.signing_key_ref();
                // Parse the stored TOML so the SignedRecipe carries a real
                // recipe struct (Default is not implemented for ShadowRecipe).
                let parsed =
                    monopsony_shadow::shadow_recipe::parse_recipe_str(&recipe.recipe_toml).ok()?;
                let signed = sign_recipe(&recipe.recipe_toml, parsed, sk);
                (
                    signed.operator_signature.signature,
                    key_id(&sk.verifying_key()),
                )
            }
            None => {
                warn!("Shadow manager unavailable; cannot sign assignment push");
                return None;
            }
        };

        let mut params = std::collections::HashMap::new();
        params.insert("operation".to_string(), "shadow_assign".to_string());
        params.insert("recipe_name".to_string(), recipe.name.clone());
        params.insert("recipe_toml".to_string(), recipe.recipe_toml.clone());
        params.insert("signature".to_string(), sig_b64);
        params.insert("key_id".to_string(), key_id_str);

        info!(
            client_id = %client_id_str,
            recipe = %first.recipe_name,
            "Pushing pending shadow assignment via control-stream Command"
        );

        Some(ServerMessage {
            msg: Some(server_message::Msg::Command(Command {
                r#type: command::CommandType::UpdateConfig as i32,
                params,
            })),
        })
    }
}

#[tonic::async_trait]
impl MonopsonyControl for MonopsonyGrpc {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let resp = self
            .handle_register(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(resp))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let resp = self
            .handle_heartbeat(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(resp))
    }

    async fn report_inventory(
        &self,
        request: Request<InventoryReport>,
    ) -> Result<Response<InventoryResponse>, Status> {
        let resp = self
            .handle_inventory(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(resp))
    }

    type ControlStreamStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn control_stream(
        &self,
        request: Request<tonic::Streaming<ClientMessage>>,
    ) -> Result<Response<Self::ControlStreamStream>, Status> {
        let db = self.db.clone();
        let tls_manager = self.tls_manager.clone();
        let shadow_manager = self.shadow_manager.clone();
        let mirror_manager = self.mirror_manager.clone();
        let mirror_verify_key = self.mirror_verify_key;
        let mut stream = request.into_inner();

        let output = async_stream::stream! {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(client_msg) => {
                        let grpc = MonopsonyGrpc::new(db.clone(), tls_manager.clone(), shadow_manager.clone())
                            .with_mirror_manager(mirror_manager.clone())
                            .with_mirror_verify_key(mirror_verify_key);
                        yield grpc.dispatch_client_message(client_msg).await;
                    }
                    Err(status) => {
                        warn!(error = %status, "Error receiving client message from stream");
                        yield Err(status);
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(output)))
    }

    // ── Shadow package RPCs ──

    async fn list_shadow_packages(
        &self,
        request: Request<ListShadowPackagesRequest>,
    ) -> Result<Response<ShadowPackageList>, Status> {
        let req = request.into_inner();
        let client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;

        let rows = self
            .db
            .list_shadow_packages_for_client(&client_id.0)
            .await
            .map_err(|e| Status::internal(format!("DB error listing shadow packages: {e}")))?;

        let packages: Vec<ShadowPackageInfo> = rows
            .iter()
            .filter_map(|row| {
                let v = row.0.as_object()?;
                Some(ShadowPackageInfo {
                    id: v
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: v
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    version: v
                        .get("version")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    architecture: v
                        .get("architecture")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    os_family: v
                        .get("os_family")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    libc: v
                        .get("libc")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    source_type: v
                        .get("source_type")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    source_url: v
                        .get("source_url")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    recipe_path: v
                        .get("recipe_path")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    install_paths: v
                        .get("install_paths")
                        .and_then(|x| x.as_str())
                        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                        .unwrap_or_default(),
                    config_paths: v
                        .get("config_paths")
                        .and_then(|x| x.as_str())
                        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                        .unwrap_or_default(),
                    service_name: v
                        .get("service_name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    masked_from_pm: v
                        .get("masked_from_pm")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    registered_at: v
                        .get("registered_at")
                        .and_then(|x| {
                            // Handle both numeric timestamps and ISO strings
                            if let Some(n) = x.as_i64() {
                                Some(n)
                            } else if let Some(s) = x.as_str() {
                                chrono::DateTime::parse_from_rfc3339(s)
                                    .ok()
                                    .map(|dt| dt.timestamp())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0),
                    last_updated_at: v
                        .get("last_updated_at")
                        .and_then(|x| {
                            if let Some(n) = x.as_i64() {
                                Some(n)
                            } else if let Some(s) = x.as_str() {
                                chrono::DateTime::parse_from_rfc3339(s)
                                    .ok()
                                    .map(|dt| dt.timestamp())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0),
                    last_checked_at: v
                        .get("last_checked_at")
                        .and_then(|x| {
                            if let Some(n) = x.as_i64() {
                                Some(n)
                            } else if let Some(s) = x.as_str() {
                                chrono::DateTime::parse_from_rfc3339(s)
                                    .ok()
                                    .map(|dt| dt.timestamp())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0),
                })
            })
            .collect();

        Ok(Response::new(ShadowPackageList { packages }))
    }

    async fn register_shadow_package(
        &self,
        request: Request<RegisterShadowPackageRequest>,
    ) -> Result<Response<ShadowPackageInfo>, Status> {
        let req = request.into_inner();
        let client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;

        if req.name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }

        // Validate the recipe TOML at ingest: a recipe that cannot parse
        // would poison the catalog for every other client (adoption parses
        // before serving). Reject early instead of storing garbage.
        if !req.recipe_toml.trim().is_empty() {
            monopsony_shadow::shadow_recipe::parse_recipe_str(&req.recipe_toml)
                .map_err(|e| Status::invalid_argument(format!("recipe TOML is invalid: {e}")))?;
        }

        let install_paths = serde_json::json!(req.install_paths);
        // TODO: serialize config_paths to JSON value when proto supports it
        let config_paths_val: Option<&serde_json::Value> = None;

        let service_name = if req.service_name.is_empty() {
            None
        } else {
            Some(req.service_name.as_str())
        };
        let masked_from_pm = if req.mask_from_pm { Some("true") } else { None };

        let id = self
            .db
            .create_shadow_package(
                &client_id.0,
                &req.name,
                &req.version,
                &req.architecture,
                &req.os_family,
                if req.libc.is_empty() {
                    None
                } else {
                    Some(req.libc.as_str())
                },
                &req.source_type,
                &req.source_url,
                &req.recipe_toml,
                &install_paths,
                config_paths_val,
                service_name,
                masked_from_pm,
            )
            .await
            .map_err(|e| Status::internal(format!("DB error creating shadow package: {e}")))?;

        // Sprint 22 (22-C1): publish to the server-global recipe catalog with
        // an Ed25519 signature from the server recipe key (when available).
        {
            let ehash = entry_hash(&req.name, &req.recipe_toml);
            let (signature, key_id_str) = if let Some(ref mgr) = self.shadow_manager {
                let signing_key = mgr.signing_key_ref();
                let sig_b64 = sign_entry(signing_key, &req.name, &req.recipe_toml);
                (
                    sig_b64,
                    monopsony_shadow::shadow_signing::key_id(&signing_key.verifying_key()),
                )
            } else {
                // No signing key reachable — store unsigned and report as not_done.
                (String::new(), "unsigned".to_string())
            };
            if let Err(e) = self
                .db
                .upsert_shadow_recipe(
                    &req.name,
                    &req.recipe_toml,
                    &signature,
                    &key_id_str,
                    &ehash,
                    Some(&client_id.0),
                )
                .await
            {
                tracing::warn!(name = %req.name, error = %e, "failed to publish shadow recipe to global catalog");
            }
        }

        Ok(Response::new(ShadowPackageInfo {
            id: id.to_string(),
            name: req.name,
            version: req.version,
            architecture: req.architecture,
            os_family: req.os_family,
            libc: req.libc,
            source_type: req.source_type,
            source_url: req.source_url,
            recipe_path: req.recipe_toml,
            install_paths: req.install_paths,
            config_paths: req.config_paths,
            service_name: req.service_name,
            masked_from_pm: req.mask_from_pm.to_string(),
            registered_at: chrono::Utc::now().timestamp(),
            last_updated_at: chrono::Utc::now().timestamp(),
            last_checked_at: 0,
        }))
    }

    async fn unregister_shadow_package(
        &self,
        request: Request<UnregisterShadowPackageRequest>,
    ) -> Result<Response<UnregisterShadowPackageResponse>, Status> {
        let req = request.into_inner();
        let _client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;
        let package_id = uuid::Uuid::parse_str(&req.package_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid package ID: {e}")))?;

        let deleted = self
            .db
            .delete_shadow_package(&package_id)
            .await
            .map_err(|e| Status::internal(format!("DB error deleting shadow package: {e}")))?;

        Ok(Response::new(UnregisterShadowPackageResponse {
            deleted,
            message: if deleted {
                format!("Shadow package {} removed", req.package_id)
            } else {
                format!("Shadow package {} not found", req.package_id)
            },
        }))
    }

    async fn check_shadow_updates(
        &self,
        request: Request<CheckShadowUpdatesRequest>,
    ) -> Result<Response<ShadowUpdateList>, Status> {
        let req = request.into_inner();
        let client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;

        // If shadow manager is available, use it for real update checking
        if let Some(ref mgr) = self.shadow_manager {
            let rows = self
                .db
                .list_shadow_packages_for_client(&client_id.0)
                .await
                .map_err(|e| Status::internal(format!("DB error: {e}")))?;

            let mut updates = vec![];
            for row in &rows {
                let v = match row.0.as_object() {
                    Some(v) => v,
                    None => continue,
                };
                let pkg_id_str = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let pkg_id = match uuid::Uuid::parse_str(pkg_id_str) {
                    Ok(id) => id,
                    Err(_) => continue,
                };
                let source_url = v.get("source_url").and_then(|x| x.as_str()).unwrap_or("");
                let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
                let tag_pattern = v
                    .get("tag_pattern")
                    .and_then(|x| x.as_str())
                    .unwrap_or(".*");

                match mgr
                    .check_one(&pkg_id, source_url, tag_pattern, version)
                    .await
                {
                    Ok(Some(info)) => {
                        updates.push(monopsony_common::proto::monopsony::ShadowUpdateInfo {
                            package_id: info.package_id,
                            package_name: info.package_name,
                            current_version: info.current_version,
                            latest_version: info.latest_version,
                            asset_url: info.asset_url.unwrap_or_default(),
                            asset_name: info.asset_name.unwrap_or_default(),
                            asset_size: info.asset_size.unwrap_or(0) as i64,
                            release_notes_url: info.release_notes_url.unwrap_or_default(),
                        });
                    }
                    Ok(None) => { /* up to date */ }
                    Err(e) => {
                        warn!(package_id = %pkg_id, error = %e, "Shadow update check failed");
                    }
                }
            }
            Ok(Response::new(ShadowUpdateList { updates }))
        } else {
            // No shadow manager -- return empty (same as before)
            Ok(Response::new(ShadowUpdateList { updates: vec![] }))
        }
    }

    async fn apply_shadow_update(
        &self,
        request: Request<ApplyShadowUpdateRequest>,
    ) -> Result<Response<ShadowUpdateResult>, Status> {
        let req = request.into_inner();
        let package_id = uuid::Uuid::parse_str(&req.package_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid package ID: {e}")))?;

        // Record the update in history. The actual update is performed client-side
        // (download, extract, apply). The server records the intent.
        let history_id = self
            .db
            .record_shadow_update(
                &package_id,
                &req.target_version,
                "", // from_version unknown to server
                "pending",
                if req.create_backup {
                    Some("pending")
                } else {
                    None
                },
            )
            .await
            .map_err(|e| Status::internal(format!("DB error recording shadow update: {e}")))?;

        Ok(Response::new(ShadowUpdateResult {
            success: true,
            message: format!(
                "Update recorded as {} (history: {})",
                req.target_version, history_id
            ),
            new_version: req.target_version,
            backup_path: String::new(),
        }))
    }

    async fn rollback_shadow_update(
        &self,
        request: Request<RollbackShadowUpdateRequest>,
    ) -> Result<Response<ShadowUpdateResult>, Status> {
        let req = request.into_inner();
        let package_id = uuid::Uuid::parse_str(&req.package_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid package ID: {e}")))?;

        // Record rollback intent in update history
        let history_id = self
            .db
            .record_shadow_update(
                &package_id,
                "", // target version from rollback
                "", // from_version unknown to server
                "rollback_pending",
                None,
            )
            .await
            .map_err(|e| Status::internal(format!("DB error recording shadow rollback: {e}")))?;

        Ok(Response::new(ShadowUpdateResult {
            success: true,
            message: format!("Rollback recorded (history: {})", history_id),
            new_version: String::new(),
            backup_path: String::new(),
        }))
    }

    async fn get_shadow_recipe(
        &self,
        request: Request<GetShadowRecipeRequest>,
    ) -> Result<Response<SignedShadowRecipe>, Status> {
        let req = request.into_inner();
        let client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;
        let package_name = req.package_name.trim().to_string();

        if package_name.is_empty() {
            return Err(Status::invalid_argument("package_name is required"));
        }

        // Catalog-first lookup (Sprint 22): a published global recipe is the
        // canonical copy — any paired client may adopt it. Fall back to the
        // requesting client's own registrations for unpublished local recipes.
        if let Ok(catalog) = self.db.list_shadow_catalog().await {
            if let Some(row) = catalog.iter().find(|r| r.name == package_name) {
                // Validate the stored TOML still parses, then sign the raw
                // recipe_toml with the server's recipe key — the client's
                // trust check verifies a signature over the TOML bytes
                // (same contract as the per-client path below). The catalog's
                // stored entry signature covers canonical_entry_json instead
                // and is NOT what adopt verifies.
                let recipe = monopsony_shadow::shadow_recipe::parse_recipe_str(&row.recipe_toml)
                    .map_err(|e| {
                        Status::internal(format!(
                            "Failed to parse catalog recipe for '{package_name}': {e}"
                        ))
                    })?;
                let mgr = self.shadow_manager.as_ref().ok_or_else(|| {
                    Status::unimplemented("Shadow manager not available for recipe signing")
                })?;
                let signed = {
                    use monopsony_shadow::shadow_signing::{key_id, sign_recipe};
                    let s = sign_recipe(&row.recipe_toml, recipe, mgr.signing_key_ref());
                    SignedShadowRecipe {
                        recipe_toml: s.recipe_toml,
                        key_id: key_id(&mgr.signing_key_ref().verifying_key()),
                        signature: s.operator_signature.signature,
                        signed_at: s.operator_signature.signed_at.timestamp(),
                    }
                };
                return Ok(Response::new(signed));
            }
        }

        // Fallback: look up the shadow package by name within the client's packages
        let rows = self
            .db
            .list_shadow_packages_for_client(&client_id.0)
            .await
            .map_err(|e| Status::internal(format!("DB error: {e}")))?;

        let pkg = rows
            .iter()
            .find_map(|row| {
                let v = row.0.as_object()?;
                if v.get("name").and_then(|x| x.as_str()) == Some(&package_name) {
                    Some(v.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                Status::not_found(format!("Shadow package '{package_name}' not found"))
            })?;

        let recipe_toml = pkg
            .get("recipe_path")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if recipe_toml.is_empty() {
            return Err(Status::not_found(format!(
                "No recipe TOML stored for '{package_name}'"
            )));
        }

        // Parse the recipe TOML to get a ShadowRecipe struct for signing
        let recipe = monopsony_shadow::shadow_recipe::parse_recipe_str(&recipe_toml)
            .map_err(|e| Status::internal(format!("Failed to parse recipe: {e}")))?;

        // Sign the recipe using the shadow manager's signing key
        let mgr = self.shadow_manager.as_ref().ok_or_else(|| {
            Status::unimplemented("Shadow manager not available for recipe signing")
        })?;

        let signed = {
            use monopsony_shadow::shadow_signing::{key_id, sign_recipe};
            let signed_recipe = sign_recipe(&recipe_toml, recipe, mgr.signing_key_ref());
            SignedShadowRecipe {
                recipe_toml: signed_recipe.recipe_toml,
                key_id: key_id(&mgr.signing_key_ref().verifying_key()),
                signature: signed_recipe.operator_signature.signature,
                signed_at: signed_recipe.operator_signature.signed_at.timestamp(),
            }
        };

        Ok(Response::new(signed))
    }

    type DownloadShadowAssetStream =
        Pin<Box<dyn Stream<Item = Result<ShadowAssetChunk, Status>> + Send>>;

    async fn download_shadow_asset(
        &self,
        request: Request<DownloadShadowAssetRequest>,
    ) -> Result<Response<Self::DownloadShadowAssetStream>, Status> {
        let req = request.into_inner();
        let _client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;

        let mgr = self.shadow_manager.as_ref().ok_or_else(|| {
            Status::unimplemented("Shadow manager not available for asset download")
        })?;

        let cache = mgr.cache_ref();

        // Look up the cached asset by package_id + target_version
        // The cache uses package name (not UUID), so we need to resolve
        let blob_data = cache
            .read_blob(&req.target_version)
            .await
            .map_err(|e| Status::internal(format!("Cache read error: {e}")))?;

        let total_size = blob_data.len() as i64;
        let chunk_size = 64 * 1024; // 64KB chunks
        let blob_hash = req.target_version.clone();

        let output = async_stream::stream! {
            for (offset, chunk) in blob_data.chunks(chunk_size).enumerate() {
                yield Ok(ShadowAssetChunk {
                    data: chunk.to_vec(),
                    offset: (offset * chunk_size) as i64,
                    total_size,
                    blob_hash: blob_hash.clone(),
                });
            }
        };

        Ok(Response::new(Box::pin(output)))
    }
    async fn list_shadow_catalog(
        &self,
        request: Request<ListShadowCatalogRequest>,
    ) -> Result<Response<ShadowCatalog>, Status> {
        let req = request.into_inner();
        if !req.client_id.is_empty() {
            ClientId::parse(&req.client_id)
                .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;
        }

        let rows = self
            .db
            .list_shadow_catalog()
            .await
            .map_err(|e| Status::internal(format!("DB error listing shadow catalog: {e}")))?;

        let whole_hash = catalog_hash(
            &rows
                .iter()
                .map(|r| r.catalog_hash.clone())
                .collect::<Vec<_>>(),
        );

        // Change-detection short-circuit: client already has this version.
        if !req.catalog_hash.is_empty() && req.catalog_hash == whole_hash {
            return Ok(Response::new(ShadowCatalog {
                catalog_hash: whole_hash,
                entries: vec![],
            }));
        }

        let entries = rows
            .into_iter()
            .map(|r| ShadowCatalogEntry {
                name: r.name,
                recipe_toml: r.recipe_toml,
                signature: r.signature,
                key_id: r.key_id,
                catalog_hash: r.catalog_hash,
                published_at: r.published_at,
                provenance_client_id: r
                    .provenance_client_id
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
            })
            .collect();

        Ok(Response::new(ShadowCatalog {
            catalog_hash: whole_hash,
            entries,
        }))
    }

    async fn report_shadow_scan(
        &self,
        request: Request<ShadowScanReport>,
    ) -> Result<Response<ShadowScanAck>, Status> {
        let req = request.into_inner();
        let client_id = ClientId::parse(&req.client_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid client ID: {e}")))?;

        let mut accepted = 0usize;
        for finding in &req.findings {
            if finding.recipe_name.is_empty() {
                return Err(Status::invalid_argument("finding.recipe_name is required"));
            }
            let detected_version = if finding.detected_version.is_empty() {
                None
            } else {
                Some(finding.detected_version.as_str())
            };
            let method = if finding.method.is_empty() {
                None
            } else {
                Some(finding.method.as_str())
            };
            self.db
                .upsert_shadow_scan_result(
                    &client_id.0,
                    &finding.recipe_name,
                    finding.installed,
                    detected_version,
                    method,
                )
                .await
                .map_err(|e| Status::internal(format!("DB error recording scan result: {e}")))?;
            accepted += 1;
        }

        Ok(Response::new(ShadowScanAck {
            accepted: true,
            message: format!("Recorded {} shadow scan finding(s)", accepted),
        }))
    }
}

pub async fn start(
    addr: SocketAddr,
    db: Arc<Database>,
    tls_manager: Arc<TlsManager>,
    mirror_manager: Option<Arc<MirrorManager>>,
    shadow_manager: Option<Arc<ShadowPackageManager>>,
    telemetry_enabled: bool,
    mirror_verify_key: [u8; 32],
    additional_tls_sans: &[String],
    access_control: Option<monopsony_common::config::AccessControlConfig>,
) -> Result<()> {
    info!(addr = %addr, "gRPC server starting (mTLS required)");

    let grpc = MonopsonyGrpc::new(db.clone(), tls_manager.clone(), shadow_manager)
        .with_mirror_manager(mirror_manager.clone())
        .with_telemetry(telemetry_enabled)
        .with_mirror_verify_key(mirror_verify_key);

    let tls_config =
        tls_manager.tonic_server_tls_config_with_sans(Some(addr.ip()), additional_tls_sans)?;

    let _ = access_control; // wired in future: tonic interceptor

    let mut builder = tonic::transport::Server::builder()
        .tls_config(tls_config)
        .context("Failed to configure TLS")?;

    if let Some(mm) = mirror_manager {
        let mirror_grpc = MirrorControlGrpc::new(mm);
        builder
            .add_service(MonopsonyControlServer::new(grpc))
            .add_service(
                monopsony_common::proto::monopsony::mirror_control_server::MirrorControlServer::new(
                    mirror_grpc,
                ),
            )
            .serve(addr)
            .await
            .context("gRPC server failed")?;
    } else {
        builder
            .add_service(MonopsonyControlServer::new(grpc))
            .serve(addr)
            .await
            .context("gRPC server failed")?;
    }

    Ok(())
}

/// gRPC service for mirror management
pub struct MirrorControlGrpc {
    mirror_manager: Arc<MirrorManager>,
}

impl MirrorControlGrpc {
    pub fn new(mirror_manager: Arc<MirrorManager>) -> Self {
        Self { mirror_manager }
    }
}

use monopsony_common::proto::monopsony::{
    DeleteMirrorConfigRequest, DeleteMirrorConfigResponse, GetMirrorStatusRequest,
    GetMirrorStatusResponse, ListMirrorsRequest, ListMirrorsResponse, MirrorInfo,
    TriggerMirrorSyncRequest, TriggerMirrorSyncResponse, UpsertMirrorConfigRequest,
    UpsertMirrorConfigResponse,
};

fn config_to_mirror_info(c: &monopsony_mirror::mirror::MirrorConfig) -> MirrorInfo {
    MirrorInfo {
        distro: c.distro.clone(),
        upstream_url: c.upstream_url.clone(),
        local_path: c.local_path.to_string_lossy().to_string(),
        sync_enabled: c.sync_enabled,
        sync_interval_secs: c.sync_interval_secs as i64,
        bandwidth_limit_bps: c.bandwidth_limit_bps.unwrap_or(0) as i64,
        include_patterns: c.include_patterns.clone(),
        exclude_patterns: c.exclude_patterns.clone(),
        public: c.public,
        last_sync: c.last_sync.unwrap_or(0),
        sync_status: c.sync_status.clone(),
        total_bytes: c.total_bytes as i64,
    }
}

#[tonic::async_trait]
impl monopsony_common::proto::monopsony::mirror_control_server::MirrorControl
    for MirrorControlGrpc
{
    async fn list_mirrors(
        &self,
        _request: Request<ListMirrorsRequest>,
    ) -> Result<Response<ListMirrorsResponse>, Status> {
        let configs = self.mirror_manager.list_configs().await;
        let mirrors: Vec<MirrorInfo> = configs.iter().map(config_to_mirror_info).collect();
        Ok(Response::new(ListMirrorsResponse { mirrors }))
    }

    async fn trigger_mirror_sync(
        &self,
        request: Request<TriggerMirrorSyncRequest>,
    ) -> Result<Response<TriggerMirrorSyncResponse>, Status> {
        let req = request.into_inner();
        if req.distro.is_empty() {
            return Err(Status::invalid_argument("distro is required"));
        }

        info!("gRPC: triggering mirror sync for {}", req.distro);
        match self.mirror_manager.trigger_sync(&req.distro).await {
            Ok(()) => Ok(Response::new(TriggerMirrorSyncResponse {
                triggered: true,
                message: format!("Sync triggered for {}", req.distro),
            })),
            Err(e) => Ok(Response::new(TriggerMirrorSyncResponse {
                triggered: false,
                message: format!("Sync failed: {}", e),
            })),
        }
    }

    async fn get_mirror_status(
        &self,
        request: Request<GetMirrorStatusRequest>,
    ) -> Result<Response<GetMirrorStatusResponse>, Status> {
        let req = request.into_inner();
        if req.distro.is_empty() {
            return Err(Status::invalid_argument("distro is required"));
        }

        match self.mirror_manager.get_config(&req.distro).await {
            Some(config) => Ok(Response::new(GetMirrorStatusResponse {
                mirror: Some(config_to_mirror_info(&config)),
            })),
            None => Err(Status::not_found(format!(
                "Mirror not found: {}",
                req.distro
            ))),
        }
    }

    async fn upsert_mirror_config(
        &self,
        request: Request<UpsertMirrorConfigRequest>,
    ) -> Result<Response<UpsertMirrorConfigResponse>, Status> {
        let req = request.into_inner();
        if req.distro.is_empty() || req.upstream_url.is_empty() {
            return Err(Status::invalid_argument(
                "distro and upstream_url are required",
            ));
        }

        // Check if this is a create or update
        let existing = self.mirror_manager.get_config(&req.distro).await;
        let is_create = existing.is_none();

        // Persist to DB if available
        if let Some(db) = self.mirror_manager.db() {
            let includes = serde_json::to_value(&req.include_patterns).unwrap_or_default();
            let excludes = serde_json::to_value(&req.exclude_patterns).unwrap_or_default();
            let config = monopsony_db::db::MirrorUpsertConfig {
                distro: req.distro.clone(),
                upstream_url: req.upstream_url.clone(),
                local_path: req.local_path.clone(),
                sync_enabled: req.sync_enabled,
                sync_interval_secs: req.sync_interval_secs,
                bandwidth_limit_bps: if req.bandwidth_limit_bps > 0 {
                    Some(req.bandwidth_limit_bps)
                } else {
                    None
                },
                include_patterns: includes,
                exclude_patterns: excludes,
                public: req.public,
            };
            db.upsert_mirror_config(&config)
                .await
                .map_err(|e| Status::internal(format!("DB error: {}", e)))?;
        }

        // Update in-memory config
        let mut new_config = monopsony_mirror::mirror::MirrorConfig {
            distro: req.distro.clone(),
            upstream_url: req.upstream_url.clone(),
            local_path: std::path::PathBuf::from(&req.local_path),
            sync_enabled: req.sync_enabled,
            sync_interval_secs: req.sync_interval_secs as u64,
            bandwidth_limit_bps: if req.bandwidth_limit_bps > 0 {
                Some(req.bandwidth_limit_bps as u64)
            } else {
                None
            },
            include_patterns: req.include_patterns,
            exclude_patterns: req.exclude_patterns,
            public: req.public,
            ..Default::default()
        };
        if let Some(old) = existing {
            new_config.last_sync = old.last_sync;
            new_config.sync_status = old.sync_status;
            new_config.total_bytes = old.total_bytes;
        }
        self.mirror_manager.upsert_config(new_config).await;

        let action = if is_create { "created" } else { "updated" };
        info!("gRPC: {} mirror config for {}", action, req.distro);
        Ok(Response::new(UpsertMirrorConfigResponse {
            created: is_create,
            message: format!("Mirror {} {}", req.distro, action),
        }))
    }

    async fn delete_mirror_config(
        &self,
        request: Request<DeleteMirrorConfigRequest>,
    ) -> Result<Response<DeleteMirrorConfigResponse>, Status> {
        let req = request.into_inner();
        if req.distro.is_empty() {
            return Err(Status::invalid_argument("distro is required"));
        }

        // Remove from DB if available
        if let Some(db) = self.mirror_manager.db() {
            db.delete_mirror_config(&req.distro)
                .await
                .map_err(|e| Status::internal(format!("DB error: {}", e)))?;
        }

        let removed = self.mirror_manager.remove_config(&req.distro).await;
        info!(
            "gRPC: delete mirror config for {} (removed={})",
            req.distro, removed
        );
        Ok(Response::new(DeleteMirrorConfigResponse {
            deleted: removed,
            message: if removed {
                format!("Mirror {} deleted", req.distro)
            } else {
                format!("Mirror {} not found", req.distro)
            },
        }))
    }
}
