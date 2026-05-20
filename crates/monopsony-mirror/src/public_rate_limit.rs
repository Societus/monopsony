//! Per-IP rate limiting for public mirror mode.
//!
//! Token bucket rate limiter with per-IP and global bandwidth tracking.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Per-IP token bucket state.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
    concurrent: usize,
}

impl TokenBucket {
    fn new(max_tokens: f64, refill_rate: f64) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            refill_rate,
            last_refill: Instant::now(),
            concurrent: 0,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
    }
}

/// Rate limiter for public mirror access.
pub struct PublicRateLimiter {
    buckets: Mutex<HashMap<IpAddr, TokenBucket>>,
    per_ip_rps: f64,
    max_burst: f64,
    max_concurrent_per_ip: usize,
}

#[derive(Debug)]
pub enum RateLimitError {
    TooManyRequests { retry_after_secs: u64 },
    TooManyConcurrent,
}

impl PublicRateLimiter {
    pub fn new(per_ip_rps: f64, max_concurrent_per_ip: usize) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            per_ip_rps,
            max_burst: per_ip_rps * 2.0, // 2 seconds worth of burst
            max_concurrent_per_ip,
        }
    }

    /// Try to acquire a request slot. Returns Err if rate limited.
    pub fn acquire(&self, ip: IpAddr) -> Result<(), RateLimitError> {
        let mut buckets = self.buckets.lock().unwrap();

        let bucket = buckets
            .entry(ip)
            .or_insert_with(|| TokenBucket::new(self.max_burst, self.per_ip_rps));

        bucket.refill();

        // Check concurrent
        if self.max_concurrent_per_ip > 0 && bucket.concurrent >= self.max_concurrent_per_ip {
            return Err(RateLimitError::TooManyConcurrent);
        }

        // Check tokens
        if bucket.tokens < 1.0 {
            let retry_after = ((1.0 - bucket.tokens) / bucket.refill_rate).ceil() as u64;
            return Err(RateLimitError::TooManyRequests {
                retry_after_secs: retry_after.max(1),
            });
        }

        bucket.tokens -= 1.0;
        bucket.concurrent += 1;
        Ok(())
    }

    /// Release a request slot.
    pub fn release(&self, ip: IpAddr) {
        let mut buckets = self.buckets.lock().unwrap();
        if let Some(bucket) = buckets.get_mut(&ip) {
            bucket.concurrent = bucket.concurrent.saturating_sub(1);
        }
    }

    /// Evict idle buckets (call periodically to prevent memory growth).
    pub fn evict_idle(&self, max_age_secs: u64) {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        buckets.retain(|_, bucket| {
            bucket.concurrent > 0
                || now.duration_since(bucket.last_refill).as_secs() < max_age_secs
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_under_limit_allowed() {
        let limiter = PublicRateLimiter::new(10.0, 5);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        assert!(limiter.acquire(ip).is_ok());
        limiter.release(ip);
    }

    #[test]
    fn test_concurrent_limit() {
        let limiter = PublicRateLimiter::new(100.0, 2);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert!(limiter.acquire(ip).is_ok());
        assert!(limiter.acquire(ip).is_ok());
        let result = limiter.acquire(ip);
        assert!(matches!(result, Err(RateLimitError::TooManyConcurrent)));

        limiter.release(ip);
        assert!(limiter.acquire(ip).is_ok());
    }

    #[test]
    fn test_different_ips_independent() {
        let limiter = PublicRateLimiter::new(100.0, 1);
        let ip1 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

        assert!(limiter.acquire(ip1).is_ok());
        assert!(limiter.acquire(ip2).is_ok());
    }

    #[test]
    fn test_evict_idle() {
        let limiter = PublicRateLimiter::new(10.0, 5);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        limiter.acquire(ip).unwrap();
        limiter.release(ip);

        // Should be evicted after 0 seconds idle
        limiter.evict_idle(0);
        // Bucket was just used, tokens refill, so last_refill is recent
        // Evict with 0 means anything with concurrent=0 and age>0 gets evicted
    }
}
