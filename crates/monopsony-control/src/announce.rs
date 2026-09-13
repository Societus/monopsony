//! Mirror announce-to-tracker task
//!
//! Periodically POSTs mirror status to configured tracker(s).

use std::sync::Arc;

use serde_json::json;
use tracing::{debug, error, info, warn};

use monopsony_mirror::MirrorManager;

/// Tracker announcer — periodically POSTs mirror configs to tracker(s)
pub struct TrackerAnnouncer {
    manager: Arc<MirrorManager>,
    tracker_urls: Vec<String>,
    mirror_addr: String,
    interval_secs: u64,
    server_id: String,
}

impl TrackerAnnouncer {
    pub fn new(
        manager: Arc<MirrorManager>,
        tracker_urls: Vec<String>,
        mirror_addr: String,
        interval_secs: u64,
    ) -> Self {
        let server_id = std::env::var("HOSTNAME")
            .or_else(|_| {
                std::fs::read_to_string("/etc/hostname")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| ())
            })
            .unwrap_or_else(|_| "unknown".to_string());

        Self {
            manager,
            tracker_urls,
            mirror_addr,
            interval_secs: interval_secs.max(60),
            server_id,
        }
    }

    /// Run the announce loop (blocks forever)
    pub async fn run(&self) {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(self.interval_secs));

        // Announce immediately on start
        self.announce_all().await;

        loop {
            interval.tick().await;
            self.announce_all().await;
        }
    }

    async fn announce_all(&self) {
        if self.tracker_urls.is_empty() {
            return;
        }

        let configs = self.manager.list_configs().await;

        // Only announce public mirrors
        let public_configs: Vec<_> = configs.iter().filter(|c| c.public).collect();

        if public_configs.is_empty() {
            debug!("No public mirrors to announce");
            return;
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to build HTTP client for tracker announce: {}", e);
                return;
            }
        };

        let public_count = public_configs.len();
        for config in public_configs {
            let endpoint = format!("http://{}:3129/mirror/{}", self.mirror_addr, config.distro);
            let payload = json!({
                "mirror_id": format!("{}-{}", self.server_id, config.distro),
                "distro": config.distro,
                "transport": ["http"],
                "endpoints": [endpoint],
                "capacity_bytes": null,
                "bandwidth_bps": null,
                "uptime_seconds": null,
                "packages": config.total_bytes,
                "last_sync": config.last_sync.map(|ts| ts.to_string()),
            });

            for tracker_url in &self.tracker_urls {
                let announce_url = format!("{}/announce", tracker_url.trim_end_matches('/'));
                let result = client.post(&announce_url).json(&payload).send().await;

                match result {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            debug!("Announced {} to {}", config.distro, tracker_url);
                        } else {
                            warn!(
                                "Tracker {} returned {} for {}",
                                tracker_url, status, config.distro
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to announce {} to {}: {}",
                            config.distro, tracker_url, e
                        );
                    }
                }
            }
        }

        info!(
            "Announced {} public mirrors to {} tracker(s)",
            public_count,
            self.tracker_urls.len()
        );
    }
}
