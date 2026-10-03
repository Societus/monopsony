//! Version normalization + total-order comparison (Sprint 20 Phase 4).
//!
//! Pure functions, no I/O. Reference semantics: alpm-version vercmp (epoch /
//! version / release) and dpkg's version-comparison rules for the version
//! body (alternating non-digit/digit chunks, `~` sorts before everything
//! including end-of-string). Implemented in-tree, not imported.
//!
//! Normalization choices (documented per plan §6 "normalizer v1"):
//! - Epoch: leading `N:` (all-digit prefix) becomes the numeric epoch.
//! - `v`/`V` prefix stripped only when followed by a digit.
//! - `+really` marker (pacman fake-version convention, e.g.
//!   `4.14+really4.15`): the REAL version is everything after the LAST
//!   `+really`; the fake prefix is dropped.
//! - Release: a trailing `-rN` (pacman) or all-digit `-N` suffix becomes the
//!   release number. Anything else (e.g. Debian's `-1ubuntu0.1`) stays inside
//!   the version string and rides the chunk compare — those revisions still
//!   order correctly because dpkg's algorithm is applied to the whole string.
//! - Unparseable input never panics: garbage bytes compare as their byte runs.

/// A version split into its orderable components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedVersion {
    /// Numeric epoch (leading `N:`), 0 when absent.
    pub epoch: u64,
    /// Version body with epoch/release/`v`-prefix/`+really` handled.
    pub version: String,
    /// Trailing `-rN` / `-N` release number, 0 when absent.
    pub release: u64,
}

/// Data-driven version scheme table (Sprint 21 slice 21-2).
///
/// Every field defaults to the hardcoded behavior of the original
/// `normalize`/`vercmp`; deployments tune the scheme via TOML
/// ([`SchemeConfig::from_toml_str`] / [`SchemeConfig::load_file`]) without
/// recompiling. Absent TOML fields keep their defaults (delta-patchable).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SchemeConfig {
    /// Strip a leading `v`/`V` when followed by a digit.
    #[serde(default = "default_true")]
    pub strip_v_prefix: bool,
    /// `+really` marker (pacman fake-version convention); `None` disables the
    /// marker handling entirely.
    #[serde(default = "default_really_marker")]
    pub really_marker: Option<String>,
    /// Epoch separator char (`:` in `1:0.9`); `None` disables epoch handling.
    #[serde(default = "default_epoch_separator")]
    pub epoch_separator: Option<char>,
    /// Letters accepted between `-` and the digits of the trailing release
    /// chunk (e.g. `r` in `-r7`). Empty disables the prefixed form; the bare
    /// all-digit `-N` form always parses.
    #[serde(default = "default_release_prefixes")]
    pub release_prefixes: Vec<String>,
    /// Tilde sorts before end-of-string (dpkg rule). `false` treats `~` as an
    /// ordinary byte.
    #[serde(default = "default_true")]
    pub strip_tilde_is_lesser: bool,
}

fn default_true() -> bool {
    true
}

fn default_really_marker() -> Option<String> {
    Some("+really".to_string())
}

fn default_epoch_separator() -> Option<char> {
    Some(':')
}

fn default_release_prefixes() -> Vec<String> {
    vec!["r".to_string()]
}

// Manual impl: derived Default would bypass the serde field defaults and has
// drifted from them before (project convention).
impl Default for SchemeConfig {
    fn default() -> Self {
        Self {
            strip_v_prefix: true,
            really_marker: Some("+really".to_string()),
            epoch_separator: Some(':'),
            release_prefixes: vec!["r".to_string()],
            strip_tilde_is_lesser: true,
        }
    }
}

impl SchemeConfig {
    /// Parse a scheme table from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<SchemeConfig, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load a scheme table from a TOML file. Absent fields keep their
    /// defaults, so partial files act as delta patches.
    pub fn load_file(path: &std::path::Path) -> std::io::Result<SchemeConfig> {
        let s = std::fs::read_to_string(path)?;
        SchemeConfig::from_toml_str(&s)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Normalize a raw version string into orderable components.
pub fn normalize(raw: &str) -> NormalizedVersion {
    normalize_with(raw, &SchemeConfig::default())
}

/// [`normalize`] with a caller-supplied scheme table.
pub fn normalize_with(raw: &str, cfg: &SchemeConfig) -> NormalizedVersion {
    let mut s = raw.trim();

    // Epoch: leading all-digit run followed by the configured separator.
    if let Some(sep) = cfg.epoch_separator {
        if let Some(sep_pos) = s.find(sep) {
            let head = &s[..sep_pos];
            if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(epoch) = head.parse::<u64>() {
                    s = &s[sep_pos + sep.len_utf8()..];
                    return finish(epoch, s, cfg);
                }
            }
        }
    }
    finish(0, s, cfg)
}

fn finish(epoch: u64, s: &str, cfg: &SchemeConfig) -> NormalizedVersion {
    let mut s = s;

    // `v`/`V` prefix, only before a digit.
    if cfg.strip_v_prefix
        && (s.starts_with('v') || s.starts_with('V'))
        && s[1..].bytes().next().is_some_and(|b| b.is_ascii_digit())
    {
        s = &s[1..];
    }

    // `+really`: keep everything after the LAST marker (the real version).
    if let Some(marker) = cfg.really_marker.as_deref() {
        if let Some(pos) = s.rfind(marker) {
            let tail = &s[pos + marker.len()..];
            if !tail.is_empty() {
                s = tail;
            }
        }
    }

    // Release: trailing `-N` or `-<prefix>N` per the scheme table.
    let mut release = 0u64;
    if let Some(dash) = s.rfind('-') {
        if dash > 0 {
            let tail = &s[dash + 1..];
            if let Some(digits) = release_digits(tail, &cfg.release_prefixes) {
                if let Ok(rel) = digits.parse::<u64>() {
                    release = rel;
                    s = &s[..dash];
                }
            }
        }
    }

    NormalizedVersion {
        epoch,
        version: s.to_string(),
        release,
    }
}

/// Split a trailing release chunk into its digit run: either the chunk is all
/// digits (`-7`) or it is `<prefix><digits>` for one of the configured
/// prefixes (`-r7`). Returns `None` when the chunk is not a release.
fn release_digits<'x>(tail: &'x str, prefixes: &[String]) -> Option<&'x str> {
    if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
        return Some(tail);
    }
    for p in prefixes {
        if let Some(rest) = tail.strip_prefix(p.as_str()) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return Some(rest);
            }
        }
    }
    None
}

/// Total-order compare: epoch, then version body (dpkg chunk rules), then
/// release. Never panics on arbitrary input.
pub fn vercmp(a: &NormalizedVersion, b: &NormalizedVersion) -> std::cmp::Ordering {
    vercmp_with(a, b, &SchemeConfig::default())
}

/// [`vercmp`] with a caller-supplied scheme table (tilde handling).
pub fn vercmp_with(
    a: &NormalizedVersion,
    b: &NormalizedVersion,
    cfg: &SchemeConfig,
) -> std::cmp::Ordering {
    a.epoch
        .cmp(&b.epoch)
        .then_with(|| ver_cmp(&a.version, &b.version, cfg))
        .then_with(|| a.release.cmp(&b.release))
}

/// Sortable canonical key for the ledger's `version_ord` column.
/// Display/debug only — ordering decisions always go through [`vercmp`].
pub fn order_key(v: &NormalizedVersion) -> String {
    format!("{:016}:{}:{:06}", v.epoch, v.version, v.release)
}

/// dpkg-style chunk compare of two version bodies.
fn ver_cmp(a: &str, b: &str, cfg: &SchemeConfig) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);

    while i < a.len() || j < b.len() {
        // --- non-digit phase: compare char by char ---
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let av = if i < a.len() && !a[i].is_ascii_digit() {
                char_weight(a[i], cfg)
            } else {
                0
            };
            let bv = if j < b.len() && !b[j].is_ascii_digit() {
                char_weight(b[j], cfg)
            } else {
                0
            };
            if av != bv {
                return av.cmp(&bv);
            }
            if av == 0 {
                break; // both sides are at a digit or exhausted
            }
            i += 1;
            j += 1;
        }
        // --- digit phase: compare numeric runs (overflow-proof) ---
        let a_digits = digit_run(a, &mut i);
        let b_digits = digit_run(b, &mut j);
        match compare_numeric_runs(a_digits, b_digits) {
            Equal => {}
            other => return other,
        }
    }
    Equal
}

/// dpkg weight: `~` sorts before end-of-string (-1), letters next, all other
/// bytes after (+256). End-of-string counts as 0. With
/// `strip_tilde_is_lesser = false`, tilde loses its special weight and is an
/// ordinary non-letter byte.
fn char_weight(c: u8, cfg: &SchemeConfig) -> i32 {
    if c == b'~' && cfg.strip_tilde_is_lesser {
        -1
    } else if c.is_ascii_alphabetic() {
        c as i32
    } else {
        c as i32 + 256
    }
}

/// Consume the leading digit run at `i`, returning it as a slice.
fn digit_run<'x>(bytes: &'x [u8], i: &mut usize) -> &'x [u8] {
    let start = *i;
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
        *i += 1;
    }
    &bytes[start..*i]
}

/// Compare two decimal runs without parsing (no overflow): strip leading
/// zeros, longer run wins, then lexicographic.
fn compare_numeric_runs(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let a = strip_zeros(a);
    let b = strip_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn strip_zeros(mut s: &[u8]) -> &[u8] {
    while s.len() > 1 && s[0] == b'0' {
        s = &s[1..];
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn n(raw: &str) -> NormalizedVersion {
        normalize(raw)
    }

    fn cmp(a: &str, b: &str) -> std::cmp::Ordering {
        vercmp(&n(a), &n(b))
    }

    #[test]
    fn epoch_dominates_version() {
        assert_eq!(cmp("1:0.9", "2.0"), Greater);
        assert_eq!(n("1:0.9").epoch, 1);
        assert_eq!(n("2.0").epoch, 0);
    }

    #[test]
    fn tilde_sorts_before_release() {
        assert_eq!(cmp("1.0~rc1", "1.0"), Less);
        assert_eq!(cmp("1.0~~", "1.0~rc1"), Less);
    }

    #[test]
    fn pacman_release_ordering() {
        assert_eq!(cmp("5.2.37-1", "5.2.38-1"), Less);
        assert_eq!(n("5.2.38-1").release, 1);
        assert_eq!(n("1.0-r7").release, 7);
        assert_eq!(n("1.0-r7").version, "1.0");
    }

    #[test]
    fn v_prefix_stripped_only_before_digit() {
        assert_eq!(cmp("v2.3.0", "2.3.0"), Equal);
        assert_eq!(n("version1").version, "version1"); // not a v-prefix
    }

    #[test]
    fn really_marker_keeps_real_version() {
        assert_eq!(n("1.2+really1.3").version, "1.3");
        assert_eq!(cmp("1.2+really1.3", "1.3"), Equal);
        // A plain '+' with no marker must not be mangled.
        assert_eq!(n("1.2+build3").version, "1.2+build3");
    }

    #[test]
    fn numeric_chunks_not_lexicographic() {
        assert_eq!(cmp("1.10", "1.9"), Greater);
        assert_eq!(cmp("1.001", "1.1"), Equal); // numeric equality
    }

    #[test]
    fn alpha_chunks_byte_wise() {
        assert_eq!(cmp("1.0a", "1.0"), Greater);
        assert_eq!(cmp("1.0a", "1.0b"), Less);
        assert_eq!(cmp("2.3.0", "2.3.0-1"), Less); // no release < release 1
    }

    #[test]
    fn debian_style_revision_rides_version_string() {
        // Non-numeric trailing -rev stays in the body and still orders.
        assert_eq!(cmp("1.2.3-1ubuntu0.1", "1.2.3-1"), Greater);
        assert_eq!(n("1.2.3-1ubuntu0.1").release, 0);
    }

    #[test]
    fn garbage_never_panics() {
        let _ = n("");
        let _ = n("!!!");
        let _ = n(":-");
        let _ = n("999999999999999999999999999");
        assert_eq!(n("").version, "");
        assert_eq!(n("!!!").version, "!!!");
    }

    #[test]
    fn huge_numeric_runs_do_not_overflow() {
        assert_eq!(
            cmp("1.99999999999999999999", "1.100000000000000000000"),
            Less
        );
    }

    #[test]
    fn order_key_format() {
        let k = order_key(&n("1:2.3-4"));
        assert_eq!(k, "0000000000000001:2.3:000004");
        let k0 = order_key(&n("2.3"));
        assert_eq!(k0, "0000000000000000:2.3:000000");
    }

    #[test]
    fn epoch_version_release_full_order() {
        let mut versions = vec!["1.0~rc1", "1.0", "1.0-1", "1.0-2", "1.1", "2:0.1"];
        versions.sort_by(|a, b| cmp(a, b));
        assert_eq!(
            versions,
            vec!["1.0~rc1", "1.0", "1.0-1", "1.0-2", "1.1", "2:0.1"]
        );
    }

    // --- Sprint 21 slice 21-2: data-driven scheme table ---

    fn n_with(raw: &str, cfg: &SchemeConfig) -> NormalizedVersion {
        normalize_with(raw, cfg)
    }

    fn cmp_with(a: &str, b: &str, cfg: &SchemeConfig) -> std::cmp::Ordering {
        vercmp_with(&n_with(a, cfg), &n_with(b, cfg), cfg)
    }

    #[test]
    fn defaults_match_normalize() {
        let inputs = [
            "1:0.9",
            "2.0",
            "1.0~rc1",
            "1.0~~",
            "5.2.38-1",
            "1.0-r7",
            "v2.3.0",
            "version1",
            "1.2+really1.3",
            "1.2+build3",
            "1.2.3-1ubuntu0.1",
            "!!!",
            "",
        ];
        for raw in inputs {
            assert_eq!(n_with(raw, &SchemeConfig::default()), n(raw), "raw={raw}");
        }
    }

    #[test]
    fn strip_v_prefix_false_makes_v_an_ordinary_byte() {
        let cfg = SchemeConfig {
            strip_v_prefix: false,
            ..SchemeConfig::default()
        };
        // 'v' keeps its letter weight (118) vs end-of-string 0 → Greater.
        assert_eq!(cmp_with("v2.3.0", "2.3.0", &cfg), Greater);
        assert_eq!(n_with("v2.3.0", &cfg).version, "v2.3.0");
        // Default still strips.
        assert_eq!(n_with("v2.3.0", &SchemeConfig::default()).version, "2.3.0");
    }

    #[test]
    fn really_marker_none_disables_marker_handling() {
        let cfg = SchemeConfig {
            really_marker: None,
            ..SchemeConfig::default()
        };
        assert_eq!(n_with("1.2+really1.3", &cfg).version, "1.2+really1.3");
        assert_eq!(
            n_with("1.2+really1.3", &SchemeConfig::default()).version,
            "1.3"
        );
    }

    #[test]
    fn epoch_separator_none_keeps_colon_in_body() {
        let cfg = SchemeConfig {
            epoch_separator: None,
            ..SchemeConfig::default()
        };
        let nv = n_with("1:0.9", &cfg);
        assert_eq!(nv.epoch, 0);
        assert_eq!(nv.version, "1:0.9");
        assert_eq!(n_with("1:0.9", &SchemeConfig::default()).epoch, 1);
    }

    #[test]
    fn release_prefixes_rel_accepts_rel_prefix() {
        let cfg = SchemeConfig {
            release_prefixes: vec!["rel".to_string()],
            ..SchemeConfig::default()
        };
        assert_eq!(n_with("1.0-rel7", &cfg).release, 7);
        assert_eq!(n_with("1.0-rel7", &cfg).version, "1.0");
        // With the default ["r"], "rel7" is not `r` + digits → no release.
        assert_eq!(n_with("1.0-rel7", &SchemeConfig::default()).release, 0);
        assert_eq!(
            n_with("1.0-rel7", &SchemeConfig::default()).version,
            "1.0-rel7"
        );
    }

    #[test]
    fn release_prefixes_empty_disables_prefixed_form() {
        let cfg = SchemeConfig {
            release_prefixes: Vec::new(),
            ..SchemeConfig::default()
        };
        let nv = n_with("1.0-r7", &cfg);
        assert_eq!(nv.release, 0);
        assert_eq!(nv.version, "1.0-r7");
        // Bare all-digit releases still parse even with no prefixes.
        assert_eq!(n_with("1.0-7", &cfg).release, 7);
        assert_eq!(n_with("1.0-7", &cfg).version, "1.0");
    }

    #[test]
    fn strip_tilde_is_lesser_false_makes_tilde_ordinary() {
        let cfg = SchemeConfig {
            strip_tilde_is_lesser: false,
            ..SchemeConfig::default()
        };
        // '~' byte weight 126+256 beats end-of-string 0 → Greater.
        assert_eq!(cmp_with("1.0~rc1", "1.0", &cfg), Greater);
        // Default keeps the dpkg rule.
        assert_eq!(cmp_with("1.0~rc1", "1.0", &SchemeConfig::default()), Less);
    }

    #[test]
    fn from_toml_str_golden_and_rejects_bad_toml() {
        let cfg =
            SchemeConfig::from_toml_str("strip_v_prefix = false\nrelease_prefixes = [\"rel\"]\n")
                .expect("golden toml must parse");
        assert!(!cfg.strip_v_prefix);
        assert_eq!(cfg.release_prefixes, vec!["rel".to_string()]);
        // Untouched fields keep their defaults (delta patching).
        assert_eq!(cfg.really_marker, SchemeConfig::default().really_marker);
        assert_eq!(cfg.epoch_separator, SchemeConfig::default().epoch_separator);
        assert!(cfg.strip_tilde_is_lesser);
        assert!(SchemeConfig::from_toml_str("this is = not [ valid toml").is_err());
    }

    #[test]
    fn toml_roundtrip_flips_v_prefix_outcome() {
        let cfg = SchemeConfig::from_toml_str("strip_v_prefix = false\n").unwrap();
        assert_eq!(cmp_with("v2.3.0", "2.3.0", &cfg), Greater);
        assert_eq!(cmp_with("v2.3.0", "2.3.0", &SchemeConfig::default()), Equal);
    }

    #[test]
    fn load_file_reads_partial_delta_patch() {
        let mut path = std::env::temp_dir();
        path.push(format!("monopsony_scheme_test_{}.toml", std::process::id()));
        std::fs::write(&path, "epoch_separator = \"!\"\n").unwrap();
        let cfg = SchemeConfig::load_file(&path).expect("delta patch loads");
        std::fs::remove_file(&path).ok();
        let nv = n_with("1:0.9", &cfg);
        assert_eq!(nv.epoch, 0);
        assert_eq!(nv.version, "1:0.9");
        // Everything else defaulted.
        assert!(cfg.strip_v_prefix);
        assert!(cfg.strip_tilde_is_lesser);
        // Missing file is an io::Error, not a panic.
        assert!(SchemeConfig::load_file(std::path::Path::new("/nonexistent/scheme.toml")).is_err());
    }
}
