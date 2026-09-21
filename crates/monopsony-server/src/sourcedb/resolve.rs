//! Resolution pipeline core (Sprint 21 slice 21-3a).
//!
//! Offline-testable building blocks for the sourcedb resolution pipeline:
//! the client-count weighted worklist, the resolution result type (unmapped
//! is a STATE, never an error), a sliding-window rate limiter, and ETag
//! revalidation matching. No network happens in these types.

use std::collections::BTreeMap;

use crate::sourcedb::ObservedVersion;

/// One worklist row: a package worth resolving upstream, weighted by the
/// max client_count seen across its inventory observations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorklistEntry {
    pub package_key: String,
    pub manager: String,
    pub repo: String,
    pub name: String,
    pub arch: String,
    pub client_count: u64,
}

/// Build the resolution worklist from the inventory union.
///
/// Entries are deduped by `package_key`; the weight is the MAX client_count
/// over all observations of that key. Packages nobody runs (client_count 0)
/// never enter the worklist. Output is sorted by package_key for determinism.
pub fn build_worklist(
    inventory: &[ObservedVersion],
    weights: &std::collections::HashMap<String, u64>,
) -> Vec<WorklistEntry> {
    let mut best: BTreeMap<&str, u64> = BTreeMap::new();
    for obs in inventory {
        let w = weights.get(&obs.package_key).copied().unwrap_or(0);
        if w == 0 {
            continue;
        }
        let slot = best.entry(obs.package_key.as_str()).or_insert(0);
        if w > *slot {
            *slot = w;
        }
    }
    best.into_iter()
        .filter_map(|(key, count)| {
            let mut parts = key.split('|');
            let manager = parts.next()?.to_string();
            let repo = parts.next()?.to_string();
            let name = parts.next()?.to_string();
            let arch = parts.next()?.to_string();
            Some(WorklistEntry {
                package_key: key.to_string(),
                manager,
                repo,
                name,
                arch,
                client_count: count,
            })
        })
        .collect()
}

/// Sentinel carried in [`Resolution::mapping`] state space: the package has
/// not been resolved yet. Unmapped is a STATE, never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unmapped {
    NotYetResolved,
}

/// Result of attempting to resolve one package_key upstream.
/// `mapping: None` = "unmapped" — a state, not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub package_key: String,
    pub mapping: Option<crate::sourcedb::sources::ResolvedMapping>,
}

/// Sliding-window rate budget snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateBudget {
    pub remaining: u64,
    pub reset_at: i64,
}

/// Sliding-window rate limiter: at most `max_per_window` admits per
/// `window_secs`, measured over admission timestamps.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    max_per_window: u64,
    window_secs: i64,
    timestamps: Vec<i64>,
}

impl RateLimiter {
    pub fn new(max_per_window: u64, window_secs: i64) -> Self {
        Self {
            max_per_window,
            window_secs,
            timestamps: Vec::new(),
        }
    }

    /// Try to admit one request at time `now`. Drops timestamps older than
    /// `now - window_secs`, admits iff the window is not full.
    pub fn try_acquire(&mut self, now: i64) -> bool {
        let cutoff = now - self.window_secs;
        self.timestamps.retain(|&t| t > cutoff);
        if self.timestamps.len() as u64 >= self.max_per_window {
            return false;
        }
        self.timestamps.push(now);
        true
    }

    /// Current budget snapshot at time `now`.
    pub fn budget(&mut self, now: i64) -> RateBudget {
        let cutoff = now - self.window_secs;
        self.timestamps.retain(|&t| t > cutoff);
        RateBudget {
            remaining: self
                .max_per_window
                .saturating_sub(self.timestamps.len() as u64),
            reset_at: self.timestamps.first().copied().unwrap_or(0) + self.window_secs,
        }
    }
}

/// Client cache revalidation semantic: true iff both etags are present and
/// equal (the client's `If-None-Match` still matches, so a 304 applies).
pub fn etag_match(etag: Option<&str>, if_none_match: Option<&str>) -> bool {
    match (etag, if_none_match) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sourcedb::package_key;

    fn obs(key: &str) -> ObservedVersion {
        ObservedVersion {
            package_key: key.to_string(),
            source: "aur".to_string(),
            version: "1.0".to_string(),
            version_ord: "0".repeat(16) + ":1.0:000000",
            scheme: "semver-ish".to_string(),
            observed_at: 100,
            observed_by: "test".to_string(),
        }
    }

    fn weights(pairs: &[(&str, u64)]) -> std::collections::HashMap<String, u64> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }

    // Worklist dedupes by package_key and takes max weight.
    #[test]
    fn test_worklist_dedupe_weight_max() {
        let k1 = package_key("pacman", "aur", "ripgrep", "x86_64");
        let k2 = package_key("apt", "ubuntu", "curl", "amd64");
        let inventory = vec![obs(&k1), obs(&k1), obs(&k2)];
        let w = weights(&[(&k1, 3), (&k1, 7), (&k2, 2)]);
        let wl = build_worklist(&inventory, &w);
        assert_eq!(wl.len(), 2);
        let rg = wl.iter().find(|e| e.name == "ripgrep").unwrap();
        assert_eq!(rg.client_count, 7); // max, not sum
        assert_eq!(rg.manager, "pacman");
        assert_eq!(rg.repo, "aur");
        assert_eq!(rg.arch, "x86_64");
    }

    // Empty inventory -> empty worklist.
    #[test]
    fn test_worklist_empty_inventory() {
        assert!(build_worklist(&[], &weights(&[])).is_empty());
    }

    // Packages nobody runs (weight 0 / absent) never enter.
    #[test]
    fn test_worklist_zero_weight_excluded() {
        let k1 = package_key("pacman", "aur", "wanted", "x86_64");
        let k2 = package_key("pacman", "aur", "unwanted", "x86_64");
        let inventory = vec![obs(&k1), obs(&k2)];
        let wl = build_worklist(&inventory, &weights(&[(&k1, 5)]));
        assert_eq!(wl.len(), 1);
        assert_eq!(wl[0].name, "wanted");
    }

    // Resolution: None mapping is a STATE (NotYetResolved), not an error.
    #[test]
    fn test_resolution_none_is_state_not_error() {
        let unmapped = Resolution {
            package_key: "pacman|aur|foo|x86_64".to_string(),
            mapping: None,
        };
        debug_assert!(
            matches!(unmapped.mapping, None),
            "None mapping must be representable without error"
        );
        let mapped = Resolution {
            package_key: unmapped.package_key.clone(),
            mapping: Some(crate::sourcedb::sources::ResolvedMapping {
                source: "aur".to_string(),
                origin: Some("foo".to_string()),
                homepage: None,
            }),
        };
        assert_eq!(unmapped.package_key, mapped.package_key);
        assert!(unmapped.mapping.is_none() && mapped.mapping.is_some());
    }

    // Rate limiter: 3 per 60s window — admits 3, rejects the 4th.
    #[test]
    fn test_rate_limiter_window() {
        let mut rl = RateLimiter::new(3, 60);
        assert!(rl.try_acquire(1000));
        assert!(rl.try_acquire(1010));
        assert!(rl.try_acquire(1020));
        assert!(!rl.try_acquire(1030)); // window full
        assert!(!rl.try_acquire(1059)); // still inside window
    }

    // Sliding: at now+61 the first timestamp has aged out, admission works.
    #[test]
    fn test_rate_limiter_slides() {
        let mut rl = RateLimiter::new(3, 60);
        assert!(rl.try_acquire(1000));
        assert!(rl.try_acquire(1010));
        assert!(rl.try_acquire(1020));
        assert!(!rl.try_acquire(1030));
        assert!(rl.try_acquire(1061)); // 1000 aged out (1061 - 60 = 1001 > 1000)
        assert_eq!(rl.timestamps.len(), 3);
    }

    // etag_match truth table (both-None is false).
    #[test]
    fn test_etag_match_truth_table() {
        assert!(etag_match(Some("\"abc\""), Some("\"abc\""))); // both, equal
        assert!(!etag_match(Some("\"abc\""), Some("\"def\""))); // both, differ
        assert!(!etag_match(Some("\"abc\""), None)); // server only
        assert!(!etag_match(None, Some("\"abc\""))); // client only
        assert!(!etag_match(None, None)); // neither
    }
}
