//! Sprint 27 Part A: client heartbeat staleness detection.
//!
//! Per-client health is derived from `clients.last_seen` (bumped by every
//! heartbeat RPC and control-stream heartbeat message). A client is *stale*
//! when its last heartbeat is older than `[metrics].stale_threshold_secs`
//! (default 900s = 15× the 60s heartbeat interval).
//!
//! Two paths keep the Prometheus view current:
//! - **Heartbeat receipt** (`note_heartbeat`): pins the client's age gauge
//!   to 0 and closes any open stale episode immediately.
//! - **Background sweep** (`ClientHealthMonitor`): a hung client stops
//!   sending heartbeats, so the sweep — not the handler — is the actual
//!   detection path. It recomputes ages from the DB every interval.
//!
//! `monopsony_clients_stale_total` counts *episodes*: it is bumped exactly
//! once per fresh→stale transition (`StaleTracker`), not once per sweep
//! tick, so it is safe to alert/rate directly.
//!
//! Label-set note: the metrics recorder retains every `client_id` label it
//! has ever seen (no series deletion API). Series count is bounded by the
//! number of distinct clients over the server process lifetime — fine at
//! the 20–200 client target scale.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use monopsony_common::models::{Client, ClientId};
use monopsony_db::Database;

/// Staleness decision: is a heartbeat `age_secs` old enough to be stale?
///
/// Boundary: `age == threshold` counts as stale (15 missed heartbeats at the
/// default). A future `last_seen` (clock stepped back) yields age 0, fresh.
pub fn is_stale(age_secs: u64, threshold_secs: u64) -> bool {
    threshold_secs > 0 && age_secs >= threshold_secs
}

/// Whole seconds between `last_seen` and `now`, clamped at 0 so a future
/// `last_seen` (NTP step-back) never reads as negative age.
pub fn heartbeat_age_secs(last_seen: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    let secs = (now - last_seen).num_seconds();
    secs.max(0) as u64
}

/// Tracks which clients are currently stale so the episode counter
/// increments once per fresh→stale transition instead of once per sweep.
#[derive(Debug, Default)]
pub struct StaleTracker {
    stale: HashSet<ClientId>,
}

impl StaleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one sweep observation for a client.
    ///
    /// Returns `true` iff this observation opens a new stale episode
    /// (client was fresh, now stale) — the caller bumps the episode counter
    /// only then.
    pub fn observe(&mut self, client_id: &ClientId, stale: bool) -> bool {
        if stale {
            self.stale.insert(client_id.clone())
        } else {
            self.stale.remove(client_id);
            false
        }
    }

    /// Drop tracking state for clients no longer present in the database
    /// (unregistered hosts must not keep episodes or counts alive).
    pub fn retain_known(&mut self, known: &HashSet<ClientId>) {
        self.stale.retain(|id| known.contains(id));
    }

    pub fn contains(&self, client_id: &ClientId) -> bool {
        self.stale.contains(client_id)
    }

    pub fn stale_count(&self) -> u64 {
        self.stale.len() as u64
    }
}

/// Shared tracker handle: the sweep task and both heartbeat ingress paths
/// (unary RPC + control stream) mutate the same episode state.
pub type SharedStaleTracker = Arc<Mutex<StaleTracker>>;

/// Heartbeat-received bookkeeping shared by the unary and streaming
/// heartbeat handlers: pin the per-client age gauge to 0 and close any open
/// stale episode for this client.
pub async fn note_heartbeat(tracker: &SharedStaleTracker, client_id: &ClientId) {
    monopsony_common::metrics::set_client_heartbeat_age(&client_id.to_string(), 0);
    tracker.lock().await.observe(client_id, false);
}

/// Same as [`note_heartbeat`] for call sites holding the gRPC layer's
/// optional handle (monitor not started → gauge still pinned, no episode
/// state to update).
pub async fn note_heartbeat_for(tracker: &Option<SharedStaleTracker>, client_id: &ClientId) {
    monopsony_common::metrics::set_client_heartbeat_age(&client_id.to_string(), 0);
    if let Some(tracker) = tracker {
        tracker.lock().await.observe(client_id, false);
    }
}

/// Pure sweep step: fold current client rows into gauges and episode state.
///
/// Returns `(stale_count, transitioned)` where `transitioned` lists clients
/// that crossed the threshold on this pass (for logging).
pub fn apply_sweep(
    tracker: &mut StaleTracker,
    clients: &[Client],
    now: DateTime<Utc>,
    threshold_secs: u64,
) -> (u64, Vec<Client>) {
    let known: HashSet<ClientId> = clients.iter().map(|c| c.id.clone()).collect();
    tracker.retain_known(&known);

    let mut stale_count = 0u64;
    let mut transitioned = Vec::new();
    for client in clients {
        let age = heartbeat_age_secs(client.last_seen, now);
        monopsony_common::metrics::set_client_heartbeat_age(&client.id.to_string(), age);
        let stale = is_stale(age, threshold_secs);
        if tracker.observe(&client.id, stale) {
            transitioned.push(client.clone());
            monopsony_common::metrics::client_stale_episode();
        }
        if stale {
            stale_count += 1;
        }
    }
    monopsony_common::metrics::set_clients_stale(stale_count);
    (stale_count, transitioned)
}

/// How often the sweep runs: quarter of the threshold, clamped to
/// [5s, 60s] — 60s at the default 900s threshold, tighter for aggressive
/// configurations so detection latency stays ≤ ~1.25× threshold.
pub fn sweep_interval(threshold_secs: u64) -> Duration {
    Duration::from_secs((threshold_secs / 4).clamp(5, 60))
}

/// Background monitor owning the shared episode tracker.
pub struct ClientHealthMonitor {
    tracker: SharedStaleTracker,
    threshold_secs: u64,
    interval: Duration,
}

impl ClientHealthMonitor {
    pub fn new(threshold_secs: u64) -> Self {
        Self {
            tracker: Arc::new(Mutex::new(StaleTracker::new())),
            threshold_secs,
            interval: sweep_interval(threshold_secs),
        }
    }

    /// Handle handed to the gRPC layer so heartbeat receipt clears state.
    pub fn tracker(&self) -> SharedStaleTracker {
        self.tracker.clone()
    }

    pub fn threshold_secs(&self) -> u64 {
        self.threshold_secs
    }

    /// Spawn the staleness sweep loop. First pass runs immediately so
    /// gauges are populated at boot.
    pub fn start(self, db: Arc<Database>) -> tokio::task::JoinHandle<()> {
        let threshold = self.threshold_secs;
        let interval = self.interval;
        let tracker = self.tracker;
        tokio::spawn(async move {
            info!(
                threshold_secs = threshold,
                sweep_interval_secs = interval.as_secs(),
                "Client staleness sweep started"
            );
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                if let Err(e) = run_sweep_once(&db, &tracker, threshold).await {
                    error!("Client staleness sweep failed: {}", e);
                }
            }
        })
    }
}

/// One sweep pass: fetch clients, apply, log transitions.
async fn run_sweep_once(
    db: &Database,
    tracker: &SharedStaleTracker,
    threshold_secs: u64,
) -> anyhow::Result<()> {
    let clients = db.list_clients().await?;
    let now = Utc::now();
    let (stale_count, transitioned) = {
        let mut tr = tracker.lock().await;
        apply_sweep(&mut tr, &clients, now, threshold_secs)
    };
    for client in &transitioned {
        warn!(
            client_id = %client.id,
            hostname = %client.hostname,
            age_secs = heartbeat_age_secs(client.last_seen, now),
            threshold_secs,
            "Client heartbeat stale"
        );
    }
    if !transitioned.is_empty() || stale_count > 0 {
        info!(
            stale = stale_count,
            total = clients.len(),
            "Client health sweep"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with_age(id: &str, age_secs: i64) -> Client {
        Client {
            id: ClientId::parse(id).unwrap(),
            hostname: format!("host-{id}"),
            os: "linux".to_string(),
            os_version: "6".to_string(),
            package_managers: vec![],
            last_seen: Utc::now() - chrono::Duration::seconds(age_secs),
            registered_at: Utc::now(),
            cert_serial: None,
            // Sprint 29 B1: no UpdateReport data in these fixtures.
            update_security: 0,
            update_other: 0,
            update_total: 0,
            update_reported_at: None,
            reported_distro: None,
            reported_distro_version: None,
            update_ready: None,
            update_readiness_notes: None,
        }
    }

    #[test]
    fn is_stale_boundary() {
        assert!(!is_stale(0, 900));
        assert!(!is_stale(899, 900));
        assert!(is_stale(900, 900), "age == threshold is stale");
        assert!(is_stale(901, 900));
    }

    #[test]
    fn is_stale_zero_threshold_disables() {
        // threshold 0 must not mark everything (incl. age 0) stale
        assert!(!is_stale(0, 0));
        assert!(!is_stale(10_000, 0), "threshold 0 disables staleness");
    }

    #[test]
    fn heartbeat_age_clamps_future_last_seen() {
        let now = Utc::now();
        assert_eq!(heartbeat_age_secs(now, now), 0);
        assert_eq!(
            heartbeat_age_secs(now + chrono::Duration::seconds(30), now),
            0,
            "future last_seen must clamp to 0"
        );
        assert_eq!(
            heartbeat_age_secs(now - chrono::Duration::seconds(901), now),
            901
        );
    }

    #[test]
    fn tracker_counts_episode_once_until_recovery() {
        let mut tr = StaleTracker::new();
        let id = ClientId::new();

        // fresh→stale: first observation transitions
        assert!(tr.observe(&id, true), "first stale observation transitions");
        // still stale next sweep: NOT a new episode
        assert!(!tr.observe(&id, true), "repeat stale is not a new episode");
        assert_eq!(tr.stale_count(), 1);

        // recovery closes the episode
        assert!(!tr.observe(&id, false));
        assert_eq!(tr.stale_count(), 0);

        // goes stale again → new episode
        assert!(tr.observe(&id, true), "second outage is a new episode");
    }

    #[test]
    fn tracker_retain_known_drops_unregistered() {
        let mut tr = StaleTracker::new();
        let gone = ClientId::new();
        let kept = ClientId::new();
        tr.observe(&gone, true);
        tr.observe(&kept, true);

        let mut known = HashSet::new();
        known.insert(kept.clone());
        tr.retain_known(&known);

        assert!(!tr.contains(&gone));
        assert!(tr.contains(&kept));
        assert_eq!(tr.stale_count(), 1);
    }

    #[test]
    fn apply_sweep_reports_counts_and_transitions() {
        let mut tr = StaleTracker::new();
        let fresh = client_with_age("00000000-0000-0000-0000-000000000001", 60);
        let stale1 = client_with_age("00000000-0000-0000-0000-000000000002", 901);
        let stale2 = client_with_age("00000000-0000-0000-0000-000000000003", 5000);
        let clients = vec![fresh.clone(), stale1.clone(), stale2.clone()];

        let (count, transitioned) = apply_sweep(&mut tr, &clients, Utc::now(), 900);
        assert_eq!(count, 2);
        assert_eq!(transitioned.len(), 2);

        // second sweep over the same rows: no new episodes
        let (count, transitioned) = apply_sweep(&mut tr, &clients, Utc::now(), 900);
        assert_eq!(count, 2);
        assert!(transitioned.is_empty(), "repeat sweep must not re-count");

        // client 2 recovers: fresh row closes its episode
        let recovered = client_with_age("00000000-0000-0000-0000-000000000002", 60);
        let (count, _) = apply_sweep(
            &mut tr,
            &[fresh.clone(), recovered, stale2.clone()],
            Utc::now(),
            900,
        );
        assert_eq!(count, 1);

        // unregistered client disappears from rows: count follows live rows
        let (count, _) = apply_sweep(&mut tr, &[fresh], Utc::now(), 900);
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn note_heartbeat_closes_episode_and_resets_gauge_state() {
        let tracker: SharedStaleTracker = Arc::new(Mutex::new(StaleTracker::new()));
        let id = ClientId::new();
        {
            let mut tr = tracker.lock().await;
            assert!(tr.observe(&id, true));
        }
        note_heartbeat(&tracker, &id).await;
        let tr = tracker.lock().await;
        assert!(!tr.contains(&id), "heartbeat closes the stale episode");
        assert_eq!(tr.stale_count(), 0);
    }

    #[test]
    fn sweep_interval_clamps() {
        assert_eq!(
            sweep_interval(900).as_secs(),
            60,
            "default: 900/4 clamped to 60"
        );
        assert_eq!(sweep_interval(120).as_secs(), 30);
        assert_eq!(sweep_interval(20).as_secs(), 5, "floor 5s");
        assert_eq!(sweep_interval(0).as_secs(), 5);
    }

    /// End-to-end check of the CheckMK-facing surface: sweep → global
    /// recorder → Prometheus text format. Installs the process-global
    /// recorder (only test in this binary to do so).
    #[test]
    fn prometheus_render_exposes_staleness_metrics() {
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .expect("install Prometheus recorder");

        let mut tr = StaleTracker::new();
        let stale_client = client_with_age("00000000-0000-0000-0000-0000000000aa", 2000);
        let fresh_client = client_with_age("00000000-0000-0000-0000-0000000000bb", 10);
        let (stale_count, _) = apply_sweep(&mut tr, &[stale_client, fresh_client], Utc::now(), 900);
        assert_eq!(stale_count, 1);

        let rendered = handle.render();
        assert!(
            rendered.contains("monopsony_client_last_heartbeat_age_seconds"),
            "age gauge missing from render:\n{}",
            rendered
        );
        assert!(
            rendered.contains("monopsony_clients_stale_total"),
            "episode counter missing from render:\n{}",
            rendered
        );
        assert!(
            rendered.contains("monopsony_clients_stale_current 1"),
            "stale-current gauge should read 1:\n{}",
            rendered
        );
        assert!(
            rendered.contains("client_id=\"00000000-0000-0000-0000-0000000000aa\""),
            "per-client label missing from render:\n{}",
            rendered
        );
    }
}
