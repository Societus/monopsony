//! Client-side HIT/MISS/STALE accounting (Sprint 26 Phase 4, B4 slice 5).
//!
//! Counts `X-Cache` response headers (values: HIT | MISS | STALE — see
//! monopsony-proxy/src/proxy.rs) observed at the client proxy's forwarding
//! point, so `monopsony-client status` can surface a live HIT ratio without
//! asking the server. No protocol work: the server already stamps the header.
//!
//! Counters persist as JSON in the client state dir
//! (`~/.local/state/monopsony/hit-counts.json`) so a daemon restart does not
//! reset the ratio the menu bar displays. The daemon flushes periodically;
//! a crash loses at most one interval. Corrupt or missing files load as
//! zeros (fail-open — counting must never break forwarding).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub struct HitCounter {
    hits: AtomicU64,
    misses: AtomicU64,
    stale: AtomicU64,
    /// Where to persist. Set by [`HitCounter::load`]; `None` = never persists
    /// (counter-only mode, used by tests).
    path: Mutex<Option<PathBuf>>,
}

/// On-disk counter snapshot (also the shape `status --quiet` reads back).
#[derive(Serialize, Deserialize, Default)]
pub struct PersistedSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub stale: u64,
}

impl HitCounter {
    /// Counter without persistence (tests). Not used by the bin target's
    /// daemon path (which loads from disk) — allowed dead there; the lib
    /// target exports it as API.
    #[allow(dead_code)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Load prior counters from `path` (missing/corrupt → zeros, fail-open)
    /// and remember it as the persist target.
    pub fn load(path: PathBuf) -> Arc<Self> {
        let counter = Self::default();
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(p) = serde_json::from_str::<PersistedSnapshot>(&content) {
                counter.hits.store(p.hits, Ordering::Relaxed);
                counter.misses.store(p.misses, Ordering::Relaxed);
                counter.stale.store(p.stale, Ordering::Relaxed);
            }
        }
        *counter.path.lock().expect("hit counter path mutex") = Some(path);
        Arc::new(counter)
    }

    /// Record one `X-Cache` header value. Unknown values are ignored —
    /// a future server adding a fourth state must not skew the ratio.
    pub fn observe(&self, value: &str) {
        match value {
            "HIT" => {
                self.hits.fetch_add(1, Ordering::Relaxed);
            }
            "MISS" => {
                self.misses.fetch_add(1, Ordering::Relaxed);
            }
            "STALE" => {
                self.stale.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// (hits, misses, stale) snapshot.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.stale.load(Ordering::Relaxed),
        )
    }

    /// Total observed responses carrying a recognized `X-Cache` value.
    #[allow(dead_code)]
    pub fn total(&self) -> u64 {
        let (h, m, s) = self.snapshot();
        h + m + s
    }

    /// HIT ratio in percent, counting STALE as a cache serve (the bytes came
    /// from our cache; only their freshness differed). `None` = nothing
    /// counted yet — the UI should say "n/a" rather than fabricate 0%.
    #[allow(dead_code)]
    pub fn ratio_pct(&self) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let (h, _, s) = self.snapshot();
        Some(((h + s) as f64 / total as f64) * 100.0)
    }

    /// Flush current counters to the persist path (atomic tmp+rename).
    /// No-op when constructed without a path. Best-effort: a failed write
    /// is logged at debug and counting continues in memory.
    pub fn persist(&self) {
        let guard = self.path.lock().expect("hit counter path mutex");
        let Some(path) = guard.as_ref() else {
            return;
        };
        let (h, m, s) = self.snapshot();
        let payload = PersistedSnapshot {
            hits: h,
            misses: m,
            stale: s,
        };
        let json = match serde_json::to_string(&payload) {
            Ok(j) => j,
            Err(e) => {
                tracing::debug!("hit counter serialize failed: {}", e);
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json.as_bytes()) {
            tracing::debug!("hit counter persist failed ({}): {}", tmp.display(), e);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::debug!("hit counter rename failed ({}): {}", path.display(), e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_and_ratio() {
        let c = HitCounter::new();
        assert!(c.ratio_pct().is_none(), "no traffic yet → n/a");
        for _ in 0..3 {
            c.observe("HIT");
        }
        c.observe("MISS");
        c.observe("STALE");
        assert_eq!(c.snapshot(), (3, 1, 1));
        let ratio = c.ratio_pct().unwrap();
        assert!((ratio - 80.0).abs() < 1e-9, "STALE counts as a serve");
    }

    #[test]
    fn unknown_values_ignored() {
        let c = HitCounter::new();
        c.observe("HIT");
        c.observe("WEIRD");
        assert_eq!(c.total(), 1);
    }

    #[test]
    fn persist_reload_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hit-counts.json");
        {
            let c = HitCounter::load(path.clone());
            c.observe("HIT");
            c.observe("HIT");
            c.observe("MISS");
            c.persist();
        }
        let reloaded = HitCounter::load(path);
        assert_eq!(reloaded.snapshot(), (2, 1, 0));
        assert!((reloaded.ratio_pct().unwrap() - 100.0 * 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn corrupt_file_loads_as_zeros() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hit-counts.json");
        std::fs::write(&path, b"{not json").expect("write junk");
        let c = HitCounter::load(path);
        assert_eq!(c.snapshot(), (0, 0, 0));
        assert!(c.ratio_pct().is_none());
    }
}
