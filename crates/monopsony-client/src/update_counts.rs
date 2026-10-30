//! Aggregated upgradable counts for the heartbeat UpdateReport
//! (Sprint 29 Phase 1, grounding D3/D4).
//!
//! The per-PM listings (`apt list --upgradable` and friends) are slow: the
//! daemon computes this at inventory cadence, caches it locally with a TTL,
//! and attaches it to heartbeats only while fresh — never per tick.

use std::time::{Duration, Instant};

/// Local TTL on computed counts. Counts change on `apt upgrade`, so an
/// hourly-ish staleness bound is plenty for an advisory surface.
pub const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);

/// Aggregate of every supported backend's upgradable listing.
#[derive(Debug, Clone, Default)]
pub struct AggregatedUpdateCounts {
    pub security: i32,
    pub other: i32,
    pub total: i32,
    /// Per-PM notes (how the security split was derived, unsupported PMs,
    /// listing failures). Counts-only telemetry never ships these — they
    /// are for the local log.
    pub notes: Vec<String>,
}

/// Run every available backend's upgradable listing and aggregate. Per-backend
/// failures degrade to a logged note, never to a failed report.
pub async fn compute() -> AggregatedUpdateCounts {
    let mut agg = AggregatedUpdateCounts::default();
    for backend in monopsony_common::package_manager::get_available_backends() {
        let kind = backend.kind();
        match backend.list_upgradable().await {
            Ok(report) if report.supported => {
                agg.security += report.security as i32;
                agg.other += report.other as i32;
                agg.notes
                    .push(format!("{}: {}", kind.as_str(), report.note));
            }
            Ok(report) => {
                agg.notes
                    .push(format!("{}: {}", kind.as_str(), report.note));
            }
            Err(e) => {
                agg.notes.push(format!(
                    "{}: upgradable listing failed: {}",
                    kind.as_str(),
                    e
                ));
            }
        }
    }
    agg.total = agg.security.saturating_add(agg.other);
    agg
}

/// True when a cached entry still carries fresh-enough data.
pub fn is_fresh(computed_at: Instant) -> bool {
    computed_at.elapsed() < CACHE_TTL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_hours_not_ticks() {
        // Grounding D3: never re-run slow listings per heartbeat tick (60s).
        assert!(CACHE_TTL > Duration::from_secs(300));
    }

    #[test]
    fn freshness_window_respected() {
        assert!(is_fresh(Instant::now()));
        assert!(!is_fresh(Instant::now() - CACHE_TTL));
    }
}
