//! OS-release availability catalog (Sprint 29 Phase 2, grounding D2).
//!
//! The server curates `(distro → currently available majors)` as the
//! `server_settings` key `os_release_catalog` (JSONB, runtime-editable).
//! This module holds the code-default seed (debian/ubuntu) and the merge
//! that turns a client's reported distro/version into `OsReleaseUpgrade`
//! rows. Invariant: an unknown major NEVER shows as an available upgrade —
//! a candidate only becomes a row when its major is strictly greater than
//! the client's current major AND the distro has a catalog entry.

use serde::{Deserialize, Serialize};

/// Code-default seed for the `os_release_catalog` server setting. Used when
/// the key is absent; an operator-set value (via `db.set_setting`) always
/// wins and is never overwritten by this default.
pub fn default_catalog() -> serde_json::Value {
    serde_json::json!({
        "debian": ["13"],
        "ubuntu": ["24.04"],
    })
}

/// A release-upgrade row produced by merging a client's reported distro
/// identity against the curated catalog. Wire-shape mirrors the proto
/// `OsReleaseUpgrade` message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OsReleaseUpgradeRow {
    pub distro: String,
    pub from_version: String,
    pub to_version: String,
    /// True only when the client's vendor preflight (Phase 3) reports clean.
    /// Phase 2 has no preflight data, so this is always false here.
    pub ready: bool,
    pub readiness_notes: Vec<String>,
}

/// Parse the leading major number out of a version string ("12" → 12,
/// "13.2" → 13, "trixie" → None). Non-numeric majors never upgrade — the
/// comparison needs an ordering we do not guess.
fn parse_major(version: &str) -> Option<u64> {
    version
        .split('.')
        .next()
        .and_then(|m| m.trim().parse::<u64>().ok())
}

/// Merge a client's reported distro/version against the curated catalog.
///
/// Rules (plan decision #3, grounding D2):
/// - no catalog entry for the distro → no rows;
/// - candidate versions whose major is NOT strictly greater than the
///   client's current major never produce a row — this is what keeps an
///   unknown/synthetic major (e.g. debian "99") from adopting an upgrade;
/// - unparsable current or candidate versions are skipped (defensive; the
///   catalog is operator-curated, client reports are not trusted).
pub fn merge_release_rows(
    catalog: &serde_json::Value,
    distro: &str,
    current_version: &str,
) -> Vec<OsReleaseUpgradeRow> {
    let Some(entries) = catalog.get(distro).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let Some(current_major) = parse_major(current_version) else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    for candidate in entries {
        let Some(to_version) = candidate.as_str() else {
            continue;
        };
        let Some(candidate_major) = parse_major(to_version) else {
            continue;
        };
        // Strictly greater only: an unknown major never shows as an
        // available upgrade, and an already-current client sees nothing.
        if candidate_major > current_major {
            rows.push(OsReleaseUpgradeRow {
                distro: distro.to_string(),
                from_version: current_version.to_string(),
                to_version: to_version.to_string(),
                ready: false,
                readiness_notes: vec![
                    "vendor preflight not run yet (sprint 29 phase 3)".to_string()
                ],
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_catalog_seeds_debian_and_ubuntu() {
        let c = default_catalog();
        assert_eq!(c["debian"][0], "13");
        assert_eq!(c["ubuntu"][0], "24.04");
    }

    #[test]
    fn known_major_upgrades_to_catalog_entry() {
        let rows = merge_release_rows(&default_catalog(), "debian", "12");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].from_version, "12");
        assert_eq!(rows[0].to_version, "13");
        assert!(!rows[0].ready, "phase 2 carries no preflight verdict");
    }

    #[test]
    fn unknown_major_never_shows_available_upgrade() {
        // Synthetic unsupported major: catalog candidate 13 is not > 99.
        let rows = merge_release_rows(&default_catalog(), "debian", "99");
        assert!(rows.is_empty());
    }

    #[test]
    fn already_current_client_gets_no_row() {
        let rows = merge_release_rows(&default_catalog(), "debian", "13");
        assert!(rows.is_empty());
    }

    #[test]
    fn distro_without_catalog_entry_gets_no_rows() {
        assert!(merge_release_rows(&default_catalog(), "arch", "rolling").is_empty());
    }

    #[test]
    fn unparsable_client_version_is_skipped() {
        assert!(merge_release_rows(&default_catalog(), "debian", "trixie/sid").is_empty());
    }

    #[test]
    fn non_string_catalog_candidates_are_ignored() {
        let catalog = serde_json::json!({ "debian": [13, "14"] });
        let rows = merge_release_rows(&catalog, "debian", "12");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].to_version, "14");
    }

    #[test]
    fn dot_versions_compare_by_major() {
        let catalog = serde_json::json!({ "ubuntu": ["25.04"] });
        let rows = merge_release_rows(&catalog, "ubuntu", "24.04");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].to_version, "25.04");
    }
}
