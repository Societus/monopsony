use monopsony_common::package_manager::PackageRef;
use monopsony_common::proto::monopsony::monopsony_control_client::MonopsonyControlClient;
use monopsony_common::proto::monopsony::{
    client_message, HeartbeatRequest, InventoryReport, PackageEntry, PairRequest, PairResponse,
    RegisterRequest, TamperAlert,
};
use std::path::Path;
use tonic::transport::Channel;
use tracing::{debug, info};

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
            std::fs::set_permissions(
                cert_dir.join("client.key"),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }

        info!("Client certificates saved to {}", cert_dir.display());
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let pair_addr = if server_addr.ends_with(":50052") {
            server_addr.to_string()
        } else {
            format!("{}:50052", server_addr.trim_end_matches(":50051"))
        };
        let mut stream = tokio::net::TcpStream::connect(&pair_addr)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to connect to pairing port {}: {}", pair_addr, e)
            })?;

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
}
