//! Statistical freeze detection (Sprint 21 slice 21-5a).
//!
//! A "freeze" is when the oracle (upstream source resolution) no longer
//! moves ahead of the cache: if the oracle's max known version_ord for a
//! package is GREATER than the newest cached version_ord, that package has
//! content the cache never picked up. The [`FreezeDetector`] turns the
//! share of such packages into a boolean alert against a configurable
//! threshold. All comparisons go through
//! [`crate::sourcedb::cmp_version_ord`] — never raw byte compares.

use crate::sourcedb::cmp_version_ord;
use std::cmp::Ordering;

/// One observation of (oracle max ord, cache newest ord) for a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeSample {
    pub package_key: String,
    /// Max version_ord known to the oracle for this package.
    pub oracle_max_ord: String,
    /// Newest version_ord present in the cache for this package.
    pub cache_newest_ord: String,
}

/// Share (0.0–1.0) of samples where the oracle is strictly ahead of the
/// cache. Empty input → 0.0 (nothing observed, nothing frozen).
pub fn share_moved_since(samples: &[FreezeSample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let moved = samples
        .iter()
        .filter(|s| cmp_version_ord(&s.oracle_max_ord, &s.cache_newest_ord) == Ordering::Greater)
        .count();
    moved as f64 / samples.len() as f64
}

/// Statistical freeze detector: alerts when the moved share exceeds a
/// configured threshold.
#[derive(Debug, Clone)]
pub struct FreezeDetector {
    pub threshold: f64,
}

impl FreezeDetector {
    /// True when `share` is strictly greater than `threshold`.
    pub fn evaluate(&self, share: f64) -> bool {
        share > self.threshold
    }
}

/// Emitted when a [`FreezeDetector`] fires.
#[derive(Debug, Clone, PartialEq)]
pub struct FreezeAlert {
    pub share: f64,
    pub threshold: f64,
    pub evaluated_at: i64,
    pub sample_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use monopsony_common::version_ord::{normalize, order_key};

    fn ord(version: &str) -> String {
        order_key(&normalize(version))
    }

    fn sample(key: &str, oracle: &str, cache: &str) -> FreezeSample {
        FreezeSample {
            package_key: key.to_string(),
            oracle_max_ord: ord(oracle),
            cache_newest_ord: ord(cache),
        }
    }

    // (1) 0/4 moved → share 0.0, not frozen at threshold 0.5.
    #[test]
    fn test_zero_moved_not_frozen() {
        let samples = vec![
            sample("k1", "1.0", "1.0"),
            sample("k2", "2.0", "2.0"),
            sample("k3", "1.0-1", "1.0-1"),
            sample("k4", "1:0.9", "1:0.9"),
        ];
        let share = share_moved_since(&samples);
        assert_eq!(share, 0.0);
        let det = FreezeDetector { threshold: 0.5 };
        assert!(!det.evaluate(share));
    }

    // (2) 3/4 moved → share 0.75, frozen at threshold 0.5.
    #[test]
    fn test_three_of_four_frozen() {
        let samples = vec![
            sample("k1", "2.0", "1.0"),
            sample("k2", "3.0", "1.0"),
            sample("k3", "4.0", "1.0"),
            sample("k4", "1.0", "1.0"),
        ];
        let share = share_moved_since(&samples);
        assert!((share - 0.75).abs() < f64::EPSILON);
        let det = FreezeDetector { threshold: 0.5 };
        assert!(det.evaluate(share));
        let alert = FreezeAlert {
            share,
            threshold: det.threshold,
            evaluated_at: 1_000,
            sample_count: samples.len(),
        };
        assert_eq!(alert.sample_count, 4);
        assert_eq!(alert.threshold, 0.5);
    }

    // (3) Equal ords never count as moved (strictly-greater compare).
    #[test]
    fn test_equal_ords_never_move() {
        let samples = vec![
            sample("k1", "1.0", "1.0"),
            sample("k2", "1.0-1", "1.0-1"),
            sample("k3", "1:0.9", "1:0.9"),
        ];
        assert_eq!(share_moved_since(&samples), 0.0);
    }

    // (4) All moved → share 1.0.
    #[test]
    fn test_all_moved_is_one() {
        let samples = vec![sample("k1", "2.0", "1.0"), sample("k2", "3.0", "2.0")];
        assert_eq!(share_moved_since(&samples), 1.0);
    }

    // (5) Empty slice → 0.0.
    #[test]
    fn test_empty_is_zero() {
        assert_eq!(share_moved_since(&[]), 0.0);
    }

    // (6) Tilde pre-release: cache "1.0~rc1" is BELOW oracle "1.0" → moved.
    //     A raw byte compare would rank "~rc1" (0x7E) above "1.0" and miss
    //     this; it must go through cmp_version_ord's component compare.
    #[test]
    fn test_tilde_ord_moves() {
        let s = FreezeSample {
            package_key: "k1".to_string(),
            oracle_max_ord: ord("1.0"),
            cache_newest_ord: ord("1.0~rc1"),
        };
        // Sanity: the ord-key components must rank 1.0 above 1.0~rc1.
        assert_eq!(
            cmp_version_ord(&s.oracle_max_ord, &s.cache_newest_ord),
            Ordering::Greater
        );
        assert_eq!(share_moved_since(&[s]), 1.0);
    }

    // (7) Threshold boundary: share == threshold does NOT fire (strict >).
    #[test]
    fn test_threshold_boundary_strict() {
        let det = FreezeDetector { threshold: 0.5 };
        assert!(!det.evaluate(0.5));
        assert!(det.evaluate(0.51));
    }

    // (8) FreezeAlert carries the evaluated inputs unchanged.
    #[test]
    fn test_freeze_alert_fields() {
        let alert = FreezeAlert {
            share: 0.75,
            threshold: 0.5,
            evaluated_at: 1_760_000_000,
            sample_count: 8,
        };
        assert_eq!(
            (
                alert.share,
                alert.threshold,
                alert.evaluated_at,
                alert.sample_count
            ),
            (0.75, 0.5, 1_760_000_000, 8)
        );
    }
}
