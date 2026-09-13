//! SSRF URL validation (shared between proxy and mirror)
//!
//! This is the same logic as monopsony_proxy::security::validate_url_host.
//! Duplicated here to avoid a circular dependency (proxy -> control -> mirror -> proxy).
//! Future: move to monopsony-common.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use tracing::{debug, warn};

/// Check if an IP address is private/link-local/loopback (SSRF prevention).
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 127
                || octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 169 && octets[1] == 254)
                || octets[0] == 0
                || (224..=239).contains(&octets[0])
                || octets == [255, 255, 255, 255]
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.is_unspecified()
                || v6.segments()[0] == 0xff00
        }
    }
}

/// Validate that a URL host does not resolve to a private IP.
pub fn validate_url_host(url: &url::Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("Blocked scheme: {}", other)),
    }

    let addrs: Vec<SocketAddr> = format!("{}:0", host)
        .to_socket_addrs()
        .map(|iter| iter.collect())
        .unwrap_or_default();

    if addrs.is_empty() {
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
