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

/// Normalize a raw version string into orderable components.
pub fn normalize(raw: &str) -> NormalizedVersion {
    let mut s = raw.trim();

    // Epoch: leading all-digit run followed by ':'.
    if let Some(colon) = s.find(':') {
        let head = &s[..colon];
        if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(epoch) = head.parse::<u64>() {
                s = &s[colon + 1..];
                return finish(epoch, s);
            }
        }
    }
    finish(0, s)
}

fn finish(epoch: u64, s: &str) -> NormalizedVersion {
    let mut s = s;

    // `v`/`V` prefix, only before a digit.
    if (s.starts_with('v') || s.starts_with('V'))
        && s[1..].bytes().next().is_some_and(|b| b.is_ascii_digit())
    {
        s = &s[1..];
    }

    // `+really`: keep everything after the LAST marker (the real version).
    if let Some(pos) = s.rfind("+really") {
        let tail = &s[pos + "+really".len()..];
        if !tail.is_empty() {
            s = tail;
        }
    }

    // Release: trailing `-rN` or `-N` (all digits after the dash).
    let mut release = 0u64;
    if let Some(dash) = s.rfind('-') {
        let tail = &s[dash + 1..];
        let digits = tail
            .strip_prefix('r')
            .unwrap_or(tail)
            .bytes()
            .all(|b| b.is_ascii_digit());
        if digits && !tail.is_empty() && tail.len() < s.len() {
            if let Ok(rel) = tail.strip_prefix('r').unwrap_or(tail).parse::<u64>() {
                // Keep at least one char of version body.
                if dash > 0 {
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

/// Total-order compare: epoch, then version body (dpkg chunk rules), then
/// release. Never panics on arbitrary input.
pub fn vercmp(a: &NormalizedVersion, b: &NormalizedVersion) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    a.epoch.cmp(&b.epoch).then_with(|| ver_cmp(&a.version, &b.version)).then_with(|| a.release.cmp(&b.release))
}

/// Sortable canonical key for the ledger's `version_ord` column.
/// Display/debug only — ordering decisions always go through [`vercmp`].
pub fn order_key(v: &NormalizedVersion) -> String {
    format!("{:016}:{}:{:06}", v.epoch, v.version, v.release)
}

/// dpkg-style chunk compare of two version bodies.
fn ver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);

    while i < a.len() || j < b.len() {
        // --- non-digit phase: compare char by char ---
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let av = if i < a.len() && !a[i].is_ascii_digit() {
                char_weight(a[i])
            } else {
                0
            };
            let bv = if j < b.len() && !b[j].is_ascii_digit() {
                char_weight(b[j])
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
        match compare_numeric_runs(&a_digits, &b_digits) {
            Equal => {}
            other => return other,
        }
    }
    Equal
}

/// dpkg weight: `~` sorts before end-of-string (-1), letters next, all other
/// bytes after (+256). End-of-string counts as 0.
fn char_weight(c: u8) -> i32 {
    if c == b'~' {
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
        assert_eq!(cmp("1.99999999999999999999", "1.100000000000000000000"), Less);
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
}
