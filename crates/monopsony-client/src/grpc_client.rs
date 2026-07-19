use monopsony_common::package_manager::PackageRef;
use monopsony_common::proto::monopsony::{
    HeartbeatRequest, InventoryReport, PackageEntry, PairRequest, PairResponse,
    RegisterRequest,
};
use monopsony_common::proto::monopsony::monopsony_control_client::MonopsonyControlClient;
use tonic::transport::Channel;
use tracing::{debug, info};
use std::path::Path;

use crate::ws_transport::WsTransport;

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
            std::fs::set_permissions(cert_dir.join("client.key"), std::fs::Permissions::from_mode(0o600))?;
        }

        info!("Client certificates saved to {}", cert_dir.display());
        Ok(())
    }
}

impl MonopsonyGrpcClient {
    pub async fn connect(addr: &str, tls: Option<tonic::transport::ClientTlsConfig>) -> anyhow::Result<Self> {
        info!(addr = addr, tls_enabled = tls.is_some(), "Connecting to monopsony server via gRPC");

        let endpoint = if let Some(tls_config) = tls {
            let uri = if addr.starts_with("https://") { addr.to_string() } else { format!("https://{}", addr) };
            Channel::from_shared(uri)?.tls_config(tls_config)?
        } else {
            let uri = if addr.starts_with("http://") || addr.starts_with("https://") { addr.to_string() } else { format!("http://{}", addr) };
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
                    let ws = WsTransport::connect(&url).await
                        .map_err(|e| std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            format!("WebSocket connect failed: {}", e),
                        ))?;
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
    ) -> anyhow::Result<(String, Option<ClientCerts>)> {
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

        Ok((resp.client_id, certs))
    }

    pub async fn heartbeat(&mut self, client_id: &str) -> anyhow::Result<i64> {
        let req = HeartbeatRequest {
            client_id: client_id.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
        };

        debug!(client_id = client_id, "Sending heartbeat");

        let resp = self.client.heartbeat(req).await?;
        let server_time = resp.into_inner().server_time;
        debug!(server_time = server_time, "Heartbeat acknowledged");

        Ok(server_time)
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
                install_source: if report_telemetry { "proxy".to_string() } else { String::new() },
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
    /// Returns (client_id, ClientCerts, grpc_addr) on success.
    pub async fn pair(
        server_addr: &str,
        code: &str,
        hostname: &str,
        os: &str,
        os_version: &str,
        package_managers: &[String],
    ) -> anyhow::Result<(String, ClientCerts, String)> {
        use prost::Message;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let pair_addr = if server_addr.ends_with(":50052") {
            server_addr.to_string()
        } else {
            format!("{}:50052", server_addr.trim_end_matches(":50051"))
        };
        let mut stream = tokio::net::TcpStream::connect(&pair_addr).await
            .map_err(|e| anyhow::anyhow!("Failed to connect to pairing port {}: {}", pair_addr, e))?;

        let req = PairRequest {
            code: code.to_string(),
            hostname: hostname.to_string(),
            os: os.to_string(),
            os_version: os_version.to_string(),
            package_managers: package_managers.to_vec(),
        };
        let req_bytes = req.encode_to_vec();
        let len = req_bytes.len() as u32;
        stream.write_all(&len.to_le_bytes()).await?;
        stream.write_all(&req_bytes).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_le_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        stream.read_exact(&mut resp_buf).await?;

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

        info!("Paired successfully, client_id={}, grpc_addr={}", resp.client_id, grpc_addr);
        Ok((resp.client_id, certs, grpc_addr))
    }

    #[allow(dead_code)] // Will be used by Status subcommand
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }
}
