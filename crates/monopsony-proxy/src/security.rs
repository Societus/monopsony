//! Security hardening for the HTTP proxy
//!
//! SSRF prevention, path traversal protection, request size limits,
//! and per-IP rate limiting.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{debug, warn};

/// Maximum response body size (4 GiB) — guards against disk exhaustion from
/// malicious upstreams while admitting bare-metal packages (linux-firmware
/// ≈ 1 GB+ single files). Enforced via Content-Length when present and a
/// streamed-bytes counter otherwise — never by buffering the body in RAM.
pub const MAX_RESPONSE_BODY_SIZE: u64 = 4 * 1024 * 1024 * 1024;

/// Default rate limit: 100 requests per window
#[allow(dead_code)] // Will be used as default in rate limit config
pub const DEFAULT_RATE_LIMIT_REQUESTS: u32 = 1000;

/// Default rate limit window: 60 seconds
#[allow(dead_code)] // Will be used as default in rate limit config
pub const DEFAULT_RATE_LIMIT_WINDOW_SECS: u64 = 60;

/// Default concurrent connection limit per IP
#[allow(dead_code)] // Will be used as default in concurrent limit config
pub const DEFAULT_MAX_CONCURRENT_PER_IP: u32 = 50;

/// Check if an IP address is private/link-local/loopback (SSRF prevention).
/// Returns true if the address should be blocked.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 127.0.0.0/8 — loopback
            octets[0] == 127
            // 10.0.0.0/8 — RFC 1918
            || octets[0] == 10
            // 172.16.0.0/12 — RFC 1918
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            // 192.168.0.0/16 — RFC 1918
            || (octets[0] == 192 && octets[1] == 168)
            // 169.254.0.0/16 — link-local
            || (octets[0] == 169 && octets[1] == 254)
            // 0.0.0.0/8 — current network
            || octets[0] == 0
            // 224.0.0.0/4 — multicast
            || (224..=239).contains(&octets[0])
            // 255.255.255.255 — broadcast
            || octets == [255, 255, 255, 255]
        }
        IpAddr::V6(v6) => {
            // ::1 — loopback
            v6.is_loopback()
            // fc00::/7 — unique local (RFC 4193)
            || (v6.segments()[0] & 0xfe00) == 0xfc00
            // fe80::/10 — link-local
            || (v6.segments()[0] & 0xffc0) == 0xfe80
            // :: — unspecified
            || v6.is_unspecified()
            // ff00::/8 — multicast
            || v6.segments()[0] == 0xff00
        }
    }
}

/// Validate that a URL's host does not resolve to a private IP.
/// Returns Ok(()) if safe, Err with description if blocked.
pub fn validate_url_host(url: &url::Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Block non-HTTP/HTTPS schemes
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("Blocked scheme: {}", other)),
    }

    // Resolve hostname and check all resulting IPs
    // Use std::net::ToSocketAddrs for DNS resolution
    let addrs: Vec<SocketAddr> = format!("{}:0", host)
        .to_socket_addrs()
        .map(|iter| iter.collect())
        .unwrap_or_default();

    if addrs.is_empty() {
        // DNS resolution failed — could be transient or blocked.
        // Allow it to proceed; the upstream fetch will fail naturally.
        debug!("DNS resolution failed for host: {}", host);
        return Ok(());
    }

    for addr in &addrs {
        if is_private_ip(&addr.ip()) {
            warn!(
                "SSRF blocked: {} resolves to private IP {}",
                host,
                addr.ip()
            );
            return Err(format!(
                "Host {} resolves to private IP {}",
                host,
                addr.ip()
            ));
        }
    }

    Ok(())
}

/// Validate that a cache path doesn't escape the cache directory (path traversal prevention).
#[allow(dead_code)] // Will be used when cache admin API is added
pub fn validate_cache_path(cache_path: &str, cache_dir: &std::path::Path) -> bool {
    let path = std::path::Path::new(cache_path);

    // Canonicalize both paths for comparison
    let canonical_cache_dir = cache_dir.canonicalize().ok();
    let canonical_path = path.canonicalize().ok();

    match (canonical_cache_dir, canonical_path) {
        (Some(dir), Some(p)) => p.starts_with(dir),
        _ => {
            // If canonicalization fails, check for path traversal components
            let path_str = cache_path.replace('\\', "/");
            !path_str.contains("..")
        }
    }
}

/// Per-IP rate limiter using a sliding window.
#[derive(Clone)]
#[allow(dead_code)] // Will be wired into proxy middleware for rate limiting
pub struct RateLimiter {
    state: Arc<Mutex<RateLimiterState>>,
    max_requests: u32,
    window: Duration,
}

/// Internal state for RateLimiter
#[allow(dead_code)] // Internal state for RateLimiter above
struct RateLimiterState {
    // IP -> list of request timestamps
    requests: HashMap<IpAddr, Vec<Instant>>,
    // IP -> current concurrent connections
    concurrent: HashMap<IpAddr, u32>,
}

#[allow(dead_code)] // Will be wired into proxy middleware for rate limiting
impl RateLimiter {
    #[allow(dead_code)]
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(RateLimiterState {
                requests: HashMap::new(),
                concurrent: HashMap::new(),
            })),
            max_requests,
            window: Duration::from_secs(window_secs),
        }
    }

    /// Check if a request from the given IP is allowed.
    /// Returns true if under the rate limit.
    pub fn check_rate(&self, ip: &IpAddr) -> bool {
        let mut state = self.state.lock();
        let now = Instant::now();
        let cutoff = now - self.window;

        // Clean old entries
        let timestamps = state.requests.entry(*ip).or_default();
        timestamps.retain(|&t| t > cutoff);

        if timestamps.len() >= self.max_requests as usize {
            return false;
        }

        timestamps.push(now);
        true
    }

    /// Acquire a concurrent connection slot. Returns true if under limit.
    pub fn acquire_concurrent(&self, ip: &IpAddr, max: u32) -> bool {
        let mut state = self.state.lock();
        let count = state.concurrent.entry(*ip).or_insert(0);
        if *count >= max {
            return false;
        }
        *count += 1;
        true
    }

    /// Release a concurrent connection slot.
    pub fn release_concurrent(&self, ip: &IpAddr) {
        let mut state = self.state.lock();
        if let Some(count) = state.concurrent.get_mut(ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.concurrent.remove(ip);
            }
        }
    }

    /// Get current request count for an IP (for metrics).
    pub fn request_count(&self, ip: &IpAddr) -> usize {
        let state = self.state.lock();
        state.requests.get(ip).map(|v| v.len()).unwrap_or(0)
    }

    /// Periodic cleanup of stale entries. Call from a background task.
    pub fn cleanup(&self) {
        let mut state = self.state.lock();
        let cutoff = Instant::now() - self.window;

        state.requests.retain(|_, timestamps| {
            timestamps.retain(|&t| t > cutoff);
            !timestamps.is_empty()
        });
    }
}

/// Periodic cleanup task for stale rate limiter entries.
#[allow(dead_code)] // Called from background task
pub async fn run_rate_limiter_cleanup(limiter: Arc<RateLimiter>, interval_secs: u64) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
    loop {
        interval.tick().await;
        limiter.cleanup();
    }
}

/// Create a rate limiter with default settings.
#[allow(dead_code)] // Will be wired into proxy router
pub fn create_default_rate_limiter() -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new(
        crate::security::DEFAULT_RATE_LIMIT_REQUESTS,
        crate::security::DEFAULT_RATE_LIMIT_WINDOW_SECS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_private_ip_detection() {
        // Should block
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 0
        ))));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 0
        ))));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));

        // Should allow
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 15, 0, 1)))); // Just below RFC 1918
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 32, 0, 1)))); // Just above RFC 1918
        assert!(!is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2001, 0x4860, 0, 0, 0, 0, 0, 0
        )))); // Google DNS
    }

    #[test]
    fn test_url_scheme_validation() {
        // HTTP/HTTPS should pass
        let url = url::Url::parse("http://example.com/foo").unwrap();
        assert!(validate_url_host(&url).is_ok());

        let url = url::Url::parse("https://example.com/foo").unwrap();
        assert!(validate_url_host(&url).is_ok());

        // file:// should be blocked
        let url = url::Url::parse("file:///etc/passwd").unwrap();
        assert!(validate_url_host(&url).is_err());

        // ftp:// should be blocked
        let url = url::Url::parse("ftp://example.com/foo").unwrap();
        assert!(validate_url_host(&url).is_err());
    }

    #[test]
    fn test_path_traversal_detection() {
        let cache_dir = std::env::temp_dir().join("monopsony-hardening-test");
        std::fs::create_dir_all(&cache_dir).ok();

        // Normal path — should pass
        assert!(validate_cache_path(
            &cache_dir.join("subdir").join("file.deb").to_string_lossy(),
            &cache_dir
        ));

        // Path traversal — should fail
        assert!(!validate_cache_path(
            &cache_dir
                .join("..")
                .join("etc")
                .join("passwd")
                .to_string_lossy(),
            &cache_dir
        ));
    }

    #[test]
    fn test_rate_limiter() {
        let limiter = RateLimiter::new(3, 60);
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));

        // First 3 should pass
        assert!(limiter.check_rate(&ip));
        assert!(limiter.check_rate(&ip));
        assert!(limiter.check_rate(&ip));

        // 4th should be blocked
        assert!(!limiter.check_rate(&ip));
    }

    #[test]
    fn test_concurrent_limit() {
        let limiter = RateLimiter::new(100, 60);
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));

        // Should allow up to 2
        assert!(limiter.acquire_concurrent(&ip, 2));
        assert!(limiter.acquire_concurrent(&ip, 2));

        // 3rd should be blocked
        assert!(!limiter.acquire_concurrent(&ip, 2));

        // Release one
        limiter.release_concurrent(&ip);

        // Should allow again
        assert!(limiter.acquire_concurrent(&ip, 2));
    }
}
