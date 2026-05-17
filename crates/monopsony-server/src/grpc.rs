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
use monopsony_common::proto::monopsony::{
    client_message, server_message, ClientMessage, HeartbeatRequest, HeartbeatResponse,
    InventoryDelta, InventoryReport, InventoryResponse, RegisterRequest, RegisterResponse,
    ServerMessage, SyncRequest,
    ListShadowPackagesRequest, ShadowPackageList, RegisterShadowPackageRequest, ShadowPackageInfo,
    UnregisterShadowPackageRequest, UnregisterShadowPackageResponse, CheckShadowUpdatesRequest,
    ShadowUpdateList, ApplyShadowUpdateRequest, RollbackShadowUpdateRequest, ShadowUpdateResult,
    GetShadowRecipeRequest, SignedShadowRecipe, DownloadShadowAssetRequest, ShadowAssetChunk,
};
use monopsony_common::proto::monopsony::monopsony_control_server::{
    MonopsonyControl, MonopsonyControlServer,
};

use crate::db::Database;
use crate::mirror::MirrorManager;
use crate::tls::TlsManager;

pub struct MonopsonyGrpc {
    db: Arc<Database>,
    tls_manager: Arc<TlsManager>,
}

impl MonopsonyGrpc {
    pub fn new(db: Arc<Database>, tls_manager: Arc<TlsManager>) -> Self {
        Self { db, tls_manager }
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

        // Generate client certificate
        let (cert, key) = self.tls_manager.generate_client_cert(&client_id.to_string())?;
        let (cert_pem, key_pem) = TlsManager::cert_and_key_to_pem(&cert, &key);

        Ok(RegisterResponse {
            client_id: client_id.to_string(),
            enabled_managers: req.package_managers,
            client_cert_pem: cert_pem,
            client_key_pem: key_pem,
            ca_cert_pem: self.tls_manager.ca_cert_pem().to_string(),
        })
        .inspect(|_resp| {
            monopsony_common::metrics::grpc_request_duration("register", start.elapsed().as_secs_f64());
            monopsony_common::metrics::client_registered();
        })
    }

    async fn handle_heartbeat(&self, req: HeartbeatRequest) -> Result<HeartbeatResponse> {
        monopsony_common::metrics::grpc_request("heartbeat");
        let start = std::time::Instant::now();
        debug!(client_id = %req.client_id, "Handling heartbeat");

        let client_id = ClientId::parse(&req.client_id)
            .context("Invalid client ID in heartbeat request")?;

        self.db.heartbeat(&client_id).await?;

        monopsony_common::metrics::grpc_request_duration("heartbeat", start.elapsed().as_secs_f64());

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

        let client_id = ClientId::parse(&req.client_id)
            .context("Invalid client ID in inventory report")?;

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
                })
            })
            .collect();

        debug!(
            client_id = %client_id,
            mapped_packages = packages.len(),
            "Recording inventory"
        );

        self.db.record_inventory(&client_id, &packages).await?;

        monopsony_common::metrics::grpc_request_duration("inventory", start.elapsed().as_secs_f64());

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

        let client_id = ClientId::parse(&req.client_id)
            .context("Invalid client ID in inventory delta")?;

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

        self.db.apply_inventory_delta(&client_id, &added, &removed).await?;

        Ok(InventoryResponse {
            accepted: true,
            message: format!("Added {} packages, removed {} packages", added.len(), removed.len()),
        })
    }

    async fn dispatch_client_message(
        &self,
        msg: ClientMessage,
    ) -> Result<ServerMessage, Status> {
        let inner = msg.msg.ok_or_else(|| Status::invalid_argument("Empty client message"))?;

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
                let _resp = self.handle_heartbeat(req).await.map_err(|e| {
                    error!(error = %e, "Heartbeat handler failed");
                    Status::internal(format!("Heartbeat failed: {}", e))
                })?;
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
        }
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

    type ControlStreamStream =
        Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn control_stream(
        &self,
        request: Request<tonic::Streaming<ClientMessage>>,
    ) -> Result<Response<Self::ControlStreamStream>, Status> {
        let db = self.db.clone();
        let tls_manager = self.tls_manager.clone();
        let mut stream = request.into_inner();

        let output = async_stream::stream! {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(client_msg) => {
                        let grpc = MonopsonyGrpc::new(db.clone(), tls_manager.clone());
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

    // ── Shadow package RPC stubs (Batch C) ──

    async fn list_shadow_packages(
        &self,
        _request: Request<ListShadowPackagesRequest>,
    ) -> Result<Response<ShadowPackageList>, Status> {
        Err(Status::unimplemented("list_shadow_packages not yet implemented"))
    }

    async fn register_shadow_package(
        &self,
        _request: Request<RegisterShadowPackageRequest>,
    ) -> Result<Response<ShadowPackageInfo>, Status> {
        Err(Status::unimplemented("register_shadow_package not yet implemented"))
    }

    async fn unregister_shadow_package(
        &self,
        _request: Request<UnregisterShadowPackageRequest>,
    ) -> Result<Response<UnregisterShadowPackageResponse>, Status> {
        Err(Status::unimplemented("unregister_shadow_package not yet implemented"))
    }

    async fn check_shadow_updates(
        &self,
        _request: Request<CheckShadowUpdatesRequest>,
    ) -> Result<Response<ShadowUpdateList>, Status> {
        Err(Status::unimplemented("check_shadow_updates not yet implemented"))
    }

    async fn apply_shadow_update(
        &self,
        _request: Request<ApplyShadowUpdateRequest>,
    ) -> Result<Response<ShadowUpdateResult>, Status> {
        Err(Status::unimplemented("apply_shadow_update not yet implemented"))
    }

    async fn rollback_shadow_update(
        &self,
        _request: Request<RollbackShadowUpdateRequest>,
    ) -> Result<Response<ShadowUpdateResult>, Status> {
        Err(Status::unimplemented("rollback_shadow_update not yet implemented"))
    }

    async fn get_shadow_recipe(
        &self,
        _request: Request<GetShadowRecipeRequest>,
    ) -> Result<Response<SignedShadowRecipe>, Status> {
        Err(Status::unimplemented("get_shadow_recipe not yet implemented"))
    }

    type DownloadShadowAssetStream = Pin<Box<dyn Stream<Item = Result<ShadowAssetChunk, Status>> + Send>>;

    async fn download_shadow_asset(
        &self,
        _request: Request<DownloadShadowAssetRequest>,
    ) -> Result<Response<Self::DownloadShadowAssetStream>, Status> {
        Err(Status::unimplemented("download_shadow_asset not yet implemented"))
    }
}

pub async fn start(
    addr: SocketAddr,
    db: Arc<Database>,
    tls_manager: Arc<TlsManager>,
    mirror_manager: Option<Arc<MirrorManager>>,
) -> Result<()> {
    info!(addr = %addr, "gRPC server starting (mTLS required)");

    let grpc = MonopsonyGrpc::new(db.clone(), tls_manager.clone());

    let tls_config = tls_manager.tonic_server_tls_config()?;

    let mut builder = tonic::transport::Server::builder()
        .tls_config(tls_config).context("Failed to configure TLS")?;

    if let Some(mm) = mirror_manager {
        let mirror_grpc = MirrorControlGrpc::new(mm);
        builder
            .add_service(MonopsonyControlServer::new(grpc))
            .add_service(
                monopsony_common::proto::monopsony::mirror_control_server::MirrorControlServer::new(mirror_grpc)
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
    ListMirrorsRequest, ListMirrorsResponse, MirrorInfo,
    TriggerMirrorSyncRequest, TriggerMirrorSyncResponse,
    GetMirrorStatusRequest, GetMirrorStatusResponse,
    UpsertMirrorConfigRequest, UpsertMirrorConfigResponse,
    DeleteMirrorConfigRequest, DeleteMirrorConfigResponse,
};

fn config_to_mirror_info(c: &crate::mirror::MirrorConfig) -> MirrorInfo {
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
impl monopsony_common::proto::monopsony::mirror_control_server::MirrorControl for MirrorControlGrpc {
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
            None => Err(Status::not_found(format!("Mirror not found: {}", req.distro))),
        }
    }

    async fn upsert_mirror_config(
        &self,
        request: Request<UpsertMirrorConfigRequest>,
    ) -> Result<Response<UpsertMirrorConfigResponse>, Status> {
        let req = request.into_inner();
        if req.distro.is_empty() || req.upstream_url.is_empty() {
            return Err(Status::invalid_argument("distro and upstream_url are required"));
        }

        // Check if this is a create or update
        let existing = self.mirror_manager.get_config(&req.distro).await;
        let is_create = existing.is_none();

        // Persist to DB if available
        if let Some(db) = self.mirror_manager.db() {
            let includes = serde_json::to_value(&req.include_patterns).unwrap_or_default();
            let excludes = serde_json::to_value(&req.exclude_patterns).unwrap_or_default();
            let config = crate::db::MirrorUpsertConfig {
                distro: req.distro.clone(),
                upstream_url: req.upstream_url.clone(),
                local_path: req.local_path.clone(),
                sync_enabled: req.sync_enabled,
                sync_interval_secs: req.sync_interval_secs,
                bandwidth_limit_bps: if req.bandwidth_limit_bps > 0 { Some(req.bandwidth_limit_bps) } else { None },
                include_patterns: includes,
                exclude_patterns: excludes,
                public: req.public,
            };
            db.upsert_mirror_config(&config).await.map_err(|e| Status::internal(format!("DB error: {}", e)))?;
        }

        // Update in-memory config
        let mut new_config = crate::mirror::MirrorConfig {
            distro: req.distro.clone(),
            upstream_url: req.upstream_url.clone(),
            local_path: std::path::PathBuf::from(&req.local_path),
            sync_enabled: req.sync_enabled,
            sync_interval_secs: req.sync_interval_secs as u64,
            bandwidth_limit_bps: if req.bandwidth_limit_bps > 0 { Some(req.bandwidth_limit_bps as u64) } else { None },
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
            db.delete_mirror_config(&req.distro).await
                .map_err(|e| Status::internal(format!("DB error: {}", e)))?;
        }

        let removed = self.mirror_manager.remove_config(&req.distro).await;
        info!("gRPC: delete mirror config for {} (removed={})", req.distro, removed);
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
