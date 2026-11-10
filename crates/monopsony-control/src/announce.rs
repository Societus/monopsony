//! Mirror announce-to-tracker task
//!
//! Periodically POSTs mirror status to configured tracker(s). When the
//! server's operator Ed25519 key is available, announces are SIGNED
//! (operator_npub + operator_signature — the tracker rejects unsigned
//! announces in token/jwt modes, design prerequisite 2). After each announce
//! round the signed blacklist export (`GET /blacklist/signed`) is ingested
//! into the mirror's blacklist store so tracker bans propagate to the
//! peers-gossip plane (Approach A, docs/plans/2026-10-31-peer-blacklist-propagation.md).

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use monopsony_common::peer_blacklist::{canonical_json, BlacklistEntry};
use monopsony_mirror::MirrorManager;

/// Announce wire payload — field-for-field mirror of the tracker's
/// `AnnounceRequest` (monopsony-tracker main.rs). The tracker verifies the
/// signature over `serde_json::to_value` of ITS struct minus
/// `operator_signature`; keeping this struct identical guarantees the
/// canonical bytes match.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnnouncePayload {
    mirror_id: String,
    distro: String,
    transport: Vec<String>,
    endpoints: Vec<String>,
    capacity_bytes: Option<u64>,
    bandwidth_bps: Option<u64>,
    uptime_seconds: Option<u64>,
    packages: Option<u64>,
    last_sync: Option<String>,
    /// Optional [lat, lon] for geo scoring
    lat_lon: Option<(f64, f64)>,
    stats: Option<AnnounceStatsPayload>,
    operator_npub: Option<String>,
    operator_signature: Option<String>,
    display_name: Option<String>,
    payment_pointer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnnounceStatsPayload {
    bytes_served_24h: Option<u64>,
    requests_24h: Option<u64>,
}

/// Canonical announce bytes: payload JSON minus the `operator_signature`
/// field, recursively sorted keys, compact — the exact bytes the tracker
/// re-derives before verifying.
fn announce_canonical_bytes(payload: &AnnouncePayload) -> Option<Vec<u8>> {
    let mut value = serde_json::to_value(payload).ok()?;
    if let Value::Object(map) = &mut value {
        map.remove("operator_signature");
    }
    Some(canonical_json(&value).into_bytes())
}

/// Tracker announcer — periodically POSTs mirror configs to tracker(s)
pub struct TrackerAnnouncer {
    manager: Arc<MirrorManager>,
    tracker_urls: Vec<String>,
    mirror_addr: String,
    interval_secs: u64,
    server_id: String,
    /// Operator Ed25519 key (shadow signing.key). When present, announces
    /// carry operator_npub + operator_signature.
    signing_key: Option<Arc<SigningKey>>,
    /// Pinned blacklist originators. Non-empty ⇒ the signed blacklist export
    /// is ingested from every tracker after each announce round.
    blacklist_trusted: Vec<String>,
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
            signing_key: None,
            blacklist_trusted: Vec::new(),
        }
    }

    /// Sign announces with the server's operator key.
    pub fn with_signing_key(mut self, key: Arc<SigningKey>) -> Self {
        self.signing_key = Some(key);
        self
    }

    /// Ingest the signed blacklist export from each tracker after each
    /// announce round, trusting only these pinned originator keys.
    pub fn with_blacklist_trusted(mut self, trusted: Vec<String>) -> Self {
        self.blacklist_trusted = trusted;
        self
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

        let mut public_count = 0usize;
        for config in public_configs {
            public_count += 1;
            let endpoint = format!("http://{}:3129/mirror/{}", self.mirror_addr, config.distro);
            let mut payload = AnnouncePayload {
                mirror_id: format!("{}-{}", self.server_id, config.distro),
                distro: config.distro.clone(),
                transport: vec!["http".to_string()],
                endpoints: vec![endpoint],
                capacity_bytes: None,
                bandwidth_bps: None,
                uptime_seconds: None,
                packages: Some(config.total_bytes),
                last_sync: config.last_sync.map(|ts| ts.to_string()),
                lat_lon: None,
                stats: None,
                operator_npub: None,
                operator_signature: None,
                display_name: None,
                payment_pointer: None,
            };

            // Sign the announce when the operator key is available
            // (prerequisite 2 — the tracker rejects unsigned announces in
            // token/jwt modes).
            if let Some(key) = &self.signing_key {
                use ed25519_dalek::Signer;
                payload.operator_npub = Some(hex::encode(key.verifying_key().as_bytes()));
                if let Some(bytes) = announce_canonical_bytes(&payload) {
                    payload.operator_signature = Some(hex::encode(key.sign(&bytes).to_bytes()));
                } else {
                    warn!(
                        distro = %config.distro,
                        "announce payload failed to canonicalize; sending unsigned"
                    );
                }
            }

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

        // Ingest the tracker's signed blacklist export (Approach A
        // propagation source) and surface own-bans operator-visibly.
        self.ingest_blacklist(&client).await;
    }

    /// GET /blacklist/signed from each tracker and merge verified entries
    /// into the mirror's blacklist store. Only runs when blacklist originators
    /// are pinned — empty pin set means remote entries are never trusted.
    async fn ingest_blacklist(&self, client: &reqwest::Client) {
        if self.blacklist_trusted.is_empty() {
            return;
        }

        for tracker_url in &self.tracker_urls {
            let url = format!("{}/blacklist/signed", tracker_url.trim_end_matches('/'));
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<Vec<BlacklistEntry>>().await {
                        Ok(entries) if !entries.is_empty() => {
                            let (accepted, rejected) =
                                self.manager.merge_peer_blacklist(entries).await;
                            debug!(
                                tracker = %tracker_url,
                                accepted,
                                rejected,
                                "ingested signed blacklist entries"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            debug!(tracker = %tracker_url, "bad blacklist/signed body: {}", e)
                        }
                    }
                }
                Ok(resp) => {
                    debug!(
                        tracker = %tracker_url,
                        "blacklist/signed HTTP {}",
                        resp.status()
                    )
                }
                Err(e) => {
                    debug!(tracker = %tracker_url, "blacklist/signed fetch failed: {}", e)
                }
            }
        }

        self.warn_own_bans().await;
    }

    /// Operator-visible warning + metric when one of OUR mirrors is banned
    /// by a trusted originator (design enforcement table: own-ban row).
    async fn warn_own_bans(&self) {
        let banned = self.manager.banned_mirror_ids().await;
        if banned.is_empty() {
            return;
        }
        let configs = self.manager.list_configs().await;
        for config in &configs {
            let mirror_id = format!("{}-{}", self.server_id, config.distro);
            if banned.contains(&mirror_id) {
                monopsony_common::metrics::mirror_blacklist_self_ban();
                warn!(
                    mirror_id = %mirror_id,
                    distro = %config.distro,
                    "THIS MIRROR IS BLACKLISTED by a trusted operator — peers and \
                     clients will exclude it from discovery; investigate or obtain a \
                     signed retraction from the originator"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    /// The canonical bytes must exclude the signature field and stay stable
    /// across sign → verify reconstruction.
    #[test]
    fn announce_canonical_bytes_exclude_signature_and_are_stable() {
        let payload = AnnouncePayload {
            mirror_id: "host-archlinux".to_string(),
            distro: "archlinux".to_string(),
            transport: vec!["http".to_string()],
            endpoints: vec!["http://host:3129/mirror/archlinux".to_string()],
            capacity_bytes: None,
            bandwidth_bps: None,
            uptime_seconds: None,
            packages: Some(1000),
            last_sync: None,
            lat_lon: None,
            stats: None,
            operator_npub: Some("ab".repeat(32)),
            operator_signature: None,
            display_name: None,
            payment_pointer: None,
        };
        let bytes1 = announce_canonical_bytes(&payload).unwrap();

        // Round-trip through serde (what the tracker does on receive): a
        // signed body deserialized back into the payload struct must yield
        // the same canonical bytes after removing the signature.
        let mut signed = payload.clone();
        let key = SigningKey::from_bytes(&[1u8; 32]);
        signed.operator_signature = Some(hex::encode(key.sign(&bytes1).to_bytes()));
        let text = serde_json::to_string(&signed).unwrap();
        let reparsed: AnnouncePayload = serde_json::from_str(&text).unwrap();
        let bytes2 = announce_canonical_bytes(&reparsed).unwrap();
        assert_eq!(bytes1, bytes2);

        // And the signature verifies over those bytes.
        use ed25519_dalek::Verifier;
        let sig_bytes = hex::decode(reparsed.operator_signature.unwrap()).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        assert!(key.verifying_key().verify(&bytes2, &sig).is_ok());
    }

    /// A tampered payload must produce different canonical bytes.
    #[test]
    fn announce_canonical_bytes_detect_tamper() {
        let payload = AnnouncePayload {
            mirror_id: "host-archlinux".to_string(),
            distro: "archlinux".to_string(),
            transport: vec!["http".to_string()],
            endpoints: vec!["http://host:3129/mirror/archlinux".to_string()],
            capacity_bytes: None,
            bandwidth_bps: None,
            uptime_seconds: None,
            packages: None,
            last_sync: None,
            lat_lon: None,
            stats: None,
            operator_npub: None,
            operator_signature: None,
            display_name: None,
            payment_pointer: None,
        };
        let original = announce_canonical_bytes(&payload).unwrap();
        let mut evil = payload.clone();
        evil.distro = "evil".to_string();
        let tampered = announce_canonical_bytes(&evil).unwrap();
        assert_ne!(original, tampered);
    }
}
