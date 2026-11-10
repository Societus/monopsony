use monopsony_common::peer_blacklist::{
    canonicalize_mirror_id, merge_entries, BlacklistEntry, BlacklistSubject,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, info, warn};

pub struct ClientDiscovery {
    service_name: String,
    /// Pinned blacklist originator keys (hex). Empty ⇒ blacklist entries from
    /// peers responses are ignored (never trust-by-default).
    blacklist_trusted: Vec<String>,
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

/// Relaxed peer-exchange entry (design prerequisite 4 repair). The mirror
/// serves `PeerEntry { mirror_id, endpoint, expires_at }` — it cannot
/// honestly fill `transport`/`score` for arbitrary remote peers, so both are
/// optional here and defaulted when converting to `DiscoveredMirror`. The
/// old code reused `DiscoveredMirror` (mandatory transport+score) and the
/// peers leg failed deserialization on EVERY response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerExchangeEntry {
    mirror_id: String,
    endpoint: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    packages: Option<u64>,
    #[serde(default)]
    last_sync: Option<String>,
    #[allow(dead_code)] // Fabricated now+1h by the mirror; informational only
    expires_at: Option<String>,
}

/// Peer exchange response (GET /.well-known/monopsony/peers) with the
/// Approach A extensions: optional signed blacklist array + optional
/// manifest.rs-style envelope signature. Serialize is required for the
/// envelope check (canonical bytes are rebuilt from the parsed document).
#[derive(Debug, Serialize, Deserialize)]
struct PeerExchangeResponse {
    /// Included so the envelope's canonical bytes reconstruct faithfully —
    /// the serving mirror signs the FULL document as served.
    #[serde(default)]
    source: Option<String>,
    peers: Vec<PeerExchangeEntry>,
    #[serde(default)]
    blacklist: Vec<BlacklistEntry>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    public_key: Option<String>,
}

impl From<PeerExchangeEntry> for DiscoveredMirror {
    fn from(e: PeerExchangeEntry) -> Self {
        Self {
            mirror_id: e.mirror_id,
            endpoint: e.endpoint,
            transport: e.transport.unwrap_or_else(|| "http".to_string()),
            packages: e.packages,
            last_sync: e.last_sync,
            score: e.score.unwrap_or(0.0),
        }
    }
}

impl ClientDiscovery {
    pub fn new(service_name: &str) -> Self {
        Self {
            service_name: service_name.to_string(),
            blacklist_trusted: Vec::new(),
        }
    }

    /// Pin blacklist originator keys (--trusted-operator on
    /// `monopsony-client discover`, repeatable). Entries from other
    /// originators are ignored.
    pub fn with_trusted_blacklist_operators(mut self, trusted: Vec<String>) -> Self {
        self.blacklist_trusted = trusted;
        self
    }

    /// Multi-source discovery: tracker → peer exchange → mDNS
    /// Returns mirrors from tracker/peers, plus any mDNS-discovered servers.
    /// When blacklist originators are pinned, verified entries from peers
    /// responses filter BOTH tracker and peers results.
    pub async fn discover_v2(
        &self,
        timeout: Duration,
        tracker_urls: &[String],
        peer_exchange_urls: &[String],
        distro: Option<&str>,
    ) -> anyhow::Result<DiscoveryResult> {
        let mut result = DiscoveryResult::default();
        let mut blacklist_entries: Vec<BlacklistEntry> = Vec::new();

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
                Ok((peers, entries)) => {
                    info!("Peer exchange {} returned {} peers", peer_url, peers.len());
                    result.peer_mirrors.extend(peers);
                    blacklist_entries.extend(entries);
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

        // 4. A6 canonicalization at the client's ingestion boundary (§9 #4):
        // every row id is canonicalized with the ONE shared rule so the
        // exact-match ban filter below AND the Discover listing operate on
        // the same canonical identity the tracker stores — a spoofed
        // lookalike cannot masquerade in output.
        result.tracker_mirrors = canonicalize_discovered(result.tracker_mirrors);
        result.peer_mirrors = canonicalize_discovered(result.peer_mirrors);

        // 5. Blacklist filter (pinned-originator entries only).
        self.apply_blacklist_filter(blacklist_entries, &mut result);

        Ok(result)
    }

    /// Apply the blacklist filter to discovery results: entries from peers
    /// responses may censor mirrors from BOTH tracker and peers results, so
    /// every entry is verified against the PINNED originator set BEFORE
    /// merging (`merge_entries` is seq-only bookkeeping — it cannot tell a
    /// forged entry from a real one). Empty pin set trusts nobody (fail
    /// closed), matching the mirror-side `merge_peer_blacklist` policy.
    fn apply_blacklist_filter(
        &self,
        mut blacklist_entries: Vec<BlacklistEntry>,
        result: &mut DiscoveryResult,
    ) {
        if blacklist_entries.is_empty() {
            return;
        }
        // Verify each entry against the PINNED originator set BEFORE
        // merging: `merge_entries` is seq-only bookkeeping and cannot tell a
        // forged entry from a real one, and the peers envelope signature is
        // self-consistency-only vs its embedded key — so an unverified entry
        // reaching the merge would censor mirrors client-side. Fail closed
        // on an empty pin set (trusts nobody), mirroring the mirror-side
        // `merge_peer_blacklist` policy.
        blacklist_entries.retain(|entry| {
            entry
                .verify_trusted(&self.blacklist_trusted)
                .map_err(|e| {
                    debug!(originator = %entry.originator, subject = %entry.subject, "{}", e);
                    e
                })
                .is_ok()
        });
        let now = chrono::Utc::now().timestamp();
        let merged = merge_entries(blacklist_entries);
        let before = result.tracker_mirrors.len() + result.peer_mirrors.len();
        result
            .tracker_mirrors
            .retain(|m| !Self::mirror_banned(&merged, &m.mirror_id, now));
        result
            .peer_mirrors
            .retain(|m| !Self::mirror_banned(&merged, &m.mirror_id, now));
        let after = result.tracker_mirrors.len() + result.peer_mirrors.len();
        if before != after {
            warn!(
                "Blacklist filter removed {} discovered mirror(s)",
                before - after
            );
        }
    }

    /// True when mirror_id is under an active `mirror:<id>` ban.
    fn mirror_banned(
        merged: &std::collections::HashMap<(String, String), BlacklistEntry>,
        mirror_id: &str,
        now: i64,
    ) -> bool {
        let subject = BlacklistSubject::mirror(mirror_id).as_subject();
        merged
            .values()
            .any(|e| e.subject == subject && e.is_active_ban(now))
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

    /// Query peer exchange endpoint (GET /.well-known/monopsony/peers).
    /// Returns the peers plus any blacklist entries carried on the response.
    /// Blacklist entries are only returned when the envelope signature —
    /// when present — verifies self-consistently (transport integrity);
    /// entry TRUST is decided later against the pinned originator set.
    async fn query_peer_exchange(
        &self,
        peer_url: &str,
    ) -> anyhow::Result<(Vec<DiscoveredMirror>, Vec<BlacklistEntry>)> {
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

        // Envelope integrity check (manifest.rs style): when the mirror
        // signed the document, verify before trusting the blacklist payload.
        // A failed check drops the blacklist array, not the peers list —
        // availability first; per-entry pinned-key verification is the real
        // trust gate.
        let blacklist = if body.blacklist.is_empty() {
            Vec::new()
        } else {
            match (&body.signature, &body.public_key) {
                (Some(sig_hex), Some(pk_hex)) => {
                    if verify_peers_envelope(&body, sig_hex, pk_hex) {
                        body.blacklist
                    } else {
                        warn!(
                            "Peer exchange {} envelope signature INVALID — ignoring its blacklist array",
                            peer_url
                        );
                        Vec::new()
                    }
                }
                _ => {
                    debug!(
                        "Peer exchange {} served a blacklist without an envelope signature",
                        peer_url
                    );
                    body.blacklist
                }
            }
        };

        let peers = body.peers.into_iter().map(Into::into).collect();
        Ok((peers, blacklist))
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

/// A6 (§9 fix #4): canonicalize every discovered row's mirror_id at the
/// client's ingestion boundary — the ONE shared rule from
/// `monopsony_common::peer_blacklist::canonicalize_mirror_id`, the same fn
/// tracker ingestion and the signed-subject parser enforce. The canonical
/// form replaces the raw id, so display and ban matching see a single
/// identity (a spoofed lookalike cannot masquerade in `Discover` output).
/// Rows whose id cannot be canonicalized (unicode lookalikes, interior
/// whitespace, control chars) are DROPPED fail-closed: they can never match
/// a canonical ban, so serving them would be exactly the evasion/spoof
/// surface A6 closes.
fn canonicalize_discovered(mirrors: Vec<DiscoveredMirror>) -> Vec<DiscoveredMirror> {
    mirrors
        .into_iter()
        .filter_map(|mut m| match canonicalize_mirror_id(&m.mirror_id) {
            Ok(id) => {
                if id != m.mirror_id {
                    debug!(
                        mirror_id = %m.mirror_id,
                        canonical = %id,
                        "canonicalized discovered mirror id"
                    );
                }
                m.mirror_id = id;
                Some(m)
            }
            Err(reason) => {
                warn!(
                    mirror_id = %m.mirror_id,
                    "dropping discovered mirror (A6 fail-closed): {}", reason
                );
                None
            }
        })
        .collect()
}

/// Verify the peers envelope signature over the canonical document minus the
/// `signature`/`public_key` fields, against the EMBEDDED public key
/// (self-consistency = transport integrity; entry trust is pinned-key).
fn verify_peers_envelope(body: &PeerExchangeResponse, sig_hex: &str, pk_hex: &str) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let Ok(mut value) = serde_json::to_value(body) else {
        return false;
    };
    if let Value::Object(map) = &mut value {
        map.remove("signature");
        map.remove("public_key");
    }
    let canonical = monopsony_common::peer_blacklist::canonical_json(&value);

    let (Ok(sig_bytes), Ok(pk_bytes)) = (hex::decode(sig_hex), hex::decode(pk_hex)) else {
        return false;
    };
    if sig_bytes.len() != 64 || pk_bytes.len() != 32 {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_bytes(pk_bytes.as_slice().try_into().unwrap()) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    vk.verify(canonical.as_bytes(), &sig).is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn test_key(seed: u8) -> ed25519_dalek::SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_mul(7);
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    }

    fn signed_ban(mirror_id: &str, seq: u64, key: &ed25519_dalek::SigningKey) -> BlacklistEntry {
        let mut entry = BlacklistEntry {
            subject: BlacklistSubject::mirror(mirror_id).as_subject(),
            action: monopsony_common::peer_blacklist::BlacklistAction::Ban,
            reason: "test".to_string(),
            originator: String::new(),
            seq,
            created_at: 1_793_500_000,
            expires_at: None,
            signature: String::new(),
        };
        entry.sign(key);
        entry
    }

    /// THE prerequisite-4 regression: the mirror's actual PeerEntry shape
    /// (mirror_id/endpoint/expires_at only) must deserialize — the old code
    /// required transport+score and failed every peers response.
    #[test]
    fn mirror_peer_entry_parses_without_transport_or_score() {
        let body = r#"{"source":"local","peers":[{"mirror_id":"host-arch","endpoint":"http://host:3129/mirror/arch","expires_at":"2026-10-31T18:00:00+00:00"}]}"#;
        let parsed: PeerExchangeResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.peers.len(), 1);
        assert!(parsed.blacklist.is_empty());

        let mirrors: Vec<DiscoveredMirror> = parsed.peers.into_iter().map(Into::into).collect();
        assert_eq!(mirrors[0].transport, "http", "defaulted transport");
        assert_eq!(mirrors[0].score, 0.0, "defaulted score");
    }

    /// The full Approach A response shape (blacklist + envelope) parses.
    #[test]
    fn peers_response_with_blacklist_and_envelope_parses() {
        let key = test_key(1);
        let entry = signed_ban("bad-arch", 1, &key);
        let mut body_value = serde_json::json!({
            "source": "local",
            "peers": [],
            "blacklist": [entry],
        });
        let doc = body_value.clone();
        let canonical = monopsony_common::peer_blacklist::canonical_json(&doc);
        let sig = key.sign(canonical.as_bytes());
        body_value["signature"] = serde_json::Value::String(hex::encode(sig.to_bytes()));
        body_value["public_key"] =
            serde_json::Value::String(hex::encode(key.verifying_key().as_bytes()));

        let parsed: PeerExchangeResponse = serde_json::from_value(body_value).unwrap();
        assert_eq!(parsed.blacklist.len(), 1);
        assert!(verify_peers_envelope(
            &parsed,
            parsed.signature.as_deref().unwrap(),
            parsed.public_key.as_deref().unwrap()
        ));
    }

    /// A tampered blacklist must FAIL the envelope check.
    #[test]
    fn envelope_check_rejects_tampered_blacklist() {
        let key = test_key(2);
        let entry = signed_ban("bad-arch", 1, &key);
        let mut body_value = serde_json::json!({
            "source": "local",
            "peers": [],
            "blacklist": [entry],
        });
        let doc = body_value.clone();
        let canonical = monopsony_common::peer_blacklist::canonical_json(&doc);
        let sig = key.sign(canonical.as_bytes());
        body_value["signature"] = serde_json::Value::String(hex::encode(sig.to_bytes()));
        body_value["public_key"] =
            serde_json::Value::String(hex::encode(key.verifying_key().as_bytes()));

        // Tamper AFTER signing.
        body_value["blacklist"][0]["reason"] = serde_json::Value::String("tampered".to_string());
        let parsed: PeerExchangeResponse = serde_json::from_value(body_value).unwrap();
        assert!(!verify_peers_envelope(
            &parsed,
            parsed.signature.as_deref().unwrap(),
            parsed.public_key.as_deref().unwrap()
        ));
    }

    /// Merge + filter semantics over verified entries: banned mirror removed,
    /// retracted mirror kept.
    #[test]
    fn blacklist_filter_merges_and_respects_retracts() {
        let key = test_key(3);
        let ban = signed_ban("banned-arch", 1, &key);
        let mut retract = signed_ban("ok-arch", 2, &key);
        retract.action = monopsony_common::peer_blacklist::BlacklistAction::Retract;
        retract.sign(&key);

        let merged = merge_entries(vec![ban, retract]);
        let now = 1_793_500_100;
        assert!(ClientDiscovery::mirror_banned(&merged, "banned-arch", now));
        assert!(!ClientDiscovery::mirror_banned(&merged, "ok-arch", now));
    }

    // -----------------------------------------------------------------------
    // A6 mirror_id canonicalization (§9 fix #4)
    // -----------------------------------------------------------------------

    fn row(mirror_id: &str) -> DiscoveredMirror {
        DiscoveredMirror {
            mirror_id: mirror_id.to_string(),
            endpoint: "http://h:3129/mirror/arch".to_string(),
            transport: "http".to_string(),
            packages: None,
            last_sync: None,
            score: 0.0,
        }
    }

    /// Discovered rows land canonical at the client's ingestion boundary:
    /// legit ids unchanged (no false drops), case/edge-space mutations
    /// normalized, unicode-lookalike and interior-whitespace ids DROPPED
    /// fail-closed (they can never match a canonical ban).
    #[test]
    fn discovered_rows_landing_canonical() {
        let rows = vec![
            row("honest-arch"),        // canonical: unchanged
            row("  Honest-ARCH "),     // normalized to the canonical identity
            row("h\u{043e}nest-arch"), // lookalike: dropped
            row("honest host"),        // interior space: dropped
        ];
        let out = canonicalize_discovered(rows);
        let ids: Vec<&str> = out.iter().map(|m| m.mirror_id.as_str()).collect();
        assert_eq!(ids, vec!["honest-arch", "honest-arch"]);
    }

    /// A6 closure on the display/filter path: a ban over the canonical id
    /// filters a row whose id arrives mutated — canonicalization turns the
    /// mutation back into the banned identity before the exact match runs,
    /// and the surviving row DISPLAYS the canonical id.
    #[test]
    fn mutated_row_id_cannot_evade_ban_filter_or_display_spoof() {
        let key = test_key(4);
        let ban = signed_ban("banned-arch", 1, &key);
        let merged = merge_entries(vec![ban]);
        let now = 1_793_500_100;

        let rows = canonicalize_discovered(vec![row("  BANNED-arch "), row("clean-arch")]);
        let kept: Vec<&DiscoveredMirror> = rows
            .iter()
            .filter(|m| !ClientDiscovery::mirror_banned(&merged, &m.mirror_id, now))
            .collect();
        let ids: Vec<&str> = kept.iter().map(|m| m.mirror_id.as_str()).collect();
        assert_eq!(ids, vec!["clean-arch"], "mutated variant of a banned id must be filtered");
    }

    /// Unpinned entries are dropped before the filter ever sees them
    /// (verify_trusted gate — mirrors the merge-path policy).
    #[test]
    fn unpinned_originator_entries_are_untrusted() {
        let key = test_key(4);
        let entry = signed_ban("banned-arch", 1, &key);
        assert!(entry.verify_trusted(&[]).is_err());
        assert!(entry.verify_trusted(&["ff".repeat(64)]).is_err());
    }

    fn mirror_fixture(mirror_id: &str) -> DiscoveredMirror {
        DiscoveredMirror {
            mirror_id: mirror_id.to_string(),
            endpoint: format!("http://{}.example:3129/mirror/arch", mirror_id),
            transport: "http".to_string(),
            packages: None,
            last_sync: None,
            score: 0.0,
        }
    }

    fn discovery_with_pins(pins: &[String]) -> ClientDiscovery {
        ClientDiscovery::new("monopsony").with_trusted_blacklist_operators(pins.to_vec())
    }

    /// DEFECT-1 regression (T9 sign-off, card t_8a0679db): the filter path
    /// must verify each entry against the PINNED originator set BEFORE
    /// merging. `merge_entries` is seq-only bookkeeping and the peers
    /// envelope signature is self-consistency-only vs its embedded key, so
    /// without the gate any gossiping peer can inject a ban (seq u64::MAX =
    /// effectively permanent) and censor any mirror client-side. A forged
    /// entry claiming a pinned-looking originator must not filter anything.
    #[test]
    fn forged_entries_claiming_pinned_originator_censor_nothing() {
        let pinned = test_key(1);
        let attacker = test_key(9);
        let disc = discovery_with_pins(&[hex::encode(pinned.verifying_key().as_bytes())]);

        // Attacker-signed ban whose originator is swapped AFTER signing to
        // the pinned npub: passes the membership half of the trust check
        // only — the signature does not verify against the claimed key.
        let mut forged_swap = signed_ban("victim-arch", u64::MAX, &attacker);
        forged_swap.originator = hex::encode(pinned.verifying_key().as_bytes());

        // The card's PoC shape: empty signature, arbitrary 64-hex originator.
        let forged_empty = BlacklistEntry {
            subject: BlacklistSubject::mirror("victim-arch").as_subject(),
            action: monopsony_common::peer_blacklist::BlacklistAction::Ban,
            reason: "censorship".to_string(),
            originator: "aa".repeat(32),
            seq: u64::MAX,
            created_at: 1_793_500_000,
            expires_at: None,
            signature: String::new(),
        };

        let mut result = DiscoveryResult {
            tracker_mirrors: vec![mirror_fixture("victim-arch"), mirror_fixture("ok-arch")],
            peer_mirrors: vec![mirror_fixture("victim-arch")],
            ..Default::default()
        };
        disc.apply_blacklist_filter(vec![forged_swap, forged_empty], &mut result);
        assert_eq!(
            result.tracker_mirrors.len(),
            2,
            "forged entries must not censor tracker results"
        );
        assert_eq!(
            result.peer_mirrors.len(),
            1,
            "forged entries must not censor peers results"
        );
    }

    /// Positive control: a correctly signed entry from the PINNED key must
    /// filter — banned mirror drops from both tracker and peers results.
    #[test]
    fn pinned_originator_entry_filters_mirrors_end_to_end() {
        let pinned = test_key(2);
        let disc = discovery_with_pins(&[hex::encode(pinned.verifying_key().as_bytes())]);
        let ban = signed_ban("banned-arch", 1, &pinned);

        let mut result = DiscoveryResult {
            tracker_mirrors: vec![mirror_fixture("banned-arch"), mirror_fixture("ok-arch")],
            peer_mirrors: vec![mirror_fixture("banned-arch"), mirror_fixture("ok-arch")],
            ..Default::default()
        };
        disc.apply_blacklist_filter(vec![ban], &mut result);
        assert!(
            !result.tracker_mirrors.iter().any(|m| m.mirror_id == "banned-arch"),
            "pinned-key ban must filter tracker results"
        );
        assert!(result.tracker_mirrors.iter().any(|m| m.mirror_id == "ok-arch"));
        assert!(
            !result.peer_mirrors.iter().any(|m| m.mirror_id == "banned-arch"),
            "pinned-key ban must filter peers results"
        );
        assert!(result.peer_mirrors.iter().any(|m| m.mirror_id == "ok-arch"));
    }

    /// Fail closed: with NO pins configured even a validly signed entry
    /// trusts nobody — the filter must never censor without an operator
    /// pinned set (never trust-by-default).
    #[test]
    fn empty_pin_set_censors_nothing_even_with_valid_entry() {
        let key = test_key(3);
        let entry = signed_ban("banned-arch", 1, &key);
        let disc = discovery_with_pins(&[]);

        let mut result = DiscoveryResult {
            tracker_mirrors: vec![mirror_fixture("banned-arch")],
            peer_mirrors: vec![mirror_fixture("banned-arch")],
            ..Default::default()
        };
        disc.apply_blacklist_filter(vec![entry], &mut result);
        assert_eq!(result.tracker_mirrors.len(), 1);
        assert_eq!(result.peer_mirrors.len(), 1);
    }
}
