use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

pub struct ClientDiscovery {
    service_name: String,
}

/// Mirror discovered from tracker or peer exchange
#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveredMirror {
    pub mirror_id: String,
    pub endpoint: String,
    pub transport: String,
    #[allow(dead_code)] // Tracker protocol field, will be used for mirror selection
    pub packages: Option<u64>,
    #[allow(dead_code)] // Tracker protocol field, will be used for mirror freshness checks
    pub last_sync: Option<String>,
    pub score: f64,
}

/// Tracker discover response
#[derive(Debug, Deserialize)]
struct DiscoverResponse {
    mirrors: Vec<DiscoveredMirror>,
    #[allow(dead_code)] // Cache TTL directive, will be used for discovery result caching
    ttl: u64,
}

/// Peer exchange response (same format, single endpoint per peer)
#[derive(Debug, Deserialize)]
struct PeerExchangeResponse {
    peers: Vec<DiscoveredMirror>,
}

impl ClientDiscovery {
    pub fn new(service_name: &str) -> Self {
        Self {
            service_name: service_name.to_string(),
        }
    }

    /// Multi-source discovery: tracker → peer exchange → mDNS
    /// Returns mirrors from tracker/peers, plus any mDNS-discovered servers.
    pub async fn discover_v2(
        &self,
        timeout: Duration,
        tracker_urls: &[String],
        peer_exchange_urls: &[String],
        distro: Option<&str>,
    ) -> anyhow::Result<DiscoveryResult> {
        let mut result = DiscoveryResult::default();

        // 1. Query tracker(s) — best source, geo-scored results
        for tracker_url in tracker_urls {
            match self.query_tracker(tracker_url, distro, timeout).await {
                Ok(mirrors) => {
                    info!("Tracker {} returned {} mirrors", tracker_url, mirrors.len());
                    result.tracker_mirrors.extend(mirrors);
                }
                Err(e) => {
                    warn!("Tracker {} query failed: {}", tracker_url, e);
                }
            }
        }

        // 2. Query peer exchange endpoints
        for peer_url in peer_exchange_urls {
            match self.query_peer_exchange(peer_url).await {
                Ok(peers) => {
                    info!("Peer exchange {} returned {} peers", peer_url, peers.len());
                    result.peer_mirrors.extend(peers);
                }
                Err(e) => {
                    warn!("Peer exchange {} query failed: {}", peer_url, e);
                }
            }
        }

        // 3. mDNS discovery (always try as fallback)
        match self.discover(timeout).await {
            Ok(services) => {
                result.mdns_services = services;
            }
            Err(e) => {
                warn!("mDNS discovery failed: {}", e);
            }
        }

        Ok(result)
    }

    /// Query tracker's GET /discover endpoint
    async fn query_tracker(
        &self,
        tracker_url: &str,
        distro: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<Vec<DiscoveredMirror>> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;

        let distro_name = distro.unwrap_or("archlinux");
        let url = format!(
            "{}/discover?distro={}&limit=10",
            tracker_url.trim_end_matches('/'),
            distro_name
        );

        debug!("Querying tracker: {}", url);
        let resp = client.get(&url).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Tracker returned HTTP {}", resp.status());
        }

        let body: DiscoverResponse = resp.json().await?;
        Ok(body.mirrors)
    }

    /// Query peer exchange endpoint (GET /.well-known/monopsony/peers)
    async fn query_peer_exchange(&self, peer_url: &str) -> anyhow::Result<Vec<DiscoveredMirror>> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;

        let url = format!(
            "{}/.well-known/monopsony/peers",
            peer_url.trim_end_matches('/')
        );

        debug!("Querying peer exchange: {}", url);
        let resp = client.get(&url).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Peer exchange returned HTTP {}", resp.status());
        }

        let body: PeerExchangeResponse = resp.json().await?;
        Ok(body.peers)
    }

    /// Original mDNS discovery
    pub async fn discover(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<Vec<monopsony_common::DiscoveryInfo>> {
        let service_type = format!("_{}._tcp.local.", self.service_name);
        let browse_timeout = timeout;

        info!("Starting mDNS discovery for {}", service_type);

        let discovered = tokio::time::timeout(timeout, async {
            tokio::task::spawn_blocking(
                move || -> anyhow::Result<Vec<monopsony_common::DiscoveryInfo>> {
                    let daemon = mdns_sd::ServiceDaemon::new()?;
                    let receiver = daemon.browse(&service_type)?;

                    let mut results = Vec::new();
                    let start = std::time::Instant::now();

                    loop {
                        let remaining = browse_timeout.saturating_sub(start.elapsed());
                        if remaining.is_zero() {
                            break;
                        }

                        match receiver.recv_timeout(remaining) {
                            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                                let address = info
                                    .get_addresses()
                                    .iter()
                                    .next()
                                    .map(|a| a.to_string())
                                    .unwrap_or_default();

                                let port = info.get_port();
                                let http_port = info
                                    .get_property_val_str("http_port")
                                    .and_then(|s| s.parse::<u16>().ok())
                                    .unwrap_or(port);
                                let grpc_port = info
                                    .get_property_val_str("grpc_port")
                                    .and_then(|s| s.parse::<u16>().ok())
                                    .unwrap_or(0);
                                let version =
                                    info.get_property_val_str("version").unwrap_or("0.1.0");

                                debug!(
                                    "Resolved: {}:{} (http={}, grpc={}, v={})",
                                    address, port, http_port, grpc_port, version
                                );

                                results.push(monopsony_common::DiscoveryInfo {
                                    address,
                                    http_port,
                                    grpc_port,
                                    version: version.to_string(),
                                });
                            }
                            Ok(mdns_sd::ServiceEvent::ServiceFound(_, name)) => {
                                debug!("Found service: {}", name);
                            }
                            Ok(mdns_sd::ServiceEvent::SearchStopped(_)) => {
                                break;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("mDNS channel error: {}", e);
                                break;
                            }
                        }
                    }

                    Ok(results)
                },
            )
            .await?
        })
        .await??;

        info!("Discovery complete, found {} service(s)", discovered.len());
        Ok(discovered)
    }

    pub async fn discover_one(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<monopsony_common::DiscoveryInfo> {
        let results = self.discover(timeout).await?;
        results.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("No Monopsony server discovered within timeout. Is the server running?")
        })
    }
}

/// Combined discovery results from all sources
#[derive(Debug, Default)]
pub struct DiscoveryResult {
    pub tracker_mirrors: Vec<DiscoveredMirror>,
    pub peer_mirrors: Vec<DiscoveredMirror>,
    pub mdns_services: Vec<monopsony_common::DiscoveryInfo>,
}

impl DiscoveryResult {
    pub fn total_results(&self) -> usize {
        self.tracker_mirrors.len() + self.peer_mirrors.len() + self.mdns_services.len()
    }
}
