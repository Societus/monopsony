//! Sprint 23 Phase C — retention engine: version floors + LRU eviction planner.
//!
//! Pure, offline module: no I/O, no DB, no clock. The janitor loop (parent-wired
//! into `purge.rs` later) gathers [`RetentionCandidate`]s for all LangArtifact
//! cache entries and calls [`plan_eviction`]; this module answers with the set
//! of cache keys to evict, oldest-access-first.
//!
//! Policy (plan §2.2):
//!
//! 1. **Version floors** — never evict, per (ecosystem, name):
//!    - the `floor_majors` most recent major *lines* (e.g. floor 2 over majors
//!      1/2/3 protects 3.x and 2.x; 1.x is evictable),
//!    - within the `0.x` line, the `floor_minors` most recent minors,
//!    - within the `0.0.x` line, the `floor_patches` most recent patches.
//!    The `0.x` line is deliberately NOT ranked among the major lines: it is
//!    governed by the finer-grained minor/patch floors instead (otherwise a
//!    lone `0.x` package would be entirely un-evictable).
//! 2. **LRU** among non-floor-protected entries, oldest `last_accessed` first.
//!    Unparseable versions rank below everything (evicted before any parseable
//!    entry, regardless of access time) and are never floor-protected.
//! 3. **Budget** — evict only while `total_size − evicted_size > budget_bytes`.
//!    `budget_bytes == 0` means eviction is off entirely (empty result).
//!
//! Versions are parsed into semver-family `(major, minor, patch, pre)` tuples;
//! full-version ordering ties go through [`version_ord::vercmp`] (never
//! lexicographic `order_key` sorts — tilde pre-releases misrank that way).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use monopsony_common::config::LanguageArtifactConfig;
use monopsony_common::version_ord;

/// One LangArtifact cache entry as observed by the caller (filesystem/DB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidate {
    pub key: String,
    /// `"npm" | "pypi" | "cargo"` — first path segment of the cache key.
    pub ecosystem: String,
    /// Parsed per-backend from the cache path.
    pub name: String,
    /// Semver-family version; may be `""` if unparseable.
    pub version: String,
    pub size_bytes: u64,
    /// Epoch seconds of last access.
    pub last_accessed: i64,
}

/// A cache path decomposed into retention-relevant coordinates.
///
/// Only *artifact-class* entries parse: metadata entries (packuments, sparse
/// index pages, simple-index pages) return `None` and must never be passed to
/// [`plan_eviction`] — the caller filters on `cache_class == LangArtifact`
/// before building candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArtifactPath {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
}

/// Decompose a Phase-B cache path into (ecosystem, name, version).
///
/// Recognized shapes (mirroring the staged backends' `url_to_cache_path`):
/// - npm tarball:   `npm/artifact/<name>/-/<file>.tgz` (scoped names OK)
/// - npm packument: `npm/<name>` → `None` (metadata, not a candidate)
/// - pip artifact:  `pip/files/.../<name>-<ver>.whl` / `.tar.gz` / `.zip`
/// - pip metadata:  `pip/<anything-not-files/...>` → `None`
/// - cargo blob:    `cargo/blob/.../crates/<name>/<version>/download`
/// - cargo index:   `cargo/index/...` → `None` (mutable metadata)
pub fn parse_artifact_path(path: &str) -> Option<ParsedArtifactPath> {
    let path = path.trim_start_matches('/');
    let (ecosystem, rest) = path.split_once('/')?;
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match ecosystem {
        "npm" => parse_npm_path(&segments),
        "pip" => parse_pip_path(&segments),
        "cargo" => parse_cargo_path(&segments),
        "brew" => parse_brew_path(&segments),
        _ => None,
    }
    .map(|(name, version)| ParsedArtifactPath {
        ecosystem: ecosystem.to_string(),
        name,
        version,
    })
}

/// npm: `["artifact", <name segments…>, "-", "<name>-<ver>.tgz"]`.
/// Anything else (packuments) is metadata → `None`.
fn parse_npm_path(segments: &[&str]) -> Option<(String, String)> {
    if *segments.first()? != "artifact" {
        return None; // packument: npm/<name> — metadata, not a candidate
    }
    let dash = segments.iter().position(|s| *s == "-")?;
    let name_segments = &segments[1..dash];
    if name_segments.is_empty() {
        return None;
    }
    let name = name_segments.join("/");
    let filename = *segments.last()?;
    let stem = filename.strip_suffix(".tgz")?;
    // Tarball stem is "<last-name-segment>-<version>".
    let name_prefix = format!("{}-", name_segments[name_segments.len() - 1]);
    let version = stem
        .strip_prefix(name_prefix.as_str())
        .unwrap_or("")
        .to_string();
    Some((name, version))
}

/// pip: `["files", …, "<name>-<ver>.whl|.tar.gz|.zip"]` (artifact class).
/// The simple-index layout (`pip/<…>` without a leading `files`) is metadata.
fn parse_pip_path(segments: &[&str]) -> Option<(String, String)> {
    if *segments.first()? != "files" {
        return None;
    }
    let filename = *segments.last()?;
    if let Some(stem) = filename.strip_suffix(".whl") {
        // Wheel: <name>-<version>-<python>-<abi>-<platform>.whl
        let mut parts = stem.split('-');
        let name = parts.next()?.to_string();
        let version = parts.next()?.to_string();
        Some((normalize_pypi_name(&name), version))
    } else {
        // sdist: <name>-<version>.tar.gz / .zip — split on the LAST '-'.
        let stem = filename
            .strip_suffix(".tar.gz")
            .or_else(|| filename.strip_suffix(".zip"))?;
        let dash = stem.rfind('-')?;
        Some((
            normalize_pypi_name(&stem[..dash]),
            stem[dash + 1..].to_string(),
        ))
    }
}

fn normalize_pypi_name(name: &str) -> String {
    // Filenames use the escaped form; canonical PyPI names use '-'.
    name.replace('_', "-")
}

/// cargo: `["blob", …, "crates", <name>, <version>, "download"]`.
/// The sparse index (`cargo/index/…`) is mutable metadata → `None`.
fn parse_cargo_path(segments: &[&str]) -> Option<(String, String)> {
    if *segments.first()? != "blob" {
        return None;
    }
    let crates_pos = segments.iter().position(|s| *s == "crates")?;
    let name = segments.get(crates_pos + 1)?.to_string();
    let version = segments.get(crates_pos + 2)?.to_string();
    Some((name, version))
}

/// brew bottles: `["bottles", <name>, <version>, "<file>.tar.gz"]` (the
/// `brew/` ecosystem prefix is already split off by the caller).
/// The api/ and v2/ shapes are metadata, not candidates → `None`.
fn parse_brew_path(segments: &[&str]) -> Option<(String, String)> {
    if segments.len() < 4 || segments[0] != "bottles" {
        return None; // api/, v2/ shapes = metadata, not candidates
    }
    Some((segments[1].to_string(), segments[2].to_string()))
}

/// Semver-family tuple passed into the floor logic.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    /// Pre-release body (`rc1` in `1.0.0-rc1`), empty when absent.
    pre: String,
    /// Original string (for `vercmp` tie-breaks).
    raw: String,
}

/// Parse a semver-family version. Returns `None` for anything without a
/// numeric major (callers treat `None` as lowest-priority / never-floored).
fn parse_semver(v: &str) -> Option<SemVer> {
    let mut s = v.trim();
    if s.is_empty() {
        return None;
    }
    // Lenient `v` prefix, matching version_ord's default scheme.
    if (s.starts_with('v') || s.starts_with('V'))
        && s[1..].bytes().next().is_some_and(|b| b.is_ascii_digit())
    {
        s = &s[1..];
    }
    let s = s.split('+').next()?; // strip build metadata
                                  // Pre-release separator: semver '-' or dpkg-tilde '~' (`1.0.0~rc1`).
    let pre_split = s.find(['-', '~']);
    let (core, pre) = match pre_split {
        Some(pos) => (&s[..pos], &s[pos + 1..]),
        None => (s, ""),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    if major_part_empty(core) {
        return None;
    }
    let minor = next_number(&mut parts)?;
    let patch = next_number(&mut parts)?;
    Some(SemVer {
        major,
        minor,
        patch,
        pre: pre.to_string(),
        raw: v.trim().to_string(),
    })
}

fn major_part_empty(core: &str) -> bool {
    core.is_empty() || core.starts_with('.')
}

fn next_number(parts: &mut std::str::Split<'_, char>) -> Option<u64> {
    match parts.next() {
        None => Some(0), // "1" / "1.2" — absent components default to 0
        Some("") => None,
        Some(s) => s.parse::<u64>().ok(),
    }
}

/// Floor-protection sets for one (ecosystem, name) group.
#[derive(Debug, Default)]
struct FloorSets {
    protected_majors: HashSet<u64>,
    protected_minors: HashSet<u64>,
    protected_patches: HashSet<u64>,
}

impl FloorSets {
    fn build(versions: &[Option<SemVer>], cfg: &LanguageArtifactConfig) -> FloorSets {
        let mut fs = FloorSets::default();
        let parsed: Vec<&SemVer> = versions.iter().flatten().collect();

        // Major lines: distinct nonzero majors, most recent first.
        let mut majors: Vec<u64> = parsed.iter().map(|v| v.major).filter(|m| *m > 0).collect();
        majors.sort_unstable_by(|a, b| b.cmp(a));
        majors.dedup();
        for m in majors.into_iter().take(cfg.floor_majors as usize) {
            fs.protected_majors.insert(m);
        }

        // 0.x line: distinct nonzero minors, most recent first.
        let mut minors: Vec<u64> = parsed
            .iter()
            .filter(|v| v.major == 0 && v.minor > 0)
            .map(|v| v.minor)
            .collect();
        minors.sort_unstable_by(|a, b| b.cmp(a));
        minors.dedup();
        for m in minors.into_iter().take(cfg.floor_minors as usize) {
            fs.protected_minors.insert(m);
        }

        // 0.0.x line: distinct patches, most recent first.
        let mut patches: Vec<u64> = parsed
            .iter()
            .filter(|v| v.major == 0 && v.minor == 0)
            .map(|v| v.patch)
            .collect();
        patches.sort_unstable_by(|a, b| b.cmp(a));
        patches.dedup();
        for p in patches.into_iter().take(cfg.floor_patches as usize) {
            fs.protected_patches.insert(p);
        }

        fs
    }

    fn protects(&self, v: &SemVer) -> bool {
        if v.major > 0 {
            self.protected_majors.contains(&v.major)
        } else if v.minor > 0 {
            self.protected_minors.contains(&v.minor)
        } else {
            self.protected_patches.contains(&v.patch)
        }
    }
}

/// Full-version ordering for eviction tie-breaks: semver rule first (release
/// > its pre-releases, `~`/`-` bodies order via vercmp), falling back to
/// `version_ord::vercmp` — never `order_key` string sorts (tilde misranks).
fn cmp_versions(a: &SemVer, b: &SemVer) -> Ordering {
    a.major
        .cmp(&b.major)
        .then_with(|| a.minor.cmp(&b.minor))
        .then_with(|| a.patch.cmp(&b.patch))
        .then_with(|| match (a.pre.is_empty(), b.pre.is_empty()) {
            (true, false) => Ordering::Greater, // release sorts after its pre-releases
            (false, true) => Ordering::Less,
            _ => version_ord::vercmp(
                &version_ord::normalize(&a.raw),
                &version_ord::normalize(&b.raw),
            ),
        })
}

/// Plan the eviction set for LangArtifact entries.
///
/// Returns the keys to evict, in eviction order (oldest-accessed first;
/// unparseable versions before parseable ones). Eviction stops as soon as
/// remaining total size would be within `cfg.budget_bytes`. `budget_bytes == 0`
/// disables eviction entirely. Floor-protected entries are never returned, even
/// when that means staying over budget.
pub fn plan_eviction(
    candidates: &[RetentionCandidate],
    cfg: &LanguageArtifactConfig,
) -> Vec<String> {
    if cfg.budget_bytes == 0 || candidates.is_empty() {
        return Vec::new();
    }

    // Floor sets per (ecosystem, name), computed from every candidate so a
    // protected version's presence shields its line regardless of budget.
    let mut groups: HashMap<(&str, &str), Vec<Option<SemVer>>> = HashMap::new();
    for c in candidates {
        groups
            .entry((c.ecosystem.as_str(), c.name.as_str()))
            .or_default()
            .push(parse_semver(&c.version));
    }
    let floors: HashMap<(&str, &str), FloorSets> = groups
        .into_iter()
        .map(|(k, vs)| (k, FloorSets::build(&vs, cfg)))
        .collect();

    // Eviction pool: unparseable versions always in (never floored, lowest
    // priority), parseable versions in only when their line is unprotected.
    let mut pool: Vec<&RetentionCandidate> = candidates
        .iter()
        .filter(|c| match parse_semver(&c.version) {
            None => true,
            Some(v) => !floors
                .get(&(c.ecosystem.as_str(), c.name.as_str()))
                .is_some_and(|f| f.protects(&v)),
        })
        .collect();

    pool.sort_by(|a, b| {
        let a_parseable = parse_semver(&a.version).is_some();
        let b_parseable = parse_semver(&b.version).is_some();
        a_parseable
            .cmp(&b_parseable) // unparseable (false) sorts first
            .then_with(|| a.last_accessed.cmp(&b.last_accessed))
            .then_with(
                || match (parse_semver(&a.version), parse_semver(&b.version)) {
                    (Some(va), Some(vb)) => cmp_versions(&va, &vb),
                    _ => Ordering::Equal,
                },
            )
    });

    let mut remaining: u64 = candidates.iter().map(|c| c.size_bytes).sum();
    let mut evicted = Vec::new();
    for c in pool {
        if remaining <= cfg.budget_bytes {
            break;
        }
        evicted.push(c.key.clone());
        remaining = remaining.saturating_sub(c.size_bytes);
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(budget: u64) -> LanguageArtifactConfig {
        LanguageArtifactConfig {
            budget_bytes: budget,
            ..LanguageArtifactConfig::default()
        }
    }

    fn cfg_floor(budget: u64, majors: u32, minors: u32, patches: u32) -> LanguageArtifactConfig {
        LanguageArtifactConfig {
            budget_bytes: budget,
            floor_majors: majors,
            floor_minors: minors,
            floor_patches: patches,
            ..LanguageArtifactConfig::default()
        }
    }

    fn cand(
        eco: &str,
        name: &str,
        version: &str,
        size: u64,
        accessed: i64,
        tag: &str,
    ) -> RetentionCandidate {
        RetentionCandidate {
            key: format!("{eco}/{name}@{version}:{tag}"),
            ecosystem: eco.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            size_bytes: size,
            last_accessed: accessed,
        }
    }

    // --- path parsing ------------------------------------------------------

    #[test]
    fn npm_tarball_path_parses_name_and_version() {
        let p = parse_artifact_path("npm/artifact/left-pad/-/left-pad-1.3.0.tgz").unwrap();
        assert_eq!(p.ecosystem, "npm");
        assert_eq!(p.name, "left-pad");
        assert_eq!(p.version, "1.3.0");
    }

    #[test]
    fn npm_scoped_tarball_path_parses() {
        let p = parse_artifact_path("npm/artifact/@babel/core/-/core-7.24.1.tgz").unwrap();
        assert_eq!(p.name, "@babel/core");
        assert_eq!(p.version, "7.24.1");
    }

    #[test]
    fn npm_packument_is_not_a_candidate() {
        // Caller contract: metadata (packument) entries are filtered out by
        // cache_class before candidates are built; the parser agrees.
        assert_eq!(parse_artifact_path("npm/left-pad"), None);
    }

    #[test]
    fn pip_wheel_path_parses() {
        let p = parse_artifact_path("pip/files/packages/ab/cd/ef/requests-2.31.0-py3-none-any.whl")
            .unwrap();
        assert_eq!(p.ecosystem, "pip");
        assert_eq!(p.name, "requests");
        assert_eq!(p.version, "2.31.0");
    }

    #[test]
    fn pip_sdist_path_parses() {
        let p = parse_artifact_path("pip/files/packages/source/fl/flask-3.0.3.tar.gz").unwrap();
        assert_eq!(p.name, "flask");
        assert_eq!(p.version, "3.0.3");
    }

    #[test]
    fn pip_simple_index_is_not_a_candidate() {
        assert_eq!(parse_artifact_path("pip/simple/flask/"), None);
    }

    #[test]
    fn cargo_blob_path_parses() {
        let p = parse_artifact_path("cargo/blob/api/v1/crates/serde/1.0.203/download").unwrap();
        assert_eq!(p.ecosystem, "cargo");
        assert_eq!(p.name, "serde");
        assert_eq!(p.version, "1.0.203");
    }

    #[test]
    fn cargo_index_is_not_a_candidate() {
        assert_eq!(parse_artifact_path("cargo/index/ca/rg/cargo"), None);
    }

    #[test]
    fn brew_bottle_path_parses() {
        let p = parse_artifact_path("brew/bottles/jq/1.8.2/jq-1.8.2.arm64_tahoe.bottle.1.tar.gz")
            .unwrap();
        assert_eq!(p.ecosystem, "brew");
        assert_eq!(p.name, "jq");
        assert_eq!(p.version, "1.8.2");
    }

    #[test]
    fn brew_api_metadata_is_not_a_candidate() {
        assert_eq!(parse_artifact_path("brew/api/formula/jq.json"), None);
    }

    #[test]
    fn brew_manifest_is_not_a_candidate() {
        assert_eq!(
            parse_artifact_path("brew/v2/homebrew/core/jq/manifests/1.8.2-1"),
            None
        );
    }

    // --- version floors ----------------------------------------------------

    #[test]
    fn major_floor_boundary_oldest_major_evictable() {
        // 3 majors, floor 2 → 1.x evictable, 2.x/3.x protected.
        let cs = vec![
            cand("npm", "app", "1.0.0", 100, 100, "a"),
            cand("npm", "app", "2.0.0", 100, 90, "b"),
            cand("npm", "app", "3.0.0", 100, 80, "c"),
        ];
        let out = plan_eviction(&cs, &cfg(250));
        assert_eq!(out, vec!["npm/app@1.0.0:a".to_string()]);
    }

    #[test]
    fn zero_x_minor_floor_boundary() {
        let cs = vec![
            cand("npm", "lib", "0.1.0", 100, 100, "a"),
            cand("npm", "lib", "0.2.0", 100, 90, "b"),
            cand("npm", "lib", "0.3.0", 100, 80, "c"),
        ];
        // floor_minors=2 (default): 0.3.x and 0.2.x protected, 0.1.x evictable.
        let out = plan_eviction(&cs, &cfg(250));
        assert_eq!(out, vec!["npm/lib@0.1.0:a".to_string()]);
    }

    #[test]
    fn zero_zero_x_patch_floor_boundary() {
        let cs = vec![
            cand("cargo", "tiny", "0.0.1", 100, 100, "a"),
            cand("cargo", "tiny", "0.0.2", 100, 90, "b"),
            cand("cargo", "tiny", "0.0.3", 100, 80, "c"),
            cand("cargo", "tiny", "0.0.4", 100, 70, "d"),
        ];
        // floor_patches=3 (default): 0.0.1 evictable even though it is the
        // (only) major/minor line.
        let out = plan_eviction(&cs, &cfg(250));
        assert_eq!(out, vec!["cargo/tiny@0.0.1:a".to_string()]);
    }

    #[test]
    fn floors_hold_even_when_over_budget() {
        // Everything floored + over budget → nothing evicted.
        let cs = vec![
            cand("npm", "app", "2.0.0", 100, 100, "a"),
            cand("npm", "app", "3.0.0", 100, 90, "b"),
        ];
        assert!(plan_eviction(&cs, &cfg(50)).is_empty());
    }

    #[test]
    fn floors_are_per_ecosystem_and_name() {
        // Same version strings, different names: each name gets its own floor
        // budget. floor_majors=1 protects only the newest major per name, so
        // every 1.0.0 entry is evictable.
        let cs = vec![
            cand("npm", "a", "1.0.0", 100, 100, "a"),
            cand("npm", "b", "1.0.0", 100, 95, "b"),
            cand("npm", "a", "2.0.0", 100, 90, "c"),
            cand("npm", "b", "2.0.0", 100, 85, "d"),
            cand("pip", "a", "1.0.0", 100, 80, "e"),
            cand("pip", "a", "2.0.0", 100, 75, "f"),
        ];
        let out = plan_eviction(&cs, &cfg_floor(250, 1, 2, 3));
        // All three 1.0.0 entries evictable; budget 250 of 600 requires all.
        assert_eq!(out.len(), 3);
        assert!(out.contains(&"npm/a@1.0.0:a".to_string()));
        assert!(out.contains(&"npm/b@1.0.0:b".to_string()));
        assert!(out.contains(&"pip/a@1.0.0:e".to_string()));
    }

    // --- budget / LRU interaction ------------------------------------------

    #[test]
    fn under_budget_evicts_nothing() {
        let cs = vec![
            cand("npm", "app", "1.0.0", 100, 100, "a"),
            cand("npm", "app", "1.1.0", 100, 90, "b"),
        ];
        assert!(plan_eviction(&cs, &cfg(10_000)).is_empty());
    }

    #[test]
    fn zero_budget_disables_eviction() {
        let cs = vec![cand("npm", "app", "1.0.0", 100, 100, "a")];
        assert!(plan_eviction(&cs, &cfg(0)).is_empty());
    }

    #[test]
    fn over_budget_evicts_oldest_non_floored_until_within_budget() {
        let cs = vec![
            cand("npm", "app", "1.0.0", 100, 500, "v1"),
            cand("npm", "app", "1.1.0", 100, 400, "v11"),
            cand("npm", "app", "2.0.0", 100, 300, "v2"),
            cand("npm", "app", "3.0.0", 100, 200, "v3"),
        ];
        // Total 400, budget 250 → must shed >150 → oldest-access first among
        // non-floored: 1.1.0 (t=400) then 1.0.0 (t=500). Remaining 200 ≤ 250,
        // 2.x/3.x floored anyway.
        let out = plan_eviction(&cs, &cfg(250));
        assert_eq!(
            out,
            vec![
                "npm/app@1.1.0:v11".to_string(),
                "npm/app@1.0.0:v1".to_string()
            ]
        );
    }

    #[test]
    fn eviction_stops_exactly_at_budget() {
        let cs = vec![
            cand("npm", "app", "1.0.0", 100, 500, "a"),
            cand("npm", "app", "1.1.0", 100, 400, "b"),
            cand("npm", "app", "2.0.0", 100, 300, "c"),
            cand("npm", "app", "3.0.0", 100, 200, "d"),
        ];
        // Oldest-accessed non-floored is 1.1.0 (t=400); evicting it leaves
        // 300 == budget → stop.
        let out = plan_eviction(&cs, &cfg(300));
        assert_eq!(out, vec!["npm/app@1.1.0:b".to_string()]);
    }

    #[test]
    fn empty_input_yields_empty_plan() {
        assert!(plan_eviction(&[], &cfg(1)).is_empty());
    }

    // --- unparseable versions ---------------------------------------------

    #[test]
    fn unparseable_version_evicted_first_and_never_floored() {
        let cs = vec![
            // Fresh but garbage version: lowest priority, goes first.
            cand("npm", "app", "not-a-version", 10, 1_000, "g"),
            // Old parseable version outside the floor.
            cand("npm", "app", "1.0.0", 100, 100, "a"),
            cand("npm", "app", "2.0.0", 100, 90, "b"),
            cand("npm", "app", "3.0.0", 100, 80, "c"),
        ];
        let out = plan_eviction(&cs, &cfg(250));
        assert_eq!(out.first(), Some(&"npm/app@not-a-version:g".to_string()));
        assert!(out.contains(&"npm/app@1.0.0:a".to_string()));
        // The garbage entry never shields anything and is never protected:
        // with a huge budget even it survives.
        assert!(plan_eviction(&cs, &cfg(1_000)).is_empty());
    }

    #[test]
    fn empty_version_string_is_unparseable_never_floored() {
        let cs = vec![
            cand("pip", "thing", "", 100, 100, "a"),
            cand("pip", "thing", "1.0.0", 100, 90, "b"),
        ];
        // floor_majors=2 would protect 1.0.0; only the "" entry is evictable.
        let out = plan_eviction(&cs, &cfg(50));
        assert_eq!(out, vec!["pip/thing@:a".to_string()]);
    }

    // --- semver-family edge cases -------------------------------------------

    #[test]
    fn prerelease_of_floored_major_is_floored_with_it() {
        let cs = vec![
            cand("npm", "app", "4.0.0-rc1", 100, 100, "rc"),
            cand("npm", "app", "3.0.0", 100, 90, "b"),
            cand("npm", "app", "2.0.0", 100, 80, "c"),
        ];
        // floor_majors=2 protects majors {4, 3}; 2.0.0 evictable.
        let out = plan_eviction(&cs, &cfg(250));
        assert_eq!(out, vec!["npm/app@2.0.0:c".to_string()]);
    }

    #[test]
    fn vercmp_tiebreak_prefers_evicting_older_version_on_equal_access_time() {
        // Same last_accessed: 1.0.0~rc1 < 1.0.0 under vercmp, so the tilde
        // pre-release is evicted before the release despite equal recency.
        let cs = vec![
            cand("npm", "app", "1.0.0", 100, 100, "rel"),
            cand("npm", "app", "1.0.0-rc1", 100, 100, "pre"),
            cand("npm", "app", "2.0.0", 100, 90, "b"),
            cand("npm", "app", "3.0.0", 100, 80, "c"),
        ];
        let out = plan_eviction(&cs, &cfg(300));
        assert_eq!(out.first(), Some(&"npm/app@1.0.0-rc1:pre".to_string()));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn v_prefix_and_epoch_do_not_block_floor_protection() {
        let cs = vec![
            cand("cargo", "w", "v1.0.0", 100, 100, "a"),
            cand("cargo", "w", "2.0.0", 100, 90, "b"),
        ];
        // floor_majors=2: both majors protected → nothing evictable.
        assert!(plan_eviction(&cs, &cfg(50)).is_empty());
    }
}
