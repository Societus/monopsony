//! Sprint 27 Part B: windowed update-failure telemetry (counts-only).
//!
//! The daemon records update-pipeline attempts and failures into a rolling
//! in-memory window; every heartbeat carries the windowed count plus the
//! last-attempt timestamp to the server (proto `HeartbeatRequest` fields
//! 3-5). No package identity is ever recorded or transmitted — v1 is
//! counts-only by policy (plan §4a/§6) until Sprint 24's `privacy_level`
//! tier lands.
//!
//! Failure semantics (v1): an *operational* failure is one where an update
//! pipeline action was attempted and errored (e.g. remote shadow-assignment
//! registration failed). Policy outcomes — trust refusals, client opt-out,
//! "package not detected" — are the system working as configured and are
//! deliberately NOT counted, so a healthy strict-policy fleet never alerts.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Default reporting window: 24 h of rolling history.
pub const DEFAULT_WINDOW_SECS: i64 = 86_400;

/// What a heartbeat reports to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateHealthSnapshot {
    /// Failures within `[window_start_ts, now]`.
    pub failed_updates_window: u32,
    /// Unix seconds of the most recent attempt; 0 = none recorded.
    pub last_update_attempt_ts: i64,
    /// Window start in unix seconds; sent as-is (server treats 0 as no data).
    pub window_start_ts: i64,
}

#[derive(Debug, Default)]
struct TrackerState {
    attempts: VecDeque<i64>,
    failures: VecDeque<i64>,
}

/// Shared, interior-mutable tracker. Cheap to clone the `Arc` around.
#[derive(Debug)]
pub struct UpdateHealthTracker {
    window_secs: i64,
    state: Mutex<TrackerState>,
}

impl UpdateHealthTracker {
    pub fn new(window_secs: i64) -> Self {
        Self {
            window_secs: window_secs.max(1),
            state: Mutex::new(TrackerState::default()),
        }
    }

    /// Record an update-pipeline attempt at `ts` (unix seconds).
    pub fn record_attempt(&self, ts: i64) {
        let mut st = self.state.lock().expect("update-health mutex poisoned");
        st.attempts.push_back(ts);
    }

    /// Record an operational failure at `ts`. A failure implies an attempt.
    pub fn record_failure(&self, ts: i64) {
        let mut st = self.state.lock().expect("update-health mutex poisoned");
        st.attempts.push_back(ts);
        st.failures.push_back(ts);
    }

    /// Prune entries older than the window and snapshot the current state
    /// relative to `now` (unix seconds).
    pub fn snapshot(&self, now: i64) -> UpdateHealthSnapshot {
        let cutoff = now.saturating_sub(self.window_secs);
        let mut st = self.state.lock().expect("update-health mutex poisoned");
        while st.attempts.front().is_some_and(|&ts| ts <= cutoff) {
            st.attempts.pop_front();
        }
        while st.failures.front().is_some_and(|&ts| ts <= cutoff) {
            st.failures.pop_front();
        }
        UpdateHealthSnapshot {
            failed_updates_window: st.failures.len().min(u32::MAX as usize) as u32,
            last_update_attempt_ts: st.attempts.back().copied().unwrap_or(0),
            window_start_ts: now.saturating_sub(self.window_secs).max(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_reports_zero_and_no_attempt() {
        let t = UpdateHealthTracker::new(DEFAULT_WINDOW_SECS);
        let s = t.snapshot(1_000_000);
        assert_eq!(s.failed_updates_window, 0);
        assert_eq!(s.last_update_attempt_ts, 0);
        assert_eq!(s.window_start_ts, 1_000_000 - DEFAULT_WINDOW_SECS);
    }

    #[test]
    fn failure_implies_attempt() {
        let t = UpdateHealthTracker::new(DEFAULT_WINDOW_SECS);
        t.record_failure(500);
        let s = t.snapshot(600);
        assert_eq!(s.failed_updates_window, 1);
        assert_eq!(s.last_update_attempt_ts, 500);
    }

    #[test]
    fn attempts_without_failures_count_only_attempts() {
        let t = UpdateHealthTracker::new(DEFAULT_WINDOW_SECS);
        t.record_attempt(100);
        t.record_attempt(200);
        t.record_failure(300);
        let s = t.snapshot(400);
        assert_eq!(s.failed_updates_window, 1);
        assert_eq!(s.last_update_attempt_ts, 300);
    }

    #[test]
    fn window_prunes_old_entries() {
        let t = UpdateHealthTracker::new(100);
        t.record_failure(1_000);
        t.record_failure(1_050);
        t.record_failure(1_200);
        // Cutoff at now-100 = 1_100: the 1_000 and 1_050 failures prune
        // (boundary ts <= cutoff is stale), 1_200 survives.
        let s = t.snapshot(1_200);
        assert_eq!(s.failed_updates_window, 1);
        assert_eq!(s.last_update_attempt_ts, 1_200);
        assert_eq!(s.window_start_ts, 1_100);
    }

    #[test]
    fn zero_window_is_clamped_not_divide_by_zero() {
        let t = UpdateHealthTracker::new(0);
        t.record_failure(10);
        let s = t.snapshot(10);
        assert_eq!(s.failed_updates_window, 1);
    }
}
