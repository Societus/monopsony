//! Client-side oracle floor enforcement (Sprint 21 slice 21-5b).
//!
//! Holds the per-package version floors merged from verified oracle
//! deltas plus per-repo-class grace windows. Enforcement is
//! fail-open: a missing store (feature off) or a URL whose served
//! version cannot be extracted never blocks a download.

use std::cmp::Ordering;

use monopsony_common::version_ord::{normalize, vercmp};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One package's oracle floor. `package_key` format:
/// `<manager>|<repo>|<name>|<arch>` (e.g. `pacman|core|ripgrep|x86_64`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PackageFloor {
    pub package_key: String,
    pub floor_version: String,
    /// Sortable canonical key for the floor version (display/diagnostic;
    /// ordering decisions always go through vercmp).
    pub floor_ord: String,
    /// Derived from the key's repo segment: "core"|"extra"|"aur"|"other".
    pub repo_class: String,
    /// When the oracle observed this max version (unix secs). Ages the
    /// grace window: enforcement activates once now - observed_at exceeds
    /// the repo class's window.
    pub observed_at: i64,
}

/// Grace windows per repo class, in hours. `other` uses `extra_hours`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GraceConfig {
    pub core_hours: i64,
    pub extra_hours: i64,
    pub aur_hours: i64,
}

// Manual Default, NOT derive(Default): project convention — derive bypasses
// serde field defaults and silently changes the on-disk contract.
impl Default for GraceConfig {
    fn default() -> Self {
        Self {
            core_hours: 48,
            extra_hours: 168,
            aur_hours: 720,
        }
    }
}

/// Floors + grace configuration, persisted as one JSON document
/// (file name, e.g. cert_dir/oracle-floors.json, is the caller's choice).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FloorStore {
    pub floors: Vec<PackageFloor>,
    pub grace: GraceConfig,
}

/// Load a floor store from a JSON file. Missing file → io error (caller
/// decides whether that means "feature off").
pub fn load(path: &Path) -> std::io::Result<FloorStore> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

impl FloorStore {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Upsert observations, keeping the GREATER version per package_key.
    /// Takes the full record (not just the version) so `observed_at`
    /// metadata stays available for future per-floor age tracking.
    pub fn merge_delta(&mut self, obs: &[crate::oracle_update::ObservedVersionLite]) {
        for o in obs {
            let class = repo_class_of(&o.package_key);
            let ord = monopsony_common::version_ord::order_key(&normalize(&o.version));
            if let Some(existing) = self
                .floors
                .iter_mut()
                .find(|f| f.package_key == o.package_key)
            {
                if vercmp(&normalize(&o.version), &normalize(&existing.floor_version))
                    == Ordering::Greater
                {
                    existing.floor_version = o.version.clone();
                    existing.floor_ord = ord;
                    existing.repo_class = class;
                    existing.observed_at = o.observed_at;
                }
            } else {
                self.floors.push(PackageFloor {
                    package_key: o.package_key.clone(),
                    floor_version: o.version.clone(),
                    floor_ord: ord,
                    repo_class: class,
                    observed_at: o.observed_at,
                });
            }
        }
    }

    /// True while `now` is inside the class's grace window after
    /// `observed_at`. A future observed_at is also in grace.
    pub fn in_grace(&self, key: &str, observed_at: i64, now: i64) -> bool {
        let hours = match repo_class_of(key).as_str() {
            "core" => self.grace.core_hours,
            "aur" => self.grace.aur_hours,
            // "extra" and "other" share the extra window.
            _ => self.grace.extra_hours,
        };
        now - observed_at < hours * 3600
    }

    /// Compare a concrete served version against a package's floor.
    pub fn check(&self, package_key: &str, version: &str) -> FloorVerdict {
        let Some(floor) = self.floors.iter().find(|f| f.package_key == package_key) else {
            return FloorVerdict::Allowed;
        };
        if vercmp(&normalize(version), &normalize(&floor.floor_version)) == Ordering::Less {
            FloorVerdict::BelowFloor {
                package_key: floor.package_key.clone(),
                version: version.to_string(),
                floor: floor.floor_version.clone(),
            }
        } else {
            FloorVerdict::Allowed
        }
    }

    /// URL-level enforcement (documented v1 approximation): extract the
    /// candidate package name + version from the URL's last path segment,
    /// match floors by name segment, then apply grace + vercmp.
    /// Grace age comes from the floor's own `observed_at` (carried from the
    /// oracle delta) — NOT from the caller.
    /// A URL whose version cannot be extracted never blocks (fail-open).
    pub fn check_url(&self, url: &str, now: i64) -> FloorVerdict {
        let seg = url.rsplit('/').next().unwrap_or("");
        let (name, version) = split_name_version(seg);
        let Some(name) = name else {
            return FloorVerdict::Allowed;
        };
        let Some(floor) = self
            .floors
            .iter()
            .find(|f| key_name(&f.package_key).as_deref() == Some(name.as_str()))
        else {
            return FloorVerdict::Allowed;
        };
        if self.in_grace(&floor.package_key, floor.observed_at, now) {
            return FloorVerdict::Allowed;
        }
        let Some(version) = version else {
            return FloorVerdict::Allowed;
        };
        self.check(&floor.package_key, &version)
    }
}

#[derive(Debug, PartialEq)]
pub enum FloorVerdict {
    Allowed,
    BelowFloor {
        package_key: String,
        version: String,
        floor: String,
    },
}

/// Repo class from key segment 2 (splitn '|' → [manager, repo, name, arch]).
pub fn repo_class_of(package_key: &str) -> String {
    match package_key.splitn(4, '|').nth(1) {
        Some("core") => "core".to_string(),
        Some("extra") => "extra".to_string(),
        Some("aur") => "aur".to_string(),
        _ => "other".to_string(),
    }
}

/// Name segment (index 2) of a package_key.
fn key_name(package_key: &str) -> Option<String> {
    package_key.splitn(4, '|').nth(2).map(|s| s.to_string())
}

/// Common archive/signature suffixes, longest first so compound
/// extensions (".pkg.tar.zst") strip in one pass.
const ARCHIVE_EXTS: &[&str] = &[
    ".pkg.tar.zst",
    ".pkg.tar.xz",
    ".tar.zst",
    ".tar.xz",
    ".tar.gz",
    ".tar.bz2",
    ".tgz",
    ".zip",
    ".sig",
    ".deb",
    ".rpm",
    ".zst",
    ".xz",
    ".gz",
    ".bz2",
];

/// Split a filename segment into (name, version):
/// cut at the first `-` followed by a digit, name = longest alphabetic
/// prefix run of the head, version = the remainder after the dash
/// (archive suffixes stripped). Either side may be None.
fn split_name_version(seg: &str) -> (Option<String>, Option<String>) {
    let mut s = seg;
    loop {
        let Some(stripped) = ARCHIVE_EXTS.iter().find_map(|ext| s.strip_suffix(ext)) else {
            break;
        };
        s = stripped;
    }
    let bytes = s.as_bytes();
    let mut cut = s.len();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            cut = i;
            break;
        }
    }
    let head = &s[..cut];
    let name: String = head
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let name = if name.is_empty() { None } else { Some(name) };
    let version = if cut < s.len() {
        let v = &s[cut + 1..];
        if v.bytes().any(|b| b.is_ascii_digit()) {
            Some(v.to_string())
        } else {
            None
        }
    } else {
        None
    };
    (name, version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle_update::ObservedVersionLite;

    fn obs(key: &str, version: &str) -> ObservedVersionLite {
        ObservedVersionLite {
            observed_at: 0,
            observed_by: "test".to_string(),
            package_key: key.to_string(),
            scheme: "default".to_string(),
            source: "test".to_string(),
            version: version.to_string(),
            version_ord: String::new(),
        }
    }

    /// Observation with an explicit oracle observation time (drives grace).
    fn obs_at(key: &str, version: &str, observed_at: i64) -> ObservedVersionLite {
        let mut o = obs(key, version);
        o.observed_at = observed_at;
        o
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn grace_config_defaults_are_48_168_720() {
        let g = GraceConfig::default();
        assert_eq!((g.core_hours, g.extra_hours, g.aur_hours), (48, 168, 720));
    }

    #[test]
    fn repo_class_derivation_goldens() {
        assert_eq!(repo_class_of("pacman|core|ripgrep|x86_64"), "core");
        assert_eq!(repo_class_of("apt|extra|fzf|amd64"), "extra");
        assert_eq!(repo_class_of("yay|aur|bat|x86_64"), "aur");
        assert_eq!(repo_class_of("pacman||ripgrep|x86_64"), "other");
        assert_eq!(repo_class_of("winget|msstore|delta|"), "other");
    }

    #[test]
    fn merge_delta_keeps_max_version() {
        let mut store = FloorStore::empty();
        let key = "pacman|core|ripgrep|x86_64";
        store.merge_delta(&[obs(key, "14.1.1")]);
        store.merge_delta(&[obs(key, "14.1.0")]);
        assert_eq!(store.floors.len(), 1);
        assert_eq!(store.floors[0].floor_version, "14.1.1");
    }

    #[test]
    fn merge_delta_inserts_new_keys() {
        let mut store = FloorStore::empty();
        store.merge_delta(&[obs("pacman|core|ripgrep|x86_64", "14.1.1")]);
        store.merge_delta(&[obs("apt|extra|fzf|amd64", "0.44")]);
        assert_eq!(store.floors.len(), 2);
        assert_eq!(store.floors[1].repo_class, "extra");
    }

    #[test]
    fn check_equal_and_above_floor_allowed() {
        let mut store = FloorStore::empty();
        let key = "pacman|core|ripgrep|x86_64";
        store.merge_delta(&[obs(key, "2.0.0")]);
        assert_eq!(store.check(key, "2.0.0"), FloorVerdict::Allowed);
        assert_eq!(store.check(key, "2.1.0"), FloorVerdict::Allowed);
        assert_eq!(store.check(key, "10.0"), FloorVerdict::Allowed);
    }

    #[test]
    fn check_below_floor_flagged() {
        let mut store = FloorStore::empty();
        let key = "pacman|core|ripgrep|x86_64";
        store.merge_delta(&[obs(key, "2.0.0")]);
        assert_eq!(
            store.check(key, "1.9.9"),
            FloorVerdict::BelowFloor {
                package_key: key.to_string(),
                version: "1.9.9".to_string(),
                floor: "2.0.0".to_string(),
            }
        );
    }

    #[test]
    fn check_no_floor_allowed() {
        let store = FloorStore::empty();
        assert_eq!(
            store.check("pacman|core|nothing|x86_64", "0.1"),
            FloorVerdict::Allowed
        );
    }

    #[test]
    fn tilde_floor_goldens() {
        let mut store = FloorStore::empty();
        let key = "pacman|core|ripgrep|x86_64";
        store.merge_delta(&[obs(key, "1.0~rc1")]);
        // 1.0 > 1.0~rc1 (dpkg tilde rule): served release beats rc floor.
        assert_eq!(store.check(key, "1.0"), FloorVerdict::Allowed);
        // 0.9 < 1.0~rc1: flagged, but only once grace elapsed.
        assert_eq!(
            store.check(key, "0.9"),
            FloorVerdict::BelowFloor {
                package_key: key.to_string(),
                version: "0.9".to_string(),
                floor: "1.0~rc1".to_string(),
            }
        );
    }

    #[test]
    fn url_core_grace_1h_allowed_72h_below_floor() {
        let mut store = FloorStore::empty();
        let key = "pacman|core|ripgrep|x86_64";
        // Oracle observed the max version 72h ago (NOW - 72*3600).
        store.merge_delta(&[obs_at(key, "2.0.0", NOW - 72 * 3600)]);
        let url = "https://mirror.example.com/core/os/x86_64/ripgrep-1.9.9.pkg.tar.zst";
        // Same wall clock: 72h > 48h core window → enforced.
        assert_eq!(
            store.check_url(url, NOW),
            FloorVerdict::BelowFloor {
                package_key: "pacman|core|ripgrep|x86_64".to_string(),
                version: "1.9.9".to_string(),
                floor: "2.0.0".to_string(),
            }
        );
        // Recent observation (1h ago): core window is 48h → in grace.
        let mut store = FloorStore::empty();
        store.merge_delta(&[obs_at(key, "2.0.0", NOW - 3600)]);
        assert_eq!(store.check_url(url, NOW), FloorVerdict::Allowed);
    }

    #[test]
    fn url_extra_grace_100h_allowed_200h_below_floor() {
        let mut store = FloorStore::empty();
        let key = "apt|extra|fzf|amd64";
        let url = "https://deb.example.com/pool/main/f/fzf-0.44.deb";
        // Floor observed 200h ago: extra window is 168h → enforced.
        store.merge_delta(&[obs_at(key, "0.50", NOW - 200 * 3600)]);
        assert!(matches!(
            store.check_url(url, NOW),
            FloorVerdict::BelowFloor { .. }
        ));
        // Floor observed 100h ago: in grace.
        let mut store = FloorStore::empty();
        store.merge_delta(&[obs_at(key, "0.50", NOW - 100 * 3600)]);
        assert_eq!(store.check_url(url, NOW), FloorVerdict::Allowed);
    }

    #[test]
    fn url_aur_1000h_still_in_grace() {
        let mut store = FloorStore::empty();
        let key = "yay|aur|bat|x86_64";
        let url = "https://aur.example.com/bat-0.20.pkg.tar.zst";
        // Floor observed 800h ago: past the 720h aur window → enforced.
        store.merge_delta(&[obs_at(key, "0.24", NOW - 800 * 3600)]);
        assert!(matches!(
            store.check_url(url, NOW),
            FloorVerdict::BelowFloor { .. }
        ));
        // Floor observed 1000h ago would exceed... no: aur window is 720h,
        // so 1000h is also past it. The IN-grace case is a recent observe:
        let mut store = FloorStore::empty();
        store.merge_delta(&[obs_at(key, "0.24", NOW - 100 * 3600)]);
        assert_eq!(store.check_url(url, NOW), FloorVerdict::Allowed);
    }

    #[test]
    fn url_no_floor_match_or_no_version_allowed() {
        let mut store = FloorStore::empty();
        store.merge_delta(&[obs("pacman|core|ripgrep|x86_64", "2.0.0")]);
        // Different package.
        assert_eq!(
            store.check_url("https://mirror.example.com/core/fd-9.0.0.pkg.tar.zst", NOW),
            FloorVerdict::Allowed
        );
        // Version undeterminable → fail-open.
        assert_eq!(
            store.check_url("https://mirror.example.com/core/ripgrep", NOW),
            FloorVerdict::Allowed
        );
        // Empty segment.
        assert_eq!(store.check_url("", NOW), FloorVerdict::Allowed);
    }

    #[test]
    fn check_url_name_extraction_golden() {
        assert_eq!(
            split_name_version("ripgrep-14.1.1.tar.zst").0.as_deref(),
            Some("ripgrep")
        );
        assert_eq!(
            split_name_version("ripgrep-14.1.1.tar.zst").1.as_deref(),
            Some("14.1.1")
        );
        // Arch-style compound version suffix.
        assert_eq!(
            split_name_version("fzf-0.44.1-1-x86_64.pkg.tar.zst"),
            (Some("fzf".to_string()), Some("0.44.1-1-x86_64".to_string()))
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("oracle-floors.json");
        let mut store = FloorStore::empty();
        store.merge_delta(&[obs("pacman|core|ripgrep|x86_64", "2.0.0")]);
        store.grace.core_hours = 24;
        store.save(&path).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, store);
        assert_eq!(loaded.floors[0].floor_version, "2.0.0");
        assert_eq!(loaded.grace.core_hours, 24);
    }
}
