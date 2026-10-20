use monopsony_common::package_manager::PackageRef;
use monopsony_common::proto::monopsony::monopsony_control_client::MonopsonyControlClient;
use monopsony_common::proto::monopsony::{
    client_message, ApplyShadowUpdateRequest, CheckShadowUpdatesRequest,
    DownloadShadowAssetRequest, GetShadowRecipeRequest, HeartbeatRequest, InventoryReport,
    ListShadowCatalogRequest, ListShadowPackagesRequest, PackageEntry, PairRequest, PairResponse,
    RegisterRequest, RegisterShadowPackageRequest, RenewClientCertRequest,
    RollbackShadowUpdateRequest, ShadowCatalogEntry,
    ShadowPackageInfo, ShadowScanFinding, ShadowScanReport, ShadowUpdateInfo,
    UnregisterShadowPackageRequest,
};
use anyhow::Context;
use std::path::Path;
use tonic::transport::Channel;
use tracing::{debug, info};

use crate::ws_transport::WsTransport;

/// Pairing client deadlines. A server that accepts the TCP connection but
/// never completes the protobuf exchange must surface an error, not an
/// infinite hang — observed 2026-10-17 when a server-side mirror-lock
/// deadlock froze the pairing response after client registration.
const PAIR_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PAIR_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Cap on the pairing response length prefix — parity with the server's
/// request cap and the WebSocket pairing path's sanity check.
const PAIR_RESPONSE_MAX: usize = 65536;

/// Once a paired client's 90-day cert has 30 days or less of validity
/// remaining, the daemon automatically re-issues it over mTLS — no human
/// re-pairing required (P1 cert lifecycle, plan D4).
pub const CERT_RENEWAL_WINDOW_DAYS: u64 = 30;

pub struct MonopsonyGrpcClient {
    client: MonopsonyControlClient<Channel>,
    client_id: Option<String>,
}

/// Stored client certificates
pub struct ClientCerts {
    pub client_cert_pem: Vec<u8>,
    pub client_key_pem: Vec<u8>,
    pub ca_cert_pem: Vec<u8>,
}

impl ClientCerts {
    /// Load certificates from a directory
    pub fn load(cert_dir: &Path) -> Option<Self> {
        let client_cert = std::fs::read(cert_dir.join("client.crt")).ok()?;
        let client_key = std::fs::read(cert_dir.join("client.key")).ok()?;
        let ca_cert = std::fs::read(cert_dir.join("ca.crt")).ok()?;

        Some(Self {
            client_cert_pem: client_cert,
            client_key_pem: client_key,
            ca_cert_pem: ca_cert,
        })
    }

    /// Save certificates to a directory
    pub fn save(&self, cert_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(cert_dir)?;
        std::fs::write(cert_dir.join("client.crt"), &self.client_cert_pem)?;
        std::fs::write(cert_dir.join("client.key"), &self.client_key_pem)?;
        std::fs::write(cert_dir.join("ca.crt"), &self.ca_cert_pem)?;

        // Set restrictive permissions on the key
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                cert_dir.join("client.key"),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }

        info!("Client certificates saved to {}", cert_dir.display());
        Ok(())
    }

    /// Expiry (`not_after`) of the client certificate, if parseable.
    pub fn expiry(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        crate::trust::client_cert_expiry(&self.client_cert_pem)
    }

    /// Hex serial of the client certificate, in the server's encoding.
    pub fn serial_hex(&self) -> Option<String> {
        crate::trust::client_cert_serial_hex(&self.client_cert_pem)
    }

    /// Save certificates atomically: all three files are staged under temp
    /// names and only renamed into place once every write succeeds, so a
    /// crash mid-rotation never leaves a mismatched cert/key pair on disk.
    pub fn save_atomic(&self, cert_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(cert_dir)?;

        let entries = [
            (
                cert_dir.join("client.crt"),
                cert_dir.join("client.crt.tmp"),
                self.client_cert_pem.as_slice(),
            ),
            (
                cert_dir.join("client.key"),
                cert_dir.join("client.key.tmp"),
                self.client_key_pem.as_slice(),
            ),
            (
                cert_dir.join("ca.crt"),
                cert_dir.join("ca.crt.tmp"),
                self.ca_cert_pem.as_slice(),
            ),
        ];

        // Stage every temp file before renaming anything: a write failure
        // here leaves the existing cert files untouched.
        for (_, tmp, content) in &entries {
            std::fs::write(tmp, content)?;
        }
        // Restrictive permissions on the key before it goes live.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &entries[1].1,
                std::fs::Permissions::from_mode(0o600),
            )?;
        }

        // Rename into place — each rename is atomic on the same filesystem.
        for (target, tmp, _) in &entries {
            std::fs::rename(tmp, target)?;
        }

        info!("Client certificates rotated atomically into {}", cert_dir.display());
        Ok(())
    }
}

impl MonopsonyGrpcClient {
    pub async fn connect(
        addr: &str,
        tls: Option<tonic::transport::ClientTlsConfig>,
    ) -> anyhow::Result<Self> {
        info!(
            addr = addr,
            tls_enabled = tls.is_some(),
            "Connecting to monopsony server via gRPC"
        );

        let endpoint = if let Some(tls_config) = tls {
            let uri = if addr.starts_with("https://") {
                addr.to_string()
            } else {
                format!("https://{}", addr)
            };
            Channel::from_shared(uri)?.tls_config(tls_config)?
        } else {
            let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
                addr.to_string()
            } else {
                format!("http://{}", addr)
            };
            Channel::from_shared(uri)?
        };

        // Connect the channel DIRECTLY — do NOT pass the Endpoint through
        // MonopsonyControlClient::connect(), because tonic's generated connect()
        // calls Endpoint::new() internally, which overwrites any custom TLS config
        // with system roots when it detects an HTTPS URI (see tonic-0.12 endpoint.rs:53).
        let channel = endpoint.connect().await?;

        let client = MonopsonyControlClient::new(channel);
        info!("gRPC connection established");

        Ok(Self {
            client,
            client_id: None,
        })
    }

    /// Connect via WebSocket tunnel — for use behind Cloudflare Tunnels or
    /// reverse proxies that terminate TLS. Opens a WSS connection, then runs
    /// the existing mTLS config as an inner TLS layer inside the WebSocket.
    ///
    /// `ws_url` is the full WebSocket URL, e.g. `wss://monopsony.example.com/ws/tunnel`.
    /// `tls` is the same Monopsony CA + client cert config used for direct connections.
    pub async fn connect_ws(
        ws_url: &str,
        tls: Option<tonic::transport::ClientTlsConfig>,
    ) -> anyhow::Result<Self> {
        info!(url = ws_url, "Connecting via WebSocket tunnel");

        // For the WebSocket path, we build a tonic Endpoint with a custom
        // connector that:
        //   1. Opens a WebSocket to ws_url
        //   2. (Optional) Wraps it with inner TLS (Monopsony CA + client cert)
        //
        // The Endpoint's URI must be https:// for tonic to accept TLS config,
        // but the actual transport is our WebSocket — the URI is just a hint.
        let ws_url_owned = ws_url.to_string();

        let mut endpoint = Channel::from_shared("https://ws-tunnel".to_string())?;

        if let Some(tls_config) = tls {
            endpoint = endpoint.tls_config(tls_config)?;
        }

        let channel = endpoint
            .connect_with_connector(tower::service_fn(move |_| {
                let url = ws_url_owned.clone();
                async move {
                    // Open WebSocket connection
                    let ws = WsTransport::connect(&url).await.map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            format!("WebSocket connect failed: {}", e),
                        )
                    })?;
                    // tokio-tungstenite's connect_async already handles WSS TLS
                    // for the outer layer. If inner TLS is needed, it's applied
                    // via tonic's tls_config on the Endpoint, which wraps our
                    // raw stream. But since tonic's connector pattern expects
                    // a raw IO stream, we return the WebSocket transport directly.
                    //
                    // The inner mTLS is handled by the tonic Endpoint's tls_config
                    // which layers TLS on top of our connector's output.
                    Ok::<_, std::io::Error>(ws)
                }
            }))
            .await?;

        let client = MonopsonyControlClient::new(channel);
        info!("WebSocket tunnel connection established");

        Ok(Self {
            client,
            client_id: None,
        })
    }

    pub async fn register(
        &mut self,
        hostname: &str,
        os: &str,
        os_version: &str,
        package_managers: &[String],
    ) -> anyhow::Result<(String, Option<ClientCerts>, Vec<u8>)> {
        let req = RegisterRequest {
            hostname: hostname.to_string(),
            os: os.to_string(),
            os_version: os_version.to_string(),
            package_managers: package_managers.to_vec(),
        };

        info!(
            hostname = hostname,
            os = os,
            os_version = os_version,
            managers = ?package_managers,
            "Registering with server"
        );

        let resp = self.client.register(req).await?;
        let resp = resp.into_inner();

        info!(
            client_id = %resp.client_id,
            enabled_managers = ?resp.enabled_managers,
            received_cert = !resp.client_cert_pem.is_empty(),
            "Registered successfully"
        );

        self.client_id = Some(resp.client_id.clone());

        // Extract certs if provided
        let certs = if !resp.client_cert_pem.is_empty() {
            Some(ClientCerts {
                client_cert_pem: resp.client_cert_pem,
                client_key_pem: resp.client_key_pem,
                ca_cert_pem: resp.ca_cert_pem.as_bytes().to_vec(),
            })
        } else {
            None
        };

        Ok((resp.client_id, certs, resp.mirror_verify_key))
    }

    /// Request a freshly CA-signed cert/key pair for `client_id` over the
    /// live mTLS channel (P1 renewal). The server binds issuance to the
    /// certificate presented on the connection, so this re-issues without
    /// any pairing code or human involvement. `current_serial` is the hex
    /// serial of the cert presented via mTLS, sent as a hint the server
    /// verifies against the peer certificate.
    ///
    /// Returns the new cert bundle and its expiry. The caller is
    /// responsible for persisting (`ClientCerts::save_atomic`) and rotating
    /// the live TLS config.
    pub async fn renew_client_cert(
        &mut self,
        client_id: &str,
        current_serial: Option<&str>,
    ) -> anyhow::Result<(ClientCerts, chrono::DateTime<chrono::Utc>)> {
        let req = RenewClientCertRequest {
            client_id: client_id.to_string(),
            current_serial: current_serial.unwrap_or_default().to_string(),
        };

        info!(
            client_id = client_id,
            current_serial = ?current_serial,
            "Requesting certificate re-issue over mTLS"
        );

        let resp = self.client.renew_client_cert(req).await?;
        let resp = resp.into_inner();

        let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(resp.expires_at, 0)
            .context("server returned invalid expires_at")?;

        let certs = ClientCerts {
            client_cert_pem: resp.client_cert_pem,
            client_key_pem: resp.client_key_pem,
            ca_cert_pem: resp.ca_cert_pem.as_bytes().to_vec(),
        };

        info!(
            client_id = client_id,
            expiry = %expires_at,
            "Client certificate re-issued by server over mTLS"
        );

        Ok((certs, expires_at))
    }

    // ─── Pairing v2 / Sunshine: HTTP-based submit → poll → collect ───────

    /// Submit a v2 pairing request over HTTP (pairing_mode = pin).
    /// Generates nothing here — the caller supplies the ephemeral pubkey and
    /// salted PIN hash. Returns the server-assigned request_id and expiry.
    pub async fn submit_pair_request_v2(
        base_url: &str,
        ephemeral_pubkey_hex: &str,
        pin_hash: &str,
        pin_salt: &str,
        claimed_hostname: &str,
        claimed_os: &str,
        claimed_os_version: &str,
        package_managers: &[String],
    ) -> anyhow::Result<(String, i64)> {
        #[derive(serde::Serialize)]
        struct PairRequestV2 {
            ephemeral_pubkey: String,
            pin_hash: String,
            pin_salt: String,
            claimed_hostname: String,
            claimed_os: String,
            claimed_os_version: String,
            package_managers: Vec<String>,
        }

        #[derive(serde::Deserialize)]
        struct PairRequestV2Response {
            request_id: String,
            expires_at: i64,
        }

        let url = format!("{}/api/v1/pair/request", base_url.trim_end_matches('/'));
        let resp: PairRequestV2Response = reqwest::Client::new()
            .post(&url)
            .json(&PairRequestV2 {
                ephemeral_pubkey: ephemeral_pubkey_hex.to_string(),
                pin_hash: pin_hash.to_string(),
                pin_salt: pin_salt.to_string(),
                claimed_hostname: claimed_hostname.to_string(),
                claimed_os: claimed_os.to_string(),
                claimed_os_version: claimed_os_version.to_string(),
                package_managers: package_managers.to_vec(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        info!(
            request_id = %resp.request_id,
            expires_at = resp.expires_at,
            "Pairing v2 request submitted"
        );
        Ok((resp.request_id, resp.expires_at))
    }

    /// Poll the status of a v2 pairing request. Returns the status string
    /// ("pending" | "approved" | "collected" | "expired" | "invalidated").
    pub async fn poll_pair_request_status(
        base_url: &str,
        request_id: &str,
    ) -> anyhow::Result<String> {
        #[derive(serde::Deserialize)]
        struct PairRequestStatusResponse {
            status: String,
        }

        let url = format!(
            "{}/api/v1/pair/request/{}/status",
            base_url.trim_end_matches('/'),
            request_id
        );
        let resp: PairRequestStatusResponse = reqwest::Client::new()
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp.status)
    }

    /// Collect a v2 pairing certificate with proof of possession.
    /// Signs `request_id` bytes with the ephemeral private key and presents
    /// the signature to the server.
    pub async fn collect_pair_cert_v2(
        base_url: &str,
        request_id: &str,
        ephemeral_pubkey_bytes: &[u8],
        signature_bytes: &[u8],
    ) -> anyhow::Result<(String, ClientCerts)> {
        #[derive(serde::Serialize)]
        struct CollectPairRequestV2 {
            request_id: String,
            ephemeral_pubkey: String,
            signature: String,
        }

        #[derive(serde::Deserialize)]
        struct CollectPairResponseV2 {
            collected: bool,
            client_id: String,
            client_cert_pem: String,
            client_key_pem: String,
            ca_cert_pem: String,
            grpc_addr: String,
            ws_url: String,
        }

        let url = format!("{}/api/v1/pair/collect", base_url.trim_end_matches('/'));
        let resp: CollectPairResponseV2 = reqwest::Client::new()
            .post(&url)
            .json(&CollectPairRequestV2 {
                request_id: request_id.to_string(),
                ephemeral_pubkey: hex::encode(ephemeral_pubkey_bytes),
                signature: hex::encode(signature_bytes),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if !resp.collected {
            anyhow::bail!("Server did not complete collection");
        }

        let certs = ClientCerts {
            client_cert_pem: resp.client_cert_pem.into_bytes(),
            client_key_pem: resp.client_key_pem.into_bytes(),
            ca_cert_pem: resp.ca_cert_pem.into_bytes(),
        };

        info!(
            client_id = %resp.client_id,
            request_id = %request_id,
            "Pairing v2 collect complete"
        );
        Ok((resp.client_id, certs))
    }

    /// Enroll via a pre-approval key (P5). Signs `monopsony-enroll-v1` with
    /// the ephemeral private key as the possession proof; on success the
    /// server registers the client and returns the cert bundle.
    pub async fn enroll_pair_v2(
        base_url: &str,
        enrollment_key: &str,
        ephemeral_pubkey_bytes: &[u8],
        signature_bytes: &[u8],
        claimed_hostname: &str,
        claimed_os: &str,
        claimed_os_version: &str,
        package_managers: &[String],
    ) -> anyhow::Result<(String, ClientCerts)> {
        #[derive(serde::Serialize)]
        struct EnrollPairRequest {
            enrollment_key: String,
            ephemeral_pubkey: String,
            signature: String,
            claimed_hostname: String,
            claimed_os: String,
            claimed_os_version: String,
            package_managers: Vec<String>,
        }

        #[derive(serde::Deserialize)]
        struct EnrollPairResponse {
            enrolled: bool,
            client_id: String,
            client_cert_pem: String,
            client_key_pem: String,
            ca_cert_pem: String,
        }

        let url = format!("{}/api/v1/pair/enroll", base_url.trim_end_matches('/'));
        let resp: EnrollPairResponse = reqwest::Client::new()
            .post(&url)
            .json(&EnrollPairRequest {
                enrollment_key: enrollment_key.to_string(),
                ephemeral_pubkey: hex::encode(ephemeral_pubkey_bytes),
                signature: hex::encode(signature_bytes),
                claimed_hostname: claimed_hostname.to_string(),
                claimed_os: claimed_os.to_string(),
                claimed_os_version: claimed_os_version.to_string(),
                package_managers: package_managers.to_vec(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if !resp.enrolled {
            anyhow::bail!("Server did not complete enrollment");
        }

        let certs = ClientCerts {
            client_cert_pem: resp.client_cert_pem.into_bytes(),
            client_key_pem: resp.client_key_pem.into_bytes(),
            ca_cert_pem: resp.ca_cert_pem.into_bytes(),
        };

        info!(
            client_id = %resp.client_id,
            "Enrollment-key pairing complete"
        );
        Ok((resp.client_id, certs))
    }

    pub async fn heartbeat(
        &mut self,
        client_id: &str,
        health: &crate::update_health::UpdateHealthSnapshot,
    ) -> anyhow::Result<i64> {
        let req = HeartbeatRequest {
            client_id: client_id.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            // Sprint 27 Part B: windowed update-failure snapshot (counts-only).
            failed_updates_window: health.failed_updates_window,
            last_update_attempt_ts: health.last_update_attempt_ts,
            window_start_ts: health.window_start_ts,
        };

        debug!(client_id = client_id, "Sending heartbeat");

        let resp = self.client.heartbeat(req).await?;
        let server_time = resp.into_inner().server_time;
        debug!(server_time = server_time, "Heartbeat acknowledged");

        Ok(server_time)
    }

    /// One-shot ControlStream carrying client TamperAlerts (Sprint 20
    /// Phase 3). The daemon drains its tamper-event spool into this; each
    /// alert rides the ClientMessage oneof (TamperAlert has no unary RPC).
    /// The server acks each message with a SyncRequest; replies are drained
    /// with a bounded timeout so a wedged server cannot hang the heartbeat.
    pub async fn report_tamper_alerts(
        &mut self,
        alerts: &[monopsony_common::proto::monopsony::TamperAlert],
    ) -> anyhow::Result<()> {
        use futures_util::StreamExt;

        if alerts.is_empty() {
            return Ok(());
        }

        // Owned copy: tonic streaming requests are 'static, so the stream
        // cannot borrow the caller's slice.
        let messages: Vec<monopsony_common::proto::monopsony::ClientMessage> = alerts
            .iter()
            .map(|alert| monopsony_common::proto::monopsony::ClientMessage {
                msg: Some(client_message::Msg::TamperAlert(alert.clone())),
            })
            .collect();
        let outbound = futures_util::stream::iter(messages);

        let mut inbound = self
            .client
            .control_stream(tonic::Request::new(outbound))
            .await?
            .into_inner();

        let drain = async {
            while let Some(msg) = inbound.next().await {
                if msg.is_err() {
                    break;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), drain)
            .await
            .map_err(|_| anyhow::anyhow!("timed out draining tamper-alert acks"))?;

        Ok(())
    }

    /// Poll for server-pushed commands (Sprint 22 slice 22-E2b).
    /// Opens a one-shot ControlStream carrying a single Heartbeat
    /// ClientMessage; the server may yield a Command (e.g. shadow_assign)
    /// instead of/alongside its ack. Drains replies with a bounded timeout
    /// like report_tamper_alerts and returns the FIRST Command found.
    pub async fn poll_pending_commands(
        &mut self,
        client_id: &str,
        health: &crate::update_health::UpdateHealthSnapshot,
    ) -> anyhow::Result<Option<monopsony_common::proto::monopsony::Command>> {
        use futures_util::StreamExt;
        use monopsony_common::proto::monopsony::{client_message, server_message, ClientMessage};

        let message = ClientMessage {
            msg: Some(client_message::Msg::Heartbeat(HeartbeatRequest {
                client_id: client_id.to_string(),
                timestamp: chrono::Utc::now().timestamp(),
                // Sprint 27 Part B: same windowed snapshot as the unary
                // heartbeat — this stream heartbeat hits the same handler.
                failed_updates_window: health.failed_updates_window,
                last_update_attempt_ts: health.last_update_attempt_ts,
                window_start_ts: health.window_start_ts,
            })),
        };
        let outbound = futures_util::stream::iter(vec![message]);

        let mut inbound = self
            .client
            .control_stream(tonic::Request::new(outbound))
            .await?
            .into_inner();

        let mut found: Option<monopsony_common::proto::monopsony::Command> = None;
        let drain = async {
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(server) => {
                        if let Some(server_message::Msg::Command(cmd)) = server.msg {
                            if found.is_none() {
                                found = Some(cmd);
                            }
                        }
                        // Keep draining acks; stream closes from the server side.
                    }
                    Err(_) => break,
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), drain).await;

        Ok(found)
    }

    pub async fn report_inventory(
        &mut self,
        client_id: &str,
        packages: &[PackageRef],
        full_sync: bool,
        report_telemetry: bool,
    ) -> anyhow::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let entries: Vec<PackageEntry> = packages
            .iter()
            .map(|p| PackageEntry {
                manager: p.manager.as_str().to_string(),
                name: p.name.clone(),
                version: p.version.clone().unwrap_or_default(),
                arch: p.architecture.clone().unwrap_or_default(),
                installed_at: if report_telemetry { now } else { 0 },
                install_source: if report_telemetry {
                    "proxy".to_string()
                } else {
                    String::new()
                },
            })
            .collect();

        let report = InventoryReport {
            client_id: client_id.to_string(),
            packages: entries.clone(),
            full_sync,
        };

        info!(
            client_id = client_id,
            package_count = entries.len(),
            full_sync = full_sync,
            "Reporting inventory"
        );

        let resp = self.client.report_inventory(report).await?;
        let resp = resp.into_inner();
        info!(accepted = resp.accepted, message = %resp.message, "Inventory response");

        Ok(())
    }

    #[allow(dead_code)] // Will be used by Status subcommand
    pub fn is_connected(&self) -> bool {
        self.client_id.is_some()
    }

    /// Perform pairing exchange over plaintext TCP on port 50052.
    /// Returns (client_id, ClientCerts, grpc_addr, ws_url, mirror_verify_key) on success.
    /// `ws_url` is `Some` when the server is configured for WebSocket tunneling.
    /// `mirror_verify_key` is the server's 32-byte Ed25519 pin-attestation key
    /// (empty/zeroed when pinning is not provisioned).
    pub async fn pair(
        server_addr: &str,
        code: &str,
        hostname: &str,
        os: &str,
        os_version: &str,
        package_managers: &[String],
    ) -> anyhow::Result<(String, ClientCerts, String, Option<String>, Vec<u8>)> {
        use prost::Message;
        use tokio::io::AsyncWriteExt;

        let pair_addr = if server_addr.ends_with(":50052") {
            server_addr.to_string()
        } else {
            format!("{}:50052", server_addr.trim_end_matches(":50051"))
        };
        let mut stream = tokio::time::timeout(
            PAIR_CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(&pair_addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Timed out connecting to pairing port {}", pair_addr))?
        .map_err(|e| anyhow::anyhow!("Failed to connect to pairing port {}: {}", pair_addr, e))?;

        let req = PairRequest {
            code: code.to_string(),
            hostname: hostname.to_string(),
            os: os.to_string(),
            os_version: os_version.to_string(),
            package_managers: package_managers.to_vec(),
            // v2 fields — empty for legacy code-based pairing
            ephemeral_pubkey: Vec::new(),
            pin_hash: String::new(),
            pin_salt: String::new(),
            claimed_hostname: String::new(),
            claimed_os: String::new(),
            claimed_os_version: String::new(),
        };
        let req_bytes = req.encode_to_vec();
        let len = req_bytes.len() as u32;
        stream.write_all(&len.to_le_bytes()).await?;
        stream.write_all(&req_bytes).await?;

        let resp_buf = tokio::time::timeout(PAIR_RESPONSE_TIMEOUT, read_pair_response(&mut stream))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Timed out after {:?} waiting for the pairing response from {} \
                         — the server accepted the connection but never completed the exchange",
                    PAIR_RESPONSE_TIMEOUT,
                    pair_addr
                )
            })??;

        let resp = PairResponse::decode(resp_buf.as_slice())
            .map_err(|e| anyhow::anyhow!("Failed to decode pair response: {}", e))?;

        let certs = ClientCerts {
            client_cert_pem: resp.client_cert_pem,
            client_key_pem: resp.client_key_pem,
            ca_cert_pem: resp.ca_cert_pem,
        };

        let grpc_addr = if !resp.grpc_addr.is_empty() {
            resp.grpc_addr
        } else {
            let host = server_addr.split(':').next().unwrap_or(server_addr);
            format!("{}:50051", host)
        };

        info!(
            "Paired successfully, client_id={}, grpc_addr={}",
            resp.client_id, grpc_addr
        );
        let ws_url = if resp.ws_url.is_empty() {
            None
        } else {
            Some(resp.ws_url)
        };
        Ok((
            resp.client_id,
            certs,
            grpc_addr,
            ws_url,
            resp.mirror_verify_key,
        ))
    }

    /// Perform pairing exchange over a WebSocket tunnel.
    ///
    /// Used when the server is behind Cloudflare Tunnels or a reverse proxy
    /// that terminates TLS — the plaintext pairing exchange (length-prefixed
    /// protobuf) is sent as binary WebSocket frames through the server's
    /// `/ws/pair` relay endpoint, which forwards them to the local pairing
    /// listener on port 50052.
    ///
    /// `ws_pair_url` is the full WebSocket URL, e.g.
    /// `wss://monopsony.example.com/ws/pair`.
    ///
    /// Returns (client_id, ClientCerts, grpc_addr, ws_url, mirror_verify_key) on success.
    pub async fn pair_ws(
        ws_pair_url: &str,
        code: &str,
        hostname: &str,
        os: &str,
        os_version: &str,
        package_managers: &[String],
    ) -> anyhow::Result<(String, ClientCerts, String, Option<String>, Vec<u8>)> {
        use futures_util::{SinkExt, StreamExt};
        use prost::Message;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        info!(
            url = ws_pair_url,
            "Connecting to pairing endpoint via WebSocket tunnel"
        );

        let (ws_stream, response) = tokio_tungstenite::connect_async(ws_pair_url)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket pairing connect failed: {}", e))?;

        debug!(status = %response.status(), "Pairing WS connected");

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Build the PairRequest as length-prefixed protobuf, exactly the same
        // wire format as the TCP pairing exchange: [u32 LE length][protobuf bytes]
        let req = PairRequest {
            code: code.to_string(),
            hostname: hostname.to_string(),
            os: os.to_string(),
            os_version: os_version.to_string(),
            package_managers: package_managers.to_vec(),
            // v2 fields — empty for legacy code-based pairing
            ephemeral_pubkey: Vec::new(),
            pin_hash: String::new(),
            pin_salt: String::new(),
            claimed_hostname: String::new(),
            claimed_os: String::new(),
            claimed_os_version: String::new(),
        };
        let req_bytes = req.encode_to_vec();
        let len = req_bytes.len() as u32;

        let mut frame = len.to_le_bytes().to_vec();
        frame.extend_from_slice(&req_bytes);

        ws_tx
            .send(WsMessage::Binary(frame))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send pairing request over WS: {}", e))?;

        // Receive the PairResponse. The server sends [u32 LE length][protobuf bytes]
        // as a single binary frame. Accumulate until we have both the length header
        // and the full payload.
        let mut buf = Vec::with_capacity(512);
        let resp = loop {
            match ws_rx.next().await {
                Some(Ok(WsMessage::Binary(data))) => {
                    buf.extend_from_slice(&data);

                    // Need at least 4 bytes for the length prefix
                    if buf.len() < 4 {
                        continue;
                    }

                    let resp_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

                    // Sanity check
                    if resp_len > 65536 {
                        anyhow::bail!("Pairing response too large: {} bytes", resp_len);
                    }

                    if buf.len() < 4 + resp_len {
                        // Need more data — the relay may split across frames
                        continue;
                    }

                    let resp_bytes = &buf[4..4 + resp_len];
                    break PairResponse::decode(resp_bytes)
                        .map_err(|e| anyhow::anyhow!("Failed to decode pair response: {}", e))?;
                }
                Some(Ok(WsMessage::Close(_))) => {
                    anyhow::bail!("Pairing WebSocket closed before response received");
                }
                Some(Ok(_)) => {
                    // Ping/Pong/Text — ignore and keep waiting
                    continue;
                }
                Some(Err(e)) => {
                    anyhow::bail!("Pairing WebSocket error: {}", e);
                }
                None => {
                    anyhow::bail!("Pairing WebSocket stream ended before response received");
                }
            }
        };

        // Close the WebSocket cleanly now that we have the response
        let _ = ws_tx.close().await;

        let certs = ClientCerts {
            client_cert_pem: resp.client_cert_pem,
            client_key_pem: resp.client_key_pem,
            ca_cert_pem: resp.ca_cert_pem,
        };

        // The grpc_addr from the server tells us where to connect for ongoing
        // control-plane traffic. When pairing over WS, the server should return
        // a wss:// or ws:// URL here (Phase 4); if it doesn't, fall back to
        // deriving a direct TCP address from the WS URL host.
        let grpc_addr = if !resp.grpc_addr.is_empty() {
            resp.grpc_addr
        } else {
            // Extract host from the ws/wss URL
            let host = ws_pair_url
                .strip_prefix("wss://")
                .or_else(|| ws_pair_url.strip_prefix("ws://"))
                .unwrap_or(ws_pair_url)
                .split('/')
                .next()
                .unwrap_or("127.0.0.1:50051");
            format!("wss://{}/ws/tunnel", host)
        };

        info!(
            "Paired via WS, client_id={}, grpc_addr={}",
            resp.client_id, grpc_addr
        );
        let ws_url = if resp.ws_url.is_empty() {
            None
        } else {
            Some(resp.ws_url)
        };
        Ok((
            resp.client_id,
            certs,
            grpc_addr,
            ws_url,
            resp.mirror_verify_key,
        ))
    }

    #[allow(dead_code)] // Will be used by Status subcommand
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    // ── Shadow package RPCs (Sprint 22 slice 22-A) ──
    // Thin transport: build the proto request, call the RPC, map Status errors
    // through anyhow, same as register()/heartbeat() above.

    /// List shadow packages registered for this client.
    #[allow(dead_code)]
    pub async fn list_shadow_packages(
        &mut self,
        client_id: &str,
    ) -> anyhow::Result<Vec<ShadowPackageInfo>> {
        let req = ListShadowPackagesRequest {
            client_id: client_id.to_string(),
        };
        debug!(client_id = client_id, "Listing shadow packages");
        let resp = self.client.list_shadow_packages(req).await?;
        let packages = resp.into_inner().packages;
        debug!(count = packages.len(), "Shadow package list received");
        Ok(packages)
    }

    /// Register a shadow package from a recipe TOML.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub async fn register_shadow_package(
        &mut self,
        client_id: &str,
        name: &str,
        version: &str,
        architecture: &str,
        os_family: &str,
        libc: &str,
        source_type: &str,
        source_url: &str,
        recipe_toml: &str,
        install_paths: &[String],
        config_paths: &[String],
        service_name: &str,
        mask_from_pm: bool,
    ) -> anyhow::Result<ShadowPackageInfo> {
        let req = RegisterShadowPackageRequest {
            client_id: client_id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            architecture: architecture.to_string(),
            os_family: os_family.to_string(),
            libc: libc.to_string(),
            source_type: source_type.to_string(),
            source_url: source_url.to_string(),
            recipe_toml: recipe_toml.to_string(),
            install_paths: install_paths.to_vec(),
            config_paths: config_paths.to_vec(),
            service_name: service_name.to_string(),
            mask_from_pm,
        };
        info!(
            client_id = client_id,
            name = name,
            "Registering shadow package"
        );
        let resp = self.client.register_shadow_package(req).await?;
        Ok(resp.into_inner())
    }

    /// Remove a shadow package registration.
    #[allow(dead_code)]
    pub async fn unregister_shadow_package(
        &mut self,
        client_id: &str,
        package_id: &str,
        unmask_from_pm: bool,
    ) -> anyhow::Result<(bool, String)> {
        let req = UnregisterShadowPackageRequest {
            client_id: client_id.to_string(),
            package_id: package_id.to_string(),
            unmask_from_pm,
        };
        info!(
            client_id = client_id,
            package_id = package_id,
            "Unregistering shadow package"
        );
        let resp = self.client.unregister_shadow_package(req).await?;
        let resp = resp.into_inner();
        Ok((resp.deleted, resp.message))
    }

    /// Check for available updates. Empty `package_ids` checks all packages.
    #[allow(dead_code)]
    pub async fn check_shadow_updates(
        &mut self,
        client_id: &str,
        package_ids: &[String],
    ) -> anyhow::Result<Vec<ShadowUpdateInfo>> {
        let req = CheckShadowUpdatesRequest {
            client_id: client_id.to_string(),
            package_ids: package_ids.to_vec(),
        };
        debug!(
            client_id = client_id,
            count = package_ids.len(),
            "Checking shadow updates"
        );
        let resp = self.client.check_shadow_updates(req).await?;
        let updates = resp.into_inner().updates;
        debug!(count = updates.len(), "Shadow update check complete");
        Ok(updates)
    }

    /// Fetch the signed recipe TOML for a package.
    /// Returns (recipe_toml, key_id, signature, signed_at).
    #[allow(dead_code)]
    pub async fn get_shadow_recipe(
        &mut self,
        client_id: &str,
        package_name: &str,
    ) -> anyhow::Result<(String, String, String, i64)> {
        let req = GetShadowRecipeRequest {
            client_id: client_id.to_string(),
            package_name: package_name.to_string(),
        };
        debug!(
            client_id = client_id,
            package_name = package_name,
            "Fetching shadow recipe"
        );
        let resp = self.client.get_shadow_recipe(req).await?;
        let resp = resp.into_inner();
        Ok((
            resp.recipe_toml,
            resp.key_id,
            resp.signature,
            resp.signed_at,
        ))
    }

    /// Ask the server to apply an update to a shadow package.
    /// Returns (success, message, new_version, backup_path).
    #[allow(dead_code)]
    pub async fn apply_shadow_update(
        &mut self,
        client_id: &str,
        package_id: &str,
        target_version: &str,
        asset_url: &str,
        create_backup: bool,
    ) -> anyhow::Result<(bool, String, String, String)> {
        let req = ApplyShadowUpdateRequest {
            client_id: client_id.to_string(),
            package_id: package_id.to_string(),
            target_version: target_version.to_string(),
            asset_url: asset_url.to_string(),
            create_backup,
        };
        info!(
            client_id = client_id,
            package_id = package_id,
            target_version = target_version,
            "Applying shadow update"
        );
        let resp = self.client.apply_shadow_update(req).await?;
        let resp = resp.into_inner();
        Ok((
            resp.success,
            resp.message,
            resp.new_version,
            resp.backup_path,
        ))
    }

    /// Ask the server to roll back a shadow package to its previous version.
    /// Returns (success, message, new_version, backup_path).
    #[allow(dead_code)]
    pub async fn rollback_shadow_update(
        &mut self,
        client_id: &str,
        package_id: &str,
    ) -> anyhow::Result<(bool, String, String, String)> {
        let req = RollbackShadowUpdateRequest {
            client_id: client_id.to_string(),
            package_id: package_id.to_string(),
        };
        info!(
            client_id = client_id,
            package_id = package_id,
            "Rolling back shadow update"
        );
        let resp = self.client.rollback_shadow_update(req).await?;
        let resp = resp.into_inner();
        Ok((
            resp.success,
            resp.message,
            resp.new_version,
            resp.backup_path,
        ))
    }

    /// Download a shadow package asset via server-side streaming.
    /// Chunks are collected in order; each chunk carrying a non-empty
    /// blob_hash is verified with SHA-256 before its bytes are accepted.
    #[allow(dead_code)]
    pub async fn download_shadow_asset(
        &mut self,
        client_id: &str,
        package_id: &str,
        target_version: &str,
    ) -> anyhow::Result<Vec<u8>> {
        use futures_util::StreamExt;
        use sha2::{Digest, Sha256};

        let req = DownloadShadowAssetRequest {
            client_id: client_id.to_string(),
            package_id: package_id.to_string(),
            target_version: target_version.to_string(),
        };
        info!(
            client_id = client_id,
            package_id = package_id,
            target_version = target_version,
            "Downloading shadow asset"
        );

        let mut inbound = self.client.download_shadow_asset(req).await?.into_inner();

        let mut blob: Vec<u8> = Vec::new();
        let mut expected_total: i64 = 0;
        while let Some(chunk) = inbound.next().await {
            let chunk = chunk.map_err(|e| anyhow::anyhow!("shadow asset stream failed: {}", e))?;

            if !chunk.blob_hash.is_empty() {
                let mut hasher = Sha256::new();
                hasher.update(&chunk.data);
                let actual = hex::encode(hasher.finalize());
                if actual != chunk.blob_hash.to_lowercase() {
                    anyhow::bail!(
                        "shadow asset chunk hash mismatch at offset {}: expected {}, got {}",
                        chunk.offset,
                        chunk.blob_hash,
                        actual
                    );
                }
            }

            if chunk.offset != blob.len() as i64 {
                anyhow::bail!(
                    "shadow asset chunk out of order: expected offset {}, got {}",
                    blob.len(),
                    chunk.offset
                );
            }
            blob.extend_from_slice(&chunk.data);
            if chunk.total_size > 0 {
                expected_total = chunk.total_size;
            }
        }

        if expected_total > 0 && blob.len() as i64 != expected_total {
            anyhow::bail!(
                "shadow asset size mismatch: expected {} bytes, received {}",
                expected_total,
                blob.len()
            );
        }

        info!(bytes = blob.len(), "Shadow asset download complete");
        Ok(blob)
    }

    // ── Shadow recipe catalog RPCs (Sprint 22 slice 22-C2) ──

    /// Pull the server's shadow recipe catalog. `known_hash` is the client's
    /// last-seen catalog hash (empty = full pull). Returns the server's
    /// current catalog hash plus all entries.
    #[allow(dead_code)]
    pub async fn list_shadow_catalog(
        &mut self,
        client_id: &str,
        known_hash: &str,
    ) -> anyhow::Result<(String, Vec<ShadowCatalogEntry>)> {
        let req = ListShadowCatalogRequest {
            client_id: client_id.to_string(),
            catalog_hash: known_hash.to_string(),
        };
        debug!(
            client_id = client_id,
            known_hash = known_hash,
            "Listing shadow catalog"
        );
        let resp = self.client.list_shadow_catalog(req).await?;
        let resp = resp.into_inner();
        debug!(
            catalog_hash = %resp.catalog_hash,
            entries = resp.entries.len(),
            "Shadow catalog received"
        );
        Ok((resp.catalog_hash, resp.entries))
    }

    /// Report local shadow-package scan findings for a catalog version.
    /// Returns (accepted, message).
    #[allow(dead_code)]
    pub async fn report_shadow_scan(
        &mut self,
        client_id: &str,
        catalog_hash: &str,
        findings: Vec<crate::catalog_scan::ScanFinding>,
    ) -> anyhow::Result<(bool, String)> {
        let findings = findings
            .into_iter()
            .map(|f| ShadowScanFinding {
                recipe_name: f.recipe_name,
                installed: f.installed,
                detected_version: f.detected_version,
                method: f.method,
            })
            .collect();
        let req = ShadowScanReport {
            client_id: client_id.to_string(),
            catalog_hash: catalog_hash.to_string(),
            findings,
        };
        let resp = self.client.report_shadow_scan(req).await?;
        let resp = resp.into_inner();
        debug!(accepted = resp.accepted, message = %resp.message, "Shadow scan ack");
        Ok((resp.accepted, resp.message))
    }
}

/// Read one `[u32 LE length][protobuf bytes]` PairResponse frame from
/// `stream`, enforcing the 64 KiB cap (parity with the server's request cap
/// and the WebSocket pairing path).
async fn read_pair_response<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;
    if resp_len > PAIR_RESPONSE_MAX {
        anyhow::bail!("Pairing response too large: {} bytes", resp_len);
    }
    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;
    Ok(resp_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_pair_response_decodes_valid_frame() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        let payload = b"resp-frame".to_vec();
        client
            .write_all(&(payload.len() as u32).to_le_bytes())
            .await
            .unwrap();
        client.write_all(&payload).await.unwrap();

        let buf = read_pair_response(&mut server).await.unwrap();
        assert_eq!(buf, payload);
    }

    #[tokio::test]
    async fn read_pair_response_rejects_oversized_length() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&1_000_000u32.to_le_bytes()).await.unwrap();

        let err = read_pair_response(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn read_pair_response_errors_on_truncated_frame() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&16u32.to_le_bytes()).await.unwrap();
        client.write_all(b"short").await.unwrap();
        drop(client); // EOF mid-frame

        assert!(read_pair_response(&mut server).await.is_err());
    }

    #[test]
    fn client_certs_save_atomic_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let certs = ClientCerts {
            client_cert_pem: b"-----BEGIN CERT-----\nfake-cert\n-----END CERT-----".to_vec(),
            client_key_pem: b"-----BEGIN KEY-----\nfake-key\n-----END KEY-----".to_vec(),
            ca_cert_pem: b"-----BEGIN CA-----\nfake-ca\n-----END CA-----".to_vec(),
        };

        certs.save_atomic(dir.path()).expect("atomic save");

        let back = ClientCerts::load(dir.path()).expect("load rotated certs");
        assert_eq!(back.client_cert_pem, certs.client_cert_pem);
        assert_eq!(back.client_key_pem, certs.client_key_pem);
        assert_eq!(back.ca_cert_pem, certs.ca_cert_pem);

        // No temp files left behind.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            !names.iter().any(|n| n.contains(".tmp")),
            "temp files must be cleaned up: {names:?}"
        );

        // Key permissions 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("client.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "client.key must be 0600");
        }
    }
}
