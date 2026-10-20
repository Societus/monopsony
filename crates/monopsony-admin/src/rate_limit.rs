//! Rate limiting and login lockout for the admin API (pairing-v2 Phase 0).
//!
//! Two guards:
//! - [`AdminRateLimiter`]: token-bucket per-IP limiting plus a global bucket
//!   (pattern reused from `monopsony-mirror/public_rate_limit.rs`). The global
//!   bucket matters because `X-Forwarded-For` is not yet trusted-proxy-gated
//!   (Phase 2), so per-IP keys are spoofable by header rotation behind a
//!   reverse proxy; the global cap bounds total abuse regardless.
//! - [`LoginGuard`]: N-fails / M-minutes lockout per (username, IP) for the
//!   dashboard login endpoint (red-team F7).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::AdminState;

/// Idle buckets older than this are evicted once the map grows past cap.
const IDLE_EVICT_AFTER: Duration = Duration::from_secs(3600);
/// Lazy-eviction threshold for tracked per-IP state.
const MAX_TRACKED: usize = 10_000;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl Bucket {
    fn new(max_tokens: f64, refill_rate: f64) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
    }
}

/// Token-bucket rate limiter: per-IP and global.
pub struct AdminRateLimiter {
    per_ip: Mutex<HashMap<IpAddr, Bucket>>,
    global: Mutex<Bucket>,
    per_ip_rps: f64,
    per_ip_burst: f64,
}

impl AdminRateLimiter {
    /// `per_ip_rps` / `global_rps` are sustained refill rates (tokens/sec);
    /// bursts are the bucket capacities.
    pub fn new(per_ip_rps: f64, per_ip_burst: f64, global_rps: f64, global_burst: f64) -> Self {
        Self {
            per_ip: Mutex::new(HashMap::new()),
            global: Mutex::new(Bucket::new(global_burst, global_rps)),
            per_ip_rps,
            per_ip_burst,
        }
    }

    /// Consume one token for `ip`. `Err(retry_after_secs)` when limited.
    pub fn check(&self, ip: IpAddr) -> Result<(), u64> {
        // Global bucket first: bounds spoofed-XFF rotation abuse.
        {
            let mut global = self.global.lock().unwrap();
            global.refill();
            if global.tokens < 1.0 {
                let retry = ((1.0 - global.tokens) / global.refill_rate).ceil() as u64;
                return Err(retry.max(1));
            }
            global.tokens -= 1.0;
        }

        let mut per_ip = self.per_ip.lock().unwrap();

        // Lazy eviction: bounded memory without a background task.
        if per_ip.len() > MAX_TRACKED {
            let now = Instant::now();
            per_ip.retain(|_, b| now.duration_since(b.last_refill) < IDLE_EVICT_AFTER);
        }

        let bucket = per_ip
            .entry(ip)
            .or_insert_with(|| Bucket::new(self.per_ip_burst, self.per_ip_rps));
        bucket.refill();
        if bucket.tokens < 1.0 {
            let retry = ((1.0 - bucket.tokens) / bucket.refill_rate).ceil() as u64;
            return Err(retry.max(1));
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
}

/// Login attempt lockout: `max_attempts` failures within `window` lock the
/// (username, IP) pair for `lockout`. Red-team F7 / pairing-v2 Phase 0.
pub struct LoginGuard {
    attempts: Mutex<HashMap<(String, IpAddr), FailState>>,
    max_attempts: u32,
    lockout: Duration,
    window: Duration,
}

#[derive(Debug)]
struct FailState {
    fails: u32,
    first_fail: Instant,
    locked_until: Option<Instant>,
}

impl LoginGuard {
    pub fn new(max_attempts: u32, lockout_minutes: u64) -> Self {
        Self::with_durations(
            max_attempts,
            Duration::from_secs(lockout_minutes * 60),
            Duration::from_secs(lockout_minutes * 60),
        )
    }

    /// Test-friendly constructor with explicit durations.
    pub fn with_durations(max_attempts: u32, lockout: Duration, window: Duration) -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
            max_attempts: max_attempts.max(1),
            lockout,
            window,
        }
    }

    /// `Some(remaining)` when this (username, IP) is currently locked out.
    pub fn is_locked(&self, username: &str, ip: IpAddr) -> Option<Duration> {
        let attempts = self.attempts.lock().unwrap();
        match attempts.get(&(username.to_string(), ip)) {
            Some(state) => match state.locked_until {
                Some(until) => {
                    let now = Instant::now();
                    if now < until {
                        Some(until - now)
                    } else {
                        None
                    }
                }
                None => None,
            },
            None => None,
        }
    }

    /// Record a failed login. Locks once failures within the window reach
    /// `max_attempts`.
    pub fn record_failure(&self, username: &str, ip: IpAddr) {
        let mut attempts = self.attempts.lock().unwrap();

        if attempts.len() > MAX_TRACKED {
            let now = Instant::now();
            attempts.retain(|_, s| {
                s.locked_until.map(|u| now < u).unwrap_or(false)
                    || now.duration_since(s.first_fail) < self.window
            });
        }

        let key = (username.to_string(), ip);
        let now = Instant::now();
        let state = attempts.entry(key).or_insert(FailState {
            fails: 0,
            first_fail: now,
            locked_until: None,
        });

        // Window elapsed since first tracked failure → start a fresh count.
        if now.duration_since(state.first_fail) >= self.window && state.locked_until.is_none() {
            state.fails = 0;
            state.first_fail = now;
        }

        state.fails += 1;
        if state.fails >= self.max_attempts {
            state.locked_until = Some(now + self.lockout);
        }
    }

    /// Record a successful login: clear the failure state for this pair.
    pub fn record_success(&self, username: &str, ip: IpAddr) {
        self.attempts
            .lock()
            .unwrap()
            .remove(&(username.to_string(), ip));
    }
}

/// Combined Phase 0 guards, carried in [`crate::AdminState`].
pub struct AdminRateLimits {
    pub http: AdminRateLimiter,
    pub login: LoginGuard,
}

/// Per-IP + global token-bucket gate for the whole admin router.
/// Returns 429 with a `Retry-After` header when limited.
pub async fn rate_limit_middleware(
    State(state): State<AdminState>,
    req: Request,
    next: Next,
) -> Response {
    let ip = crate::auth::extract_client_ip(&req, &state.trusted_proxies);
    if let Err(retry_after) = state.rate_limits.http.check(ip) {
        let body = format!(
            r#"{{"error":"rate_limited","message":"Too many requests","retry_after_secs":{}}}"#,
            retry_after
        );
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", retry_after.to_string())
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
    }
    next.run(req).await
}

impl AdminRateLimits {
    /// Build from `[dashboard]` config values (already defaulted).
    pub fn from_config(
        rate_limit_rps: f64,
        rate_limit_burst: u64,
        login_max_attempts: u32,
        login_lockout_minutes: u64,
    ) -> Self {
        // Global bucket is an order of magnitude above per-IP so a single
        // heavy client cannot starve the whole admin API.
        let global_rps = rate_limit_rps * 10.0;
        let global_burst = rate_limit_burst * 10;
        Self {
            http: AdminRateLimiter::new(
                rate_limit_rps,
                rate_limit_burst as f64,
                global_rps,
                global_burst as f64,
            ),
            login: LoginGuard::new(login_max_attempts, login_lockout_minutes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, 0, 1))
    }

    // ─── AdminRateLimiter ────────────────────────────────────────────

    #[test]
    fn limiter_allows_under_burst() {
        let l = AdminRateLimiter::new(100.0, 5.0, 1000.0, 50.0);
        for _ in 0..5 {
            assert!(l.check(ip(1, 1)).is_ok());
        }
    }

    #[test]
    fn limiter_blocks_over_burst() {
        let l = AdminRateLimiter::new(0.001, 3.0, 1000.0, 50.0); // ~no refill
        for _ in 0..3 {
            assert!(l.check(ip(2, 1)).is_ok());
        }
        assert!(l.check(ip(2, 1)).is_err());
    }

    #[test]
    fn limiter_per_ip_independent() {
        let l = AdminRateLimiter::new(0.001, 2.0, 1000.0, 50.0);
        assert!(l.check(ip(3, 1)).is_ok());
        assert!(l.check(ip(3, 1)).is_ok());
        assert!(l.check(ip(3, 1)).is_err());
        assert!(l.check(ip(3, 2)).is_ok());
    }

    #[test]
    fn limiter_global_cap_enforced_across_ips() {
        // Global burst 4; rotate IPs to bypass per-IP buckets.
        let l = AdminRateLimiter::new(100.0, 10.0, 0.001, 4.0);
        for i in 0..4u8 {
            assert!(l.check(ip(4, i)).is_ok());
        }
        // Global bucket drained: even a fresh IP is rejected.
        assert!(l.check(ip(5, 99)).is_err());
    }

    #[test]
    fn limiter_tokens_refill_over_time() {
        // 50 tokens/sec: after draining, 100ms restores ~5 tokens.
        let l = AdminRateLimiter::new(50.0, 2.0, 1000.0, 50.0);
        assert!(l.check(ip(6, 1)).is_ok());
        assert!(l.check(ip(6, 1)).is_ok());
        assert!(l.check(ip(6, 1)).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert!(l.check(ip(6, 1)).is_ok());
    }

    #[test]
    fn limiter_retry_after_is_positive() {
        let l = AdminRateLimiter::new(0.001, 1.0, 1000.0, 50.0);
        assert!(l.check(ip(7, 1)).is_ok());
        match l.check(ip(7, 1)) {
            Err(retry) => assert!(retry >= 1),
            Ok(()) => panic!("expected rate limit"),
        }
    }

    // ─── LoginGuard ──────────────────────────────────────────────────

    #[test]
    fn lockout_after_max_failures() {
        let g = LoginGuard::with_durations(3, Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..2 {
            g.record_failure("admin", ip(10, 1));
            assert!(g.is_locked("admin", ip(10, 1)).is_none());
        }
        g.record_failure("admin", ip(10, 1)); // 3rd failure
        assert!(g.is_locked("admin", ip(10, 1)).is_some());
    }

    #[test]
    fn lockout_is_per_username_and_ip() {
        let g = LoginGuard::with_durations(2, Duration::from_secs(60), Duration::from_secs(60));
        g.record_failure("admin", ip(11, 1));
        g.record_failure("admin", ip(11, 1));
        assert!(g.is_locked("admin", ip(11, 1)).is_some());
        // Different IP: unaffected.
        assert!(g.is_locked("admin", ip(11, 2)).is_none());
        // Different username, same IP: unaffected.
        assert!(g.is_locked("other", ip(11, 1)).is_none());
    }

    #[test]
    fn lockout_success_resets_count() {
        let g = LoginGuard::with_durations(3, Duration::from_secs(60), Duration::from_secs(60));
        g.record_failure("admin", ip(12, 1));
        g.record_failure("admin", ip(12, 1));
        g.record_success("admin", ip(12, 1));
        // Count restarted: two more failures alone do not lock.
        g.record_failure("admin", ip(12, 1));
        g.record_failure("admin", ip(12, 1));
        assert!(g.is_locked("admin", ip(12, 1)).is_none());
        g.record_failure("admin", ip(12, 1));
        assert!(g.is_locked("admin", ip(12, 1)).is_some());
    }

    #[test]
    fn lockout_window_expires_old_failures() {
        // 1ms window: failures decay immediately.
        let g = LoginGuard::with_durations(2, Duration::from_secs(60), Duration::from_millis(1));
        g.record_failure("admin", ip(13, 1));
        std::thread::sleep(Duration::from_millis(5));
        g.record_failure("admin", ip(13, 1)); // outside window → fresh count
        assert!(g.is_locked("admin", ip(13, 1)).is_none());
    }

    #[test]
    fn lockout_reports_remaining_duration() {
        let g = LoginGuard::with_durations(1, Duration::from_secs(120), Duration::from_secs(60));
        g.record_failure("admin", ip(14, 1));
        let remaining = g.is_locked("admin", ip(14, 1)).expect("locked");
        assert!(remaining <= Duration::from_secs(120) && remaining > Duration::from_secs(60));
    }

    #[test]
    fn from_config_scales_global_bucket() {
        let limits = AdminRateLimits::from_config(20.0, 40, 5, 15);
        // Per-IP burst 40 passes.
        for _ in 0..40 {
            assert!(limits.http.check(ip(15, 1)).is_ok());
        }
        assert!(limits.http.check(ip(15, 1)).is_err());
        // Global burst 400: fresh IPs still fine after one IP drained itself.
        assert!(limits.http.check(ip(15, 2)).is_ok());
    }
}
