//! sourcedb (Sprint 21): observation log for version sources.
//!
//! The observation log is append-only: every poll/lookup produces an
//! [`ObservedVersion`] row keyed by [`package_key`]. Rows are never mutated;
//! duplicates (same package_key + source + version) are unioned keeping the
//! later `observed_at`. `version_ord` is a lexicographically sortable canonical
//! key produced by `monopsony_common::version_ord::order_key` — comparisons
//! here are plain string compares, never version parsing.
//!
// Only `refresh` and `delivery` are wired into the server today; the store,
// freeze, resolve, and sources submodules are staged API (tested, unwired).
// Remove this allow as each piece gains a live caller.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod delivery;
pub mod freeze;
pub mod refresh;
pub mod resolve;
pub mod sources;

/// Canonical cross-package-manager key for one (manager, repo, name, arch)
/// tuple. Pinned cross-sprint contract — do not change the format.
pub fn package_key(manager: &str, repo: &str, name: &str, arch: &str) -> String {
    format!("{}|{}|{}|{}", manager, repo, name, arch)
}

/// One append-only observation of a version from a source.
///
/// `source` examples: "aur", "github:owner/repo", "arch:extra".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedVersion {
    pub package_key: String,
    pub source: String,
    pub version: String,
    /// Lexicographically sortable canonical key (version_ord::order_key).
    pub version_ord: String,
    pub scheme: String,
    pub observed_at: i64,
    pub observed_by: String,
}

impl ObservedVersion {
    /// Identity triple for merge purposes.
    fn ident(&self) -> (&str, &str, &str) {
        (&self.package_key, &self.source, &self.version)
    }
}

/// Union existing + incoming observations by (package_key, source, version).
///
/// On a duplicate triple the row with the LATER `observed_at` wins; ties keep
/// the existing row. Rows are never mutated — the result is a fresh vector.
/// Output is sorted by (package_key, source, version) for determinism.
pub fn merge_observations(
    existing: &[ObservedVersion],
    incoming: &[ObservedVersion],
) -> Vec<ObservedVersion> {
    let mut by_ident: BTreeMap<(&str, &str, &str), &ObservedVersion> = BTreeMap::new();
    for obs in existing.iter().chain(incoming.iter()) {
        match by_ident.get(&obs.ident()) {
            // Tie (equal observed_at) keeps the existing entry.
            Some(cur) if obs.observed_at > cur.observed_at => {
                by_ident.insert(obs.ident(), obs);
            }
            _ => {
                by_ident.entry(obs.ident()).or_insert(obs);
            }
        }
    }
    by_ident.into_values().cloned().collect()
}

/// The per-key maximum by `version_ord` comparison. The ord key is a
/// canonical, orderable string, so no version parsing happens here — but
/// note: plain lexicographic compare is NOT enough because `~` (0x7E) sorts
/// above `:` in ASCII, which would rank `1.0~rc1` above `1.0`. Comparison is
/// therefore done on the ord-key components (fixed-width epoch string,
/// tilde-aware dpkg-style body compare, zero-padded release string).
pub fn floor_for(package_key: &str, obs: &[ObservedVersion]) -> Option<ObservedVersion> {
    obs.iter()
        .filter(|o| o.package_key == package_key)
        .max_by(|a, b| cmp_version_ord(&a.version_ord, &b.version_ord))
        .cloned()
}

/// Compare two `version_ord` keys (format `"{epoch:016}:{body}:{release:06}"`)
/// component-wise: epoch as fixed-width string, body with tilde-aware
/// dpkg-style weights, release as fixed-width string. Never parses raw
/// version strings.
pub fn cmp_version_ord(a: &str, b: &str) -> std::cmp::Ordering {
    let (ae, ab, ar) = split_ord_key(a);
    let (be, bb, br) = split_ord_key(b);
    ae.cmp(be)
        .then_with(|| cmp_ord_body(ab, bb))
        .then_with(|| ar.cmp(br))
}

/// Split an order_key into (epoch, body, release) at the fixed 16-digit
/// epoch boundary and the final `:` separator.
fn split_ord_key(k: &str) -> (&str, &str, &str) {
    let safe = |n: usize| std::cmp::min(n, k.len());
    let epoch = &k[..safe(16)];
    let rest = k.get(17..).unwrap_or(""); // skip 16 digits + ':'
    match rest.rfind(':') {
        Some(i) => (epoch, &rest[..i], &rest[i + 1..]),
        None => (epoch, rest, ""),
    }
}

/// dpkg-style compare of an order_key body: alternating non-digit chunks
/// (byte weights, `~` below end-of-string) and digit runs (numeric).
fn cmp_ord_body(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() || j < b.len() {
        // Non-digit phase: weighted byte compare, end-of-string = 0.
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let av = if i < a.len() && !a[i].is_ascii_digit() {
                ord_weight(a[i])
            } else {
                0
            };
            let bv = if j < b.len() && !b[j].is_ascii_digit() {
                ord_weight(b[j])
            } else {
                0
            };
            if av != bv {
                return av.cmp(&bv);
            }
            if av == 0 {
                break;
            }
            i += 1;
            j += 1;
        }
        // Digit phase: numeric compare of the runs (zero-stripped).
        let a_start = i;
        while i < a.len() && a[i].is_ascii_digit() {
            i += 1;
        }
        let b_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let (an, bn) = (&a[a_start..i], &b[b_start..j]);
        let (an, bn) = (strip_zeros(an), strip_zeros(bn));
        if an.len() != bn.len() {
            return an.len().cmp(&bn.len());
        }
        match an.cmp(bn) {
            Equal => {}
            other => return other,
        }
    }
    Equal
}

/// dpkg weight: `~` sorts before end-of-string (-1), letters next, all other
/// bytes after (+256). End-of-string counts as 0.
fn ord_weight(c: u8) -> i32 {
    if c == b'~' {
        -1
    } else if c.is_ascii_alphabetic() {
        c as i32
    } else {
        c as i32 + 256
    }
}

fn strip_zeros(mut s: &[u8]) -> &[u8] {
    while s.len() > 1 && s[0] == b'0' {
        s = &s[1..];
    }
    if s == b"0" {
        &[]
    } else {
        s
    }
}

/// Storage backend for the observation log.
pub trait ObservationStore {
    fn append(&mut self, obs: ObservedVersion) -> anyhow::Result<()>;
    /// Merge a batch into the store; returns the number of distinct triples
    /// added (replacements of existing triples do not count).
    fn merge_incoming(&mut self, incoming: &[ObservedVersion]) -> anyhow::Result<usize>;
    fn floor_for_key(&self, package_key: &str) -> anyhow::Result<Option<ObservedVersion>>;
    fn all_for_key(&self, package_key: &str) -> anyhow::Result<Vec<ObservedVersion>>;
}

/// In-memory [`ObservationStore`] keyed by (package_key, source, version).
#[derive(Debug, Default, Clone)]
pub struct InMemoryStore {
    rows: BTreeMap<(String, String, String), ObservedVersion>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl ObservationStore for InMemoryStore {
    fn append(&mut self, obs: ObservedVersion) -> anyhow::Result<()> {
        let key = (
            obs.package_key.clone(),
            obs.source.clone(),
            obs.version.clone(),
        );
        match self.rows.get(&key) {
            Some(cur) if obs.observed_at > cur.observed_at => {
                self.rows.insert(key, obs);
            }
            Some(_) => {} // tie keeps existing
            None => {
                self.rows.insert(key, obs);
            }
        }
        Ok(())
    }

    fn merge_incoming(&mut self, incoming: &[ObservedVersion]) -> anyhow::Result<usize> {
        let before = self.rows.len();
        let existing: Vec<ObservedVersion> = self.rows.values().cloned().collect();
        let merged = merge_observations(&existing, incoming);
        let added = merged.len() - before;
        self.rows.clear();
        for row in merged {
            self.rows.insert(
                (
                    row.package_key.clone(),
                    row.source.clone(),
                    row.version.clone(),
                ),
                row,
            );
        }
        Ok(added)
    }

    fn floor_for_key(&self, package_key: &str) -> anyhow::Result<Option<ObservedVersion>> {
        let rows: Vec<ObservedVersion> = self
            .rows
            .values()
            .filter(|o| o.package_key == package_key)
            .cloned()
            .collect();
        Ok(floor_for(package_key, &rows))
    }

    fn all_for_key(&self, package_key: &str) -> anyhow::Result<Vec<ObservedVersion>> {
        Ok(self
            .rows
            .values()
            .filter(|o| o.package_key == package_key)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monopsony_common::version_ord::{normalize, order_key};

    fn ord(version: &str) -> String {
        order_key(&normalize(version))
    }

    fn obs(key: &str, source: &str, version: &str, at: i64) -> ObservedVersion {
        ObservedVersion {
            package_key: key.to_string(),
            source: source.to_string(),
            version: version.to_string(),
            version_ord: ord(version),
            scheme: "semver-ish".to_string(),
            observed_at: at,
            observed_by: "test".to_string(),
        }
    }

    const K: &str = "pacman|extra|linux|!x86_64";

    // (a) empty + two distinct-triple rows merges to exactly 2 rows.
    #[test]
    fn test_merge_empty_plus_two() {
        let incoming = vec![obs(K, "aur", "1.0", 100), obs(K, "github:o/r", "2.0", 100)];
        let merged = merge_observations(&[], &incoming);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, "aur");
        assert_eq!(merged[1].source, "github:o/r");
    }

    // (b) re-appending the same triple with a later observed_at keeps 2 rows
    // and the kept row carries the later timestamp.
    #[test]
    fn test_merge_later_observed_at_wins() {
        let existing = vec![obs(K, "aur", "1.0", 100), obs(K, "github:o/r", "2.0", 100)];
        let incoming = vec![obs(K, "aur", "1.0", 200)];
        let merged = merge_observations(&existing, &incoming);
        assert_eq!(merged.len(), 2);
        let aur = merged.iter().find(|o| o.source == "aur").unwrap();
        assert_eq!(aur.observed_at, 200);
        assert_eq!(aur.version, "1.0");
    }

    // (b2) tie on observed_at keeps the existing row.
    #[test]
    fn test_merge_tie_keeps_existing() {
        let existing = vec![obs(K, "aur", "1.0", 100)];
        let incoming = vec![obs(K, "aur", "1.0", 100)];
        let merged = merge_observations(&existing, &incoming);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], existing[0]);
    }

    // (c) same version from two sources -> 2 rows (source is identity).
    #[test]
    fn test_same_version_two_sources() {
        let existing = vec![obs(K, "aur", "1.0", 100)];
        let incoming = vec![obs(K, "arch:extra", "1.0", 150)];
        let merged = merge_observations(&existing, &incoming);
        assert_eq!(merged.len(), 2);
    }

    // (d) floor of {"1.0~rc1","1.0","1.0-1"} is "1.0-1".
    #[test]
    fn test_floor_release_wins() {
        let rows = vec![
            obs(K, "aur", "1.0~rc1", 100),
            obs(K, "aur", "1.0", 101),
            obs(K, "aur", "1.0-1", 102),
        ];
        let floor = floor_for(K, &rows).unwrap();
        assert_eq!(floor.version, "1.0-1");
    }

    // (e) floor of {"2.0", "1:0.9"} is "1:0.9" (epoch dominates).
    #[test]
    fn test_floor_epoch_dominates() {
        let rows = vec![
            obs(K, "aur", "2.0", 100),
            obs(K, "arch:extra", "1:0.9", 101),
        ];
        let floor = floor_for(K, &rows).unwrap();
        assert_eq!(floor.version, "1:0.9");
    }

    // (f) floor of an empty slice is None.
    #[test]
    fn test_floor_empty_is_none() {
        assert!(floor_for(K, &[]).is_none());
    }

    // (g) floor ignores rows from other package_keys.
    #[test]
    fn test_floor_ignores_other_keys() {
        let other = "apt|ubuntu|curl|amd64";
        let rows = vec![
            obs(K, "aur", "1.0", 100),
            obs(other, "apt:ubuntu", "9.9", 200),
        ];
        let floor = floor_for(K, &rows).unwrap();
        assert_eq!(floor.package_key, K);
        assert_eq!(floor.version, "1.0");
        assert!(floor_for(other, &rows).unwrap().version == "9.9");
    }

    // (h) merge of two disjoint sets is commutative.
    #[test]
    fn test_merge_commutative() {
        let a = vec![obs(K, "aur", "1.0", 100), obs(K, "github:o/r", "2.0", 110)];
        let b = vec![obs(K, "arch:extra", "3.0", 120), obs(K, "aur", "0.9", 90)];
        let ab = merge_observations(&a, &b);
        let ba = merge_observations(&b, &a);
        assert_eq!(ab.len(), ba.len());
        let triples = |v: &[ObservedVersion]| -> Vec<(String, String, String)> {
            let mut t: Vec<_> = v
                .iter()
                .map(|o| (o.package_key.clone(), o.source.clone(), o.version.clone()))
                .collect();
            t.sort();
            t
        };
        assert_eq!(triples(&ab), triples(&ba));
    }

    // InMemoryStore: append + merge_incoming counts only new triples.
    #[test]
    fn test_in_memory_store_append_and_merge_count() {
        let mut store = InMemoryStore::new();
        store.append(obs(K, "aur", "1.0", 100)).unwrap();
        store.append(obs(K, "aur", "1.0", 50)).unwrap(); // older: ignored
        assert_eq!(store.len(), 1);
        assert_eq!(store.floor_for_key(K).unwrap().unwrap().observed_at, 100);

        let added = store
            .merge_incoming(&[obs(K, "aur", "1.0", 300), obs(K, "github:o/r", "2.0", 300)])
            .unwrap();
        assert_eq!(added, 1); // aur/1.0 replaced in place, github row is new
        assert_eq!(store.len(), 2);
        assert_eq!(store.all_for_key(K).unwrap().len(), 2);
        assert_eq!(
            store
                .all_for_key(K)
                .unwrap()
                .iter()
                .map(|o| o.observed_at)
                .max(),
            Some(300)
        );
    }

    // package_key format is pinned: manager|repo|name|arch.
    #[test]
    fn test_package_key_format() {
        assert_eq!(
            package_key("pacman", "extra", "linux", "!x86_64"),
            "pacman|extra|linux|!x86_64"
        );
        assert_eq!(package_key("apt", "", "curl", "amd64"), "apt||curl|amd64");
    }
}
